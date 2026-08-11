#!/usr/bin/env python3
"""从 router.rs 提取路由表并生成 OpenAPI 3.0 规范（spec/openapi.json）。

生成的是结构性规范：paths/methods/参数/响应骨架，用于契约审计与 SDK 生成。
运行：python3 scripts/gen-openapi.py
"""
import json
import re
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
ROUTER = ROOT / "crates" / "clouisle-api" / "src" / "router.rs"
OUT = ROOT / "spec" / "openapi.json"

def extract_routes():
    src = ROUTER.read_text()
    # .route(\n "path",\n method(handler),\n )  或单行
    pattern = re.compile(r'\.route\(\s*"([^"]+)"\s*,\s*(\w+)\(([^)]*?)\)', re.S)
    routes = []
    seen = set()
    for path, method, handler in pattern.findall(src):
        if path in seen:
            continue
        seen.add(path)
        routes.append((path, method.upper(), handler.strip()))
    return routes

def param_schema(name: str):
    return {
        "name": name,
        "in": "path",
        "required": True,
        "schema": {"type": "string"},
    }

def build_paths(routes):
    paths = {}
    for path, method, handler in routes:
        op = {
            "operationId": handler.rsplit("::", 1)[-1],
            "summary": handler,
            "responses": {
                "200": {"description": "OK"},
                "400": {"description": "Bad request"},
                "401": {"description": "Unauthenticated"},
                "403": {"description": "Forbidden"},
                "404": {"description": "Not found"},
                "500": {"description": "Internal error"},
            },
        }
        path_params = [p.strip("{}") for p in re.findall(r"\{([^}]+)\}", path)]
        if path_params:
            op["parameters"] = [param_schema(p) for p in path_params]
        openapi_path = re.sub(r"\{([^}]+)\}", r"{\1}", path)
        paths.setdefault(openapi_path, {})[method.lower()] = op
    return paths

def main():
    routes = extract_routes()
    spec = {
        "openapi": "3.0.3",
        "info": {
            "title": "Clouisle Sandbox API",
            "version": "0.1.0",
            "description": (
                "Clouisle Sandbox control plane: sandbox lifecycle, exec, files, "
                "E2B-compatible endpoints, cloud control plane, health/metrics. "
                "Generated from router.rs; structural contract for audit/SDK."
            ),
        },
        "servers": [{"url": "http://localhost:8080"}],
        "security": [{"bearerAuth": []}],
        "components": {
            "securitySchemes": {
                "bearerAuth": {"type": "http", "scheme": "bearer"},
            }
        },
        "paths": build_paths(routes),
    }
    OUT.parent.mkdir(parents=True, exist_ok=True)
    OUT.write_text(json.dumps(spec, indent=2, ensure_ascii=False))
    print(f"wrote {OUT} with {len(routes)} operations")

if __name__ == "__main__":
    main()
