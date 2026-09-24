"""Checker: the database must open, pass integrity_check, and hold every
acknowledged transaction (rows 1..k with no gaps, k >= last acknowledged).

usage: check.py DIR MARKS
"""
import re
import sqlite3
import sys

db_dir, marks = sys.argv[1:3]
acked = [int(n) for n in re.findall(r"committed (\d+)", open(marks).read())]
last = max(acked, default=0)

con = sqlite3.connect(f"{db_dir}/test.db")  # rolls back a hot journal / replays the WAL
status = con.execute("pragma integrity_check").fetchone()[0]
if status != "ok":
    sys.exit(f"integrity_check: {status}")
tables = {r[0] for r in con.execute("select name from sqlite_master where type = 'table'")}
ids = [r[0] for r in con.execute("select id from t order by id")] if "t" in tables else []
if ids != list(range(1, len(ids) + 1)):
    sys.exit(f"rows are not a prefix: {ids}")
if len(ids) < last:
    sys.exit(f"commit {last} was acknowledged but only {len(ids)} rows survived")
