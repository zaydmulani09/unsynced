# The persistence model

This document specifies exactly which on-disk states `unsynced` considers
possible after a crash. Everything else in the tool (recording, checking,
minimization) is plumbing around this definition.

## Traces

A trace is an initial directory snapshot plus a sequence of operations
`op₀ … opₙ₋₁` issued by the workload:

| op | meaning |
|---|---|
| `create p`, `mkdir p` | new, empty file / directory |
| `write p off data` | write bytes at an offset (appends are writes at the old end) |
| `truncate p len` | set a file's length |
| `rename a b` | atomically move `a` to `b`, replacing `b` |
| `unlink p`, `rmdir p` | remove a directory entry |
| `fsync p` | `fsync`/`fdatasync` on a file or a directory |
| `sync` | flush everything |
| `mark text` | not a disk op: something the workload *told the world* (stdout) |

A crash **at point `k`** means ops `0..k` were issued and nothing after.

## Micro-ops

Ops are compiled, by replaying them on an inode-level model of the file
system, into the smallest units a file system may persist independently:

| micro-op | produced by |
|---|---|
| `Link(dir, name, ino)` | `create`, `mkdir` |
| `Unlink(dir, name)` | `unlink`, `rmdir` |
| `Rename(dir₁, name₁ → dir₂, name₂, ino)` | `rename` — one micro-op, so a rename is atomic |
| `Write(ino, off, bytes)` | `write`, split at every `block_size` boundary (default 4096) |
| `SetLen(ino, len)` | `truncate`, and every `write` that extends a file |

Micro-ops reference inodes, not paths, so a write issued before a rename still
lands in the renamed file. A file's visible content is its data buffer cut
(or zero-extended) to its length; bytes written past the persisted length are
invisible, and a length that persisted without its data exposes zeros.

## Durability

Each micro-op is *durable from* the first barrier that covers it:

| barrier | posix covers | ext4-ordered covers |
|---|---|---|
| `fsync` on file *f* | `Write`/`SetLen` on *f* | same, **plus every earlier metadata micro-op** (one journal) |
| `fsync` on directory *d* | `Link`/`Unlink` in *d*, `Rename` into *d* | same, plus every earlier metadata micro-op |
| `sync` | everything | everything |

Note what posix does **not** give you: `fsync` on a new file does not make its
directory entry durable, and a rename is durable only once its target
directory is synced.

## Crash states

For a crash at point `k`, let `D` be the micro-ops (of ops `0..k`) durable
from a barrier before `k`, and `U` the rest. A crash state is `D ∪ S` for a
subset `S ⊆ U` that is **closed** under the profile's rules. Its on-disk
content is the initial snapshot with the micro-ops of `D ∪ S` applied in
program order.

**posix** — no rules: any subset of `U` is legal. This is the weakest
behavior the POSIX specification permits and what portable code must
survive.

**ext4-ordered** (Linux default, `data=ordered`, `auto_da_alloc`):

1. *Journaled metadata commits in order.* If a metadata micro-op (`Link`,
   `Unlink`, `Rename`, `SetLen`) persisted, every earlier metadata micro-op
   persisted.
2. *Ordered data.* If a `SetLen` from a file-extending write persisted, every
   earlier `Write` to that inode persisted.
3. *`auto_da_alloc`.* If a `Rename` that replaced an existing file persisted,
   every earlier `Write` to the renamed inode persisted.

Closure only ever adds micro-ops, so any candidate subset extends to a unique
smallest legal state.

## Enumeration

Per crash point with `m = |U|`:

- `m ≤ exhaustive_limit` (default 10): all `2^m` subsets.
- otherwise: all/none, each single micro-op lost, each in-order prefix, each
  whole op lost, and `samples` (default 32) seeded random subsets.

States are closed, materialized, and deduplicated by
`(hash of the directory content, number of marks emitted)`; the checker runs
once per unique pair.

## Verdicts

A failing state is minimized greedily: re-persist lost ops (whole ops first,
then single micro-ops) while the checker keeps failing. The result is the set
of lost operations that is sufficient for the failure. It is then classified:

| kind | test on the minimized state |
|---|---|
| non-atomic update | nothing was lost: the workload itself passes through a state the checker rejects |
| unsynced | the state passes when the checker is told nothing was acknowledged: durable-before-acknowledged was violated |
| torn write | part of one write persisted and part did not |
| reordering | a later, non-durable op persisted while an earlier one was lost |

Failures are grouped by `(kind, files involved)`.

## Crashes during recovery

With `check_with_recovery` (CLI: `--recover`), each crash state is repaired by
the recovery function, which returns the trace of its own writes, with the
crash state as its initial snapshot. Each distinct recovery trace is then
explored like a workload: crash it at every point, run recovery again on the
result, and verify against the marks of the *first* crash. A recovery that
passes this survives a second crash at any point. Deeper nesting (a crash
during the second recovery) is not explored.

## What is not modeled

- Writes through `mmap`, `fallocate`, `copy_file_range`, hard and symbolic
  links: the strace frontend warns when a workload uses them under the root.
- `fdatasync` is treated as `fsync`.
- Drives that lie about flushes, and file systems other than the two profiles
  (btrfs, XFS, APFS, NTFS semantics differ).
