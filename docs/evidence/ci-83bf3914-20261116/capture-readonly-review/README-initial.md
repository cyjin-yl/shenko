# Read-only capture failure investigation

This investigation concerns the actual capture failures in CI run
`43944367545`, source `83bf391440adbe3e153b036ba8fd1ecfe5867202`. It changes
no product source, adds no acceptance-test window, and does not run Rust or
the project's pinned SQLite binary.

## Observed C API result

`observe_sqlite.py` uses the already installed `libsqlite3.so.0`, version
**3.46.1**, source ID recorded in `observation-2.json`. This is not the pinned
3.46.0 engine. Python's own built-in `_sqlite3` is 3.53.1; it was not used for
the successful C API observation.

The bounded run used only an owned in-memory database with the same
external-content FTS5 trigger shape as the existing capture journey:

| Boundary | Observed result |
| --- | --- |
| After initial autocommit and at capture entry | No busy statement. |
| UPDATE completed and its caller statement finalized | One busy, read-only VDBE remained; `sqlite3_sql` returned NULL. |
| After ROLLBACK TO and RELEASE | No busy statement remained. |
| Unqualified `count(*)` on the main table | READ callback had table `facts`, empty column, NULL database; result 1. |
| Same unqualified count with a TEMP table shadowing that name | Identical READ identity fields, NULL database; result 2. |
| Explicit `main.facts` count | READ callback named database `main`; result 1. |

The full callbacks, all statement snapshots, queries, return code and source
file hashes are in `observation-2.json`; stderr is retained separately. A
first attempt to load the Python extension failed because `_sqlite3` is a
built-in module without `__file__`. Its exact script and stderr are preserved
as `observe_sqlite_attempt1.py` and `observation.stderr`; `observation.json`
was empty. The successful second attempt did not overwrite those originals.

## Primary source correspondence

The SQLite `version-3.46.0` tag resolved to
`bebe2d8be8acfd02592c4972f4ba32c3b4e4a33f`; rusqlite `v0.32.1` resolved to
`499cc7bb986e04cc66e6ed762522f7ea449178d1`. The complete small source files,
tag responses, fetch URLs and SHA-256 inventory are retained here. The large
project-pinned amalgamation blob was not obtained or independently compared.

- SQLite `select.c`, 7654–7670: a table without referenced columns, including
  `count(*)`, emits READ with an empty column and `pItem->zDatabase`.
- `auth.c`, 197–229: `sqlite3AuthCheck` passes this database argument to the
  callback without substituting `main`.
- rusqlite `inner_connection.rs`, 323–334: `is_busy` scans every statement
  returned by `sqlite3_next_stmt` and calls `sqlite3_stmt_busy`.
- rusqlite `row.rs`, 17–23 and 109–115: dropping `Rows` resets its statement;
  this does not close FTS's independently held internal blob reader.
- SQLite `fts5_index.c`, 817–823: FTS caches a read-only incremental BLOB
  reader. `vdbeblob.c`, 245–256 and 63–75, creates its VDBE and pauses it at
  `OP_ResultRow`/`SQLITE_ROW`.
- The existing rollback closes this reader through `fts5_main.c`
  `fts5RollbackToMethod`, `fts5_storage.c` `sqlite3Fts5StorageRollback`,
  `fts5_index.c` `sqlite3Fts5IndexRollback` and
  `sqlite3Fts5IndexCloseReader`, then `sqlite3_blob_close`.
- SQLite `vdbeapi.c`, 2023–2026: busy means the VDBE is in its running state;
  it does not distinguish this internal reader from a caller's cursor.

## Bounded repair implications

The end-of-evaluation busy check precedes the normal capture rollback in the
failing source. Moving that check to after successful rollback/release can
avoid this internal-reader false positive while retaining the entry check
against a cursor already active before observation. This is a source-backed
repair suggestion, not a compiled or executed fix. Do not ignore all NULL-SQL
statements or delete the active-cursor guard based on this observation.

A NULL database on a no-column READ is legitimate, but checking only that its
table name also exists in `main` is insufficient: the TEMP shadow observation
is a direct counterexample. Any main-database attribution must separately
prove the absence of TEMP/attached-database ambiguity or keep an equally
strict refusal. Do not weaken the foreign-database guard to unconditional
`None => main`.

The existing CI capture journey and native journeys still need to execute
against the repaired Rust source and project-pinned SQLite. This investigation
does not turn either prior failure into a passing result.
