#!/usr/bin/env python3
"""Temporary byte-preserving, file-queue HTTP relay for the Box Qwen service.

The external tool orchestrator moves the queued bytes. This server does not
construct prompts, infer answers, parse model content, or replay requests.
All successful model bodies, including SSE, are buffered and forwarded intact.
"""
import argparse
import hashlib
import json
import os
import threading
import time
import uuid
from datetime import datetime, timezone
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path


def utc():
    return datetime.now(timezone.utc).isoformat()


def sha(data):
    return hashlib.sha256(data).hexdigest()


def atomic(path, data):
    temporary = path.with_name(path.name + ".tmp")
    temporary.write_bytes(data)
    temporary.replace(path)


def write_json(path, value):
    atomic(path, (json.dumps(value, ensure_ascii=False, indent=2) + "\n").encode())


class Relay(ThreadingHTTPServer):
    daemon_threads = True

    def __init__(self, address, root):
        self.root = root
        self.queue = root / "queue"
        self.queue.mkdir(parents=True, exist_ok=True)
        self.lock = threading.Lock()
        self.sequence = 0
        super().__init__(address, Handler)

    def event(self, value):
        value = {"utc": utc(), **value}
        line = json.dumps(value, ensure_ascii=False) + "\n"
        with self.lock:
            with (self.root / "events.jsonl").open("a") as stream:
                stream.write(line)
        print(line, end="", flush=True)


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *_):
        pass

    def transport_error(self, status, reason):
        body = json.dumps({"error": {"type": "shenko_box_relay_transport_error", "message": reason}}).encode()
        self.send_response_only(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Connection", "close")
        self.end_headers()
        self.wfile.write(body)
        self.close_connection = True

    def do_GET(self):
        self.handle_request()

    def do_POST(self):
        self.handle_request()

    def handle_request(self):
        received = time.monotonic()
        if (self.command, self.path) not in {
            ("GET", "/v1/models"), ("POST", "/v1/chat/completions")
        }:
            return self.transport_error(501, "Route is outside this temporary relay's scope")
        # No credential is needed for either loopback endpoint. Never retain it.
        if self.headers.get("Authorization") or self.headers.get("X-Api-Key"):
            return self.transport_error(400, "Unexpected credential header; no request forwarded")
        if self.headers.get("Transfer-Encoding"):
            return self.transport_error(400, "Chunked request bodies are not supported by this relay")
        try:
            size = int(self.headers.get("Content-Length", "0"))
            if not 0 <= size <= 16 * 1024 * 1024:
                raise ValueError("Request body is outside relay size bounds")
            body = self.rfile.read(size)
            if len(body) != size:
                raise ValueError("Incomplete request body")
        except Exception as error:
            return self.transport_error(400, str(error))
        with self.server.lock:
            self.server.sequence += 1
            number = self.server.sequence
        identity = f"{number:04d}-{uuid.uuid4().hex}"
        directory = self.server.queue / identity
        directory.mkdir()
        request = {
            "id": identity, "method": self.command, "path": self.path,
            "received_at_utc": utc(), "body_bytes": len(body),
            "request_sha256": sha(body),
            "content_type": self.headers.get("Content-Type", "application/json"),
            "authorization_header_present": False,
            "transport": "Box connector file relay; whole-response buffering including SSE",
        }
        atomic(directory / "request.body", body)
        write_json(directory / "request.json", request)
        (directory / "request.ready").touch()
        self.server.event({"event": "queued", "id": identity, "method": self.command,
                           "path": self.path, "body_bytes": len(body), "request_sha256": sha(body)})
        # The real provider has its own 180-second deadline. Do not silently retry
        # an inference if its caller disconnects. Keep the response evidence.
        while not (directory / "response.ready").exists():
            if time.monotonic() - received >= 600:
                write_json(directory / "delivery.json", {"status": "relay_wait_expired", "elapsed_ms": (time.monotonic()-received)*1000})
                return self.transport_error(504, "Timed out waiting for the one remote attempt; no retry")
            time.sleep(0.05)
        receipt = json.loads((directory / "remote.json").read_text())
        response = (directory / "response.body").read_bytes()
        assert sha(body) == receipt["request_sha256"]
        assert sha(response) == receipt["response_sha256"]
        elapsed = (time.monotonic() - received) * 1000
        delivery = {"id": identity, "delivered_at_utc": utc(), "relay_elapsed_ms": elapsed,
                    "request_sha256": sha(body), "response_sha256": sha(response),
                    "response_bytes": len(response), "remote": receipt,
                    "timing_caveat": "Includes connector queue, transfer, polling, and whole-response buffering; not native provider latency"}
        try:
            if receipt["curl_exit"] != 0 or not 100 <= receipt["http_status"] <= 599:
                delivery["status"] = "transport_error"
                self.transport_error(502, f"Remote curl attempt failed with exit {receipt['curl_exit']}; raw partial evidence retained; no retry")
            else:
                self.send_response_only(receipt["http_status"])
                self.send_header("Content-Type", receipt["content_type"])
                self.send_header("Content-Length", str(len(response)))
                self.send_header("Connection", "close")
                self.end_headers()
                self.wfile.write(response)
                self.wfile.flush()
                self.close_connection = True
                delivery["status"] = "forwarded_raw_response"
        except (BrokenPipeError, ConnectionResetError, TimeoutError) as error:
            delivery["status"] = "caller_disconnected"
            delivery["error"] = type(error).__name__
        finally:
            write_json(directory / "delivery.json", delivery)
            self.server.event({"event": "delivery", "id": identity, "status": delivery["status"],
                               "http_status": receipt["http_status"], "curl_exit": receipt["curl_exit"],
                               "response_bytes": len(response), "relay_elapsed_ms": elapsed})


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--root", type=Path, required=True)
    parser.add_argument("--port", type=int, default=18833)
    options = parser.parse_args()
    options.root.mkdir(parents=True, exist_ok=True)
    server = Relay(("127.0.0.1", options.port), options.root)
    write_json(options.root / "server.json", {"pid": os.getpid(), "host": "127.0.0.1", "port": options.port,
               "started_at_utc": utc(), "source_sha256": sha(Path(__file__).read_bytes()),
               "upstream": "Box 127.0.0.1:18831", "inference_retries": 0,
               "body_transformations": "none", "streaming": "buffered without changing SSE bytes"})
    server.event({"event": "listening", "host": "127.0.0.1", "port": options.port, "pid": os.getpid()})
    server.serve_forever(poll_interval=0.2)
