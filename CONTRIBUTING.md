# Contributing

Bug reports with a trace bundle attached (`unsynced record -o bundle -- …`)
are the most useful kind: they reproduce anywhere with `unsynced check`.

Before sending a change:

```sh
cargo fmt
cargo clippy --all-targets -- -D warnings
cargo test
```

`tests/strace_e2e.rs` only runs on Linux with `strace` installed; CI covers it.

Where things live:

| file | what |
|---|---|
| `src/trace.rs` | trace and bundle format |
| `src/model.rs` | compiling ops to micro-ops, durability barriers, profiles |
| `src/crash.rs` | crash-state enumeration and profile closure |
| `src/check.rs` | checking, minimization, classification, reports, recovery |
| `src/record.rs` | the in-process `Recorder` |
| `src/strace.rs` | the strace frontend |
| `src/main.rs` | the CLI |

Changes to what counts as a legal crash state belong in
[docs/model.md](docs/model.md) as well as in code, with a test in
`tests/patterns.rs` or the module's unit tests.
