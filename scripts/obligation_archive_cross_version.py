#!/usr/bin/env python3
"""Drive the two-binary archive protocol proof for archive schema 5 <-> 6.

The consumer program is `crates/shenko-store/examples/obligation_archive_consumer.rs`.
It is compiled twice by the parent:

  * the *old* binary from a schema-5 tree (a `d6eb9fb` checkout). Build it from
    a crate whose `[[bin]] path` is the consumer source and whose `shenko-store`
    dependency points at the old tree (the retained
    `build/obligation-lifecycle-20261105/old5-reader` probe crate is that);
  * the *current* binary with `cargo build --example obligation_archive_consumer`
    in the schema-6 tree.

This script never edits an archive and never rewrites a serialized schema
number. It records the sha256 of both binaries and of every archive, checks
that every archive is byte-identical after the whole run, and only compares the
schema each binary *reports* with the schema the bytes declare.

Every command, its exit status, its stdout/stderr and all checks are written to
`<root>/receipt.json`; a failed or inconclusive run keeps that receipt.

Usage:
  python3 scripts/obligation_archive_cross_version.py \
      --old5-binary <old schema-5 consumer> \
      --new6-binary <current schema-6 consumer> \
      --root <fresh durable directory> \
      (--obligation-root <store root with a genuine committed obligation> |
       --new6-archive <genuine schema-6 archive>) \
      [--obligation-receipt <program-1 receipt.json>] \
      [--expect-obligation-id <obligation id>]

No model, provider or network is involved; the facts are authored fixtures
created through each binary's own Store. This is not a power-loss, distributed
installation or model-efficacy proof.
"""
import argparse
import hashlib
import json
import pathlib
import sqlite3
import subprocess
import sys
import time

TERMINAL_CONTRACT = "subject/obligation-terminal/1"


def sha256(path):
    digest = hashlib.sha256()
    with open(path, "rb") as handle:
        for block in iter(lambda: handle.read(1 << 20), b""):
            digest.update(block)
    return digest.hexdigest()


def archive_header(path):
    """Read the archive's own manifest frame without rewriting anything."""
    with open(path, "rb") as handle:
        magic = handle.readline().rstrip(b"\n").decode()
        header = handle.readline().rstrip(b"\n").decode().split()
        if magic != "shenko-cognition-archive/1":
            raise SystemExit(f"{path} is not a cognition archive: {magic}")
        if not header or header[0] != "manifest":
            raise SystemExit(f"{path} does not start with its manifest: {header}")
        manifest = json.loads(handle.read(int(header[1])))
    counts = {}
    for row in manifest.get("rows") or []:
        counts[row.get("table", "?")] = counts.get(row.get("table", "?"), 0) + 1
    return {
        "schema": manifest.get("schema"),
        "fingerprint": manifest.get("fingerprint"),
        "fact_classes": manifest.get("fact_classes"),
        "row_counts": counts,
        "rows": manifest.get("rows") or [],
        "gaps": manifest.get("gaps") or [],
    }


def row_field(row, name):
    for value in row.get("values") or []:
        if value[0] == name:
            return value[1]
    return None


def settlements_from_rows(rows):
    settlements = []
    for row in rows:
        if row.get("table") != "subject_intents":
            continue
        if row_field(row, "contract") != TERMINAL_CONTRACT:
            continue
        receipt = row_field(row, "result_ref")
        settlements.append(
            {"intent_id": row.get("key"), "receipt": json.loads(receipt) if receipt else None}
        )
    return settlements


def settlement_rows(facts):
    return [
        {"intent_id": entry.get("key"), "receipt": entry.get("receipt")}
        for entry in (facts or {}).get("settlements") or []
    ]


def typed(facts):
    """The replicated part of a consumer fact report."""
    facts = facts or {}
    return {
        "row_counts": facts.get("row_counts") or {},
        "rows": facts.get("rows") or {},
        "obligations": facts.get("obligations") or [],
        "settlements": facts.get("settlements") or [],
    }


