"""
Clouisle Sandbox Python SDK — API client.

Fully typed with Python type hints.
"""

from __future__ import annotations

from typing import Any, Optional

import httpx

from .types import (
    ExecRequest,
    ExecResult,
    HealthResponse,
    ImageRef,
    NetworkConfig,
    Resources,
    Sandbox,
    SandboxListResponse,
    SandboxSpec,
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

    # ──────────────────────────────────────────
    #  File Transfer
    # ──────────────────────────────────────────

    def upload_file(self, sandbox_id: str, path: str, data: bytes) -> dict[str, Any]:
        """Upload a file to a sandbox."""
        return self._post_raw(f"/api/v1/sandboxes/{sandbox_id}/files/upload", path, data)

    def download_file(self, sandbox_id: str, path: str) -> bytes:
        """Download a file from a sandbox."""
        url = f"{self._base_url}/api/v1/sandboxes/{sandbox_id}/files/download"
        resp = self._request("GET", url, params={"path": path})
        return resp.content

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

    # ──────────────────────────────────────────
    #  Internal
    # ──────────────────────────────────────────

    @staticmethod
    def _spec_to_dict(spec: SandboxSpec) -> dict[str, Any]:
        """Serialize a SandboxSpec to a dict."""
        return {
            "image": {"reference": spec.image.reference, "digest": spec.image.digest},
            "resources": {
                "vcpu": spec.resources.vcpu,
                "memory_mb": spec.resources.memory_mb,
                "disk_mb": spec.resources.disk_mb,
                "bandwidth_mbps": spec.resources.bandwidth_mbps,
                "iops": spec.resources.iops,
            },
            "network": {
                "enabled": spec.network.enabled,
                "allow_egress": spec.network.allow_egress,
            },
            "env": spec.env,
            "ttl_secs": spec.ttl_secs,
            "start_timeout_secs": spec.start_timeout_secs,
            "restart_policy": spec.restart_policy,
        }

    def _sandbox_from_dict(self, data: dict[str, Any]) -> Sandbox:
        spec = data.get("spec", {})
        resources_raw = spec.get("resources", {})
        network_raw = spec.get("network", {})
        vmm_raw = data.get("vmm_meta", {})

        spec_obj = SandboxSpec(
            image=ImageRef(
                reference=spec.get("image", {}).get("reference", ""),
                digest=spec.get("image", {}).get("digest"),
            ),
            resources=Resources(
                vcpu=int(resources_raw.get("vcpu", 1)),
                memory_mb=int(resources_raw.get("memory_mb", 256)),
                disk_mb=int(resources_raw.get("disk_mb", 512)),
                bandwidth_mbps=resources_raw.get("bandwidth_mbps"),
                iops=resources_raw.get("iops"),
            ),
            network=NetworkConfig(
                enabled=bool(network_raw.get("enabled", True)),
                allow_egress=list(network_raw.get("allow_egress", [])),
            ),
            env=dict(spec.get("env", {})),
            ttl_secs=spec.get("ttl_secs"),
            start_timeout_secs=int(spec.get("start_timeout_secs", 10)),
            restart_policy=spec.get("restart_policy", "never"),
        )

        vmm_obj = None
        if vmm_raw:
            vmm_obj = VmmMeta(
                backend=vmm_raw.get("backend", ""),
                pid=vmm_raw.get("pid"),
                api_socket=vmm_raw.get("api_socket"),
                vsock_socket=vmm_raw.get("vsock_socket"),
                vmm_id=vmm_raw.get("vmm_id"),
                extra=dict(vmm_raw.get("extra", {})),
            )

        return Sandbox(
            id=data.get("id", ""),
            spec=spec_obj,
            status=data.get("status", ""),
            created_at=data.get("created_at", ""),
            updated_at=data.get("updated_at", ""),
            ready_at=data.get("ready_at"),
            vmm_meta=vmm_obj,
            node_id=data.get("node_id"),
        )

    def _headers(self) -> dict[str, str]:
        headers = {"Content-Type": "application/json"}
        if self._api_key:
            headers["Authorization"] = f"Bearer {self._api_key}"
        return headers

    def _url(self, path: str) -> str:
        return f"{self._base_url}{path}"

    def _request(self, method: str, url: str, *, params: Optional[dict[str, str]] = None, json: Optional[dict[str, Any]] = None) -> httpx.Response:
        resp = self._http.request(method, url, params=params, json=json, headers=self._headers())
        if resp.status_code >= 400:
            self._raise_error(resp)
        return resp

    def _get(self, path: str, params: Optional[dict[str, str]] = None) -> dict[str, Any]:
        resp = self._request("GET", self._url(path), params=params)
        return resp.json()

    def _post(self, path: str, body: dict[str, Any]) -> dict[str, Any]:
        resp = self._request("POST", self._url(path), json=body)
        return resp.json()

    def _post_raw(self, path: str, file_path: str, data: bytes) -> dict[str, Any]:
        url = f"{self._url(path)}?path={file_path}"
        resp = self._http.post(url, content=data, headers=self._headers())
        if resp.status_code >= 400:
            self._raise_error(resp)
        return resp.json()

    def _delete(self, path: str) -> None:
        self._request("DELETE", self._url(path))

    def _raise_error(self, resp: httpx.Response) -> None:
        try:
            body = resp.json()
            error = body.get("error", {})
            code = error.get("code", "UNKNOWN")
            message = error.get("message", resp.text)
            details = error.get("details")
        except Exception:
            code = "HTTP"
            message = resp.text
            details = None
        raise SandboxError(resp.status_code, code, message, details)