#!/usr/bin/env python3
"""Build pinned Node/V8 WASIX plus its recorded patch, then the original journey."""

import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import platform
import shutil
import signal
import subprocess
import sys
import tarfile
import time
import traceback
import urllib.error
import urllib.request

import real_node_wasix_journey as journey


MIB, GIB = journey.MIB, journey.GIB
SOURCE_URL = "https://github.com/Multi-V-VM/node-wasix32.git"
SOURCE_COMMIT = "20871d1fb49e1af91b1fc729b0cf4e119c0bfd6f"
SOURCE_TREE = "1f91f029db924986b371c4b848fb65ffd080613d"
SOURCE_PATCH = {
    "path": "crates/shenko-wasm/ports/node-wasix/patches/0001-wasi-archive-response-files.patch",
    "sha256": "93970b459d28ad3ac6af9c6446bc175220b4ea3b2f9ab29f5330710454d5fee9",
    "source_path": "tools/gyp/pylib/gyp/generator/make.py",
    "before_sha256": "29092beb6082ee2170676449f74204f73a9843486e2468f073bca75468ce2245",
    "after_sha256": "da8c30caf420fcbd795a8ed9d06b1bb730aec1d138c555d558f5b8f6b0ba3b02",
    "purpose": "use the existing per-object response-file archive recipes for wasi/wasm instead of one oversized command argument",
}
BUILD_SECONDS = 7200
ASSETS = (
    {"name": "wasixcc", "version": "0.4.7", "source_commit": "d59389f68277c60b65cb1cc1524de641d845d336",
     "url": "https://github.com/wasix-org/wasixcc/releases/download/v0.4.7/wasixcc-x86_64-unknown-linux-gnu.tar.gz",
     "bytes": 3258983, "sha256": "d39db81b6b76d9567e0f61955f390b8586d932ab683816989e23305f27099c2f", "expanded_byte_limit": 32 * MIB},
    {"name": "llvm", "version": "21.1.206", "source_commit": "98c00cb849c94742b2b29918bb414d236c83ae97",
     "url": "https://github.com/wasix-org/llvm-project/releases/download/21.1.206/LLVM-Linux-x86_64-gnu.tar.gz",
     "bytes": 75596113, "sha256": "ab32cf976226dc9478c9cb8331fa877163ff34d78b190fb201d6eac92cb7dab0", "expanded_byte_limit": GIB},
    {"name": "sysroot", "version": "2026-10-02.1", "source_commit": "2025b44a5d2eb22b64ecdbf1bb16f3e7699dfb60",
     "url": "https://github.com/wasix-org/wasix-libc/releases/download/v2026-10-02.1/sysroot.tar.gz",
     "bytes": 5951821, "sha256": "ffc1a6f52e81a4945a9df7bb2fee632a051403eb64316b006543ea54cd779c6b", "expanded_byte_limit": 128 * MIB},
    {"name": "binaryen", "version": "128", "source_commit": "06cc1d9b452df3e7f4d80bb9314a863c69d4aad6",
     "url": "https://github.com/WebAssembly/binaryen/releases/download/version_128/binaryen-version_128-x86_64-linux.tar.gz",
     "bytes": 114606053, "sha256": "4ce79586d1c4762502eebe9a1db071fa5e446ef8897f2f766eb1cce5ec6dee9e", "expanded_byte_limit": GIB},
)
CRITICAL_SOURCE = (
    "configure.py", "common.gypi", "node.gyp", "node.gypi",
    "tools/v8_gypfiles/features.gypi", "tools/v8_gypfiles/toolchain.gypi", "tools/v8_gypfiles/v8.gyp",
    "tools/gyp/pylib/gyp/generator/make.py",
    "deps/v8/src/builtins/wasm32/builtins-wasm32.cc", "lib/internal/buffer.js", "src/node_version.h",
)


