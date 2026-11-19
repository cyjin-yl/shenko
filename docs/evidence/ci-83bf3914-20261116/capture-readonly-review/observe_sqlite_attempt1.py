#!/usr/bin/env python3
"""Observe the installed SQLite C API, not the project's pinned build.

Only an owned in-memory database is touched. This is a bounded diagnostic of
the existing capture journey's FTS and database-identity boundaries; it is not
a new Rust acceptance test or evidence that the pinned SQLite/Rust fix passed.
"""
import ctypes as c
import hashlib
import json
from pathlib import Path
import sqlite3
import _sqlite3

lib = c.CDLL(_sqlite3.__file__)
pointer = c.c_void_p
lib.sqlite3_libversion.restype = c.c_char_p
lib.sqlite3_sourceid.restype = c.c_char_p
lib.sqlite3_open.argtypes = [c.c_char_p, c.POINTER(pointer)]
lib.sqlite3_open.restype = c.c_int
lib.sqlite3_close_v2.argtypes = [pointer]
lib.sqlite3_close_v2.restype = c.c_int
lib.sqlite3_errmsg.argtypes = [pointer]
lib.sqlite3_errmsg.restype = c.c_char_p
lib.sqlite3_exec.argtypes = [pointer, c.c_char_p, pointer, pointer, pointer]
lib.sqlite3_exec.restype = c.c_int
lib.sqlite3_prepare_v2.argtypes = [pointer, c.c_char_p, c.c_int, c.POINTER(pointer), pointer]
lib.sqlite3_prepare_v2.restype = c.c_int
lib.sqlite3_step.argtypes = [pointer]
lib.sqlite3_step.restype = c.c_int
lib.sqlite3_finalize.argtypes = [pointer]
lib.sqlite3_finalize.restype = c.c_int
lib.sqlite3_column_int64.argtypes = [pointer, c.c_int]
lib.sqlite3_column_int64.restype = c.c_longlong
lib.sqlite3_next_stmt.argtypes = [pointer, pointer]
lib.sqlite3_next_stmt.restype = pointer
lib.sqlite3_stmt_busy.argtypes = [pointer]
lib.sqlite3_stmt_busy.restype = c.c_int
lib.sqlite3_stmt_readonly.argtypes = [pointer]
lib.sqlite3_stmt_readonly.restype = c.c_int
lib.sqlite3_sql.argtypes = [pointer]
lib.sqlite3_sql.restype = c.c_char_p
authorizer_type = c.CFUNCTYPE(c.c_int, pointer, c.c_int, c.c_char_p,
                            c.c_char_p, c.c_char_p, c.c_char_p)
lib.sqlite3_set_authorizer.argtypes = [pointer, authorizer_type, pointer]
lib.sqlite3_set_authorizer.restype = c.c_int


def text(value):
    return None if value is None else value.decode("utf-8")


result = {
    "scope": "non-pinned installed SQLite C API observation only",
    "sqlite_version": text(lib.sqlite3_libversion()),
    "sqlite_source_id": text(lib.sqlite3_sourceid()),
    "python_sqlite_version": sqlite3.sqlite_version,
    "loaded_module": _sqlite3.__file__,
    "pinned_project_sqlite": "3.46.0",
    "authorizer_events": [],
    "snapshots": [],
    "queries": [],
}
result["is_project_pinned_version"] = result["sqlite_version"] == "3.46.0"
db = pointer()
phase = "opening"


def check(rc):
    if rc != 0:
        raise RuntimeError(f"SQLite {rc} during {phase}: {text(lib.sqlite3_errmsg(db))}")


@authorizer_type
def authorize(_context, action, arg1, arg2, database, trigger):
    result["authorizer_events"].append({
        "phase": phase, "action": action, "arg1": text(arg1),
        "arg2": text(arg2), "database": text(database), "trigger": text(trigger),
    })
    return 0


def execute(sql):
    check(lib.sqlite3_exec(db, sql.encode(), None, None, None))


def integer_query(sql):
    statement = pointer()
    check(lib.sqlite3_prepare_v2(db, sql.encode(), -1, c.byref(statement), None))
    values = []
    try:
        while True:
            rc = lib.sqlite3_step(statement)
            if rc == 101:
                break
            if rc != 100:
                check(rc)
            values.append(lib.sqlite3_column_int64(statement, 0))
    finally:
        check(lib.sqlite3_finalize(statement))
    result["queries"].append({"phase": phase, "sql": sql, "values": values})


def snapshot(label):
    statements = []
    current = lib.sqlite3_next_stmt(db, None)
    while current:
        statements.append({
            "sql": text(lib.sqlite3_sql(current)),
            "busy": bool(lib.sqlite3_stmt_busy(current)),
            "readonly": bool(lib.sqlite3_stmt_readonly(current)),
        })
        current = lib.sqlite3_next_stmt(db, current)
    result["snapshots"].append({"label": label, "statements": statements,
                                "connection_is_busy": any(x["busy"] for x in statements)})


check(lib.sqlite3_open(b":memory:", c.byref(db)))
try:
    phase = "initialize"
    execute("""
      CREATE TABLE facts(id INTEGER PRIMARY KEY, body TEXT);
      CREATE VIRTUAL TABLE search USING fts5(body, content='facts', content_rowid='id');
      CREATE TRIGGER facts_insert AFTER INSERT ON facts BEGIN
        INSERT INTO search(rowid, body) VALUES(new.id, new.body); END;
      CREATE TRIGGER facts_update AFTER UPDATE ON facts BEGIN
        INSERT INTO search(search, rowid, body) VALUES('delete', old.id, old.body);
        INSERT INTO search(rowid, body) VALUES(new.id, new.body); END;
      INSERT INTO facts VALUES(1, 'before');
    """)
    snapshot("after_initial_autocommit")
    execute("BEGIN IMMEDIATE; SAVEPOINT captured;")
    snapshot("capture_entry")
    check(lib.sqlite3_set_authorizer(db, authorize, None))
    phase = "main_count"
    integer_query("SELECT count(*) FROM facts")
    phase = "fts_trigger_update"
    execute("UPDATE facts SET body='after' WHERE id=1")
    snapshot("after_complete_update_statement")
    check(lib.sqlite3_set_authorizer(db, authorizer_type(), None))
    execute("ROLLBACK TO captured; RELEASE captured;")
    snapshot("after_capture_rollback")
    execute("COMMIT")
    snapshot("after_outer_commit")
    phase = "temp_shadow_setup"
    execute("CREATE TEMP TABLE facts(id INTEGER PRIMARY KEY); INSERT INTO temp.facts VALUES(8),(9);")
    check(lib.sqlite3_set_authorizer(db, authorize, None))
    phase = "unqualified_temp_shadow_count"
    integer_query("SELECT count(*) FROM facts")
    phase = "qualified_main_count"
    integer_query("SELECT count(*) FROM main.facts")
    check(lib.sqlite3_set_authorizer(db, authorizer_type(), None))
finally:
    result["close_returncode"] = lib.sqlite3_close_v2(db)

result["source_files"] = []
for path in sorted((Path(__file__).parent / "sources").iterdir()):
    data = path.read_bytes()
    result["source_files"].append({"path": "sources/" + path.name,
                                    "bytes": len(data), "sha256": hashlib.sha256(data).hexdigest()})
print(json.dumps(result, indent=2, ensure_ascii=False))
