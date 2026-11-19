#!/usr/bin/env python3
"""Exercise retained GYP archive recipes locally; never build or execute Node.

The observed OpenSSL output paths come from the complete original CI ZIP. The
objects used here are hard links to one newly compiled native C object. This
checks the make/exec argument boundary and response-file archive membership,
not WASIX compiler compatibility, the original uncaptured argv or Node behavior.
"""

import argparse
import datetime
import hashlib
import json
import os
from pathlib import Path
import platform
import shlex
import signal
import subprocess
import time
import zipfile

ORIGINAL_ZIP_SHA256 = "56ac75e7ca5580b4c0ce19c5a7dcc96f61ffb31e049f67f323194fb33fb8b73d"


def digest(raw):
    return hashlib.sha256(raw).hexdigest()


def now():
    return datetime.datetime.now(datetime.timezone.utc).isoformat()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--original-zip", type=Path, required=True)
    parser.add_argument("--work", type=Path, required=True)
    parser.add_argument("--evidence", type=Path, required=True)
    args = parser.parse_args()
    work, evidence = args.work.resolve(), args.evidence.resolve()
    assert not any(character.isspace() or character in "$#%:;`\\" for character in str(work))
    assert work != evidence and not evidence.is_relative_to(work) and not work.is_relative_to(evidence)
    work.mkdir(parents=True, exist_ok=False)
    evidence.mkdir(parents=True, exist_ok=False)
    (evidence / "probe-source.py").write_bytes(Path(__file__).read_bytes())
    records = []
    summary = {
        "started_at": now(), "scope": "local GNU make/native ar command-length diagnostic only",
        "node_or_wasix_executed": False, "original_ci_argv_captured": False,
        "syscall_trace_captured": False,
        "platform": platform.platform(), "page_bytes": os.sysconf("SC_PAGE_SIZE"),
        "arg_max_bytes": os.sysconf("SC_ARG_MAX"), "work": str(work),
    }
    environment = {"PATH": "/usr/bin:/bin", "LANG": "C.UTF-8", "LC_ALL": "C.UTF-8"}

    def run(label, command, expected=0):
        record = {"label": label, "command": command, "started_at": now()}
        records.append(record)
        started = time.monotonic()
        with (evidence / (label + ".stdout")).open("wb") as stdout, (evidence / (label + ".stderr")).open("wb") as stderr:
            process = subprocess.Popen(command, cwd=work, env=environment, stdout=stdout, stderr=stderr, start_new_session=True)
            try:
                record["exit_code"] = process.wait(timeout=30)
            except BaseException:
                if process.poll() is None:
                    try:
                        os.killpg(process.pid, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
                try:
                    record["exit_code"] = process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    record.update(exit_code=None, cleanup_outcome_unknown=True)
                record["interrupted"] = True
                raise
            finally:
                record.update(finished_at=now(), elapsed_seconds=time.monotonic() - started,
                              leader_reaped=process.poll() is not None)
        for channel in ("stdout", "stderr"):
            raw = (evidence / (label + "." + channel)).read_bytes()
            record[channel] = {"bytes": len(raw), "sha256": digest(raw)}
        if expected is not None:
            assert record["exit_code"] == expected, record
        return record

    try:
        original = args.original_zip.read_bytes()
        assert digest(original) == ORIGINAL_ZIP_SHA256
        with zipfile.ZipFile(args.original_zip) as archive:
            def member(suffix):
                names = [name for name in archive.namelist() if name == suffix or name.endswith("/" + suffix)]
                assert len(names) == 1, names
                raw = archive.read(names[0])
                return names[0], raw

            make_name, make_bytes = member("build-configuration/out/Makefile")
            log_name, log_bytes = member("compile.stdout")
        summary["original_inputs"] = {
            "zip_sha256": ORIGINAL_ZIP_SHA256,
            "generated_makefile": {"member": make_name, "sha256": digest(make_bytes)},
            "compile_stdout": {"member": log_name, "sha256": digest(log_bytes)},
        }
        observed = []
        for line in log_bytes.decode().splitlines():
            if "/obj.target/openssl/" not in line or "wasixcc -o " not in line:
                continue
            words = shlex.split(line)
            output = words[words.index("-o") + 1]
            if "/obj.target/openssl/" in output and output.endswith(".o"):
                observed.append(output)
        assert len(observed) == len(set(observed)) == 928
        (evidence / "observed-object-paths.json").write_text(json.dumps(observed, indent=2) + "\n")
        summary["observed_object_path_count"] = len(observed)
        summary["observed_joined_path_bytes"] = len(" ".join(observed).encode())
        run("make-version", ["/usr/bin/make", "--version"])
        run("ar-version", ["/usr/bin/ar", "--version"])
        run("gcc-version", ["/usr/bin/gcc", "--version"])
        (work / "probe.c").write_text("int shenko_archive_probe(void) { return 0; }\n")
        run("native-object", ["/usr/bin/gcc", "-c", str(work / "probe.c"), "-o", str(work / "probe.o")])
        native_object = (work / "probe.o").read_bytes()
        (evidence / "native-object.o").write_bytes(native_object)
        summary["native_object_sha256"] = digest(native_object)
        objects = []
        for path in observed:
            relative = Path(path.split("/obj.target/openssl/", 1)[1])
            assert not relative.is_absolute() and ".." not in relative.parts
            output = work / "observed-paths" / "out/Release/obj.target/openssl" / relative
            output.parent.mkdir(parents=True, exist_ok=True)
            os.link(work / "probe.o", output)
            objects.append(output)
        summary["local_joined_path_bytes"] = len(" ".join(map(str, objects)).encode())
        assert summary["local_joined_path_bytes"] > 128 * 1024
        header = make_bytes.decode().split('# Declare the "all" target first', 1)[0]
        assert header.count("define do_cmd\n") == 1
        (evidence / "retained-make-header.txt").write_text(header)
        for variant, recipe in (
            ("legacy", "$(call do_cmd,alink)"),
            ("response-normal", "$(call create_archive,$@,$^)"),
            ("response-thin", "$(call create_thin_archive,$@,$^)"),
        ):
            target = work / variant / "libopenssl.a"
            target.parent.mkdir()
            makefile = work / (variant + ".mk")
            makefile.write_text(header + "\n.DEFAULT_GOAL := all\n.PHONY: all\nall: " + str(target)
                                + "\n" + str(target) + ": TOOLSET := target\n" + str(target)
                                + ": " + " ".join(map(str, objects)) + "\n\t" + recipe + "\n")
            (evidence / makefile.name).write_bytes(makefile.read_bytes())
            command = ["/usr/bin/make", "--no-print-directory", "-f", str(makefile), "V=1", "-j1",
                       "AR.target=/usr/bin/ar", "builddir=" + str(work / "build"), "all"]
            if variant == "legacy":
                outcome = run(variant, command, expected=None)
                summary["legacy_exit_code"] = outcome["exit_code"]
                assert outcome["exit_code"] != 0
                assert b"Argument list too long" in (evidence / "legacy.stderr").read_bytes()
                assert not target.exists()
            else:
                run(variant, command)
                response = Path(str(target) + ".ar-file-list").read_bytes()
                assert response == ("\n".join(map(str, objects)) + "\n").encode()
                (evidence / (variant + ".ar-file-list")).write_bytes(response)
                run(variant + "-members", ["/usr/bin/ar", "t", str(target)])
                members = (evidence / (variant + "-members.stdout")).read_text().splitlines()
                expected = [path.name for path in objects] if variant == "response-normal" else list(map(str, objects))
                assert members == expected
                summary[variant] = {"member_count": len(members), "member_order_matches": True,
                                    "response_bytes": len(response), "response_sha256": digest(response),
                                    "archive_bytes": target.stat().st_size, "archive_sha256": digest(target.read_bytes())}
        summary["passed"] = True
    except BaseException as error:
        summary["passed"] = False
        summary["failure"] = {"type": type(error).__name__, "message": str(error)}
        raise
    finally:
        summary["finished_at"] = now()
        (evidence / "commands.json").write_text(json.dumps(records, indent=2) + "\n")
        (evidence / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
        print(json.dumps(summary, indent=2))


if __name__ == "__main__":
    main()
