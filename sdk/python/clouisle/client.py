"""
Clouisle Sandbox Python SDK — API client.

Fully typed with Python type hints.
"""

from typing import Iterator, Literal, Optional, cast
from urllib.parse import quote

import httpx

from .types import (
    DirEntry,
    ExecRequest,
    ExecResult,
    ExecStreamEvent,
    ExecutionRecord,
    HealthResponse,
    ImageRef,
    JsonObject,
    JsonValue,
    ListFilesResponse,
    MountSpec,
    NetworkConfig,
    Resources,
    Sandbox,
    SandboxListResponse,
    SandboxSpec,
    SecretSpec,
    StatusResponse,
    VmmMeta,
)


class SandboxError(Exception):
    """Raised when the API returns an error."""

    def __init__(self, status_code: int, code: str, message: str, details: Optional[object] = None) -> None:
        self.status_code = status_code
        self.code = code
        self.message = message
        self.details = details
        super().__init__(f"[{status_code}] {code}: {message}")


class Client:
    """Clouisle Sandbox API client."""

    def __init__(self, base_url: str, api_key: str = "") -> None:
        self._base_url = base_url.rstrip("/")
        self._api_key = api_key
        self._http = httpx.Client(timeout=30.0)

    # ──────────────────────────────────────────
    #  Sandbox Lifecycle
    # ──────────────────────────────────────────

    def create_sandbox(self, spec: SandboxSpec) -> Sandbox:
        """Create a sandbox."""
        data = self._post("/api/v1/sandboxes", self._spec_to_dict(spec))
        return self._sandbox_from_dict(data)

    def get_sandbox(self, sandbox_id: str) -> Sandbox:
        """Get a sandbox by ID."""
        data = self._get(f"/api/v1/sandboxes/{sandbox_id}")
        return self._sandbox_from_dict(data)

    def list_sandboxes(
        self,
        status: Optional[str] = None,
        limit: Optional[int] = None,
        offset: Optional[int] = None,
    ) -> SandboxListResponse:
        """List sandboxes with optional filters."""
        params: dict[str, str] = {}
        if status is not None:
            params["status"] = status
        if limit is not None:
            params["limit"] = str(limit)
        if offset is not None:
            params["offset"] = str(offset)
        data = self._get("/api/v1/sandboxes", params=params)
        items = [self._sandbox_from_dict(i) for i in data.get("items", [])]
        return SandboxListResponse(items=items, total=int(data.get("total", 0)))

    def delete_sandbox(self, sandbox_id: str) -> None:
        """Delete a sandbox."""
        self._delete(f"/api/v1/sandboxes/{sandbox_id}")

    # ──────────────────────────────────────────
    #  Command Execution
    # ──────────────────────────────────────────

    def exec(self, sandbox_id: str, req: ExecRequest) -> ExecResult:
        """Execute a command synchronously."""
        body = {
            "argv": req.argv,
            "env": req.env,
            "cwd": req.cwd,
            "timeout_ms": req.timeout_ms,
            "stream": req.stream,
        }
        data = self._post(f"/api/v1/sandboxes/{sandbox_id}/exec", body)
        return ExecResult(
            exec_id=data.get("exec_id", ""),
            exit_code=int(data.get("exit_code", -1)),
            stdout=data.get("stdout", ""),
            stderr=data.get("stderr", ""),
            duration_ms=int(data.get("duration_ms", 0)),
            timed_out=bool(data.get("timed_out", False)),
            stdout_truncated=bool(data.get("stdout_truncated", False)),
            stderr_truncated=bool(data.get("stderr_truncated", False)),
        )

    def exec_cmd(self, sandbox_id: str, argv: list[str], timeout_ms: int = 30000) -> ExecResult:
        """Convenience: exec with just argv."""
        return self.exec(sandbox_id, ExecRequest(argv=argv, timeout_ms=timeout_ms))

    def stream_exec(self, sandbox_id: str, req: ExecRequest) -> Iterator[ExecStreamEvent]:
        """Execute a command and yield ordered SSE output events."""
        url = self._url(f"/api/v1/sandboxes/{sandbox_id}/exec/stream")
        body: JsonObject = {
            "argv": req.argv,
            "env": req.env,
            "cwd": req.cwd,
            "timeout_ms": req.timeout_ms,
            "stream": True,
        }
        with self._http.stream("POST", url, json=body, headers=self._headers()) as response:
            if response.status_code >= 400:
                self._raise_error(response)
            event: Optional[Literal["stdout", "stderr", "exit", "error"]] = None
            for line in response.iter_lines():
                if line.startswith("event: "):
                    candidate = line[7:]
                    if candidate in {"stdout", "stderr", "exit", "error"}:
                        event = cast(Literal["stdout", "stderr", "exit", "error"], candidate)
                elif line.startswith("data: ") and event is not None:
                    yield ExecStreamEvent(event=event, data=line[6:])
                    event = None

    def get_execution(self, sandbox_id: str, execution_id: str) -> ExecutionRecord:
        """Get one persisted execution record."""
        return self._execution_from_dict(
            self._get(f"/api/v1/sandboxes/{sandbox_id}/exec/{execution_id}")
        )

    def list_executions(self, sandbox_id: str, limit: Optional[int] = None) -> list[ExecutionRecord]:
        """List persisted execution records, optionally limited."""
        params = {} if limit is None else {"limit": str(limit)}
        response = self._request(
            "GET", self._url(f"/api/v1/sandboxes/{sandbox_id}/exec"), params=params
        )
        data = cast(JsonValue, response.json())
        return [self._execution_from_dict(item) for item in _as_json_list(data)]

    # ──────────────────────────────────────────
    #  File Transfer
    # ──────────────────────────────────────────

    def upload_file(self, sandbox_id: str, path: str, data: bytes) -> JsonObject:
        """Upload a file to a sandbox."""
        return self._post_raw(f"/api/v1/sandboxes/{sandbox_id}/files/upload", path, data)

    def download_file(self, sandbox_id: str, path: str) -> bytes:
        """Download a file from a sandbox."""
        url = f"{self._base_url}/api/v1/sandboxes/{sandbox_id}/files/download"
        resp = self._request("GET", url, params={"path": path})
        return resp.content

    def list_files(self, sandbox_id: str, path: str) -> ListFilesResponse:
        """List a directory inside a sandbox."""
        data = self._get(f"/api/v1/sandboxes/{sandbox_id}/files/ls", params={"path": path})
        return ListFilesResponse(items=[
            DirEntry(
                name=_as_str(_as_object(item).get("name", "")),
                size=_as_int(_as_object(item).get("size", 0)),
                mode=_as_int(_as_object(item).get("mode", 0)),
                mtime=_as_int(_as_object(item).get("mtime", 0)),
                is_dir=_as_bool(_as_object(item).get("is_dir", False)),
            )
            for item in _as_json_list(data.get("items", []))
        ])

    # ──────────────────────────────────────────
    #  Observability
    # ──────────────────────────────────────────

    def health(self) -> HealthResponse:
        """Health check."""
        data = self._get("/health")
        return HealthResponse(
            status=data.get("status", ""),
            store=data.get("store", ""),
            version=data.get("version", ""),
        )

    def liveness(self) -> StatusResponse:
        """Liveness check."""
        data = self._get("/health/live")
        return StatusResponse(status=_as_str(data.get("status", "")))

    def readiness(self) -> StatusResponse:
        """Readiness check."""
        data = self._get("/health/ready")
        return StatusResponse(status=_as_str(data.get("status", "")))

    def metrics(self) -> str:
        """Return Prometheus metrics text."""
        return self._request("GET", self._url("/metrics")).text

    # ──────────────────────────────────────────
    #  Internal
    # ──────────────────────────────────────────

    @staticmethod
    def _spec_to_dict(spec: SandboxSpec) -> JsonObject:
        """Serialize a SandboxSpec to a JSON object."""
        return {
            "image": {"reference": spec.image.reference, "digest": spec.image.digest},
            "resources": {
                "vcpu": spec.resources.vcpu,
                "memory_mb": spec.resources.memory_mb,
                "disk_mb": spec.resources.disk_mb,
                "bandwidth_mbps": spec.resources.bandwidth_mbps,
                "iops": spec.resources.iops,
                "pids_max": spec.resources.pids_max,
            },
            "network": {
                "enabled": spec.network.enabled,
                "allow_egress": spec.network.allow_egress,
                "deny_egress": spec.network.deny_egress,
            },
            "mounts": [{"source": mount.source, "target": mount.target, "readonly": mount.readonly} for mount in spec.mounts],
            "secrets": [{"name": secret.name, "value": secret.value} for secret in spec.secrets],
            "ttl_secs": spec.ttl_secs,
            "start_timeout_secs": spec.start_timeout_secs,
            "env": spec.env,
            "node_selector": spec.node_selector,
            "restart_policy": spec.restart_policy,
            "tenant_id": spec.tenant_id,
        }

    def _sandbox_from_dict(self, data: JsonObject) -> Sandbox:
        spec = _as_object(data.get("spec", {}))
        resources_raw = _as_object(spec.get("resources", {}))
        network_raw = _as_object(spec.get("network", {}))
        vmm_raw = _as_object(data.get("vmm_meta", {}))

        spec_obj = SandboxSpec(
            image=ImageRef(
                reference=_as_str(_as_object(spec.get("image", {})).get("reference", "")),
                digest=_as_optional_str(_as_object(spec.get("image", {})).get("digest")),
            ),
            resources=Resources(
                vcpu=_as_int(resources_raw.get("vcpu", 1)),
                memory_mb=_as_int(resources_raw.get("memory_mb", 256)),
                disk_mb=_as_int(resources_raw.get("disk_mb", 512)),
                bandwidth_mbps=_as_optional_int(resources_raw.get("bandwidth_mbps")),
                iops=_as_optional_int(resources_raw.get("iops")),
                pids_max=_as_optional_int(resources_raw.get("pids_max")),
            ),
            network=NetworkConfig(
                enabled=_as_bool(network_raw.get("enabled", True)),
                allow_egress=_as_str_list(network_raw.get("allow_egress", [])),
                deny_egress=_as_str_list(network_raw.get("deny_egress", [])),
            ),
            mounts=[
                MountSpec(
                    source=_as_str(_as_object(item).get("source", "")),
                    target=_as_str(_as_object(item).get("target", "")),
                    readonly=_as_bool(_as_object(item).get("readonly", False)),
                )
                for item in _as_json_list(spec.get("mounts", []))
            ],
            secrets=[
                SecretSpec(
                    name=_as_str(_as_object(item).get("name", "")),
                    value=_as_str(_as_object(item).get("value", "")),
                )
                for item in _as_json_list(spec.get("secrets", []))
            ],
            ttl_secs=_as_optional_int(spec.get("ttl_secs")),
            start_timeout_secs=_as_int(spec.get("start_timeout_secs", 10)),
            env=_as_str_dict(spec.get("env", {})),
            node_selector=_as_str_dict(spec.get("node_selector", {})),
            restart_policy=_as_restart_policy(spec.get("restart_policy", "never")),
            tenant_id=_as_optional_str(spec.get("tenant_id")),
        )

        vmm_obj = None
        if vmm_raw:
            vmm_obj = VmmMeta(
                backend=_as_str(vmm_raw.get("backend", "")),
                pid=_as_optional_int(vmm_raw.get("pid")),
                api_socket=_as_optional_str(vmm_raw.get("api_socket")),
                vsock_socket=_as_optional_str(vmm_raw.get("vsock_socket")),
                vmm_id=_as_optional_str(vmm_raw.get("vmm_id")),
                extra=_as_str_dict(vmm_raw.get("extra", {})),
            )

        return Sandbox(
            id=_as_str(data.get("id", "")),
            spec=spec_obj,
            status=_as_str(data.get("status", "")),
            created_at=_as_str(data.get("created_at", "")),
            updated_at=_as_str(data.get("updated_at", "")),
            ready_at=_as_optional_str(data.get("ready_at")),
            vmm_meta=vmm_obj,
            node_id=_as_optional_str(data.get("node_id")),
        )

    @staticmethod
    def _execution_from_dict(data: JsonObject) -> ExecutionRecord:
        return ExecutionRecord(
            id=_as_str(data.get("id", "")),
            sandbox_id=_as_str(data.get("sandbox_id", "")),
            exit_code=_as_int(data.get("exit_code", -1)),
            stdout=_as_bytes(data.get("stdout", [])),
            stderr=_as_bytes(data.get("stderr", [])),
            started_at=_as_str(data.get("started_at", "")),
            finished_at=_as_str(data.get("finished_at", "")),
            timed_out=_as_bool(data.get("timed_out", False)),
            stdout_truncated=_as_bool(data.get("stdout_truncated", False)),
            stderr_truncated=_as_bool(data.get("stderr_truncated", False)),
            node_id=_as_optional_str(data.get("node_id")),
        )

    def _headers(self) -> dict[str, str]:
        headers = {"Content-Type": "application/json"}
        if self._api_key:
            headers["Authorization"] = f"Bearer {self._api_key}"
        return headers

    def _url(self, path: str) -> str:
        return f"{self._base_url}{path}"

    def _request(
        self,
        method: str,
        url: str,
        *,
        params: Optional[dict[str, str]] = None,
        json: Optional[JsonObject] = None,
    ) -> httpx.Response:
        resp = self._http.request(method, url, params=params, json=json, headers=self._headers())
        if resp.status_code >= 400:
            self._raise_error(resp)
        return resp

    def _get(self, path: str, params: Optional[dict[str, str]] = None) -> JsonObject:
        resp = self._request("GET", self._url(path), params=params)
        return cast(JsonObject, resp.json())

    def _post(self, path: str, body: JsonObject) -> JsonObject:
        resp = self._request("POST", self._url(path), json=body)
        return cast(JsonObject, resp.json())

    def _post_raw(self, path: str, file_path: str, data: bytes) -> JsonObject:
        url = f"{self._url(path)}?path={quote(file_path, safe='')}"
        resp = self._http.post(url, content=data, headers=self._headers())
        if resp.status_code >= 400:
            self._raise_error(resp)
        return cast(JsonObject, resp.json())

    def _delete(self, path: str) -> None:
        self._request("DELETE", self._url(path))

    def _raise_error(self, resp: httpx.Response) -> None:
        try:
            body = cast(JsonObject, resp.json())
            error = _as_object(body.get("error", {}))
            code = _as_str(error.get("code", "UNKNOWN"))
            message = _as_str(error.get("message", resp.text))
            details = error.get("details")
        except (TypeError, ValueError):
            code = "HTTP"
            message = resp.text
            details = None
        raise SandboxError(resp.status_code, code, message, details)