def store_counts(store_root):
    """Independent local truth: rows that no archive refusal may add."""
    database = pathlib.Path(store_root) / "store.db"
    connection = sqlite3.connect(str(database))
    try:
        def count(sql, parameters=()):
            return connection.execute(sql, parameters).fetchone()[0]

        return {
            "subject_obligations": count("SELECT count(*) FROM subject_obligations"),
            "terminal_settlements": count(
                "SELECT count(*) FROM subject_intents WHERE contract=?", (TERMINAL_CONTRACT,)
            ),
            "cognition_baselines": count("SELECT count(*) FROM cognition_baselines"),
            "cognition_import_order": count("SELECT count(*) FROM cognition_import_order"),
            "commit_log": count("SELECT count(*) FROM commit_log"),
            "ledger_max_seq": count("SELECT COALESCE(MAX(seq),-1) FROM commit_log"),
        }
    finally:
        connection.close()


class Run:
    def __init__(self, root):
        self.root = root
        self.commands = []
        self.checks = []
        self.artifacts = {}
        self.started = time.time()

    def record(self, binary, args, deadline=900):
        argv = [str(binary)] + [str(argument) for argument in args]
        started = time.monotonic()
        result = subprocess.run(argv, capture_output=True, timeout=deadline)
        entry = {
            "argv": argv,
            "exit_code": result.returncode,
            "elapsed_seconds": round(time.monotonic() - started, 3),
            "stdout": result.stdout.decode(errors="replace"),
            "stderr": result.stderr.decode(errors="replace"),
        }
        self.commands.append(entry)
        return entry

    def parse(self, entry, expect=0):
        if entry["exit_code"] != expect:
            raise RuntimeError(
                f"{entry['argv'][0]} {' '.join(entry['argv'][1:])} exited "
                f"{entry['exit_code']}: {entry['stderr'].strip()[:800]}"
            )
        try:
            return json.loads(entry["stdout"].strip().splitlines()[-1])
        except (IndexError, json.JSONDecodeError) as error:
            raise RuntimeError(
                f"consumer did not print one JSON object: {error}: {entry['stdout'][:800]}"
            )

    def check(self, name, passed, detail):
        self.checks.append({"check": name, "passed": bool(passed), "detail": detail})

    def artifact(self, path):
        path = pathlib.Path(path)
        entry = {"path": str(path), "bytes": path.stat().st_size, "sha256": sha256(path)}
        self.artifacts[str(path)] = entry
        return entry


