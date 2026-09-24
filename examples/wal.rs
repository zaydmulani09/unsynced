//! A tiny append-only key-value log, checked by unsynced.
//!
//! 1. The naive log fsyncs every append yet still has two crash bugs: records
//!    have no checksum (a torn append recovers as garbage), and the log's
//!    directory entry is never made durable (acknowledged puts vanish).
//! 2. The fixed log checksums records and fsyncs the directory once.
//! 3. Recovery must cut a torn tail off before new appends. Rewriting the log
//!    with its valid records looks equivalent to truncating it, but only the
//!    truncate survives a second crash *during* recovery.
//!
//!     cargo run --example wal

use std::io::Write;
use std::path::Path;

use unsynced::{Crash, Options, Recorder, Trace, check, check_with_recovery};

const PUTS: usize = 3;

fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & (crc & 1).wrapping_neg());
        }
    }
    !crc
}

/// `[len u32][crc u32 if checksummed][payload]`
fn encode(payload: &str, checksum: bool) -> Vec<u8> {
    let mut rec = (payload.len() as u32).to_le_bytes().to_vec();
    if checksum {
        rec.extend(crc32(payload.as_bytes()).to_le_bytes());
    }
    rec.extend(payload.as_bytes());
    rec
}

/// Read records until one is incomplete (or fails its checksum).
/// Returns them and the length of the valid prefix.
fn read_log(log: &[u8], checksum: bool) -> (Vec<String>, usize) {
    let header = if checksum { 8 } else { 4 };
    let (mut out, mut at) = (Vec::new(), 0);
    while log.len() - at >= header {
        let len = u32::from_le_bytes(log[at..at + 4].try_into().unwrap()) as usize;
        if len == 0 || log.len() - at < header + len {
            break;
        }
        let payload = &log[at + header..at + header + len];
        if checksum && crc32(payload) != u32::from_le_bytes(log[at + 4..at + 8].try_into().unwrap()) {
            break;
        }
        out.push(String::from_utf8_lossy(payload).into_owned());
        at += header + len;
    }
    (out, at)
}

fn value(i: usize) -> String {
    // Big enough that each record straddles a 4 KiB block boundary.
    format!("key{i}={}", char::from(b'a' + i as u8).to_string().repeat(3000))
}

fn workload(fixed: bool) -> std::io::Result<Trace> {
    let dir = std::env::temp_dir().join(format!("unsynced-wal-{fixed}"));
    let _ = std::fs::remove_dir_all(&dir);
    let rec = Recorder::new(&dir)?;
    let mut log = rec.append("kv.log")?;
    if fixed {
        rec.sync_dir("")?; // make the new file's directory entry durable
    }
    for i in 0..PUTS {
        log.write_all(&encode(&value(i), fixed))?;
        log.sync_data()?;
        rec.mark(format!("put {i}"));
    }
    Ok(rec.finish())
}

/// The log must hold a prefix of the puts containing every acknowledged one.
fn checker(crash: &Crash, fixed: bool) -> Result<(), String> {
    let bytes = std::fs::read(crash.dir.join("kv.log")).unwrap_or_default();
    let (got, _) = read_log(&bytes, fixed);
    for (i, v) in got.iter().enumerate() {
        if *v != value(i) {
            return Err(format!("record {i} recovered as garbage ({:?}...)", &v[..v.len().min(12)]));
        }
    }
    if got.len() < crash.marks.len() {
        return Err(format!("{} puts acknowledged, {} recovered", crash.marks.len(), got.len()));
    }
    Ok(())
}

/// Cut a torn tail off the log, recording what we do.
fn repair(dir: &Path, safe: bool) -> std::io::Result<Trace> {
    let rec = Recorder::new(dir)?;
    let bytes = std::fs::read(dir.join("kv.log")).unwrap_or_default();
    let (_, valid) = read_log(&bytes, true);
    if valid < bytes.len() {
        if safe {
            let mut f = rec.open("kv.log")?;
            f.set_len(valid as u64)?;
            f.sync_all()?;
        } else {
            let mut f = rec.create("kv.log")?; // truncates: the old records are now at risk
            f.write_all(&bytes[..valid])?;
            f.sync_all()?;
        }
    }
    Ok(rec.finish())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    for fixed in [false, true] {
        println!("== {} log", if fixed { "fixed" } else { "naive" });
        let report = check(&workload(fixed)?, &Options::default(), |c| checker(c, fixed))?;
        println!("{report}");
    }
    for safe in [false, true] {
        println!("== fixed log, tail repair by {}", if safe { "truncation" } else { "rewriting" });
        let report = check_with_recovery(
            &workload(true)?,
            &Options::default(),
            |dir| repair(dir, safe).map_err(|e| e.to_string()),
            |c| checker(c, true),
        )?;
        println!("{report}");
    }
    Ok(())
}
