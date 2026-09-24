//! End-to-end: trace real processes with strace and check them through the CLI.
//! Linux only; skipped when strace is not installed.
#![cfg(target_os = "linux")]

use std::path::PathBuf;
use std::process::Command;

fn have_strace() -> bool {
    Command::new("strace").arg("-V").output().is_ok_and(|o| o.status.success())
}

fn seed(name: &str, files: &[(&str, &str)]) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("unsynced-e2e-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    for (p, d) in files {
        std::fs::write(dir.join(p), d).unwrap();
    }
    dir
}

fn unsynced(args: &[&str]) -> (i32, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_unsynced")).args(args).output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
    println!("$ unsynced {}\n{text}", args.join(" "));
    (out.status.code().unwrap_or(-1), text)
}

/// cfg must be v1 or v2, and v2 once "saved" was printed.
const CHECK: &str =
    r#"c=$(cat {dir}/cfg 2>/dev/null); [ "$c" = v2 ] || { ! grep -q saved {marks} && [ "$c" = v1 ]; }"#;

#[test]
fn shell_replace_without_fsync_is_caught() {
    if !have_strace() {
        eprintln!("strace not installed; skipping");
        return;
    }
    let init = seed("sh-init", &[("cfg", "v1")]);
    let init = init.to_str().unwrap();
    let workload = "printf v2 > {dir}/tmp && mv {dir}/tmp {dir}/cfg && echo saved";
    let (code, out) = unsynced(&["run", "--init", init, "--check", CHECK, "--", "sh", "-c", workload]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("reordering") && out.contains("fsync `tmp` before `rename tmp -> cfg`"), "{out}");
}

#[test]
fn python_correct_recipe_is_clean() {
    if !have_strace() {
        eprintln!("strace not installed; skipping");
        return;
    }
    let init = seed("py-init", &[("cfg", "v1")]);
    let script = r#"
import os, sys
d = sys.argv[1]
with open(f"{d}/tmp", "w") as f:
    f.write("v2"); f.flush(); os.fsync(f.fileno())
os.rename(f"{d}/tmp", f"{d}/cfg")
fd = os.open(d, os.O_RDONLY); os.fsync(fd); os.close(fd)
print("saved", flush=True)
"#;
    let (code, out) = unsynced(&[
        "run",
        "--init",
        init.to_str().unwrap(),
        "--check",
        CHECK,
        "--",
        "python3",
        "-c",
        script,
        "{dir}",
    ]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("OK"), "{out}");
}

#[test]
fn record_then_check_roundtrips_a_bundle() {
    if !have_strace() {
        eprintln!("strace not installed; skipping");
        return;
    }
    let bundle = std::env::temp_dir().join("unsynced-e2e-bundle");
    let _ = std::fs::remove_dir_all(&bundle);
    let (code, out) = unsynced(&[
        "record",
        "-o",
        bundle.to_str().unwrap(),
        "--",
        "sh",
        "-c",
        "echo hello >> {dir}/log && echo more >> {dir}/log && sync",
    ]);
    assert_eq!(code, 0, "{out}");
    let (_, shown) = unsynced(&["show", bundle.to_str().unwrap()]);
    assert!(shown.contains("write log [0..6)") && shown.contains("write log [6..11)"), "{shown}");
    assert!(shown.contains("sync"), "{shown}");
    let (code, out) = unsynced(&["check", bundle.to_str().unwrap(), "--check", "true"]);
    assert_eq!(code, 0, "{out}");
}

#[test]
fn recovery_that_rewrites_in_place_is_caught() {
    if !have_strace() {
        eprintln!("strace not installed; skipping");
        return;
    }
    // Two records; the first is synced and acknowledged. Recovery "cleans" the
    // log by rewriting it in place, which a second crash can interrupt.
    let workload = "printf 'a\n' >> {dir}/log && sync && echo saved && printf 'b\n' >> {dir}/log";
    let recover = "grep -x -e a -e b {dir}/log > {dir}/clean; cat {dir}/clean > {dir}/log; rm {dir}/clean";
    let check = "grep -qx a {dir}/log || ! grep -q saved {marks}";
    let (code, out) = unsynced(&["run", "--check", check, "--recover", recover, "--", "sh", "-c", workload]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("during recovery") && out.contains("recovery ran and crashed again"), "{out}");
}
