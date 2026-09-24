"""Recovery: open the database, which rolls back a hot journal or replays and
checkpoints the WAL, then close it cleanly.

usage: recover.py DIR
"""
import sqlite3
import sys

con = sqlite3.connect(f"{sys.argv[1]}/test.db")
con.execute("pragma integrity_check").fetchall()
con.close()