def prepare_root(root):
    root = pathlib.Path(root)
    if root.exists():
        if any(root.iterdir()):
            raise SystemExit(f"{root} is not a fresh root: it already contains entries")
    else:
        root.mkdir(parents=True)
    return root


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--old5-binary", required=True)
    parser.add_argument("--new6-binary", required=True)
    parser.add_argument("--root", required=True)
    parser.add_argument("--obligation-root")
    parser.add_argument("--new6-archive")
    parser.add_argument("--obligation-receipt")
    parser.add_argument("--expect-obligation-id")
    options = parser.parse_args()

    root = prepare_root(options.root)
    old5 = pathlib.Path(options.old5_binary).resolve()
    new6 = pathlib.Path(options.new6_binary).resolve()
    for binary in (old5, new6):
        if not binary.exists():
            raise SystemExit(f"{binary} does not exist")
    run = Run(root)
    state = {"state": "failed"}
    try:
        old5_artifact = run.artifact(old5)
        new6_artifact = run.artifact(new6)
        run.check(
            "two_distinct_binaries",
            old5_artifact["sha256"] != new6_artifact["sha256"],
            {"old5": old5_artifact, "new6": new6_artifact},
        )

        # 1. A genuine, version-native old fact set and its schema-5 archive.
        old5_source = root / "old5-source"
        seeded = run.parse(run.record(old5, ["seed", old5_source]))
        run.check(
            "old5_binary_reports_schema5",
            seeded["sender_schema"] == 5 and seeded["executable"]["declared_archive_schema"] == 5,
            {"sender_schema": seeded["sender_schema"], "executable": seeded["executable"]},
        )
        old_archive = root / "old5.archive"
        exported_old = run.parse(
            run.record(old5, ["export", old5_source, old_archive, "--expect-schema=5"])
        )
        run.check(
            "old5_export_is_a_complete_schema5_archive",
            exported_old["writer_schema"] == 5
            and exported_old["declared"]["schema"] == 5
            and exported_old["complete"] is True
            and exported_old["declared"]["row_counts"].get("memory_entries", 0) >= 2,
            {"declared": exported_old["declared"], "complete": exported_old["complete"]},
        )
        old_facts = typed(exported_old["facts"])
        old_manifest = archive_header(old_archive)
        run.check(
            "old5_archive_declares_its_genuine_rows",
            old_manifest["schema"] == 5
            and old_manifest["row_counts"].get("memory_entries", 0) >= 2
            and old_manifest["row_counts"].get("memory_intents", 0) >= 1,
            {"row_counts": old_manifest["row_counts"]},
        )
        run.artifact(old_archive)

        # 2. The genuine schema-6 archive, carrying a real obligation settlement.
        new6_declared_schema = None
        if options.new6_archive:
            new_archive = pathlib.Path(options.new6_archive).resolve()
            if not new_archive.exists():
                raise SystemExit(f"{new_archive} does not exist")
            new_header = archive_header(new_archive)
            new_settlements = settlements_from_rows(new_header["rows"])
            new_obligation_keys = [
                row.get("key")
                for row in new_header["rows"]
                if row.get("table") == "subject_obligations"
            ]
            run.check(
                "supplied_new6_archive_is_genuine_with_obligation_facts",
                new_header["schema"] == 6
                and len(new_obligation_keys) >= 1
                and any(
                    settlement["receipt"]["outcome"]["outcome"] == "applied"
                    for settlement in new_settlements
                    if settlement["receipt"]
                ),
                {"schema": new_header["schema"], "row_counts": new_header["row_counts"]},
            )
        else:
            if not options.obligation_root:
                raise SystemExit("one of --obligation-root or --new6-archive is required")
            new_archive = root / "new6.archive"
            exported_new = run.parse(
                run.record(
                    new6,
                    ["export", pathlib.Path(options.obligation_root), new_archive,
                     "--expect-schema=6"],
                )
            )
            new6_declared_schema = exported_new["executable"]["declared_archive_schema"]
            new_header = exported_new["declared"]
            new_facts = typed(exported_new["facts"])
            new_settlements = settlement_rows(new_facts)
            new_obligation_keys = [row.get("key") for row in new_facts["obligations"]]
            run.check(
                "new6_export_is_a_complete_schema6_archive_with_the_settlement",
                exported_new["writer_schema"] == 6
                and new6_declared_schema == 6
                and exported_new["complete"] is True
                and exported_new["declared"]["schema"] == 6
                and len(new_obligation_keys) >= 1
                and any(
                    settlement["receipt"]["outcome"]["outcome"] == "applied"
                    for settlement in new_settlements
                ),
                {"declared": exported_new["declared"], "settlements": new_settlements},
            )
        run.artifact(new_archive)
        new_header = archive_header(new_archive)
        run.check(
            "new6_archive_header_matches_its_facts",
            new_header["schema"] == 6
            and new_header["row_counts"].get("subject_obligations", 0) >= 1,
            {"schema": new_header["schema"], "row_counts": new_header["row_counts"]},
        )
        if options.expect_obligation_id:
            run.check(
                "new6_archive_is_the_expected_obligation",
                options.expect_obligation_id in new_obligation_keys,
                {"expect": options.expect_obligation_id,
                 "obligation_keys": new_obligation_keys},
            )
        if options.obligation_receipt:
            program_one = json.loads(pathlib.Path(options.obligation_receipt).read_text())
            evidence = program_one.get("evidence") or {}
            recorded_obligation = (evidence.get("origin") or {}).get("obligation_id")
            run.check(
                "new6_archive_is_consistent_with_the_partition_receipt",
                program_one.get("state") == "passed"
                and not program_one.get("failed_checks")
                and bool(evidence.get("peer_a") or evidence.get("store_d")),
                {
                    "partition_state": program_one.get("state"),
                    "partition_failed_checks": program_one.get("failed_checks"),
                    "recorded_obligation": recorded_obligation,
                    "obligation_ids": [
                        store.get("obligation", {}).get("obligation_id")
                        for store in evidence.get("reopened") or []
                    ],
                },
            )
            if recorded_obligation:
                run.check(
                    "new6_archive_carries_the_obligation_the_partition_settled",
                    recorded_obligation in new_obligation_keys,
                    {"recorded": recorded_obligation, "archive": new_obligation_keys},
                )

        # 3. The current reader accepts the genuine old archive, and a reopen
        #    keeps exactly the typed facts it admitted.
        from_old5 = root / "new6-from-old5"
        imported = run.parse(
            run.record(new6, ["import", from_old5, old_archive, "--expect-schema=5",
                             f"--expect-fingerprint={old_manifest['fingerprint']}"])
        )
        if new6_declared_schema is None:
            new6_declared_schema = imported["reader_schema"]
        run.check(
            "new6_accepts_the_genuine_old5_archive",
            imported["verified"] is True
            and imported["reader_schema"] == 6
            and imported["writer_schema"] == 5
            and typed(imported["facts_after_reopen"]) == old_facts,
            {
                "verified": imported["verified"],
                "writer_schema": imported["writer_schema"],
                "row_counts": (imported["facts_after_reopen"] or {}).get("row_counts"),
            },
        )

        # 4. Relay: import, reopen, export as schema 6, re-import into a fresh
        #    reader. The typed facts must be identical at every hop.
        relay_store = root / "new6-relay"
        relay_archive = root / "relay.archive"
        relayed = run.parse(
            run.record(new6, ["relay", relay_store, old_archive, relay_archive,
                             "--expect-schema=6",
                             f"--expect-fingerprint={old_manifest['fingerprint']}"])
        )
        run.check(
            "relay_preserves_the_old_facts_in_a_schema6_archive",
            relayed["relayed_declared"]["schema"] == 6
            and typed(relayed["facts_after_reopen"]) == old_facts,
            {"relayed_declared": relayed["relayed_declared"],
             "facts": relayed["facts_after_reopen"]},
        )
        run.artifact(relay_archive)
        relay_header = archive_header(relay_archive)
        relay_receiver = root / "relay-receiver"
        reimported = run.parse(
            run.record(new6, ["import", relay_receiver, relay_archive, "--expect-schema=6",
                             f"--expect-fingerprint={relay_header['fingerprint']}"])
        )
        run.check(
            "relay_reimport_preserves_typed_facts",
            reimported["verified"] is True
            and reimported["writer_schema"] == 6
            and typed(reimported["facts_after_reopen"]) == old_facts,
            {"row_counts": (reimported["facts_after_reopen"] or {}).get("row_counts")},
        )

        # 5. The old reader must refuse the genuine new archive before
        #    publication: a store that already holds its own legitimate old
        #    facts keeps them, gains no obligation row and no baseline.
        old_receiver = root / "old5-receiver"
        run.parse(run.record(old5, ["import", old_receiver, old_archive, "--expect-schema=5",
                                    f"--expect-fingerprint={old_manifest['fingerprint']}"]))
        before = store_counts(old_receiver)
        old5_facts_before = run.parse(run.record(old5, ["facts", old_receiver]))
        refusal = run.parse(
            run.record(old5, ["refuse", old_receiver, new_archive,
                             f"--expect-schema={new_header['schema']}",
                             f"--expect-fingerprint={new_header['fingerprint']}"])
        )
        after = store_counts(old_receiver)
        old5_facts_after = run.parse(run.record(old5, ["facts", old_receiver]))
        run.check(
            "old5_refuses_genuine_new6_before_publication",
            refusal["refused"] is True
            and refusal["facts_unchanged"] is True
            and refusal["reopen_valid"] is True
            and "unsupported cognition archive schema 6" in refusal["error"],
            {"error": refusal["error"], "writer_schema": refusal["writer_schema"]},
        )
        run.check(
            "old5_refusal_publishes_nothing",
            before == after
            and before["subject_obligations"] == 0
            and before["terminal_settlements"] == 0
            and before["cognition_baselines"] == 0
            and typed(old5_facts_before) == typed(old5_facts_after)
            and "subject_obligations" not in (typed(old5_facts_after)["rows"] or {}),
            {"before": before, "after": after},
        )

        # 6. The relayed schema-6 archive is refused the same way: a relay can
        #    never launder new facts into an archive an old reader accepts.
        relay_refused = run.parse(
            run.record(old5, ["refuse", old_receiver, relay_archive, "--expect-schema=6",
                             f"--expect-fingerprint={relay_header['fingerprint']}"])
        )
        after_relay = store_counts(old_receiver)
        old5_facts_final = run.parse(run.record(old5, ["facts", old_receiver]))
        run.check(
            "old5_refuses_the_relayed_schema6_archive",
            relay_refused["refused"] is True
            and after_relay == before
            and "unsupported cognition archive schema 6" in relay_refused["error"]
            and typed(old5_facts_final) == old_facts,
            {"error": relay_refused["error"], "before": before, "after": after_relay},
        )

        # 7. No archive was rewritten by any consumer.
        stable = []
        for archive in (old_archive, new_archive, relay_archive):
            recorded = run.artifacts.get(str(archive)) or {}
            stable.append(recorded.get("sha256") == sha256(archive))
        run.check("archives_are_byte_stable", all(stable), {"archives": run.artifacts})

        state.update(
            {
                "state": "passed",
                "schemas": {
                    "old5_binary": seeded["sender_schema"],
                    "new6_binary": new6_declared_schema,
                    "old5_archive": old_manifest["schema"],
                    "new6_archive": new_header["schema"],
                    "relayed_archive": relayed["relayed_declared"]["schema"],
                },
                "old5_settlements": settlement_rows(old_facts),
                "new6_settlements": new_settlements,
                "old5_facts": old_facts,
                "old5_archive": str(old_archive),
                "new6_archive": str(new_archive),
                "relay_archive": str(relay_archive),
            }
        )
    except BaseException as error:  # keep every failed run
        state["error"] = f"{type(error).__name__}: {error}"
    finally:
        failed = [check["check"] for check in run.checks if not check["passed"]]
        receipt = {
            "state": "passed" if state.get("state") == "passed" and not failed else "failed",
            "scope": (
                "Actual separately compiled schema-5 and schema-6 archive writer/readers. "
                "Genuine old facts carrying a typed memory intent, a genuine schema-6 obligation "
                "settlement, cross-version admission, relay, refusal before publication and byte "
                "stability. Not a power-loss, distributed-installation or model-efficacy proof."
            ),
            "root": str(root),
            "started_at": time.strftime("%Y-%m-%dT%H:%M:%S%z", time.localtime(run.started)),
            "elapsed_seconds": round(time.time() - run.started, 3),
            "failed_checks": failed,
            "checks": run.checks,
            "artifacts": run.artifacts,
            "commands": run.commands,
            "results": state,
        }
        path = root / "receipt.json"
        path.write_text(json.dumps(receipt, indent=2) + "\n")
        print(json.dumps({"state": receipt["state"], "failed_checks": failed,
                          "receipt": str(path)}))
        if receipt["state"] != "passed":
            sys.exit(1)


if __name__ == "__main__":
    main()
