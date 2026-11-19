#!/usr/bin/env python3
"""Bounded, non-pinned C API observations for the existing capture guard.

This is not a product test. It uses only an owned in-memory database and the
already installed shared SQLite library. No product or persistent DB is opened.
"""
import ctypes as c
import ctypes.util
import json

library = ctypes.util.find_library("sqlite3")
if not library:
    raise RuntimeError("no existing shared SQLite library")
lib = c.CDLL(library)
P, I, S = c.c_void_p, c.c_int, c.c_char_p
bindings = {
    "sqlite3_libversion": (S, []), "sqlite3_sourceid": (S, []),
    "sqlite3_open": (I, [S, c.POINTER(P)]), "sqlite3_close_v2": (I, [P]),
    "sqlite3_errmsg": (S, [P]), "sqlite3_exec": (I, [P, S, P, P, P]),
    "sqlite3_prepare_v2": (I, [P, S, I, c.POINTER(P), P]),
    "sqlite3_step": (I, [P]), "sqlite3_finalize": (I, [P]),
    "sqlite3_column_int64": (c.c_longlong, [P, I]),
    "sqlite3_next_stmt": (P, [P, P]), "sqlite3_stmt_busy": (I, [P]),
    "sqlite3_stmt_readonly": (I, [P]), "sqlite3_sql": (S, [P]),
    "sqlite3_blob_open": (I, [P, S, S, S, c.c_longlong, I, c.POINTER(P)]),
    "sqlite3_blob_read": (I, [P, P, I, I]),
    "sqlite3_blob_write": (I, [P, P, I, I]), "sqlite3_blob_close": (I, [P]),
}
for name, (result_type, arguments) in bindings.items():
    fn = getattr(lib, name)
    fn.restype, fn.argtypes = result_type, arguments
AUTHORIZE = c.CFUNCTYPE(I, P, I, S, S, S, S)
lib.sqlite3_set_authorizer.argtypes = [P, AUTHORIZE, P]
lib.sqlite3_set_authorizer.restype = I


def text(value):
    return None if value is None else value.decode()


result = {"scope": "non-pinned existing SQLite library, owned memory DB only",
          "library": library, "version": text(lib.sqlite3_libversion()),
          "source_id": text(lib.sqlite3_sourceid()), "events": [],
          "snapshots": [], "operations": []}
phase = "setup"
db = P()


def checked(rc):
    if rc != 0:
        raise RuntimeError(f"{phase}: SQLite {rc}: {text(lib.sqlite3_errmsg(db))}")


@AUTHORIZE
def authorize(_ctx, action, arg1, arg2, database, trigger):
    result["events"].append({"phase": phase, "action": action, "arg1": text(arg1),
                             "arg2": text(arg2), "database": text(database),
                             "trigger": text(trigger)})
    return 0


def execute(sql, require_success=True):
    rc = lib.sqlite3_exec(db, sql.encode(), None, None, None)
    result["operations"].append({"phase": phase, "sql": sql, "returncode": rc,
                                  "error": None if rc == 0 else text(lib.sqlite3_errmsg(db))})
    if require_success:
        checked(rc)
    return rc


def prepare(sql):
    statement = P()
    checked(lib.sqlite3_prepare_v2(db, sql.encode(), -1, c.byref(statement), None))
    return statement


def query(sql):
    statement = prepare(sql)
    values = []
    try:
        while True:
            rc = lib.sqlite3_step(statement)
            if rc == 101:
                break
            if rc != 100:
                checked(rc)
            values.append(lib.sqlite3_column_int64(statement, 0))
    finally:
        checked(lib.sqlite3_finalize(statement))
    result["operations"].append({"phase": phase, "sql": sql, "values": values})


