#!/usr/bin/env python3
"""Two bounded exporter checks; synthetic SQLite rows are not native E1 evidence."""
import argparse
import hashlib
import importlib.util
import json
from pathlib import Path
import sqlite3
import subprocess
import sys
import tempfile
import zipfile


def sha(raw):
    return hashlib.sha256(raw).hexdigest()


def run(exporter, root, output, label):
    archive = output / f"{label}.zip"
    command = [sys.executable, str(exporter), "--root", str(root), "--output",
               str(archive), "--source-revision", "synthetic-exporter-check"]
    result = subprocess.run(command, capture_output=True, timeout=30, check=False)
    (output / f"{label}.stdout.log").write_bytes(result.stdout)
    (output / f"{label}.stderr.log").write_bytes(result.stderr)
    with zipfile.ZipFile(archive) as bundle:
        files = {name: bundle.read(name) for name in bundle.namelist()}
    return result.returncode, files, {"command": command, "exit_code": result.returncode,
                                    "archive_sha256": sha(archive.read_bytes())}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--exporter", type=Path, required=True)
    parser.add_argument("--output-dir", type=Path, required=True)
    args = parser.parse_args()
    args.output_dir.mkdir(parents=True, exist_ok=True)
    spec = importlib.util.spec_from_file_location("exporter", args.exporter)
    exporter = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(exporter)
    observations = {
        "scope": "two synthetic exporter scenarios; no SSH, Store protocol or Rust execution",
        "exporter_sha256": sha(args.exporter.read_bytes()),
        "digest_labels": "synthetic 64-hex labels; BLAKE3 verification is not claimed",
        "runs": [],
    }
    with tempfile.TemporaryDirectory(prefix="native-e1-exporter-check-") as temporary:
        root = Path(temporary)
        secret = "SYNTHETIC-CREDENTIAL-DO-NOT-EXPORT"

        def reference(key, raw, namespace="test"):
            return {"digest": "blake3:" + key * 64, "bytes": len(raw),
                    "media_type": "application/json", "store_namespace": namespace,
                    "codec": {"name": "synthetic", "major": 1}, "locator": "ignored"}

        safe = b'{"original":"exact bytes \\u2603", "source_commit":{"ledger_id":"original-A","seq":19}}'
        credential = json.dumps({"nested": "  " + json.dumps({"token": secret})}).encode()
        missing_ref = reference("c", b"missing")
        secret_ref = reference("b", credential)
        safe_ref = reference("a", safe)
        encoded_row = "  " + json.dumps({"safe": safe_ref, "missing": missing_ref,
                                         "private_ref": secret_ref})
        transition = json.dumps({"rows": [{"type": "text", "value": encoded_row}]}).encode()
        transition_ref = reference("d", transition, "commit-authority")
        original_data = "  {\n  \"original\": \"☃\", \"root\": " + json.dumps(transition_ref) + "\n}\n"
        for name in ("source", "receiver", "parked"):
            directory = root / name
            directory.mkdir()
            with sqlite3.connect(directory / "shenko.db") as connection:
                for table in exporter.TABLES:
                    if table == "commit_log":
                        connection.execute("CREATE TABLE commit_log(ledger_id TEXT, seq INTEGER, created_at TEXT, entity_count INTEGER)")
                        connection.execute("INSERT INTO commit_log VALUES('original-A',19,'2026-11-16 19:00:00',3)")
                    else:
                        connection.execute(f'CREATE TABLE "{table}"(data TEXT)')
                connection.execute("INSERT INTO operations VALUES(?)", (original_data,))
                connection.execute("INSERT INTO tasks VALUES(?)", (json.dumps({"wrapped": "  " + json.dumps({"api_key": secret})}),))
                connection.execute("CREATE TABLE owner_admin_tokens(token TEXT)")
                connection.execute("INSERT INTO owner_admin_tokens VALUES(?)", (secret,))
            for key, raw in (("a", safe), ("b", credential), ("d", transition)):
                digest = key * 64
                path = directory / "cas" / digest[:2] / digest[2:4] / digest
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(raw)
        (root / "effects").write_bytes(b"effect\neffect\n")
        before = {str(path.relative_to(root)): sha(path.read_bytes())
                  for path in root.rglob("*") if path.is_file()}
        code, files, observation = run(args.exporter, root, args.output_dir, "selected-originals")
        assert code == 0, observation
        assert not any(secret.encode() in raw for raw in files.values())
        manifest = json.loads(files["manifest.json"])
        assert not manifest["errors"]
        for name in ("source", "receiver", "parked"):
            rows = json.loads(files[name + "/ledger-rows.json"])
            assert rows["tables"]["operations"][0]["values"]["data"] == original_data
            assert rows["tables"]["commit_log"][0]["values"]["seq"] == 19
            assert "owner_admin_tokens" in rows["omitted_tables"]
            assert len(rows["excluded_rows"]) == 1
            report = json.loads(files[name + "/cas-references.json"])
            assert any(item["reference"]["digest"] == safe_ref["digest"] and "/<json>/safe" in item["via"] for item in report["references"])
            assert any("missing" in item for item in report["references"])
            assert "b" * 64 in report["excluded_digests"]
            assert files[f"{name}/cas/aa/aa/" + "a" * 64] == safe
            assert files[f"{name}/cas/dd/dd/" + "d" * 64] == transition
        after = {str(path.relative_to(root)): sha(path.read_bytes())
                 for path in root.rglob("*") if path.is_file()}
        assert before == after, "the exporter changed original fixture bytes"
        observation["assertions"] = "original text/rows/bytes unchanged; nested Cell::Text reference traversed; missing CAS explicit; secret row/object and unselected table excluded"
        observations["runs"].append(observation)

        # The failure-path scenario retains source originals while reporting
        # one absent Store and one incompatible schema as a nonzero export.
        (root / "parked" / "shenko.db").unlink()
        with sqlite3.connect(root / "receiver" / "shenko.db") as connection:
            connection.execute("DROP TABLE commit_log")
        code, files, observation = run(args.exporter, root, args.output_dir, "partial-failure")
        assert code == 1, observation
        manifest = json.loads(files["manifest.json"])
        assert {error["store"] for error in manifest["errors"]} == {"receiver", "parked"}
        assert "source/ledger-rows.json" in files
        assert "receiver/ledger-rows.json" in files
        assert "parked/ledger-rows.json" not in files
        assert not (root / "parked" / "shenko.db").exists()
        observation["assertions"] = "missing database and incompatible schema cannot claim successful retention; available originals survive and missing database is not created"
        observations["runs"].append(observation)
    (args.output_dir / "observations.json").write_text(json.dumps(observations, indent=2) + "\n")
    print(json.dumps(observations, indent=2))


if __name__ == "__main__":
    main()
