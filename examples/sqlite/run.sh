#!/bin/sh
# Check SQLite commit durability under each journal_mode/synchronous setting,
# then crash SQLite's own recovery (hot-journal rollback, WAL checkpoint).
# usage: examples/sqlite/run.sh   (Linux; needs strace, python3, unsynced on PATH or $UNSYNCED)
set -u
bin=${UNSYNCED:-unsynced}
here=$(cd "$(dirname "$0")" && pwd)
check="python3 $here/check.py {dir} {marks}"
for cfg in "delete full" "delete extra" "wal normal" "wal full" "delete off"; do
  set -- $cfg
  echo "=== journal_mode=$1 synchronous=$2"
  "$bin" run --check "$check" -- python3 "$here/workload.py" {dir} "$1" "$2"
  echo "exit status: $?"
  echo
done
for cfg in "delete extra" "wal full"; do
  set -- $cfg
  echo "=== journal_mode=$1 synchronous=$2, crashing recovery too"
  "$bin" run --check "$check" --recover "python3 $here/recover.py {dir}" --max-states 4000 \
    -- python3 "$here/workload.py" {dir} "$1" "$2"
  echo "exit status: $?"
  echo
done
