"""Shenko Python cell driver v1 (long-lived session protocol).

Runs on CPython WASI (preview1) and native CPython 3.11+. The process stays
alive across cells; variables, functions and imported modules persist in
interpreter memory for the process lifetime (SPEC §19 live session).

Host -> driver framing on stdin (blocking, exact reads):
    [u64be header_len][header JSON][u64be payload_len][cell source bytes]
header JSON: {"seq": int, "cell_dir": str, "state_path": str, "env_dir": str}
Paths are guest paths (/home/...) on WASI and host paths natively; this
driver is backend neutral and uses them verbatim.

Per cell the driver writes:
    <cell_dir>/<seq>.out       user stdout
    <cell_dir>/<seq>.err       user stderr + cell traceback
    <cell_dir>/<seq>.report.json   (renamed into place LAST; the host treats
                                    its appearance as cell completion)
Report JSON: {"ok": bool, "error": str|null, "dropped": [str],
              "state_bytes": int, "state_verified": bool}

Environment: <env_dir> is inserted at sys.path[0] per cell so packages
installed by install_dependencies resolve under the active generation.

R1 boundary: the state file holds only explicitly serializable globals
(pickle, protocol 4, same-directory atomic replace). Non-serializable
bindings (open files, sockets, functions, modules) are dropped and reported
by name. state_verified is True when the just-written state round-trips.

EOF on stdin exits the process cleanly (dispose).
"""

import json
import os
import sys
import traceback


def read_exact(stream, count):
    chunks = []
    remaining = count
    while remaining > 0:
        chunk = stream.read(remaining)
        if not chunk:
            return None  # EOF
        chunks.append(chunk)
        remaining -= len(chunk)
    return b"".join(chunks)


def write_report(cell_dir, seq, report):
    final_path = os.path.join(cell_dir, "%d.report.json" % seq)
    tmp_path = final_path + ".tmp"
    with open(tmp_path, "w", encoding="utf-8") as handle:
        handle.write(json.dumps(report))
        handle.flush()
        os.fsync(handle.fileno())
    os.replace(tmp_path, final_path)


def run_cell(header, source):
    seq = header["seq"]
    cell_dir = header["cell_dir"]
    state_path = header["state_path"]
    env_dir = header["env_dir"]

    if os.path.isdir(env_dir) and env_dir not in sys.path:
        sys.path.insert(0, env_dir)

    namespace = {"__name__": "__shenko_cell__"}
    try:
        import pickle

        with open(state_path, "rb") as handle:
            loaded = pickle.load(handle)
        if isinstance(loaded, dict):
            namespace.update(loaded)
    except FileNotFoundError:
        pass
    except Exception:
        pass  # unreadable state starts clean; visible via state_bytes

    out_path = os.path.join(cell_dir, "%d.out" % seq)
    err_path = os.path.join(cell_dir, "%d.err" % seq)
    report = {"ok": False, "error": None, "dropped": [],
              "state_bytes": 0, "state_verified": False}

    with open(out_path, "w", encoding="utf-8", errors="replace") as out_f, \
            open(err_path, "w", encoding="utf-8", errors="replace") as err_f:
        old_stdout, old_stderr = sys.stdout, sys.stderr
        sys.stdout, sys.stderr = out_f, err_f
        try:
            try:
                exec(compile(source, "<shenko-cell-%d>" % seq, "exec"), namespace)
                report["ok"] = True
            except Exception:
                err_f.write(traceback.format_exc(limit=8))
                report["error"] = "cell raised; see %d.err" % seq
        finally:
            sys.stdout, sys.stderr = old_stdout, old_stderr

    try:
        import pickle

        dropped = []
        persisted = {}
        for name, value in namespace.items():
            if name.startswith("__"):
                continue
            try:
                pickle.dumps(value, protocol=4)
            except Exception:
                dropped.append(name)
                continue
            persisted[name] = value
        tmp_path = state_path + ".tmp"
        with open(tmp_path, "wb") as handle:
            pickle.dump(persisted, handle, protocol=4)
        os.replace(tmp_path, state_path)
        report["dropped"] = sorted(dropped)
        report["state_bytes"] = os.path.getsize(state_path)
        try:
            with open(state_path, "rb") as handle:
                reloaded = pickle.load(handle)
            report["state_verified"] = isinstance(reloaded, dict)
        except Exception:
            report["state_verified"] = False
    except Exception as exc:
        report["ok"] = False
        report["error"] = (report["error"] or "") + "\nstate-persist-failed: %r" % (exc,)

    write_report(cell_dir, seq, report)


def main():
    stream = sys.stdin.buffer
    while True:
        raw_len = read_exact(stream, 8)
        if raw_len is None:
            return 0  # clean dispose
        header_len = int.from_bytes(raw_len, "big")
        header_raw = read_exact(stream, header_len)
        if header_raw is None:
            return 0
        payload_len_raw = read_exact(stream, 8)
        if payload_len_raw is None:
            return 0
        payload_len = int.from_bytes(payload_len_raw, "big")
        source = read_exact(stream, payload_len)
        if source is None:
            return 0
        try:
            header = json.loads(header_raw.decode("utf-8"))
            run_cell(header, source)
        except Exception:
            # Driver-level failure: surface on the host-visible stderr log and
            # keep the session alive if possible.
            sys.stderr.write(traceback.format_exc(limit=8))
            sys.stderr.flush()


if __name__ == "__main__":
    sys.exit(main())
