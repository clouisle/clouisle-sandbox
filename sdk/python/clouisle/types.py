"""
Clouisle Sandbox Python SDK — domain types.

All types are fully annotated with Python type hints.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Optional


@dataclass
class ImageRef:
    """Image reference for a sandbox."""
    reference: str
    digest: Optional[str] = None


@dataclass
class Resources:
    """Resources allocated to a sandbox."""
    vcpu: int = 1
    memory_mb: int = 256
    disk_mb: int = 512
    bandwidth_mbps: Optional[int] = None
    iops: Optional[int] = None


@dataclass
class NetworkConfig:
    """Network configuration."""
    enabled: bool = True
    allow_egress: list[str] = field(default_factory=list)


@dataclass
class SandboxSpec:
    """Spec for creating a sandbox."""
    image: ImageRef
    resources: Resources = field(default_factory=Resources)
    network: NetworkConfig = field(default_factory=NetworkConfig)
    env: dict[str, str] = field(default_factory=dict)
    ttl_secs: Optional[int] = None
    start_timeout_secs: int = 10
    restart_policy: str = "never"


@dataclass
class VmmMeta:
    """VMM runtime metadata."""
    backend: str
    pid: Optional[int] = None
    api_socket: Optional[str] = None
    vsock_socket: Optional[str] = None
    vmm_id: Optional[str] = None
    extra: dict[str, str] = field(default_factory=dict)


@dataclass
class Sandbox:
    """A sandbox instance."""
    id: str
    spec: SandboxSpec
    status: str
    created_at: str
    updated_at: str
    ready_at: Optional[str] = None
    vmm_meta: Optional[VmmMeta] = None
    node_id: Optional[str] = None


@dataclass
class ExecRequest:
    """Command execution request."""
    argv: list[str]
    env: dict[str, str] = field(default_factory=dict)
    cwd: Optional[str] = None
    timeout_ms: int = 30000
    stream: bool = False


@dataclass
class ExecResult:
    """Result of a command execution."""
    exec_id: str
    exit_code: int
    stdout: str
    stderr: str
    duration_ms: int
    timed_out: bool = False
    stdout_truncated: bool = False
    stderr_truncated: bool = False


@dataclass
class ExecutionRecord:
    """Persisted execution record."""
    id: str
    sandbox_id: str
    exit_code: int
    stdout: str
    stderr: str
    started_at: str
    finished_at: str
    timed_out: bool = False
    stdout_truncated: bool = False
    stderr_truncated: bool = False
    node_id: Optional[str] = None


@dataclass
class DirEntry:
    """Directory entry."""
    name: str
    size: int
    mode: int
    mtime: int
    is_dir: bool


@dataclass
class ListFilesResponse:
    """Directory listing response."""
    items: list[DirEntry]


@dataclass
class SandboxListResponse:
    """Sandbox list response."""
    items: list[Sandbox]
    total: int


@dataclass
class HealthResponse:
    """Health check response."""
    status: str
    store: str
    version: str


@dataclass
class ApiError:
    """API error payload."""
    code: str
    message: str
    details: Optional[object] = None