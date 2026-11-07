"""Two-process consumer for foreign-author PMA reminder admission (before/after).

What this program does
----------------------
It renders and builds two small Rust binaries and runs them as separate
processes over **one** private installation:

1. ``--before-repo`` — the pre-fix source tree. The **producer** links that
   tree's real crates, creates two genuine subjects, two genuine research
   epochs owned by the owner agent's subject-root task, admits one real
   ``pma.evaluate`` native task for the *other* agent and one for the owner,
   lets the real broker, handler and policy parser commit both decisions, and
   records the old preparation boundary's result. One HTTP body per call is
   scripted; the receipt labels that surface explicitly.
2. ``--candidate-repo`` — the source tree under test. The **reader** links it
   and reopens the same installation twice plus once inside the real
   projection runtime.

What the reader must show (the acceptance is in the reader's own receipt)
------------------------------------------------------------------------
* the agent drain and both prepared-context read paths omit the foreign
  reminder and still prepare the genuinely own-authored one;
* the typed omission identity matches the accepted receipt → operation → task
  chain and the epoch's committed owning task, recomputed by the reader;
* the committed decision, its content revision, the operation, the attempt,
  the receipt, the epoch, the lineage and the subject head are byte-identical
  to what the old producer recorded;
* repeated reopens change no durable byte and produce no duplicate refusal;
* the real projection runtime publishes only the owned reminder, and the
  candidate's refresh hook keeps that batch stable;
* the original bounded request is never resent (the reader has no broker,
  provider or HTTP dependency, and the operation still holds exactly one
  attempt and one receipt).

What it deliberately never does
-------------------------------
* no verification of its own: it builds and runs the two consumers and
  records argv, exit codes, durations and digests. It performs no unit test,
  no lint, no formatting and no source edit;
* it never writes into either repository (both crates are rendered into the
  output work directory and only *read* the repo trees);
* it never fabricates an observation: a missing or pre-fixed before tree, a
  candidate without the admission API, or a failed check is reported as such
  and never translated into a pass.

Usage::

    python3 scripts/pma_legacy_reminder_consumer.py \\
        --before-repo <pre-fix worktree> --candidate-repo <candidate worktree> \\
        --installation <fresh empty dir> --out <evidence dir> \\
        [--target <cargo target dir>] [--config <cargo config>] \\
        [--deadline 180] [--passes 2]

Exit codes: 0 all checks pass; 2 usage/preflight/build/run error; 3 the old
producer ran but its own boundary did not expose the foreign reminder (the
premise does not hold); 4 the candidate reader reported failing checks; 5 the
two reader processes disagreed with each other.
"""

import argparse
import hashlib
import json
import os
import pathlib
import shutil
import subprocess
import sys
import time

SCRIPTS = pathlib.Path(__file__).resolve().parent
CRATES = SCRIPTS / "pma_legacy_reminder_consumer"
DEFAULT_CONFIG = pathlib.Path(
    "/tank/shared/projects/shenko/build/ecosystem-compat-20261104/combined-config.toml"
)
CARGO_ENV = {
    "CARGO_HOME": "/usr/local/cargo",
    "RUSTUP_HOME": "/usr/local/rustup",
    "PATH": "/usr/local/cargo/bin:" + os.environ.get("PATH", ""),
    "CARGO_NET_OFFLINE": "true",
    "CARGO_TERM_COLOR": "never",
    "RUST_BACKTRACE": "1",
}

# The exact strings the candidate must provide, checked textually (a *source*
# precondition, not a compile or behaviour proof).
CANDIDATE_MARKERS = [
    ("crates/shenko-store/src/research.rs", "pub fn pending_intervention_omissions"),
    ("crates/shenko-store/src/research.rs", "InterventionOmissionKind"),
    ("crates/shenko-memory/src/intervention_runtime.rs", "let mut state = self.state.lock();"),
    ("crates/shenko-memory/src/intervention_runtime.rs", "state.commit_seq = None;"),
]
# The old producer must NOT already contain the fix, or "before" is a lie.
BEFORE_MARKERS = [
    ("crates/shenko-store/src/research.rs", "fn pending_intervention_omissions"),
]
# The reader must not be able to send anything.
NO_SEND_MARKERS = ["shenko-broker", "local_provider", "reqwest", "hyper", "tungstenite"]


def sha256_file(path):
    return hashlib.sha256(pathlib.Path(path).read_bytes()).hexdigest()


