#!/usr/bin/env python3
"""Read the retained originals; no network, build, extraction, or guest execution."""
import base64
import hashlib
import json
import pathlib
import posixpath
import re
import zipfile


ROOT = pathlib.Path(__file__).resolve().parent


def sha256(data):
    return hashlib.sha256(data).hexdigest()


def git_blob(data):
    return hashlib.sha1(b"blob " + str(len(data)).encode() + b"\0" + data).hexdigest()


def read_json(name):
    return json.loads((ROOT / name).read_text())


def write_json(name, value):
    (ROOT / name).write_text(json.dumps(value, ensure_ascii=False, indent=2) + "\n")


source = read_json("artifact-source.json")
archive = (ROOT / "original.zip").read_bytes()
assert len(archive) == source["size_in_bytes"]
assert sha256(archive) == source["digest"].removeprefix("sha256:")
checkout = json.loads(read_json("checkout-commit.tool.json")["structuredContent"]["content"])
assert checkout["sha"] == "2d275430b103fb0e13316ba51cc5b5c5fdafa262"
assert checkout["tree"]["sha"] == "d695242eb0005a1d06256aab91e9e649f1cb8a86"
assert checkout["sha"] in (ROOT / "job.log").read_text()

with zipfile.ZipFile(ROOT / "original.zip") as original:
    assert original.testzip() is None
    members = [{"path": name, "bytes": len(original.read(name)),
                "sha256": sha256(original.read(name))} for name in original.namelist()]
    by_name = {entry["path"]: entry for entry in members}
    internal = json.loads(original.read("files.json"))
    for entry in internal:
        assert entry == by_name[entry["path"]], entry["path"]
    for selected in sorted((ROOT / "selected-originals").rglob("*")):
        if selected.is_file():
            member = selected.relative_to(ROOT / "selected-originals").as_posix()
            assert selected.read_bytes() == original.read(member), member
    bindings = []
    for response_name, member in [("build-source.tool.json", "builder.py"),
                                  ("runner-source.tool.json", "runner.py"),
                                  ("patch-source.tool.json", "source.patch"),
                                  ("workflow-source.tool.json", None)]:
        response = read_json(response_name)["structuredContent"]
        data = base64.b64decode(response["content"], validate=True)
        assert git_blob(data) == response["sha"]
        if member is not None:
            assert data == original.read(member)
        else:
            assert data == (ROOT / "workflow.original.yml").read_bytes()
        bindings.append({"response": response_name, "artifact_member": member,
                         "git_blob": response["sha"], "bytes": len(data),
                         "sha256": sha256(data)})
    tree = {}
    for entry in original.read("source-files.stdout").decode().split("\0"):
        if entry:
            metadata, path = entry.split("\t", 1)
            mode, kind, oid, size = metadata.split()
            tree[path] = {"mode": mode, "kind": kind, "blob": oid, "bytes": int(size)}
    symlinks = []
    for path, entry in tree.items():
        if entry["mode"] != "120000":
            continue
        target = read_json("upstream-" + entry["blob"] + ".tool.json")["structuredContent"]["content"]
        data = target.encode()
        assert len(data) == entry["bytes"] and git_blob(data) == entry["blob"]
        absolute = target.startswith("/")
        resolved = None if absolute else posixpath.normpath(posixpath.join(posixpath.dirname(path), target))
        exists = resolved in tree or any(p.startswith(resolved + "/") for p in tree) if resolved else False
        symlinks.append({"path": path, **entry, "target": target, "absolute": absolute,
                         "relative_target_in_tree": exists, "relative_target": resolved})
    assert len(symlinks) == 8
    assert [s["path"] for s in symlinks if s["absolute"]] == ["deps/v8/include/wasi/wasi-simdutf-compat.h"]
    upstream_blobs = []
    for file in sorted(ROOT.glob("blob-*")):
        data = file.read_bytes()
        assert git_blob(data) == file.name.removeprefix("blob-")
        upstream_blobs.append({"file": file.name, "git_blob": git_blob(data),
                               "bytes": len(data), "sha256": sha256(data)})
    summary = json.loads(original.read("summary.json"))
    compile_observation = json.loads(original.read("compile.observation.json"))
    stderr = original.read("compile.stderr").decode().splitlines()
    errors = [{"line": i, "text": line} for i, line in enumerate(stderr, 1)
              if re.search(r"fatal error:|Error [0-9]|Error: Command failed|error generated", line)]
    stdout = original.read("compile.stdout").decode().splitlines()
    openssl = [{"line": i, "text": line} for i, line in enumerate(stdout, 1) if "libopenssl.a" in line]
    observations = {key: compile_observation[key] for key in
                    ("command", "started_at", "finished_at", "elapsed_seconds", "exit_code",
                     "stopped", "leader_reaped", "group_remaining", "rss_peak_bytes_observed", "limits")}
    observations["sample_count"] = len(compile_observation["samples"])
    for key in ("effective_headroom_bytes", "disk_free_bytes"):
        values = [sample["resources"][key] for sample in compile_observation["samples"]
                  if isinstance(sample.get("resources", {}).get(key), int)]
        observations["minimum_observed_" + key] = min(values)
    write_json("zip-verification.json", {"archive_bytes": len(archive), "sha256": sha256(archive),
               "crc_verified": True, "internal_manifest_entries_verified": len(internal), "members": members})
    write_json("observation.json", {
        "kind": "actual_hosted_source_build_failure", "run_id": 43945415290,
        "job_id": 133573644956, "attempt": 1, "head_sha": source["workflow_run"]["head_sha"],
        "actual_checkout": checkout["sha"], "actual_tree": checkout["tree"]["sha"],
        "checkout_parents": [p["sha"] for p in checkout["parents"]],
        "upstream_commit": "20871d1fb49e1af91b1fc729b0cf4e119c0bfd6f",
        "upstream_tree": "1f91f029db924986b371c4b848fb65ffd080613d",
        "source_bindings": bindings, "compile": observations, "stderr_errors": errors,
        "openssl_archive_commands": openssl, "source_summary": summary, "symlinks": symlinks,
        "verified_upstream_blobs": upstream_blobs,
        "module_members": [name for name in original.namelist() if name.endswith(".wasm")],
        "guest_executed": False, "reopen_attempted": False, "product_capability_installed": False,
        "previous_source_build": {"run_id": 43922989826, "result": "failure",
            "cause": "OpenSSL archive recipe: make printf Argument list too long",
            "artifact_sha256": "56ac75e7ca5580b4c0ce19c5a7dcc96f61ffb31e049f67f323194fb33fb8b73d"},
        "scope": "Original source build only; no new build, limit change, module, guest result, or named acceptance."
    })
print(json.dumps({"zip_members_verified": len(members), "internal_entries_verified": len(internal),
                  "source_bindings_verified": len(bindings), "symlink_blobs_verified": len(symlinks),
                  "failure": errors[0], "product_tested": False}))
