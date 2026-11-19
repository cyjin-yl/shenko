#!/usr/bin/env python3
"""Offline adapter only: calls the exact published read-only inspector on a verified ZIP."""
import contextlib, hashlib, importlib.util, json, pathlib, resource, signal, sys, time, traceback, zipfile
root = pathlib.Path(__file__).resolve().parent
out = root / "analysis"
out.mkdir(exist_ok=True)
module_path = root / "inspect_model5_artifact.py"
spec = importlib.util.spec_from_file_location("inspector", module_path)
inspector = importlib.util.module_from_spec(spec)
spec.loader.exec_module(inspector)
original = root / "original.zip"
started = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
state = {"inspection_complete": False, "mode": "offline_local_zip", "started_at": started,
 "original_product_outcome_unchanged": True, "diagnostic_source_revision": "a9c17717a6effb504c63ff5248401c8857d65558",
 "inspector_blob_sha": "3c0e2e7a9ca1bd9d143bdc53ed68c0eb9bda4805",
 "inspector_sha256": hashlib.sha256(module_path.read_bytes()).hexdigest(),
 "python": sys.version, "phase": "zip_verification", "errors": [],
 "metadata_boundary": "GitHub metadata was fetched through the connector separately. This offline adapter does not re-run verify_metadata or download_zip."}
def timeout(_signum, _frame):
    raise RuntimeError("480 second offline inspection deadline")
signal.signal(signal.SIGALRM, timeout)
signal.alarm(480)
resource.setrlimit(resource.RLIMIT_AS, (1024*1024*1024, 1024*1024*1024))
exit_code = 1
with (out / "inspector.stdout").open("w", encoding="utf-8") as stdout, (out / "inspector.stderr").open("w", encoding="utf-8") as stderr:
    with contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
        try:
            raw = original.read_bytes()
            assert len(raw) == inspector.ZIP_BYTES, "original ZIP length mismatch"
            assert hashlib.sha256(raw).hexdigest() == inspector.ZIP_SHA256, "original ZIP SHA256 mismatch"
            src = module_path.read_bytes()
            assert hashlib.sha1(b"blob " + str(len(src)).encode() + b"\0" + src).hexdigest() == state["inspector_blob_sha"], "published inspector blob mismatch"
            state["original_zip"] = {"path": "../original.zip", "bytes": len(raw), "sha256": hashlib.sha256(raw).hexdigest(), "independently_verified": True}
            del raw
            with zipfile.ZipFile(original) as archive:
                state["phase"] = "member_inventory"
                members = inspector.inventory_zip(out, archive)
                state["phase"] = "analysis"
                state.update(inspector.analyze(out, archive, members))
            state["inspection_complete"] = True
            state["phase"] = "complete"
            exit_code = 0
        except Exception as error:
            state["errors"].append({"type": type(error).__name__, "message": str(error)})
            traceback.print_exc()
        finally:
            signal.alarm(0)
            state["finished_at"] = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
            inspector.save(out / "inspection-state.json", state)
            inspector.output_index(out)
print(json.dumps(state, ensure_ascii=False))
raise SystemExit(exit_code)

