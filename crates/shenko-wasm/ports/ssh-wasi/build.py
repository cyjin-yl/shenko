#!/usr/bin/env python3
"""Build the pinned WASI SSH client and retain its inputs, log and checksum."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess


def sha256(path):
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--sdk", type=Path, required=True)
    parser.add_argument("--output-dir", type=Path, required=True)
    parser.add_argument("--target-dir", type=Path, required=True)
    parser.add_argument("--profile", choices=["dev", "release"], default="release")
    parser.add_argument("--offline", action="store_true")
    args = parser.parse_args()
    port = Path(__file__).resolve().parent
    repo = port.parents[3]
    sdk = args.sdk.resolve(strict=True)
    clang = sdk / "bin/clang"
    ar = sdk / "bin/llvm-ar"
    if not clang.is_file() or not ar.is_file():
        parser.error("SDK must contain bin/clang and bin/llvm-ar")
    output = args.output_dir.resolve()
    output.mkdir(parents=True, exist_ok=False)
    target = args.target_dir.resolve()
    env = os.environ.copy()
    env.update({"CC_wasm32_wasip1": str(clang), "AR_wasm32_wasip1": str(ar),
                "CARGO_TARGET_DIR": str(target), "CARGO_INCREMENTAL": "0",
                "CARGO_PROFILE_DEV_DEBUG": "0"})
    inputs = [port / "Cargo.toml", port / "Cargo.lock", repo / "rust-toolchain.toml"]
    for directory in [port / "src", port / "runtime-support", repo / "third-party/russh-duplex"]:
        inputs.extend(path for path in directory.rglob("*")
                      if path.is_file() and (path.suffix == ".rs" or path.name == "Cargo.toml"))
    command = ["cargo", "build", "--locked", "--manifest-path", str(port / "Cargo.toml"),
               "--target", "wasm32-wasip1", "--profile", args.profile, "-j1"]
    if args.offline:
        command.append("--offline")
    receipt = {
        "kind": "ssh_wasi_build", "command": command,
        "source_commit": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=repo, text=True).strip(),
        "source_dirty": bool(subprocess.check_output(["git", "status", "--porcelain"], cwd=repo)),
        "source_sha256": {str(path.relative_to(repo)): sha256(path) for path in sorted(set(inputs))},
        "rustc": subprocess.check_output(["rustc", "--version", "--verbose"], text=True),
        "sdk_clang": subprocess.check_output([str(clang), "--version"], text=True),
        "sdk_clang_sha256": sha256(clang),
        "profile": args.profile,
    }
    with (output / "build.log").open("wb") as log:
        result = subprocess.run(command, cwd=repo, env=env, stdout=log, stderr=subprocess.STDOUT)
    receipt["exit_code"] = result.returncode
    if result.returncode == 0:
        artifact = target / "wasm32-wasip1" / ("debug" if args.profile == "dev" else "release") / "shenko-ssh-wasi.wasm"
        retained = output / "shenko-ssh-wasi.wasm"
        shutil.copyfile(artifact, retained)
        receipt.update({"artifact": retained.name, "artifact_bytes": retained.stat().st_size,
                        "artifact_sha256": sha256(retained)})
    (output / "build-receipt.json").write_text(json.dumps(receipt, indent=2) + "\n")
    print(json.dumps({"output_dir": str(output), "exit_code": result.returncode}))
    raise SystemExit(result.returncode)


if __name__ == "__main__":
    main()