def _as_object(value: JsonValue) -> JsonObject:
    return value if isinstance(value, dict) else {}


def _as_str(value: JsonValue) -> str:
    return value if isinstance(value, str) else str(value)


def _as_optional_str(value: JsonValue) -> Optional[str]:
    return value if isinstance(value, str) else None


def _as_int(value: JsonValue) -> int:
    return int(value) if isinstance(value, (int, float, str)) else 0


def _as_optional_int(value: JsonValue) -> Optional[int]:
    return _as_int(value) if value is not None else None


def _as_bool(value: JsonValue) -> bool:
    return value if isinstance(value, bool) else bool(value)


def _as_str_list(value: JsonValue) -> list[str]:
    return [item for item in value if isinstance(item, str)] if isinstance(value, list) else []


def _as_str_dict(value: JsonValue) -> dict[str, str]:
    return {key: item for key, item in value.items() if isinstance(item, str)} if isinstance(value, dict) else {}


def _as_bytes(value: JsonValue) -> bytes:
    if isinstance(value, list) and all(isinstance(item, int) and 0 <= item <= 255 for item in value):
        return bytes(cast(list[int], value))
    if isinstance(value, str):
        return value.encode()
    return b""


def _as_json_list(value: JsonValue) -> list[JsonObject]:
    return [item for item in value if isinstance(item, dict)] if isinstance(value, list) else []


def _as_restart_policy(value: JsonValue) -> Literal["never", "on_failure", "always"]:
    return value if value in ("never", "on_failure", "always") else "never"