def build_budget(observation):
    journey.require(observation["error"] is None, observation["error"])
    headroom = observation["effective_headroom_bytes"]
    reserve = max(2 * GIB, headroom // 4)
    rss = min(10 * GIB, headroom - reserve) // (64 * MIB) * (64 * MIB)
    journey.require(rss >= 4 * GIB, "source build cannot retain its reserve and a 4 GiB diagnostic RSS budget")
    return {
        "rss_monitor_bytes": rss, "available_memory_reserve_bytes": reserve,
        "free_disk_reserve_bytes": max(4 * GIB, min(8 * GIB, observation["disk_free_bytes"] // 10)),
        "address_space_hard_bytes": 24 * GIB, "cpu_soft_seconds": BUILD_SECONDS,
        "file_hard_bytes": 2 * GIB, "monitor_interval_seconds": 0.2,
        "output_monitor_bytes": 128 * MIB,
        "basis": "actual visible headroom; reserve max(2 GiB, one quarter); sampled group RSS remainder capped at 10 GiB; this is an experimental budget, not a measured compiler requirement",
        "enforcement": "per-process hard AS/CPU/file limits, sampled group RSS/headroom/disk/output guards; one make job and one Binaryen worker; total build-stage wall budget 7200 seconds",
    }


def extract_verified(archive_path, destination, asset, reserve, deadline):
    destination.mkdir()
    manifest = []
    total = 0
    seen = set()
    with tarfile.open(archive_path, "r:gz") as archive:
        for member in archive:
            journey.require(journey.STOP_SIGNAL is None and time.monotonic() < deadline, "toolchain extraction interrupted or expired")
            journey.require(shutil.disk_usage(destination).free > reserve + max(0, member.size), "toolchain extraction reached disk reserve")
            relative = PurePosixPath(member.name)
            journey.require(not relative.is_absolute() and ".." not in relative.parts, "archive path escapes its owned directory")
            key = str(relative)
            journey.require(key not in seen or member.isdir(), "duplicate archive file")
            seen.add(key)
            journey.require(member.isdir() or member.isreg() or member.issym() or member.islnk(), "unsupported archive file type")
            total += member.size
            journey.require(total <= asset["expanded_byte_limit"] and len(manifest) < 100000, "toolchain expansion exceeded its recorded bound")
            # The stdlib data filter checks both link targets and real extraction paths.
            archive.extract(member, destination, filter="data")
            manifest.append({"path": key, "bytes": member.size, "type": member.type.decode("ascii"), "link": member.linkname})
    return {"expanded_bytes": total, "members": manifest}


def fetch_toolchain(work, evidence, resources, deadline):
    roots = {}
    for asset in ASSETS:
        report = {**asset, "started_at": journey.now(), "verified": False}
        target = work / "downloads" / (asset["name"] + ".tar.gz")
        reader = None
        try:
            observed = resources.snapshot()
            budget = build_budget(observed)
            journey.require(observed["disk_free_bytes"] > budget["free_disk_reserve_bytes"] + asset["bytes"] + asset["expanded_byte_limit"], "insufficient observed disk for the next fixed toolchain asset")
            with urllib.request.urlopen(asset["url"], timeout=30) as response:
                reader = journey.DownloadReader(response, asset["bytes"], min(deadline, time.monotonic() + 600), work, budget["free_disk_reserve_bytes"])
                with target.open("xb") as output:
                    while chunk := reader.read(MIB):
                        output.write(chunk)
            journey.require(reader.count == asset["bytes"] and reader.sha.hexdigest() == asset["sha256"], "toolchain archive size or SHA256 mismatch")
            root = work / "toolchains" / asset["name"]
            report.update(extract_verified(target, root, asset, budget["free_disk_reserve_bytes"], deadline))
            report["verified"] = True
            roots[asset["name"]] = root
            target.unlink()
        finally:
            if reader is not None:
                report.update(actual_bytes=reader.count, actual_sha256=reader.sha.hexdigest())
            report["finished_at"] = journey.now()
            journey.write_json(evidence / (asset["name"] + "-download.json"), report)
    return roots


def unique_file(root, name, parent=None):
    candidates = [path for path in root.rglob(name) if path.is_file() and (parent is None or path.parent.name == parent)]
    journey.require(len(candidates) == 1, "toolchain has no unique " + str(parent) + "/" + name)
    result = candidates[0]
    journey.require(result.resolve(strict=True).is_relative_to(root.resolve(strict=True)), "toolchain symlink escapes its verified archive")
    return result


def keep_build_inputs(source, evidence):
    destination = evidence / "source-inputs"
    entries = []
    for relative in CRITICAL_SOURCE:
        path = source / relative
        target = destination / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(path, target)
        entries.append({"path": relative, "bytes": path.stat().st_size, "sha256": journey.digest(path)})
    journey.write_json(evidence / "critical-source-files.json", entries)


def keep_build_configuration(source, evidence):
    destination = evidence / "build-configuration"
    total = 0
    for relative in (
        "config.gypi", "config.mk", "config.status", "out/Makefile", "out/node.target.mk",
        "out/deps/openssl/openssl.target.mk", "out/deps/openssl/openssl.host.mk",
        "out/Release/obj.target/deps/openssl/libopenssl.a.ar-file-list",
    ):
        path = source / relative
        if not path.is_file():
            continue
        total += path.stat().st_size
        journey.require(total <= 16 * MIB, "generated configuration capture exceeded its bound")
        target = destination / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(path, target)


def apply_source_patch(source, repository, evidence, run):
    patch = repository / SOURCE_PATCH["path"]
    target = source / SOURCE_PATCH["source_path"]
    journey.require(journey.digest(patch) == SOURCE_PATCH["sha256"], "archive recipe patch identity changed")
    journey.require(journey.digest(target) == SOURCE_PATCH["before_sha256"], "fixed upstream GYP source differs before patching")
    shutil.copyfile(patch, evidence / "source.patch")
    run("source-patch-check", ["git", "apply", "--check", str(patch)])
    run("source-patch-apply", ["git", "apply", "--whitespace=error-all", str(patch)])
    journey.require(journey.digest(target) == SOURCE_PATCH["after_sha256"], "patched GYP source differs from the recorded postimage")
    run("source-patched-paths", ["git", "diff", "--name-only", "HEAD", "--"])
    journey.require((evidence / "source-patched-paths.stdout").read_text().splitlines() == [SOURCE_PATCH["source_path"]], "source patch changed an unrecorded tracked file")
    run("source-patched-diff", ["git", "diff", "--binary", "HEAD", "--"])
    patched = evidence / "effective-source-inputs" / SOURCE_PATCH["source_path"]
    patched.parent.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(target, patched)
    record = {**SOURCE_PATCH, "upstream_commit": SOURCE_COMMIT, "upstream_tree": SOURCE_TREE,
              "effective_diff_sha256": journey.digest(evidence / "source-patched-diff.stdout")}
    journey.write_json(evidence / "source-patch-record.json", record)
    return record


def verify_archive_response_rule(source, evidence):
    path = source / "out/deps/openssl/openssl.target.mk"
    text = path.read_text()
    # The previous run observed 928 OpenSSL compile outputs and a printf length
    # failure at this archive target. Refuse before compilation if configure
    # did not select a response-file recipe, preserving normal/thin archive mode.
    normal = text.count("$(call create_archive,$@,$^)")
    thin = text.count("$(call create_thin_archive,$@,$^)")
    selected = normal + thin == 1 and "$(call do_cmd,alink" not in text
    journey.write_json(evidence / "openssl-archive-rule.json", {
        "source_path": str(path.relative_to(source)), "sha256": journey.digest(path),
        "response_file_rule_selected": selected,
        "archive_kind": "normal" if normal == 1 else "thin" if thin == 1 else None,
        "scope": "generated rule check only; not a compiler or guest execution result",
    })
    journey.require(selected, "generated OpenSSL target did not select the recorded response-file archive rule")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source-revision", required=True)
    parser.add_argument("--work-dir", type=Path, required=True)
    parser.add_argument("--evidence-dir", type=Path, required=True)
    args = parser.parse_args()
    evidence, work = args.evidence_dir.resolve(), args.work_dir.resolve()
    journey.require(evidence != work and not evidence.is_relative_to(work) and not work.is_relative_to(evidence), "work and evidence directories must be separate")
    os.umask(0o077)
    evidence.mkdir(parents=True, mode=0o700)
    summary = {"started_at": journey.now(), "stage": "admission", "checks": {}, "build_succeeded": False, "reopen_attempted": False,
               "feasibility_passed": False, "product_capability_installed": False, "named_target_acceptance": False}
    lock = None
    owned_work = False
    source = work / "source"
    deadline = time.monotonic() + BUILD_SECONDS
    try:
        journey.require(sys.platform == "linux" and platform.machine() == "x86_64", "the fixed toolchain requires Linux x86_64")
        journey.require(hasattr(tarfile, "data_filter"), "safe toolchain extraction requires the stdlib tar data filter")
        for number in (signal.SIGINT, signal.SIGTERM):
            signal.signal(number, journey.handle_signal)
        work.mkdir(parents=True, mode=0o700)
        owned_work = True
        for directory in ("source", "toolchains", "downloads", "bin", "guest", "tmp", "runtime-home", "cache"):
            (work / directory).mkdir(mode=0o700)
        lock = open("/tmp/shenko-cargo.lock", "a")
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        repository = Path(__file__).resolve().parent.parent
        head = subprocess.run(["git", "-C", str(repository), "rev-parse", "HEAD"], check=True, capture_output=True, text=True, timeout=5).stdout.strip()
        journey.require(head == args.source_revision and len(head) == 40, "requested Shenko source revision does not match the checkout")
        journey.require(journey.digest(repository / journey.DRIVER_PATH) == journey.DRIVER_SHA256, "original driver changed")
        first_input = journey.frames([(1, journey.FIRST_SOURCE), (2, journey.SECOND_SOURCE)])
        journey.require(hashlib.sha256(first_input).hexdigest() == journey.FIRST_INPUT_SHA256, "original framed input changed")
        (evidence / "planned-first.stdin").write_bytes(first_input)
        (evidence / "planned-reopened.stdin").write_bytes(journey.frames([(3, journey.REOPEN_SOURCE)]))
        shutil.copyfile(Path(__file__), evidence / "builder.py")
        shutil.copyfile(Path(journey.__file__), evidence / "runner.py")
        shutil.copyfile(repository / journey.DRIVER_PATH, work / "guest/js_cell_driver.mjs")
        for directory in ("env", "cells"):
            (work / "guest" / directory).mkdir()
        (work / "guest/env/package.json").write_text('{"name":"node-wasix-probe","private":true}\n')
        journey.write_json(evidence / "manifest.json", {
            "source_revision": head, "upstream_repository": SOURCE_URL, "upstream_commit": SOURCE_COMMIT, "upstream_tree": SOURCE_TREE,
            "source_patch": SOURCE_PATCH,
            "source_transfer_bytes": None, "source_transfer_size_note": "Git transfer size is not known in advance; the actual object store and worktree are observed after fetch",
            "builder_sha256": journey.digest(Path(__file__)), "runner_sha256": journey.digest(Path(journey.__file__)),
            "driver_sha256": journey.DRIVER_SHA256, "first_input_sha256": journey.FIRST_INPUT_SHA256,
            "toolchain_assets": ASSETS, "wasmer_archive_sha256": journey.WASMER_SHA256,
            "platform": platform.platform(), "inherited_process_limits": Path("/proc/self/limits").read_text(),
            "product_capability_installed": False, "named_target_acceptance": False,
        })
        resources = journey.VisibleResources(work)
        initial = resources.snapshot()
        journey.write_json(evidence / "resources-before-source-build.json", initial)
        journey.write_json(evidence / "build-resource-budget.json", build_budget(initial))
        journey.require(initial["disk_free_bytes"] >= 24 * GIB, "source build requires a 24 GiB starting disk budget; actual build size remains unmeasured")
        environment = {"PATH": "/usr/bin:/bin", "LANG": "C.UTF-8", "LC_ALL": "C.UTF-8", "TMPDIR": str(work / "tmp"),
                       "GIT_CONFIG_NOSYSTEM": "1", "GIT_CONFIG_GLOBAL": "/dev/null", "GIT_TERMINAL_PROMPT": "0",
                       "MAKEFLAGS": "-j1", "CMAKE_BUILD_PARALLEL_LEVEL": "1", "BINARYEN_CORES": "1", "RAYON_NUM_THREADS": "1"}

        def run(label, command, wall=120, *, network=False):
            summary["stage"] = label
            remaining = deadline - time.monotonic()
            journey.require(remaining > 0, "total source-build deadline expired")
            budget = build_budget(resources.snapshot())
            result = journey.run_owned(label, command, b"", work, evidence, resources, budget,
                                       wall=min(wall, remaining), cwd=source, environment=environment,
                                       deny_network=not network, sample_seconds=5)
            journey.require(result["exit_code"] == 0 and result["stopped"] is None, label + " failed; see its original output and process observation")
            return result

        run("source-init", ["git", "init", "--quiet"])
        run("source-fetch", ["git", "-c", "protocol.file.allow=never", "-c", "protocol.ext.allow=never", "fetch", "--no-tags", "--depth=1", SOURCE_URL, SOURCE_COMMIT], wall=600, network=True)
        run("source-checkout", ["git", "checkout", "--detach", "--quiet", "FETCH_HEAD"])
        run("source-identity", ["git", "rev-parse", "HEAD", "HEAD^{tree}"])
        journey.require((evidence / "source-identity.stdout").read_text().splitlines() == [SOURCE_COMMIT, SOURCE_TREE], "fetched upstream source identity differs")
        run("source-integrity", ["git", "fsck", "--full", "--strict"], wall=180)
        run("source-files", ["git", "ls-tree", "-rlz", "HEAD"])
        records = (evidence / "source-files.stdout").read_bytes().split(b"\0")
        tracked_bytes = 0
        tracked_count = 0
        for record in records:
            if record:
                fields = record.partition(b"\t")[0].split()
                journey.require(fields[1] == b"blob", "unrecorded upstream submodule requires a separate source binding")
                tracked_bytes += int(fields[3])
                tracked_count += 1
        git_bytes = sum(path.stat().st_size for path in (source / ".git").rglob("*") if path.is_file())
        journey.write_json(evidence / "source-size.json", {"tracked_files": tracked_count, "tracked_file_bytes": tracked_bytes, "git_directory_bytes": git_bytes,
                                                          "network_transfer_bytes": None, "note": "on-disk Git objects do not measure bytes transmitted on the network"})
        journey.require(not (source / "out").exists(), "source checkout already contains build output")
        keep_build_inputs(source, evidence)
        patch_record = apply_source_patch(source, repository, evidence, run)
        summary["stage"] = "download-toolchain"
        roots = fetch_toolchain(work, evidence, resources, deadline)
        installer = unique_file(roots["wasixcc"], "wasixccenv")
        clang = unique_file(roots["llvm"], "clang", "bin")
        wasm_opt = unique_file(roots["binaryen"], "wasm-opt", "bin")
        libc = unique_file(roots["sysroot"], "libc.a", "wasm32-wasi")
        sysroot = libc.parent.parent.parent
        journey.require((sysroot / "include/stdlib.h").is_file(), "fixed WASIX sysroot layout is not recognized")
        environment.update({"WASIXCC_LLVM_LOCATION": str(clang.parent.parent), "WASIXCC_BINARYEN_LOCATION": str(wasm_opt.parent.parent),
                            "WASIXCC_SYSROOT": str(sysroot), "WASIXCC_SYSROOT_PREFIX": str(roots["sysroot"]),
                            "WASIXCC_WASM_EXCEPTIONS": "no", "WASIXCC_RUN_WASM_OPT": "yes", "WASIXCC_GENERATE_SHELL_SCRIPT": "no",
                            "WASIXCC_LINKER_FLAGS": "--threads=1"})
        run("install-compiler-wrappers", [str(installer), "install-executables", str(work / "bin")])
        environment.update({"CC": str(work / "bin/wasixcc"), "CXX": str(work / "bin/wasix++"),
                            "AR": str(work / "bin/wasixar"), "RANLIB": str(work / "bin/wasixranlib"), "NM": str(work / "bin/wasixnm"),
                            "CC_host": "/usr/bin/gcc", "CXX_host": "/usr/bin/g++", "AR_host": "/usr/bin/ar",
                            "CC_target": str(work / "bin/wasixcc"), "CXX_target": str(work / "bin/wasix++"), "AR_target": str(work / "bin/wasixar")})
        tools = {"wasixcc": installer, "clang": clang, "binaryen": wasm_opt, "host-gcc": Path("/usr/bin/gcc"),
                 "host-g++": Path("/usr/bin/g++"), "make": Path("/usr/bin/make"), "python": Path("/usr/bin/python3")}
        tool_records = {}
        for name, executable in tools.items():
            run("tool-" + name.replace("+", "p"), [str(executable), "--version"], wall=30)
            tool_records[name] = {"path": str(executable), "sha256": journey.digest(executable), "bytes": executable.stat().st_size}
        journey.write_json(evidence / "executed-build-tools.json", tool_records)
        # Snapshot and code cache are already disabled by upstream cross-compiling
        # semantics. OpenSSL stays enabled; only native assembler use is disabled.
        configure = ["/usr/bin/python3", "configure.py", "--dest-cpu=wasm32", "--dest-os=wasi", "--cross-compiling", "--openssl-no-asm"]
        run("configure", configure, wall=300)
        keep_build_configuration(source, evidence)
        verify_archive_response_rule(source, evidence)
        make = ["/usr/bin/make", "-C", "out", "BUILDTYPE=Release", "V=1", "-j1", "node"]
        run("compile", make, wall=BUILD_SECONDS)
        run("source-patched-diff-after-build", ["git", "diff", "--binary", "HEAD", "--"])
        journey.require((evidence / "source-patched-diff-after-build.stdout").read_bytes()
                        == (evidence / "source-patched-diff.stdout").read_bytes(),
                        "tracked source changed beyond the recorded archive patch during build")
        output = source / "out/Release/node"
        journey.require(output.is_file() and not output.is_symlink() and output.stat().st_size <= 512 * MIB, "fresh Node output is missing, aliased, or exceeds its retained bound")
        with output.open("rb") as module:
            journey.require(module.read(8) == b"\0asm\x01\0\0\0", "fresh output is not a WebAssembly core module")
        module_hash = journey.digest(output)
        journey.require(module_hash != journey.NODE_SHA256, "source build unexpectedly reused the failed release module")
        journey.require(module_hash != journey.digest(source / "node.wasm"), "source build unexpectedly reused the stale tracked Node module")
        shutil.copyfile(output, work / "node.wasm")
        shutil.copyfile(output, evidence / "built-node.wasm")
        attestation = {"kind": "observed-local-build-record", "cryptographically_signed": False, "build_succeeded": True,
                       "source_repository": SOURCE_URL, "source_commit": SOURCE_COMMIT, "source_tree": SOURCE_TREE,
                       "tracked_source_matches_recorded_patch_after_build": True, "source_patch": patch_record,
                       "fresh_output_directory": True,
                       "configure_command": configure, "compile_command": make, "toolchain_assets": ASSETS, "build_tools": tool_records,
                       "module_bytes": output.stat().st_size, "module_sha256": module_hash, "module_path": "out/Release/node",
                       "shenko_source_revision": head, "builder_sha256": journey.digest(Path(__file__)), "recorded_at": journey.now()}
        journey.write_json(evidence / "build-attestation.json", attestation)
        summary["build_succeeded"] = True
        summary["module_sha256"] = module_hash
        summary["stage"] = "original-runtime-journey"
        runtime_budget = journey.choose_budget(resources.snapshot())
        executable, module = journey.fetch_assets(work, evidence, runtime_budget["free_disk_reserve_bytes"], download_node=False)
        journey.require(journey.digest(module) == module_hash, "built module changed before runtime observation")
        journey.require(journey.digest(work / "guest/js_cell_driver.mjs") == journey.DRIVER_SHA256, "original guest driver changed during the build")
        journey.require(not any((work / "guest/cells").iterdir()) and not (work / "guest/state.json").exists() and not (work / "guest/witness.json").exists(), "the source build modified the fresh guest evidence directory")
        runtime_budget = journey.choose_budget(resources.snapshot())
        journey.write_json(evidence / "resource-budget.json", runtime_budget)
        journey.run_journey(executable, module, work, evidence, resources, runtime_budget, summary)
    except BaseException as error:
        message = "HTTP status " + str(error.code) if isinstance(error, urllib.error.HTTPError) else str(error)
        summary["error"] = {"type": type(error).__name__, "message": message}
        (evidence / "failure.txt").write_text(message + "\n" if isinstance(error, urllib.error.HTTPError) else "".join(traceback.format_exception_only(type(error), error)))
    finally:
        try:
            summary["owned_groups_settled"] = journey.groups_settled(evidence)
            journey.require(summary["owned_groups_settled"], "an owned outcome is unconfirmed; output is not a stable snapshot")
            if owned_work:
                keep_build_configuration(source, evidence)
                journey.capture_guest(work, evidence)
        except BaseException as error:
            summary["evidence_capture_error"] = {"type": type(error).__name__, "message": str(error)}
        summary["compiled_cache_files"] = [str(path.relative_to(work)) for path in (work / "cache").rglob("*") if path.is_file()] if owned_work else []
        summary["stop_signal"] = journey.STOP_SIGNAL
        summary["finished_at"] = journey.now()
        summary["feasibility_passed"] = summary["build_succeeded"] and bool(summary["checks"]) and all(summary["checks"].values()) and summary["reopen_attempted"] and not any(key in summary for key in ("error", "evidence_capture_error")) and not summary["compiled_cache_files"] and journey.STOP_SIGNAL is None
        journey.write_json(evidence / "summary.json", summary)
        entries = [{"path": str(path.relative_to(evidence)), "bytes": path.stat().st_size, "sha256": journey.digest(path)}
                   for path in sorted(evidence.rglob("*")) if path.is_file() and path.name != "files.json"]
        journey.write_json(evidence / "files.json", entries)
        if lock is not None:
            lock.close()
    print(json.dumps(summary), flush=True)
    return 0 if summary["feasibility_passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