def snapshot(label):
    statements = []
    current = lib.sqlite3_next_stmt(db, None)
    while current:
        statements.append({"sql": text(lib.sqlite3_sql(current)),
                           "busy": bool(lib.sqlite3_stmt_busy(current)),
                           "readonly": bool(lib.sqlite3_stmt_readonly(current))})
        current = lib.sqlite3_next_stmt(db, current)
    result["snapshots"].append({"label": label, "statements": statements,
                                "busy": any(x["busy"] for x in statements)})


checked(lib.sqlite3_open(b":memory:", c.byref(db)))
checked(lib.sqlite3_set_authorizer(db, authorize, None))
try:
    execute("CREATE TABLE payloads(id INTEGER PRIMARY KEY, data BLOB); "
            "INSERT INTO payloads VALUES(1,X'01020304'),(2,X'05060708'); "
            "CREATE VIRTUAL TABLE search USING fts5(body); "
            "INSERT INTO search VALUES('first observation');")

    phase = "escaped_read_cursor_and_public_read_blob"
    execute("BEGIN IMMEDIATE; SAVEPOINT captured;")
    statement = prepare("SELECT id FROM payloads ORDER BY id")
    first = lib.sqlite3_step(statement)
    blob = P()
    checked(lib.sqlite3_blob_open(db, b"main", b"payloads", b"data", 1, 0, c.byref(blob)))
    snapshot("read_handles_before_rollback")
    execute("ROLLBACK TO captured; RELEASE captured;", require_success=False)
    snapshot("read_handles_after_rollback_before_any_resume")
    data = c.create_string_buffer(4)
    read_rc = lib.sqlite3_blob_read(blob, data, 4, 0)
    next_rc = lib.sqlite3_step(statement)
    result["operations"].append({"phase": phase, "first_row_step": first,
                                  "resume_row_step": next_rc, "blob_read_after_rollback": read_rc,
                                  "blob_data": data.raw.hex()})
    checked(lib.sqlite3_finalize(statement))
    checked(lib.sqlite3_blob_close(blob))
    snapshot("read_handles_closed")
    execute("ROLLBACK")

    phase = "escaped_public_write_blob"
    execute("BEGIN IMMEDIATE; SAVEPOINT captured;")
    blob = P()
    checked(lib.sqlite3_blob_open(db, b"main", b"payloads", b"data", 1, 1, c.byref(blob)))
    value = c.create_string_buffer(b"ABCD")
    checked(lib.sqlite3_blob_write(blob, value, 4, 0))
    snapshot("write_blob_before_rollback")
    execute("ROLLBACK TO captured; RELEASE captured;", require_success=False)
    snapshot("write_blob_after_rollback_before_any_resume")
    write_rc = lib.sqlite3_blob_write(blob, value, 4, 0)
    result["operations"].append({"phase": phase, "blob_write_after_rollback": write_rc})
    checked(lib.sqlite3_blob_close(blob))
    snapshot("write_blob_closed")
    execute("ROLLBACK")

    phase = "completed_read_only_fts_query"
    execute("BEGIN IMMEDIATE; SAVEPOINT captured;")
    query("SELECT count(*) FROM search WHERE search MATCH 'observation'")
    snapshot("fts_read_complete_before_rollback")
    execute("ROLLBACK TO captured; RELEASE captured;")
    snapshot("fts_read_complete_after_rollback")
    execute("ROLLBACK")

    phase = "empty_temp_and_attached_schema_aliases"
    execute("CREATE TEMP TABLE transient(id); DROP TABLE transient; ATTACH ':memory:' AS auxiliary;")
    query("SELECT count(*) FROM sqlite_temp_master")
    query("SELECT count(*) FROM sqlite_temp_schema")
    query("SELECT count(*) FROM temp.sqlite_schema")
    query("SELECT count(*) FROM auxiliary.sqlite_schema")
    query("SELECT count(*) FROM sqlite_schema")
finally:
    result["close_returncode"] = lib.sqlite3_close_v2(db)

result["project_pinned_execution"] = False
print(json.dumps(result, indent=2))
