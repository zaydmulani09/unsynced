//! A tiny append-only key-value log, checked by unsynced.
//!
//! The naive version fsyncs every append yet still has two crash bugs:
//! records have no checksum (a torn append recovers as garbage), and the
//! log's directory entry is never made durable (acknowledged puts vanish).
//! The fixed version checksums records and fsyncs the directory once.
//!
//!     cargo run --example wal

use std::io::Write;

use unsynced::{Crash, Options, Recorder, Trace, check};

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
fn recover(mut log: &[u8], checksum: bool) -> Vec<String> {
    let header = if checksum { 8 } else { 4 };
    let mut out = Vec::new();
    while log.len() >= header {
        let len = u32::from_le_bytes(log[..4].try_into().unwrap()) as usize;
        if len == 0 || log.len() < header + len {
            break;
        }
        let payload = &log[header..header + len];
        if checksum && crc32(payload) != u32::from_le_bytes(log[4..8].try_into().unwrap()) {
            break;
        }
        out.push(String::from_utf8_lossy(payload).into_owned());
        log = &log[header + len..];
    }
    out
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

/// Recovery must return a prefix of the puts containing every acknowledged one.
fn checker(crash: &Crash, fixed: bool) -> Result<(), String> {
    let bytes = std::fs::read(crash.dir.join("kv.log")).unwrap_or_default();
    let got = recover(&bytes, fixed);
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

fn main() -> Result<(), Box<dyn std::error::Error>> {
    for fixed in [false, true] {
        println!("== {} log", if fixed { "fixed" } else { "naive" });
        let report = check(&workload(fixed)?, &Options::default(), |c| checker(c, fixed))?;
        println!("{report}");
    }
    Ok(())
}
