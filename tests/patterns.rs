//! Ground truth: the four classic ways to update a file, and what each one
//! survives. Expected verdicts follow Pillai et al., "All File Systems Are Not
//! Created Equal" (OSDI '14) and the ext4 `auto_da_alloc` heuristic.

use std::io::Write;
use std::path::PathBuf;

use unsynced::{Crash, Kind, Options, Profile, Recorder, Report, Trace, check};

const OLD: &str = "version=1\n";
const NEW: &str = "version=2\n";

#[derive(Clone, Copy, Debug)]
enum Pattern {
    /// Truncate and rewrite the file in place, then fsync.
    InPlace,
    /// Write a temp file, rename it over the original. No fsync at all.
    RenameNoFsync,
    /// Write a temp file, fsync it, rename it. Directory never fsynced.
    RenameNoDirFsync,
    /// Write, fsync, rename, fsync the directory. The correct recipe.
    Correct,
}

fn workload(p: Pattern, profile: Profile) -> Trace {
    let dir: PathBuf = std::env::temp_dir().join(format!("unsynced-pattern-{p:?}-{profile:?}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("config"), OLD).unwrap();

    let rec = Recorder::new(&dir).unwrap();
    let target = if matches!(p, Pattern::InPlace) { "config" } else { "config.tmp" };
    let mut f = rec.create(target).unwrap();
    f.write_all(NEW.as_bytes()).unwrap();
    if !matches!(p, Pattern::RenameNoFsync) {
        f.sync_all().unwrap();
    }
    drop(f);
    if !matches!(p, Pattern::InPlace) {
        rec.rename("config.tmp", "config").unwrap();
    }
    if matches!(p, Pattern::Correct) {
        rec.sync_dir("").unwrap();
    }
    rec.mark("saved");
    rec.finish()
}

/// The config must always be the old or the new version, and the new one once "saved" was printed.
fn checker(crash: &Crash) -> Result<(), String> {
    let got = std::fs::read_to_string(crash.dir.join("config")).map_err(|e| format!("config missing: {e}"))?;
    let acked = crash.marks.iter().any(|m| m == "saved");
    match got.as_str() {
        NEW => Ok(()),
        OLD if !acked => Ok(()),
        OLD => Err("acknowledged update lost".into()),
        other => Err(format!("config corrupted: {other:?}")),
    }
}

fn run(p: Pattern, profile: Profile) -> Report {
    let opts = Options { profile, jobs: 2, ..Options::default() };
    let report = check(&workload(p, profile), &opts, checker).unwrap();
    println!("--- {p:?} under {profile:?}\n{report}");
    report
}

fn corrupts(r: &Report) -> bool {
    r.vulnerabilities.iter().any(|v| !v.message.contains("acknowledged"))
}

#[test]
fn in_place_rewrite_is_never_atomic() {
    for profile in [Profile::Posix, Profile::Ext4Ordered] {
        let r = run(Pattern::InPlace, profile);
        assert!(corrupts(&r), "{r}");
        // Even with every write persisted, a crash between truncate and write
        // leaves an empty file: the bug is in the workload, not the disk.
        assert!(r.vulnerabilities.iter().any(|v| v.kind == Kind::NonAtomic && v.lost.is_empty()), "{r}");
    }
}

#[test]
fn rename_without_fsync_corrupts_on_posix_but_not_ext4() {
    let posix = run(Pattern::RenameNoFsync, Profile::Posix);
    assert!(corrupts(&posix), "{posix}");
    assert!(posix.vulnerabilities.iter().any(|v| v.hint.contains("before `rename")), "{posix}");
    // auto_da_alloc flushes the data before a replacing rename commits, so ext4
    // only loses durability (the rename itself can be rolled back).
    let ext4 = run(Pattern::RenameNoFsync, Profile::Ext4Ordered);
    assert!(!ext4.is_clean() && !corrupts(&ext4), "{ext4}");
}

#[test]
fn missing_directory_fsync_loses_acknowledged_updates() {
    for profile in [Profile::Posix, Profile::Ext4Ordered] {
        let r = run(Pattern::RenameNoDirFsync, profile);
        assert!(!r.is_clean() && !corrupts(&r), "{r}");
        assert!(r.vulnerabilities.iter().all(|v| v.hint.contains("fsync the directory")), "{r}");
    }
}

#[test]
fn the_correct_recipe_is_clean() {
    for profile in [Profile::Posix, Profile::Ext4Ordered] {
        let r = run(Pattern::Correct, profile);
        assert!(r.is_clean(), "{r}");
    }
}
