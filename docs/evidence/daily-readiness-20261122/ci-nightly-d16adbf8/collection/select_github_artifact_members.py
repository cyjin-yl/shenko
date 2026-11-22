#!/usr/bin/env python3
"""Retain selected artifact members with HTTP ranges; never print signed URLs."""
import argparse
import hashlib
import io
import json
from pathlib import Path, PurePosixPath
import re
import subprocess
import urllib.error
import urllib.request
import zipfile


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, request, fp, code, message, headers, newurl):
        return None


def archive_url(artifact):
    token = subprocess.check_output(["gh", "auth", "token"], stderr=subprocess.DEVNULL).decode().strip()
    request = urllib.request.Request(
        f"https://api.github.com/repos/cyjin-yl/shenko/actions/artifacts/{artifact}/zip",
        headers={"Authorization": "Bearer " + token, "Accept": "application/vnd.github+json"})
    try:
        with urllib.request.build_opener(NoRedirect()).open(request, timeout=45):
            raise RuntimeError("Expected an artifact redirect")
    except urllib.error.HTTPError as error:
        if error.code != 302 or not error.headers.get("Location"):
            raise RuntimeError(f"Artifact redirect failed with status {error.code}") from None
        return error.headers["Location"]


class RangeReader(io.RawIOBase):
    def __init__(self, url, size):
        self.url, self.size, self.position = url, size, 0
        self.cache, self.transferred, self.requests = [], 0, 0

    def seekable(self):
        return True

    def readable(self):
        return True

    def tell(self):
        return self.position

    def seek(self, offset, whence=0):
        self.position = offset if whence == 0 else (self.position if whence == 1 else self.size) + offset
        if not 0 <= self.position <= self.size:
            raise ValueError("ZIP seek outside artifact")
        return self.position

    def read(self, length=-1):
        length = self.size - self.position if length < 0 else min(length, self.size - self.position)
        if length == 0:
            return b""
        if length > 16 * 1024 * 1024:
            raise ValueError("Refusing an unbounded artifact read")
        start = self.position
        for offset, data in self.cache:
            if offset <= start and start + length <= offset + len(data):
                self.position += length
                return data[start-offset:start-offset+length]
        end = min(self.size - 1, start + max(length, 65536) - 1)
        request = urllib.request.Request(self.url, headers={"Range": f"bytes={start}-{end}"})
        try:
            with urllib.request.urlopen(request, timeout=45) as response:
                if response.status != 206 or response.headers.get("Content-Range") != f"bytes {start}-{end}/{self.size}":
                    raise RuntimeError("Artifact server did not honor the exact bounded range")
                data = response.read(end-start+2)
        except urllib.error.URLError:
            raise RuntimeError("Artifact range request failed; signed URL omitted") from None
        if len(data) != end-start+1:
            raise RuntimeError("Artifact range length differs")
        self.cache.append((start, data))
        self.transferred += len(data)
        self.requests += 1
        self.position += length
        return data[:length]


def retain(path, data):
    path.parent.mkdir(parents=True, exist_ok=True)
    if path.exists() and path.read_bytes() != data:
        raise ValueError("Different retained member already exists")
    path.write_bytes(data)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--artifact", required=True, type=int)
    parser.add_argument("--pattern", required=True)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    metadata_raw = subprocess.check_output(["gh", "api", f"repos/cyjin-yl/shenko/actions/artifacts/{args.artifact}"])
    metadata = json.loads(metadata_raw)
    args.output.mkdir(parents=True, exist_ok=True)
    retain(args.output / "artifact-metadata.json", metadata_raw)
    reader = RangeReader(archive_url(args.artifact), metadata["size_in_bytes"])
    selected = []
    with zipfile.ZipFile(reader) as archive:
        entries = archive.infolist()
        retain(args.output / "artifact-index.json", (json.dumps([
            {"name": item.filename, "bytes": item.file_size, "compressed_bytes": item.compress_size,
             "crc32": f"{item.CRC:08x}"} for item in entries], indent=2) + "\n").encode())
        for item in entries:
            if item.is_dir() or not re.search(args.pattern, item.filename):
                continue
            path = PurePosixPath(item.filename)
            if path.is_absolute() or ".." in path.parts or item.file_size > 2 * 1024 * 1024:
                raise ValueError("Selected member path or size outside bounded scope")
            if sum(row["bytes"] for row in selected) + item.file_size > 16 * 1024 * 1024:
                raise ValueError("Selected artifact members exceed bounded scope")
            raw = archive.read(item)
            retain(args.output / path, raw)
            selected.append({"path": str(path), "bytes": len(raw),
                             "sha256": hashlib.sha256(raw).hexdigest(), "crc32": f"{item.CRC:08x}"})
    report = {"artifact_id": args.artifact, "artifact_metadata_digest": metadata.get("digest"),
              "source_revision": metadata["workflow_run"]["head_sha"],
              "verification_scope": "Selected ZIP members read with CRC checking and individually SHA256 hashed; complete ZIP digest not independently checked.",
              "artifact_zip_bytes": metadata["size_in_bytes"], "range_requests": reader.requests,
              "range_downloaded_bytes": reader.transferred, "selected": selected}
    retain(args.output / "selection.json", (json.dumps(report, indent=2) + "\n").encode())
    print(json.dumps(report, indent=2))


if __name__ == "__main__":
    main()
