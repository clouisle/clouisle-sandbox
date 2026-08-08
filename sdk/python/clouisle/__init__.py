"""
Clouisle Sandbox Python SDK.

Usage:
    from clouisle import Client, SandboxSpec, ImageRef, ExecRequest

    client = Client("http://localhost:8080", "my-api-key")
    spec = SandboxSpec(image=ImageRef(reference="alpine:latest"))
    sandbox = client.create_sandbox(spec)
    result = client.exec_cmd(sandbox.id, ["echo", "hello"])
"""

from .client import Client, SandboxError
from .types import (
    ApiError,
    DirEntry,
    ExecRequest,
    ExecResult,
    ExecutionRecord,
    HealthResponse,
    ImageRef,
    ListFilesResponse,
    NetworkConfig,
    Resources,
    Sandbox,
    SandboxListResponse,
    SandboxSpec,
    VmmMeta,
)

__all__ = [
    "Client",
    "SandboxError",
    "ApiError",
    "DirEntry",
    "ExecRequest",
    "ExecResult",
    "ExecutionRecord",
    "HealthResponse",
    "ImageRef",
    "ListFilesResponse",
    "NetworkConfig",
    "Resources",
    "Sandbox",
    "SandboxListResponse",
    "SandboxSpec",
    "VmmMeta",
]

__version__ = "0.1.0"