def sha256_bytes(data):
    return hashlib.sha256(data).hexdigest()


def run(out, receipt, name, argv, timeout, cwd, check=None):
    """One observed process. Records everything; decides nothing on its own."""
    started = time.time()
    try:
        completed = subprocess.run(
            argv,
            cwd=str(cwd),
            env={**os.environ, **CARGO_ENV},
            capture_output=True,
            timeout=timeout,
        )
        timed_out = False
        stdout, stderr, code = completed.stdout, completed.stderr, completed.returncode
    except subprocess.TimeoutExpired as expired:
        timed_out = True
        stdout = expired.stdout or b""
        stderr = expired.stderr or b""
        code = None
    seconds = round(time.time() - started, 3)
    record = {
        "argv": argv,
        "cwd": str(cwd),
        "exit_code": code,
        "timed_out": timed_out,
        "seconds": seconds,
        "stdout_sha256": sha256_bytes(stdout),
        "stderr_sha256": sha256_bytes(stderr),
    }
    (out / f"{name}.stdout").write_bytes(stdout)
    (out / f"{name}.stderr").write_bytes(stderr)
    receipt.append(record)
    if check is not None and (code != check or timed_out):
        raise SystemExit(
            f"{name} exited {code} (timed_out={timed_out}); see {out / (name + '.stderr')}"
        )
    return record


def render(crate, work_dir, repo):
    target = work_dir / crate
    if target.exists():
        shutil.rmtree(target)
    shutil.copytree(CRATES / crate, target)
    template = (CRATES / crate / "Cargo.toml.in").read_text()
    rendered = template.replace("@BEFORE_REPO@", str(repo)).replace("@CANDIDATE_REPO@", str(repo))
    (target / "Cargo.toml.in").unlink()
    (target / "Cargo.toml").write_text(rendered)
    return {
        "package": crate,
        "repo": str(repo),
        "manifest_sha256": sha256_file(target / "Cargo.toml"),
        "source_sha256": sha256_file(target / "src" / "main.rs"),
        "template_sha256": sha256_file(CRATES / crate / "Cargo.toml.in"),
    }


def preflight(before_repo, candidate_repo, manifest):
    """Textual source preconditions, labelled as such. Compile/behaviour is
    decided by the build and the reader's own checks, never here."""
    problems = []
    for relative, marker in BEFORE_MARKERS:
        path = before_repo / relative
        if not path.is_file():
            problems.append(f"before repo is missing {relative}")
            continue
        if marker in path.read_text(errors="replace"):
            problems.append(
                f"before repo already contains {marker!r} in {relative}: it is not a pre-fix tree, "
                "so the before/after premise would be fabricated"
            )
    for relative, marker in CANDIDATE_MARKERS:
        path = candidate_repo / relative
        if not path.is_file():
            problems.append(f"candidate repo is missing {relative}")
            continue
        if marker not in path.read_text(errors="replace"):
            problems.append(f"candidate repo is missing {marker!r} in {relative}")
    reader_manifest = (CRATES / "reader" / "Cargo.toml.in").read_text()
    for marker in NO_SEND_MARKERS:
        if marker in reader_manifest:
            problems.append(
                f"the reader manifest mentions {marker!r}: it could send, which must stay impossible"
            )
    manifest["textual_preflight"] = {
        "before_repo": str(before_repo),
        "candidate_repo": str(candidate_repo),
        "before_must_not_contain": BEFORE_MARKERS,
        "candidate_must_contain": CANDIDATE_MARKERS,
        "reader_must_not_depend_on": NO_SEND_MARKERS,
        "problems": problems,
        "note": "textual source precondition only; not a compile, behaviour or acceptance proof",
    }
    if problems:
        raise SystemExit("preflight failed:\n  " + "\n  ".join(problems))


def repo_state(repo):
    state = {"path": str(repo)}
    try:
        head = subprocess.run(
            ["git", "-C", str(repo), "rev-parse", "HEAD"],
            capture_output=True,
            text=True,
            timeout=30,
        )
        state["head"] = head.stdout.strip() if head.returncode == 0 else None
        status = subprocess.run(
            ["git", "-C", str(repo), "status", "--porcelain"],
            capture_output=True,
            text=True,
            timeout=30,
        )
        state["dirty"] = status.stdout.splitlines() if status.returncode == 0 else None
    except (OSError, subprocess.SubprocessError) as error:  # git is optional
        state["error"] = repr(error)
    return state


