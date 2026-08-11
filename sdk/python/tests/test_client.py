"""Python SDK client tests against a fake HTTP server."""

import json
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import pytest

from clouisle.client import Client, SandboxError
from clouisle.types import (
    ExecRequest,
    ImageRef,
    Resources,
    SandboxSpec,
)


class Handler(BaseHTTPRequestHandler):
    requests = []  # type: ignore

    def _record(self, body: bytes = b"") -> None:
        try:
            parsed = json.loads(body) if body else None
        except (ValueError, json.JSONDecodeError):
            parsed = None
        self.requests.append(
            {
                "method": self.command,
                "url": self.path,
                "auth": self.headers.get("Authorization"),
                "content_type": self.headers.get("Content-Type"),
                "body": parsed,
            }
        )

    def _json(self, status: int, payload: dict) -> None:
        data = json.dumps(payload).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_POST(self) -> None:
        length = int(self.headers.get("Content-Length", 0))
        body = self.rfile.read(length) if length else b""
        self._record(body)
        if self.path.startswith("/api/v1/sandboxes/") and self.path.endswith("/exec"):
            self._json(
                200,
                {
                    "exec_id": "e-1",
                    "exit_code": 0,
                    "stdout": "hello\n",
                    "stderr": "",
                    "duration_ms": 1,
                },
            )
        elif self.path == "/api/v1/sandboxes":
            self._json(201, {"id": "sbx-1", "status": "running"})
        else:
            self._json(200, {"ok": True})

    def do_GET(self) -> None:
        self._record()
        if self.path.startswith("/api/v1/sandboxes/"):
            self._json(200, {"id": "sbx-1", "status": "running"})
        else:
            self._json(200, {"items": [], "total": 0})

    def log_message(self, *args) -> None:  # silence
        pass


@pytest.fixture()
def server():
    Handler.requests = []
    httpd = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=httpd.serve_forever, daemon=True).start()
    yield f"http://127.0.0.1:{httpd.server_address[1]}"
    httpd.shutdown()


def test_create_sandbox_sends_post_with_bearer(server):
    client = Client(server, "secret-key")
    sandbox = client.create_sandbox(
        SandboxSpec(
            image=ImageRef(reference="alpine:latest"),
            resources=Resources(vcpu=1, memory_mb=256, disk_mb=512),
        )
    )
    assert sandbox.id == "sbx-1"
    assert sandbox.status == "running"
    req = Handler.requests[0]
    assert req["method"] == "POST"
    assert req["url"] == "/api/v1/sandboxes"
    assert req["auth"] == "Bearer secret-key"
    assert req["body"]["image"]["reference"] == "alpine:latest"


def test_get_and_exec_target_scoped_paths(server):
    client = Client(server, "k")
    sandbox = client.get_sandbox("sbx-1")
    assert sandbox.id == "sbx-1"
    result = client.exec("sbx-1", ExecRequest(argv=["echo", "hello"], timeout_ms=5000))
    assert result.exit_code == 0
    assert result.stdout == "hello\n"
    assert [r["url"] for r in Handler.requests] == [
        "/api/v1/sandboxes/sbx-1",
        "/api/v1/sandboxes/sbx-1/exec",
    ]


def test_trailing_slash_normalized(server):
    client = Client(f"{server}/", "k")
    client.get_sandbox("sbx-1")
    assert Handler.requests[0]["url"] == "/api/v1/sandboxes/sbx-1"


def test_error_maps_to_sandbox_error():
    class ErrHandler(BaseHTTPRequestHandler):
        def do_GET(self):
            data = json.dumps(
                {
                    "error": {
                        "code": "NOT_FOUND",
                        "message": "sbx-x not found",
                        "details": None,
                    }
                }
            ).encode()
            self.send_response(404)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)

        def log_message(self, *args):
            pass

    httpd = ThreadingHTTPServer(("127.0.0.1", 0), ErrHandler)
    threading.Thread(target=httpd.serve_forever, daemon=True).start()
    try:
        client = Client(f"http://127.0.0.1:{httpd.server_address[1]}", "k")
        with pytest.raises(SandboxError) as exc_info:
            client.get_sandbox("sbx-x")
        err = exc_info.value
        assert err.status_code == 404
        assert err.code == "NOT_FOUND"
        assert "not found" in err.message
    finally:
        httpd.shutdown()


def test_stream_exec_yields_sse_events():
    class SseHandler(BaseHTTPRequestHandler):
        def do_POST(self):
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.end_headers()
            self.wfile.write(b"event: stdout\ndata: hello\n\n")
            self.wfile.write(b"event: stderr\ndata: warn\n\n")
            self.wfile.write(b"event: exit\ndata: 0\n\n")

        def log_message(self, *args):
            pass

    httpd = ThreadingHTTPServer(("127.0.0.1", 0), SseHandler)
    threading.Thread(target=httpd.serve_forever, daemon=True).start()
    try:
        client = Client(f"http://127.0.0.1:{httpd.server_address[1]}", "k")
        events = list(
            client.stream_exec("sbx-1", ExecRequest(argv=["echo", "hello"], timeout_ms=5000))
        )
        assert [(e.event, e.data) for e in events] == [
            ("stdout", "hello"),
            ("stderr", "warn"),
            ("exit", "0"),
        ]
    finally:
        httpd.shutdown()


def test_upload_file_sends_raw_body_with_path(server):
    client = Client(server, "k")
    client.upload_file("sbx-1", "/work/a.txt", b"payload")
    req = Handler.requests[0]
    assert req["url"] == "/api/v1/sandboxes/sbx-1/files/upload?path=%2Fwork%2Fa.txt"
    assert req["content_type"] == "application/json"
