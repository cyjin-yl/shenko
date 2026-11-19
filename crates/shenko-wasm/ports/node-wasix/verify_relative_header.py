#!/usr/bin/env python3
"""Check the actual fixed patch helpers on retained source bytes; never run Node."""
import argparse
import ast
import hashlib
import json
import os
from pathlib import Path
import platform
import subprocess
import sys
import zipfile


def sha256(raw):
    return hashlib.sha256(raw).hexdigest()


def write_json(path, value):
    path.write_text(json.dumps(value, ensure_ascii=False, indent=2) + "\n")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--original-evidence", type=Path, required=True)
    parser.add_argument("--work", type=Path, required=True)
    parser.add_argument("--evidence", type=Path, required=True)
    args = parser.parse_args()
    repository = Path(__file__).resolve().parents[4]
    sys.dont_write_bytecode = True
    sys.path.insert(0, str(repository / "scripts"))
    import build_node_wasix_journey as builder

    work, evidence = args.work.resolve(), args.evidence.resolve()
    assert work != evidence and not work.is_relative_to(evidence) and not evidence.is_relative_to(work)
    work.mkdir(parents=True, exist_ok=False)
    evidence.mkdir(parents=True, exist_ok=False)
    (evidence / "probe-source.py").write_bytes(Path(__file__).read_bytes())
    (evidence / "builder-source.py").write_bytes(Path(builder.__file__).read_bytes())
    original = args.original_evidence.resolve()
    archive = (original / "original.zip").read_bytes()
    assert sha256(archive) == "8048e747580e94197b730e1ebec321088dd45171a0ca1f9edb5076379f895ba5"
    with zipfile.ZipFile(original / "original.zip") as retained:
        gyp = retained.read("source-inputs/" + builder.SOURCE_PATCH["source_path"])
    header = (original / ("blob-" + builder.SOURCE_LINK_PATCH["target_blob"])).read_bytes()
    before = (original / ("blob-" + builder.SOURCE_LINK_PATCH["before_blob"])).read_bytes()
    assert sha256(gyp) == builder.SOURCE_PATCH["before_sha256"]
    assert sha256(header) == builder.SOURCE_LINK_PATCH["target_sha256"]
    assert builder.source_blob(before) == builder.SOURCE_LINK_PATCH["before_blob"]
    commands = []
    windows = []
    environment = {"PATH": "/usr/bin:/bin", "LANG": "C.UTF-8", "LC_ALL": "C.UTF-8",
                   "GIT_CONFIG_NOSYSTEM": "1", "GIT_CONFIG_GLOBAL": "/dev/null", "GIT_TERMINAL_PROMPT": "0"}
    summary = {"scope": "Synthetic minimal Git fixtures seeded by verified original bytes; not the upstream checkout or a compiler run",
               "node_or_wasix_executed": False, "product_capability_installed": False,
               "python": sys.version, "platform": platform.platform(), "windows": windows,
               "original_zip_sha256": sha256(archive), "builder_sha256": sha256(Path(builder.__file__).read_bytes()),
               "original_upstream_commit": builder.SOURCE_COMMIT, "original_upstream_tree": builder.SOURCE_TREE,
               "original_header_sha256": sha256(header), "original_link_blob": builder.source_blob(before)}

    def run_at(source, output, label, command):
        result = subprocess.run(command, cwd=source, env=environment, capture_output=True, timeout=10)
        (output / (label + ".stdout")).write_bytes(result.stdout)
        (output / (label + ".stderr")).write_bytes(result.stderr)
        commands.append({"window": source.name, "label": label, "command": command,
                         "exit_code": result.returncode, "stdout_sha256": sha256(result.stdout),
                         "stderr_sha256": sha256(result.stderr)})
        assert result.returncode == 0, commands[-1]

    try:
        for name, refusal in (("relocated", None),
                              ("changed-link", "fixed upstream header link differs before patching"),
                              ("changed-header", "fixed upstream header target differs before patching")):
            source, output = work / name, evidence / name
            source.mkdir(); output.mkdir()
            gyp_path = source / builder.SOURCE_PATCH["source_path"]
            gyp_path.parent.mkdir(parents=True)
            gyp_path.write_bytes(gyp)
            target = source / builder.SOURCE_LINK_PATCH["target_path"]
            target.parent.mkdir(parents=True)
            target.write_bytes(header if name != "changed-header" else header + b"\n")
            link = source / builder.SOURCE_LINK_PATCH["source_path"]
            link.parent.mkdir(parents=True)
            os.symlink(os.fsdecode(before) if name != "changed-link" else "../../unrecorded-header.h", link)
            original_link = os.readlink(link)
            run = lambda label, command: run_at(source, output, label, command)
            run("init", ["git", "init", "--quiet"])
            run("add", ["git", "add", "--all"])
            run("fixture-commit", ["git", "-c", "user.name=Shenko source fixture", "-c", "user.email=fixture@invalid",
                                   "commit", "--quiet", "-m", "Synthetic original-byte patch fixture"])
            archive_record = builder.apply_source_patch(source, repository, output, run)
            try:
                link_record = builder.apply_source_link_patch(source, repository, output, run)
            except RuntimeError as error:
                assert refusal is not None and str(error) == refusal, str(error)
                assert os.readlink(link) == original_link
                assert target.read_bytes() == (header if name != "changed-header" else header + b"\n")
                assert not (output / "source-link-patch-apply.stdout").exists()
                windows.append({"name": name, "expected_refusal": refusal, "actual_refusal": str(error),
                                "link_unchanged": True, "target_unchanged": True, "link_patch_not_executed": True})
            else:
                assert refusal is None
                assert link.is_symlink() and os.readlink(link) == builder.SOURCE_LINK_PATCH["after_target"]
                assert link.resolve(strict=True) == target.resolve(strict=True)
                assert link.read_bytes() == header and target.read_bytes() == header
                windows.append({"name": name, "resolved_target": builder.SOURCE_LINK_PATCH["target_path"],
                                "mode_preserved": True, "target_unchanged": True,
                                "archive_patch": archive_record, "link_patch": link_record})
        current = ast.parse(Path(builder.__file__).read_text())
        base = subprocess.run(["git", "show", "HEAD:scripts/build_node_wasix_journey.py"],
                              cwd=repository, capture_output=True, check=True).stdout
        old = ast.parse(base)
        def component(tree, name):
            return next(node for node in tree.body if getattr(node, "name", None) == name
                        or isinstance(node, ast.Assign) and any(isinstance(t, ast.Name) and t.id == name for t in node.targets))
        unchanged = ["build_budget", "BUILD_SECONDS", "ASSETS", "fetch_toolchain", "verify_archive_response_rule", "apply_source_patch"]
        for name in unchanged:
            assert ast.dump(component(current, name)) == ast.dump(component(old, name)), name
        for name in ("scripts/real_node_wasix_journey.py", ".github/workflows/node-wasix-source-build.yml"):
            prior = subprocess.run(["git", "show", "HEAD:" + name], cwd=repository, capture_output=True, check=True).stdout
            assert (repository / name).read_bytes() == prior
        summary.update(passed=True, unchanged_source_components=unchanged,
                       runtime_runner_unchanged=True, workflow_unchanged=True, distinct_windows=len(windows))
    except BaseException as error:
        summary.update(passed=False, error={"type": type(error).__name__, "message": str(error)})
        raise
    finally:
        write_json(evidence / "commands.json", commands)
        write_json(evidence / "summary.json", summary)
        print(json.dumps(summary, ensure_ascii=False, indent=2))


if __name__ == "__main__":
    main()
