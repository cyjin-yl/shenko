#!/usr/bin/env python3
"""Read/claim exact queue bodies and install verified remote response bytes."""
import argparse
import base64
import gzip
import hashlib
import json
import re
from pathlib import Path
from relay_server import atomic, sha, utc, write_json


def checked_directory(root, identity):
    if not re.fullmatch(r"[0-9]{4}-[a-f0-9]{32}", identity):
        raise ValueError("Invalid relay request id")
    return root / "queue" / identity


def claim(root):
    for directory in sorted((root / "queue").iterdir()):
        if not (directory / "request.ready").exists() or (directory / "request.claimed").exists():
            continue
        try:
            with (directory / "request.claimed").open("x") as stream:
                stream.write(utc() + "\n")
        except FileExistsError:
            continue
        request = json.loads((directory / "request.json").read_text())
        body = (directory / "request.body").read_bytes()
        assert sha(body) == request["request_sha256"]
        # Decode/re-encode is checked here; JSON content itself is never parsed.
        text = body.decode("utf-8", errors="strict")
        assert text.encode("utf-8") == body
        print(json.dumps({"pending": True, "request": request, "body_utf8": text}, ensure_ascii=False))
        return
    print('{"pending":false}')


def ingest(root, envelope_path):
    envelope = json.loads(envelope_path.read_text())
    receipt = envelope["receipt"]
    directory = checked_directory(root, receipt["id"])
    local_request = (directory / "request.body").read_bytes()
    assert sha(local_request) == receipt["request_sha256"], "Request SHA256 differs across machines"
    compressed = b"".join(base64.b64decode(chunk, validate=True) for chunk in envelope["gzip_base64_chunks"])
    assert sha(compressed) == receipt["response_gzip_sha256"], "Compressed transfer SHA256 mismatch"
    response = gzip.decompress(compressed)
    assert sha(response) == receipt["response_sha256"], "Response SHA256 differs across machines"
    assert len(response) == receipt["response_bytes"]
    headers = base64.b64decode(envelope["headers_base64"], validate=True)
    assert sha(headers) == receipt["headers_sha256"]
    assert "\r" not in receipt["content_type"] and "\n" not in receipt["content_type"]
    atomic(directory / "response.body.gz", compressed)
    atomic(directory / "response.body", response)
    atomic(directory / "response.headers", headers)
    write_json(directory / "remote.json", receipt)
    write_json(directory / "transfer-verification.json", {
        "verified_at_utc": utc(), "request_bytes_identical": True, "response_bytes_identical": True,
        "request_sha256": receipt["request_sha256"], "response_sha256": receipt["response_sha256"],
        "compression": "gzip only for connector transport; decoded before HTTP response",
        "response_model_content_parsed_or_rewritten": False, "inference_retries": 0,
    })
    (directory / "response.ready").touch()
    print(json.dumps({"id": receipt["id"], "verified": True, "http_status": receipt["http_status"],
                      "curl_exit": receipt["curl_exit"], "response_bytes": len(response)}))


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--root", type=Path, required=True)
    parser.add_argument("command", choices=["claim", "ingest"])
    parser.add_argument("envelope", nargs="?", type=Path)
    options = parser.parse_args()
    if options.command == "claim":
        claim(options.root)
    else:
        ingest(options.root, options.envelope)