def compared_view(reader_receipt):
    """The stable cross-process projection: everything the second reader must
    reproduce exactly. Absolute paths, durations, SQLite side files and the
    ledger's run sequence are dropped because they legitimately differ between
    processes; every committed fact, every prepared-path result, every
    omission record and every check outcome stays."""

    def without_seq(facts):
        return {key: value for key, value in facts.items() if key != "commit_seq"}

    return {
        "checks": [{"name": item["name"], "ok": item["ok"]} for item in reader_receipt["checks"]["items"]],
        "passes": [
            {
                "facts": without_seq(pass_["observed"]["facts"]),
                "prepared_paths": pass_["observed"]["prepared_paths"],
                "omissions": pass_["observed"]["omissions"],
                "durable_sha256": pass_["files"]["durable_sha256"],
            }
            for pass_ in reader_receipt["passes"]
        ],
        "runtime": {
            key: reader_receipt["runtime"][key]
            for key in (
                "batch_before_refresh",
                "batch_after_refresh",
                "refresh_ok",
                "foreign_body_published",
                "own_body_published",
            )
        },
        "before_receipt_sha256": reader_receipt["before_receipt"]["sha256"],
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--before-repo", required=True, type=pathlib.Path)
    parser.add_argument("--candidate-repo", required=True, type=pathlib.Path)
    parser.add_argument("--installation", required=True, type=pathlib.Path)
    parser.add_argument("--out", required=True, type=pathlib.Path)
    parser.add_argument("--target", type=pathlib.Path, default=None)
    parser.add_argument("--config", type=pathlib.Path, default=DEFAULT_CONFIG)
    parser.add_argument("--deadline", type=int, default=180)
    parser.add_argument("--passes", type=int, default=2)
    args = parser.parse_args()

    before_repo = args.before_repo.resolve()
    candidate_repo = args.candidate_repo.resolve()
    out = args.out.resolve()
    installation = args.installation.resolve()
    out.mkdir(parents=True, exist_ok=True)
    if installation.exists() and any(installation.iterdir()):
        raise SystemExit(f"--installation must be fresh: {installation} is not empty")
    installation.mkdir(parents=True, exist_ok=True)
    if not args.config.is_file():
        raise SystemExit(f"missing cargo config {args.config}")
    work = out / "work"
    work.mkdir(parents=True, exist_ok=True)
    target = args.target or (out / "target")

    manifest = {
        "schema_version": 1,
        "driver": "scripts/pma_legacy_reminder_consumer.py",
        "driver_sha256": sha256_file(__file__),
        "contract_reading": "SPEC §51.1–§51.10 with docs/decisions/0002/0003 (PR180 merge 2cb17172)",
        "config": {"path": str(args.config), "sha256": sha256_file(args.config)},
        "repos": {"before": repo_state(before_repo), "candidate": repo_state(candidate_repo)},
        "crates": {
            "producer": render("producer", work, before_repo),
            "reader": render("reader", work, candidate_repo),
        },
    }
    preflight(before_repo, candidate_repo, manifest)

    runs = []
    builds = []
    cargo = ["cargo", "--config", str(args.config)]
    for name, crate in (("producer", "producer"), ("reader", "reader")):
        directory = work / crate
        run(
            out,
            builds,
            f"{name}-metadata",
            cargo + ["metadata", "--offline", "--format-version", "1"],
            max(60, args.deadline),
            directory,
            check=0,
        )
        env_target = ["--target-dir", str(target)]
        run(
            out,
            builds,
            f"{name}-build",
            cargo + ["build", "--offline", "-j1", *env_target, "--message-format=json"],
            max(600, args.deadline * 3),
            directory,
            check=0,
        )

    producer_bin = target / "debug" / "pma-legacy-reminder-producer"
    reader_bin = target / "debug" / "pma-legacy-reminder-reader"
    for binary in (producer_bin, reader_bin):
        if not binary.is_file():
            raise SystemExit(f"the build produced no {binary}")

    before_receipt = out / "before.json"
    producer_run = run(
        out,
        runs,
        "producer",
        [
            str(producer_bin),
            "--installation",
            str(installation),
            "--out",
            str(before_receipt),
            "--deadline-secs",
            str(args.deadline),
        ],
        max(60, args.deadline * 2),
        out,
        check=None,
    )
    if producer_run["exit_code"] == 3:
        verdict = {
            "verdict": "before_path_not_vulnerable",
            "explanation": "the old producer completed but its own preparation boundary did not "
            "publish or drain the foreign reminder; there is nothing for the candidate to omit, "
            "so no after claim is made",
        }
        (out / "receipt.json").write_text(
            json.dumps({"manifest": manifest, "builds": builds, "runs": runs, **verdict}, indent=2)
        )
        print(json.dumps(verdict, indent=2))
        return 3
    if producer_run["exit_code"] != 0:
        raise SystemExit(
            f"producer exited {producer_run['exit_code']}; see {out / 'producer.stderr'}"
        )
    before = json.loads(before_receipt.read_text())

    reader_receipts = []
    for attempt in (1, 2):
        receipt_path = out / f"after-{attempt}.json"
        record = run(
            out,
            runs,
            f"reader-{attempt}",
            [
                str(reader_bin),
                "--installation",
                str(installation),
                "--before-receipt",
                str(before_receipt),
                "--out",
                str(receipt_path),
                "--passes",
                str(args.passes),
                "--deadline-secs",
                str(min(args.deadline, 60)),
            ],
            max(60, args.deadline),
            out,
            check=None,
        )
        if not receipt_path.is_file():
            raise SystemExit(
                f"reader attempt {attempt} wrote no receipt (exit {record['exit_code']}); "
                f"see {out / f'reader-{attempt}.stderr'}"
            )
        reader_receipts.append(json.loads(receipt_path.read_text()))

    first, second = reader_receipts
    cross_process_equal = compared_view(first) == compared_view(second)
    checks_ok = bool(first["checks"]["all_ok"]) and bool(second["checks"]["all_ok"])
    reader_exit_ok = [record["exit_code"] for record in runs[-2:]] == [0, 0]

    summary = {
        "before_vulnerable": before["old_path"]["vulnerable"],
        "before_owner_drain_ids": before["old_path"]["owner_agent_drain_ids"],
        "before_published_keys": before["old_path"]["published_batch_keys"],
        "foreign_reminder": before["reminders"]["foreign"]["json"]["reminder_id"],
        "own_reminder": before["reminders"]["own"]["json"]["reminder_id"],
        "after_omissions": first["passes"][0]["observed"]["omissions"],
        "after_drain_ids": first["passes"][0]["observed"]["prepared_paths"]["agent_drain_ids"],
        "after_published_keys": first["runtime"]["batch_before_refresh"],
        "reopen_facts_identical": all(
            check["ok"] for check in first["checks"]["items"] if "reopen_" in check["name"]
        ),
        "runtime_durable_delta": {
            key: value
            for key, value in first["runtime_durable_delta"].items()
            if key != "explanation"
        },
        "checks_failed": first["checks"]["failures"] + second["checks"]["failures"],
        "cross_process_equal": cross_process_equal,
        "reader_exit_codes": [record["exit_code"] for record in runs[-2:]],
        "checks_ok": checks_ok and reader_exit_ok and cross_process_equal,
    }
    if summary["checks_ok"]:
        verdict = "candidate_omits_foreign_author_reminder"
        code = 0
    elif not cross_process_equal:
        verdict = "reader_processes_disagree"
        code = 5
    else:
        verdict = "candidate_checks_failed"
        code = 4

    (out / "receipt.json").write_text(
        json.dumps(
            {
                "manifest": manifest,
                "builds": builds,
                "runs": runs,
                "before": {
                    "path": str(before_receipt),
                    "sha256": sha256_file(before_receipt),
                    "contract": before.get("contract"),
                    "scripted": before.get("scripted"),
                    "files": before.get("files"),
                },
                "after": [
                    {"path": str(out / f"after-{index}.json"), "sha256": sha256_file(out / f"after-{index}.json")}
                    for index in (1, 2)
                ],
                "summary": summary,
                "verdict": verdict,
                "limits": {
                    "source_preflight": "textual only; the build and the reader's own checks decide",
                    "producer_contract": "pre-fix code by construction; it proves no §51 behaviour",
                    "refresh_failure_path": "not induced",
                    "scope": "bounded local mechanism observation; not model efficacy and not an "
                    "account-level acceptance",
                },
            },
            indent=2,
            ensure_ascii=False,
        )
    )
    print(json.dumps({"verdict": verdict, "summary": summary}, indent=2, ensure_ascii=False))
    return code


if __name__ == "__main__":
    sys.exit(main())
