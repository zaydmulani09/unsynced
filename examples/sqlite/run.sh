#!/bin/sh
# Check SQLite commit durability under each journal_mode/synchronous setting.
# usage: examples/sqlite/run.sh   (Linux; needs strace, python3, unsynced on PATH or $UNSYNCED)
set -u
bin=${UNSYNCED:-unsynced}
here=$(cd "$(dirname "$0")" && pwd)
for cfg in "delete full" "delete extra" "wal normal" "wal full" "delete off"; do
  set -- $cfg
  echo "=== journal_mode=$1 synchronous=$2"
  "$bin" run --check "python3 $here/check.py {dir} {marks}" -- python3 "$here/workload.py" {dir} "$1" "$2"
  echo "exit status: $?"
  echo
done
