# unsynced

[![CI](https://github.com/zaydmulani09/unsynced/actions/workflows/ci.yml/badge.svg)](https://github.com/zaydmulani09/unsynced/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/unsynced.svg)](https://crates.io/crates/unsynced)
[![docs.rs](https://docs.rs/unsynced/badge.svg)](https://docs.rs/unsynced)

**Find the crash-consistency bugs in your program.** `unsynced` records what a
program does to a directory, works out every on-disk state a power loss could
leave behind, and runs your checker against each one. When a state fails, it
cuts the failure down to the few lost operations that cause it and tells you
which `fsync` is missing.

```text
$ unsynced run --check 'python3 check.py {dir} {marks}' -- python3 workload.py {dir} delete full
101 ops, 102 crash points, 570 unique crash states checked (posix model)
FAIL: 5 crash states rejected, 1 distinct vulnerability:

[1] unsynced (5 failing states)
  crash after   #27 mark "committed 1"
  lost          #26 unlink test.db-journal
  checker said  commit 1 was acknowledged but only 0 rows survived
  fix           fsync the directory `.` after `unlink test.db-journal` and before acknowledging ("committed 1")
```

That's a real run on SQLite: in its default rollback-journal mode with
`synchronous=FULL`, a commit becomes durable only when the journal's *deletion*
is durable, and SQLite doesn't fsync the directory after the delete. unsynced
found this with no knowledge of SQLite. It's the documented gap that
[`synchronous=EXTRA`](https://www.sqlite.org/pragma.html#pragma_synchronous) closes
([full results below](#case-study-sqlite)).

## Why

`write()` returning doesn't make data durable, and even `fsync()` doesn't
cover everything. File systems may persist writes out of order, tear them at
block boundaries, save a file's new size without its data, and drop a
directory entry unless *the directory* is fsynced. Code that works in
every test can still lose acknowledged data, or corrupt its state, when the
power fails at the wrong moment. Studies keep finding these bugs in mature
software: [Pillai et al. (OSDI '14)](https://www.usenix.org/conference/osdi14/technical-sessions/presentation/pillai)
found 60 of them across 11 widely used applications, including databases and
version-control systems.

Tools for finding them are scarce. ALICE, from that paper, is unmaintained,
and projects like HashiCorp's raft-wal and Dgraph keep their own patched forks. LazyFS
injects lost writes through FUSE but doesn't search for bugs. Kernel-level
testers (CrashMonkey, ACE) target file systems, not applications. unsynced
is a maintained, single-binary tool (and Rust library) that does the search
and explains what it finds.

## Install

```sh
cargo install unsynced
```

The CLI traces programs with `strace` on Linux (`apt install strace`). The
Rust library works on every OS.

## Use it on any program (Linux)

```sh
unsynced run --check 'CHECK {dir} {marks}' -- WORKLOAD {dir}
```

- `WORKLOAD` runs once, traced, against a fresh directory (`{dir}`; seed it
  with `--init DIR`). Whatever it prints to stdout is recorded as
  *acknowledgements*: print `committed 42` after a commit returns.
- `CHECK` runs once per unique crash state, on a directory holding that state.
  `{marks}` is a file holding what the workload had printed before the crash.
  Exit 0 if the state is acceptable.
- Exit status: `0` no bugs, `1` bugs found, `2` error. It works in CI.

```sh
# Is replace-by-rename safe without fsync? (No.)
unsynced run --init seed/ \
  --check 'c=$(cat {dir}/cfg); [ "$c" = v2 ] || { ! grep -q saved {marks} && [ "$c" = v1 ]; }' \
  -- sh -c 'printf v2 > {dir}/tmp && mv {dir}/tmp {dir}/cfg && echo saved'
```

```text
4 ops, 5 crash points, 14 unique crash states checked (posix model)
FAIL: 8 crash states rejected, 2 distinct vulnerabilities:

[1] reordering (7 failing states)
  crash after   #2 rename tmp -> cfg
  lost          #1 write tmp [0..2)
  but persisted #2 rename tmp -> cfg
  checker said  checker failed (exit status: 1)
  fix           fsync `tmp` before `rename tmp -> cfg` (op #2); the rename reached disk before the data it points to

[2] unsynced (1 failing states)
  crash after   #3 mark "saved"
  lost          #2 rename tmp -> cfg
  checker said  checker failed (exit status: 1)
  fix           fsync the directory `.` after `rename tmp -> cfg` and before acknowledging ("saved")
```

`unsynced record -o BUNDLE -- …` saves a trace; `unsynced check BUNDLE --check …`
checks it later, anywhere. `unsynced show BUNDLE` lists its operations.

### Crash the recovery too

Recovery code (journal rollback, WAL replay, repairing a torn tail) writes to
disk, so a second crash can interrupt it. With `--recover CMD`, every crash
state is first repaired by `CMD`, which is itself traced and crashed at each
of its steps before being run again and checked:

```sh
unsynced run --check 'CHECK {dir} {marks}' --recover 'RECOVER {dir}' -- WORKLOAD {dir}
```

## Use it from Rust (any OS)

Do your file operations through a `Recorder` and check the trace in a test:

```rust
use std::io::Write;
use unsynced::{Options, Recorder, check};

let rec = Recorder::new(&dir)?;
let mut f = rec.create("config.tmp")?;
f.write_all(b"v2")?;
f.sync_all()?;
rec.rename("config.tmp", "config")?;
rec.sync_dir("")?;
rec.mark("saved");
let trace = rec.finish();

let report = check(&trace, &Options::default(), |crash| {
    let got = std::fs::read(crash.dir.join("config")).unwrap_or_default();
    let saved = crash.marks.iter().any(|m| m == "saved");
    match (saved, got.as_slice()) {
        (_, b"v2") | (false, b"v1") => Ok(()),
        _ => Err(format!("config = {got:?}")),
    }
})?;
assert!(report.is_clean(), "{report}");
```

`check_with_recovery(trace, opts, recover, verify)` crash-tests recovery the
same way. [`examples/wal.rs`](examples/wal.rs) is a small append-only log that
shows it end to end: a naive log (a torn write and a missing directory fsync),
a fixed one, and two ways to repair a torn tail of which only one survives a
crash during recovery:

```sh
cargo run --example wal
```

## Case study: SQLite

[`examples/sqlite`](examples/sqlite) commits five transactions under each
journal mode and `synchronous` level, printing `committed N` after each. The
checker requires `integrity_check` to pass and every acknowledged row to be
present. CI runs it on every push (SQLite 3.45, posix model):

| journal_mode | synchronous | unsynced finds | SQLite's documentation says |
|---|---|---|---|
| delete | full | acknowledged commit lost: journal `unlink` not durable (1 root cause) | EXTRA adds a directory sync after the journal is unlinked "to commit a transaction" |
| delete | extra | nothing (656 states) | durable |
| wal | normal | acknowledged commit lost: WAL not fsynced before the commit returns (1 root cause) | "a transaction committed in WAL mode with synchronous=NORMAL might roll back following a power loss" |
| wal | full | nothing (1132 states) | durable |
| delete | off | `database disk image is malformed`: journal reordered after the database write, torn journal pages | "the database might become corrupted" |

Every verdict matches SQLite's documented guarantees. unsynced was told
nothing about SQLite; it only watched the syscalls. In WAL mode SQLite also
writes its shared-memory index through `mmap`, which unsynced can't see and
warns about. SQLite rebuilds that index on recovery, so the verdicts hold.

With `--recover`, the harness also crashes SQLite's own recovery. Opening the
database rolls back a hot journal, or replays and checkpoints the WAL, and
each of those runs is traced and crashed at every step:

| journal_mode | synchronous | workload states | recovery crash states | result |
|---|---|---|---|---|
| delete | extra | 656 | 365 | clean |
| wal | full | 1132 | 4000 (stopped by `--max-states`) | clean |

## How it works

```text
 workload ──strace / Recorder──▶ trace ──compile──▶ micro-ops ──enumerate──▶ crash states
                                                                               │ dedupe
 report ◀── group ◀── classify ◀── minimize ◀── failures ◀── your checker ◀────┘ (parallel)
```

1. **Record.** The strace frontend runs `strace -f -y -xx`. The `-y` flag
   annotates every fd with its current path, so renames and `openat` dirfds
   resolve without extra work. It tracks open file descriptions to recover
   the offsets of plain `write()`s through `lseek`, `O_APPEND`, `dup` and
   `fork`. The `Recorder` does the same in-process.
2. **Compile.** Ops are replayed on an inode-level model and split into the
   units a file system may persist independently: directory-entry changes,
   block-sized write chunks, size changes. Each one is tagged with the
   barrier (`fsync`/`sync`) that makes it durable.
3. **Enumerate.** At each crash point, durable micro-ops persist and the rest
   persist in any subset the persistence model allows: exhaustively when
   there are few, otherwise with a fixed battery of strategies plus seeded
   random samples. States are deduplicated by content.
4. **Check.** Your checker runs on each unique state, in parallel.
5. **Explain.** Each failure is greedily minimized to the lost operations
   that are enough to cause it. It's then classified as non-atomic update,
   unsynced (acknowledged before durable), torn write or reordering, grouped
   by root cause, and turned into a concrete fix.

The persistence model is specified in [docs/model.md](docs/model.md). Two profiles:

| | `posix` (default) | `ext4` (`data=ordered`) |
|---|---|---|
| unsynced ops lost independently | yes | metadata only in order |
| file size persists without its data | yes | no |
| `fsync(file)` makes its new directory entry durable | no | yes (one journal) |
| replace-by-rename flushes data first (`auto_da_alloc`) | no | yes |

`posix` is the weakest behavior the standard allows, and what portable code
should survive. `ext4` shows why a bug you can't reproduce on your laptop is
still a bug: `tests/patterns.rs` pins the classic replace-by-rename-without-
fsync pattern as *corrupting* under `posix` but only *non-durable* under
`ext4`.

## Limitations

- **Frontends.** Tracing arbitrary programs needs Linux and strace. Writes
  through `mmap`, `fallocate`, `copy_file_range`, and hard or symbolic links
  aren't modeled; unsynced warns when a workload uses them under the root.
- **Search.** Beyond 10 in-flight micro-ops per crash point the search samples
  instead of enumerating, so a clean report is strong evidence, not a proof.
  `--exhaustive`, `--samples` and `--max-states` trade time for coverage.
- **Scale.** Every candidate state is materialized and hashed, so cost grows
  with ops × in-flight window × data size. A few hundred ops enumerate in well
  under a second; 2,000 appends with an fsync every 50 take about 20 s (82k
  unique states) before the checker runs. Record a focused workload, not a
  whole test suite.
- **Model.** `fdatasync` is treated as `fsync`. Drives that ignore flushes,
  and file systems other than the two profiles (XFS, btrfs, APFS, NTFS),
  aren't modeled.
- **Your checker is the oracle.** unsynced finds states your checker rejects;
  write it to demand what your program promises.

## Roadmap

- More persistence profiles (XFS, btrfs, APFS, where `fsync` doesn't flush
  the drive but `F_FULLFSYNC` does).
- An `LD_PRELOAD` frontend (no strace, works for macOS/FreeBSD) and a FUSE
  frontend that can see `mmap` writes.
- Smarter search: partial-order reduction across independent files, so
  exhaustive search scales to bigger workloads.
- Recorders for more languages, emitting the [bundle format](src/trace.rs).

## License

Licensed under either of [Apache License 2.0](LICENSE-APACHE) or
[MIT](LICENSE-MIT) at your option.
