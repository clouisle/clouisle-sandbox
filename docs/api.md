# API 参考

完整 OpenAPI 3.0 规范见 [spec/openapi.json](../spec/openapi.json)（95 operations，由 `scripts/gen-openapi.py` 从路由表生成）。本文为核心端点指南。

认证：除 `/health*`、`/metrics` 外全部端点要求 `Authorization: Bearer <key>`。

## 核心沙盒 API（v1）

| 方法 | 路径 | 说明 |
|---|---|---|
| POST | `/api/v1/sandboxes` | 创建沙盒（201 同步 / 202 异步） |
| GET | `/api/v1/sandboxes` | 列表（`?status=&limit=&offset=`，响应 `{items,total}`） |
| GET | `/api/v1/sandboxes/{id}` | 详情（含 spec/status/vmm_meta） |
| POST | `/api/v1/sandboxes/{id}/recover` | 重跑 provision（幂等恢复） |
| DELETE | `/api/v1/sandboxes/{id}` | 删除（204；二次 404） |

### 创建请求（`SandboxSpec` 扁平展开）
```json
{
  "image": {"reference": "docker.io/library/alpine:latest", "digest": null},
  "resources": {"vcpu": 1, "memory_mb": 256, "disk_mb": 512,
                "bandwidth_mbps": null, "iops": null, "pids_max": 512},
  "network": {"enabled": true, "allow_egress": [], "deny_egress": []},
  "mounts": [], "secrets": [], "env": {},
  "ttl_secs": null, "start_timeout_secs": 10, "sync": true,
  "init_command": [], "restart_policy": "never", "tenant_id": null,
  "node_selector": {}, "metadata": {}, "volume_mounts": []
}
```

## 执行

| 方法 | 路径 | 说明 |
|---|---|---|
| POST | `/api/v1/sandboxes/{id}/exec` | 同步执行（返回 exit_code/stdout/stderr/截断标志） |
| POST | `/api/v1/sandboxes/{id}/exec/stream` | SSE 流式输出 |
| GET | `/api/v1/sandboxes/{id}/exec` | 执行历史（`?limit=`） |
| GET | `/api/v1/sandboxes/{id}/exec/{exec_id}` | 单条记录 |

请求：`{"argv":["sh","-c","..."], "env":{}, "cwd":"/tmp", "timeout_ms":30000, "stream":false}`

## 文件

| 方法 | 路径 | 说明 |
|---|---|---|
| POST | `/api/v1/sandboxes/{id}/files/upload?path=/work/a.txt` | 上传（raw body） |
| GET | `/api/v1/sandboxes/{id}/files/download?path=...` | 下载 |
| GET | `/api/v1/sandboxes/{id}/files/ls?path=/work` | 列目录 |

## 镜像

| 方法 | 路径 | 说明 |
|---|---|---|
| POST | `/api/v1/images/prefetch` | 异步预拉取（202 + job_id） |
| GET | `/api/v1/images/prefetch/{job_id}` | 拉取状态 |

## E2B 兼容端点

| 方法 | 路径 | 说明 |
|---|---|---|
| POST | `/sandboxes` | E2B 创建（`templateID` / `envVars` / `timeout`） |
| GET | `/v2/sandboxes` | 分页列表（`X-Total-Running`） |
| GET | `/sandboxes/{id}` | 详情 |
| POST | `/sandboxes/{id}/connect` | envd 连接 |
| POST | `/sandboxes/{id}/pause` `/resume` `/timeout` `/refresh` | 生命周期 |
| PUT | `/sandboxes/{id}/network` | 网络更新 |
| POST | `/files` | 文件上传 |
| POST | `/process.Process/Start` `/List` `/Connect` `/Update` `/SendInput` `/StreamInput` `/SendSignal` `/CloseStdin` | 交互式进程 |
| POST | `/filesystem.Filesystem/{ListDir,Stat,MakeDir,Move,Remove,WatchDir,...}` | 文件系统 RPC |
| POST | `/sandboxes/{id}/snapshots` · `GET /snapshots` · `POST /sandboxes/{id}/fork` | 快照 / Fork |
| POST | `/init` `/envs` `/files/compose` | envd REST |

## 云控制面

| 方法 | 路径 | 说明 |
|---|---|---|
| GET/POST | `/teams` | 团队 |
| GET/POST | `/api-keys` · `PATCH/DELETE /api-keys/{id}` | API key（单向散列） |
| POST/DELETE | `/access-tokens` | 访问令牌 |
| GET/POST | `/volumes` · `/volumecontent/*` | 卷与内容 |
| GET/POST | `/v3/templates` `/v2/templates` · builds/tags/aliases | 模板 |
| GET | `/nodes` | 节点 |
| POST | `/admin/teams/{id}/sandboxes/kill` 等 | 管理端点 |

## 健康与指标

| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/health` | `{"status":"ok","store":"ok","version":...}` |
| GET | `/health/live` · `/health/ready` | 存活/就绪（断库时 ready=503 not_ready） |
| GET | `/metrics` | Prometheus 文本 |
| GET | `/healthz` | 204 |

## 错误格式

统一错误响应（双格式兼容官方 SDK）：
```json
{
  "error": {"code": "NOT_FOUND", "message": "sandbox ... not found", "details": null},
  "code": "NOT_FOUND", "message": "sandbox ... not found", "details": null
}
```
常见 code：`VALIDATION`(400)、`UNAUTHENTICATED`(401)、`FORBIDDEN`(403)、`NOT_FOUND`(404)、`INVALID_STATE`(409)、`RESOURCE_EXHAUSTED`(507)、`TIMEOUT`(504)、`VMM`/`IO`/`NETWORK`(5xx)。
