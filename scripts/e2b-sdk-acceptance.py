#!/usr/bin/env python3
"""Official E2B Python SDK acceptance against the local Clouisle API.

Requires a running local API (all-in-one with KVM) with a valid `e2b_`-prefixed
API key, and an already-warmed image (the official SDK only parses the 201
synchronous create response; cold images return 202 and are not supported by
the SDK).

Usage:
    E2B_API_URL=http://127.0.0.1:18080 \
    E2B_SANDBOX_URL=http://127.0.0.1:18080 \
    E2B_API_KEY=e2b_0000000000000000000000000000000000000000 \
    python scripts/e2b-sdk-acceptance.py

Covered (platform API): create, is_running, get_info, kill, pause, resume,
connect. The SDK's commands/files/pty calls target the sandbox-internal envd
service (sandbox URL) and are intentionally out of scope for the local
control-plane-hosted envd endpoints.
"""

import os
import sys


def main() -> int:
    from e2b import Sandbox

    template = os.environ.get("E2B_SDK_TEMPLATE", "docker.io/library/alpine:latest")

    sb = Sandbox.create(template=template, timeout=300)
    print(f"created: {sb.sandbox_id}")

    assert sb.is_running(), "sandbox must be running"
    print("is_running: True")

    info = Sandbox.get_info(sb.sandbox_id)
    assert info.sandbox_id == sb.sandbox_id
    assert info.state == "running"
    assert info.end_at is not None, "endAt must be present"
    print(f"get_info: {info.sandbox_id} state={info.state} endAt={info.end_at}")

    sb.pause()
    print("paused")
    resumed = Sandbox.connect(sb.sandbox_id, timeout=60)
    assert resumed.is_running()
    print("connect/resume: True")

    Sandbox.kill(sb.sandbox_id)
    print("killed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
