import ast
import hashlib
import json
import os
import pathlib
import py_compile
import subprocess
import sys
import tempfile

import yaml

root = pathlib.Path(__file__).resolve().parent
workflow_path = root / ".github/workflows/model5-artifact-inspection.yml"
workflow_bytes = workflow_path.read_bytes()
workflow = yaml.load(workflow_bytes, Loader=yaml.BaseLoader)
assert workflow["on"] == {"push": {"branches": ["diagnostics/model5-artifact-43916477826-20261116"]}}
assert workflow["permissions"] == {"contents": "read", "actions": "read"}
assert list(workflow["jobs"]) == ["inspect-original-evidence", "recover-fixed-source"]
source_bytes = (root / "scripts/inspect_model5_artifact.py").read_bytes()
source_blob = hashlib.sha1(b"blob " + str(len(source_bytes)).encode() + b"\0" + source_bytes).hexdigest()
assert source_blob == "3c0e2e7a9ca1bd9d143bdc53ed68c0eb9bda4805"
py_compile.compile(str(root / "scripts/inspect_model5_artifact.py"), doraise=True)
shell_steps = []
for job_name, job in workflow["jobs"].items():
    assert job["timeout-minutes"] == "10"
    assert all("runner." not in value for value in job.get("env", {}).values())
    for step in job["steps"]:
        if "run" in step:
            checked = subprocess.run(["bash", "-n"], input=step["run"], text=True, capture_output=True)
            assert checked.returncode == 0, (step["name"], checked.stderr)
            shell_steps.append({"job": job_name, "step": step["name"], "exit": checked.returncode})
        if step.get("uses", "").startswith("actions/checkout@"):
            assert step["uses"] == "actions/checkout@11bd71901bbe5b1630ceea73d27597364c9af683"
            assert step["with"]["persist-credentials"] == "false"
recovery = workflow["jobs"]["recover-fixed-source"]
checkout = next(step for step in recovery["steps"] if step.get("uses", "").startswith("actions/checkout@"))
assert checkout["with"]["ref"] == "7905e8413b96015d96ea301bd1f3b6937dcfff90"
assert checkout["with"]["fetch-depth"] == "1"
assert checkout["with"]["lfs"] == checkout["with"]["submodules"] == "false"
step = next(step for step in recovery["steps"] if "RECOVERY_COMMIT" in step.get("env", {}))
inline_python = step["run"].split("python3 - <<'PY'\n", 1)[1].rsplit("\nPY\n", 1)[0]
ast.parse(inline_python)
commands = []
def command(arguments, cwd=None):
    result = subprocess.run(arguments, cwd=cwd, text=True, capture_output=True, check=True)
    commands.append({"argv": arguments, "stdout": result.stdout, "stderr": result.stderr, "exit": result.returncode})
    return result.stdout.strip()
with tempfile.TemporaryDirectory(prefix="model5-source-recovery-probe-") as temporary:
    work = pathlib.Path(temporary)
    origin = work / "origin"
    origin.mkdir()
    command(["git", "init", "-q", str(origin)])
    (origin / "plain.txt").write_text("parent\n")
    command(["git", "add", "."], origin)
    command(["git", "-c", "user.name=Recovery Probe", "-c", "user.email=probe@example.invalid", "commit", "-qm", "parent"], origin)
    parent = command(["git", "rev-parse", "HEAD"], origin)
    (origin / "plain.txt").write_text("exact current bytes\n")
    (origin / "bytes.bin").write_bytes(bytes(range(256)))
    (origin / "executable").write_text("#!/bin/false\n")
    (origin / "executable").chmod(0o755)
    (origin / "source-link").symlink_to("plain.txt")
    command(["git", "add", "."], origin)
    command(["git", "-c", "user.name=Recovery Probe", "-c", "user.email=probe@example.invalid", "commit", "-qm", "current"], origin)
    expected_commit = command(["git", "rev-parse", "HEAD"], origin)
    expected_tree = command(["git", "rev-parse", "HEAD^{tree}"], origin)
    command(["git", "clone", "-q", "--depth=1", origin.as_uri(), str(work / "recovered-source")])
    evidence = work / ".ci/evidence/source-recovery"
    evidence.mkdir(parents=True)
    runner_temp = work / "runner-temp"
    runner_temp.mkdir()
    environment = os.environ.copy()
    environment.update({
        "GITHUB_WORKSPACE": str(work),
        "RUNNER_TEMP": str(runner_temp),
        "GITHUB_SHA": "local-restoration-probe-not-a-product-run",
        "RECOVERY_COMMIT": expected_commit,
        "RECOVERY_TREE": expected_tree,
    })
    result = subprocess.run(
        ["bash", "--noprofile", "--norc", "-e", "-o", "pipefail", "-c", step["run"]],
        cwd=work, env=environment, text=True, capture_output=True, timeout=30,
    )
    (root / "local-recovery-probe.stdout").write_text(result.stdout)
    (root / "local-recovery-probe.stderr").write_text(result.stderr)
    (root / "local-recovery-probe-setup.json").write_text(json.dumps(commands, indent=2) + "\n")
    assert result.returncode == 0, (result.returncode, result.stderr)
    manifest = json.loads((evidence / "source-recovery-manifest.json").read_text())
    assert manifest["source_commit"] == expected_commit
    assert manifest["source_tree"] == expected_tree
    assert manifest["history_complete"] is False
    assert manifest["original_parent_commits"] == [parent]
    assert manifest["shallow_boundary"] == [expected_commit]
    assert manifest["verified_git_blobs"] == 4
    imported = runner_temp / "shenko-source-verification.git"
    parent_lookup = subprocess.run(
        ["git", "-C", str(imported), "cat-file", "-e", parent + "^{commit}"],
        text=True, capture_output=True,
    )
    assert parent_lookup.returncode != 0
    (root / "local-recovery-probe-manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    probe = {
        "scope": "One generated local shallow Git repository; no Shenko source or artifact executed",
        "current_commit_and_tree_preserved": True,
        "archive_regular_binary_executable_symlink_blobs_verified": 4,
        "original_parent_recorded_but_missing_as_declared": True,
        "empty_repository_pack_import_and_fsck_exit": 0,
        "history_complete": False,
        "exit": result.returncode,
    }
report = {
    "python": sys.version,
    "pyyaml": yaml.__version__,
    "inspector_original_a9_git_blob": source_blob,
    "inspector_py_compile_exit": 0,
    "workflow_sha256": hashlib.sha256(workflow_bytes).hexdigest(),
    "yaml_parse": "PyYAML BaseLoader, preserving GitHub on key",
    "shell_syntax_steps": shell_steps,
    "inline_python_ast_parse": True,
    "local_shallow_recovery_probe": probe,
    "github_workflow_validation_ran": False,
    "historical_model_inspection_ran": False,
    "product_tested": False,
}
(root / "local-validation.json").write_text(json.dumps(report, indent=2) + "\n")
print(json.dumps(report, indent=2))

