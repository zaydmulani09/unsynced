//! Crashing the recovery: a repair that rewrites a file in place is only safe
//! if nothing interrupts it.

use std::io::Write;
use std::path::Path;

use unsynced::{Crash, Kind, Options, Recorder, Report, Trace, check_with_recovery};

/// Append "a" (synced, acknowledged), then "b" (neither).
fn workload(rewrite: bool) -> Trace {
    let dir = std::env::temp_dir().join(format!("unsynced-recovery-{rewrite}"));
    let _ = std::fs::remove_dir_all(&dir);
    let rec = Recorder::new(&dir).unwrap();
    let mut log = rec.append("log").unwrap();
    rec.sync_dir("").unwrap();
    log.write_all(b"a\n").unwrap();
    log.sync_all().unwrap();
    rec.mark("saved a");
    log.write_all(b"b\n").unwrap();
    rec.finish()
}

/// Keep only complete lines, either by rewriting the file or by truncating it.
fn repair(dir: &Path, rewrite: bool) -> Result<Trace, String> {
    let rec = Recorder::new(dir).map_err(|e| e.to_string())?;
    let data = std::fs::read(dir.join("log")).unwrap_or_default();
    let keep = data.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
    if keep < data.len() {
        let io = |e: std::io::Error| e.to_string();
        if rewrite {
            let mut f = rec.create("log").map_err(io)?;
            f.write_all(&data[..keep]).map_err(io)?;
            f.sync_all().map_err(io)?;
        } else {
            let mut f = rec.open("log").map_err(io)?;
            f.set_len(keep as u64).map_err(io)?;
            f.sync_all().map_err(io)?;
        }
    }
    Ok(rec.finish())
}

fn verify(c: &Crash) -> Result<(), String> {
    let data = std::fs::read_to_string(c.dir.join("log")).unwrap_or_default();
    if !c.marks.is_empty() && !data.starts_with("a\n") {
        return Err(format!("acknowledged record lost: {data:?}"));
    }
    if !data.lines().all(|l| l == "a" || l == "b") {
        return Err(format!("garbage in log: {data:?}"));
    }
    Ok(())
}

fn run(rewrite: bool) -> Report {
    let opts = Options { jobs: 2, ..Options::default() };
    let r = check_with_recovery(&workload(rewrite), &opts, |d| repair(d, rewrite), verify).unwrap();
    println!("{r}");
    r
}

#[test]
fn rewriting_repair_breaks_under_a_second_crash() {
    let r = run(true);
    assert!(r.recovery_states > 0, "{r}");
    assert!(r.vulnerabilities.iter().any(|v| v.in_recovery.is_some() && v.kind == Kind::NonAtomic), "{r}");
    // Every failure comes from crashing the recovery, not the workload.
    assert!(r.vulnerabilities.iter().all(|v| v.in_recovery.is_some()), "{r}");
}

#[test]
fn truncating_repair_survives_a_second_crash() {
    let r = run(false);
    assert!(r.recovery_states > 0 && r.is_clean(), "{r}");
}
