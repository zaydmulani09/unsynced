# Changelog

## 0.1.0

First release.

- Trace format: an initial directory snapshot plus file-system ops, stored as
  a bundle (`initial/` + `trace.jsonl`).
- Persistence model: traces compile to micro-ops (directory entries,
  block-sized write chunks, size changes) tagged with the barrier that makes
  them durable; `posix` and `ext4` (data=ordered, auto_da_alloc) profiles.
- Crash-state enumeration: exhaustive for small in-flight sets, otherwise a
  fixed battery of strategies plus seeded samples; content-based dedupe.
- Checking: parallel checker runs, greedy minimization, classification
  (non-atomic update, unsynced, torn write, reordering), grouping by root
  cause, and concrete fixes.
- Crashes during recovery: `check_with_recovery` / `--recover`.
- Frontends: `Recorder` (in-process, any OS) and strace (any program, Linux).
- CLI: `run`, `record`, `check`, `show`; JSON reports; exit status 1 on bugs.
