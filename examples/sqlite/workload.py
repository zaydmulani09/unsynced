"""Commit five transactions to a SQLite database, announcing each commit.

usage: workload.py DIR JOURNAL_MODE SYNCHRONOUS
"""
import sqlite3
import sys

db_dir, journal_mode, synchronous = sys.argv[1:4]
con = sqlite3.connect(f"{db_dir}/test.db", isolation_level=None)
con.execute(f"pragma journal_mode={journal_mode}")
con.execute(f"pragma synchronous={synchronous}")
con.execute("create table t (id integer primary key, payload text)")
for i in range(1, 6):
    con.execute("begin")
    con.execute("insert into t values (?, ?)", (i, "x" * 3000))
    con.execute("commit")
    # Printing is the acknowledgement: unsynced records it as a mark.
    print(f"committed {i}", flush=True)
con.close()
