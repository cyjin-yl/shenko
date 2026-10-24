# Native POSIX/Python3 receipt helper, version 1. Not a WASM implementation.
# The SSH account and receipt root are an explicitly registered trust boundary.
import base64
import hashlib
import json
import os
import stat
import subprocess
import sys
import uuid

LIMIT = 1024 * 1024
root, operation, seal, mode = sys.argv[1:]
assert str(uuid.UUID(operation)) == operation
assert os.path.isabs(root)

def write_atomic(path, value):
    tmp = path + ".new"
    with open(tmp, "x", encoding="utf-8") as f:
        json.dump(value, f, separators=(",", ":"))
        f.flush()
        os.fsync(f.fileno())
    os.replace(tmp, path)
    fd = os.open(os.path.dirname(path), os.O_RDONLY | os.O_DIRECTORY)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)

def bounded(path, limit=LIMIT):
    with open(path, "rb") as f:
        data = f.read(limit + 1)
    return base64.b64encode(data[:limit]).decode(), len(data) > limit

def inspect():
    directory = os.path.join(root, operation)
    try:
        with open(os.path.join(directory, "intent.json"), encoding="utf-8") as f:
            intent = json.load(f)
        if intent["seal"] != seal:
            return {"state": "conflict", "operation_id": operation, "seal": seal}
        with open(os.path.join(directory, "receipt.json"), encoding="utf-8") as f:
            return json.load(f)
    except FileNotFoundError:
        return {"state": "unknown", "operation_id": operation, "seal": seal}

if mode == "query":
    # Missing state is NOT permission to execute, recreate, or retry.
    print(json.dumps(inspect(), separators=(",", ":")))
    sys.exit(0)

payload = json.load(sys.stdin)
os.makedirs(root, mode=0o700, exist_ok=True)
st = os.lstat(root)
assert stat.S_ISDIR(st.st_mode) and st.st_uid == os.getuid() and st.st_mode & 0o077 == 0
folder = os.path.join(root, operation)
try:
    os.mkdir(folder, mode=0o700)
except FileExistsError:
    print(json.dumps(inspect(), separators=(",", ":")))
    sys.exit(0)
write_atomic(os.path.join(folder, "intent.json"), {"seal": seal, "action": payload})
stdout = os.path.join(folder, "stdout")
stderr = os.path.join(folder, "stderr")
code = 0
verification = None
with open(stdout, "xb") as out, open(stderr, "xb") as err:
    try:
        if payload["kind"] == "exec":
            code = subprocess.run(payload["argv"], input=base64.b64decode(payload["stdin_b64"], validate=True), stdout=out, stderr=err, check=False).returncode
        elif payload["kind"] == "upload":
            data = base64.b64decode(payload["data_b64"], validate=True)
            assert len(data) <= LIMIT and os.path.isabs(payload["path"])
            destination = payload["path"]
            tmp = os.path.join(os.path.dirname(destination), ".shenko-" + operation)
            # Create private from the first instant, regardless of umask. A
            # later chmod cannot revoke a descriptor another account opened.
            fd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
            with os.fdopen(fd, "wb") as f:
                f.write(data)
                f.flush()
                os.fsync(f.fileno())
            os.replace(tmp, destination)
            fd = os.open(os.path.dirname(destination), os.O_RDONLY | os.O_DIRECTORY)
            try:
                os.fsync(fd)
            finally:
                os.close(fd)
            verification = hashlib.sha256(data).hexdigest()
        elif payload["kind"] == "download":
            assert os.path.isabs(payload["path"])
            with open(payload["path"], "rb") as f:
                data = f.read(LIMIT + 1)
            assert len(data) <= LIMIT
            out.write(data)
            verification = hashlib.sha256(data).hexdigest()
        else:
            raise ValueError("unsupported action")
    except Exception as e:
        code = 1
        err.write((type(e).__name__ + ": " + str(e)).encode())
    out.flush()
    err.flush()
    os.fsync(out.fileno())
    os.fsync(err.fileno())
out, out_cut = bounded(stdout)
err, err_cut = bounded(stderr, LIMIT // 4)
receipt = {"schema": 1, "operation_id": operation, "seal": seal, "state": "completed", "exit_code": code,
           "stdout_b64": out, "stderr_b64": err, "stdout_truncated": out_cut, "stderr_truncated": err_cut, "sha256": verification}
write_atomic(os.path.join(folder, "receipt.json"), receipt)
# A broken connection here does not erase the already sealed receipt.
print(json.dumps(receipt, separators=(",", ":")))
