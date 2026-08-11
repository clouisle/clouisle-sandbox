# Clouisle Sandbox

每个沙盒都是一台**独立微虚拟机**（Firecracker + KVM，自有内核与 rootfs）。宿主经帧协议与 guest agent 通信，提供命令执行、文件传输、网络隔离、快照/Fork、多节点调度与 E2B 兼容的 SDK。

## 快速开始（5 分钟）

### Linux + KVM（生产语义）

```bash
docker compose up -d
curl localhost:8080/health          # {"status":"ok","store":"ok",...}

export KEY="<docker-compose.yml 中的 CLOUISLE_API_KEYS>"

# 创建微VM 沙盒
curl -X POST localhost:8080/api/v1/sandboxes \
  -H "Authorization: Bearer $KEY" -H 'Content-Type: application/json' \
  -d '{"image":{"reference":"docker.io/library/alpine:latest"}}'
# → 201 {"id":"019f...","status":"running",...}

# 执行命令
curl -X POST localhost:8080/api/v1/sandboxes/<id>/exec \
  -H "Authorization: Bearer $KEY" -H 'Content-Type: application/json' \
  -d '{"argv":["echo","hello from microVM"],"timeout_ms":10000}'

# 删除
curl -X DELETE localhost:8080/api/v1/sandboxes/<id> -H "Authorization: Bearer $KEY"
```

### macOS / Windows（Docker Desktop）

```bash
docker compose -f docker-compose.dev.yml up --build
# API: http://localhost:18080  key: e2b_dev_...（dev:full）
```
沙盒以 Docker 容器运行（注入 agent，同一 API 契约）；不承诺生产隔离（无快照/iops/带宽/allowlist）。

### 用 SDK

```python
from e2b import Sandbox
sb = Sandbox(template="docker.io/library/alpine:latest", api_key="$KEY")
print(sb.commands.run("echo hi").stdout)   # hi
sb.kill()
```

## 功能一览

| 功能 | 说明 | 文档 |
|---|---|---|
| 沙盒生命周期 | 创建/列表/详情/删除/恢复/TTL/restart 策略 | [features.md](docs/features.md#1-沙盒生命周期) |
| 命令执行 | 同步/SSE 流式/交互式 PTY/超时/历史 | [features.md](docs/features.md#2-命令执行) |
| 文件传输 | 上传/下载/列目录/路径安全 | [features.md](docs/features.md#3-文件传输) |
| 网络隔离 | 每沙盒 netns + nftables + DNS 代理 + 带宽 | [features.md](docs/features.md#4-网络隔离) |
| 资源与数据面 | CPU/内存/磁盘/pids/带宽/iops/挂载/密钥 | [features.md](docs/features.md#5-资源与数据面) |
| 快照预热 | create 命中快照 **0.2s**（冷创建 ~6s） | [features.md](docs/features.md#6-快照与预热) |
| 快照 / Fork | 公开 API + 子网继承 | [features.md](docs/features.md#6-快照与预热) |
| E2B 兼容 | 官方 SDK / Filesystem / Process / envd | [features.md](docs/features.md#7-e2b-兼容) |
| 多节点 | clouisled gRPC 调度 | [features.md](docs/features.md#8-多节点clouisled-grpc) |
| Docker 开发后端 | macOS/Windows 本地开发 | [features.md](docs/features.md#12-docker-开发后端docker-dev) |

## 架构

- **控制面** `clouisle-apiserver`：REST、认证、资源调度、状态机、预热池、清理
- **执行后端**：`firecracker`（生产）/ `docker-dev`（开发）/ 远程 `clouisled` 节点
- **guest agent**：静态二进制（PID 1），帧协议服务 exec/文件/PTY/secret
- 存储：SQLite / PostgreSQL 自动选择；网络：每沙盒 netns + nftables
- 详见 [architecture.md](docs/architecture.md)

## 文档

| 主题 | 文档 |
|---|---|
| 快速上手 | [docs/quickstart.md](docs/quickstart.md) |
| 系统架构 | [docs/architecture.md](docs/architecture.md) |
| 功能设计（每个功能：设计/配置/验证） | [docs/features.md](docs/features.md) |
| 配置参考（CLI/env/Compose/后端差异） | [docs/configuration.md](docs/configuration.md) |
| API 参考 | [docs/api.md](docs/api.md) · [spec/openapi.json](spec/openapi.json) |
| 部署（生产/开发/多节点/HA） | [docs/deployment.md](docs/deployment.md) |
| 本地开发（构建/测试/结构） | [docs/development.md](docs/development.md) |
| 详细设计记录 | [docs/plan/](docs/plan/)（快照预热、E2B、验收、安全等） |

## 配置速查

```bash
# API key（key:tenant:scope）
export CLOUISLE_API_KEYS="prod-key:team-a:full"

# 存储：SQLite 或 PostgreSQL（自动选择）
clouisle-api --db clouisle.db
clouisle-api --db "postgres://user:pass@host:5432/clouisle"

# 后端选择
clouisle-api --backend firecracker    # 生产（默认，需 KVM）
clouisle-api --backend docker-dev     # 本地开发（Docker）

# 快照预热池
export CLOUISLE_WARM_POOL_MIN_IDLE=2
```
完整配置见 [docs/configuration.md](docs/configuration.md)。

## 验证与质量

```bash
cargo test --workspace                # 206 个单测
cargo test -p clouisle-api --features test-utils --tests   # 79 个 API 用例
cargo clippy --workspace --all-targets                     # 零警告
cd sdk/typescript && npm test         # TS SDK 6 用例
cd sdk/python && .venv/bin/python -m pytest tests -q      # Python SDK 6 用例
```
服务器全量验收（211 用例）与安全报告见 [docs/plan/server-acceptance-report.md](docs/plan/server-acceptance-report.md)、[docs/plan/security-acceptance.md](docs/plan/security-acceptance.md)。

## 已知边界

- 快照预热需兼容内核（实测 `/opt/clouisle/vmlinux-vsock` 4.14.193；自定义 7.0 内核恢复崩溃）
- 沙盒共享 rootfs 已隔离（每沙盒副本）；快照 clone 共享为 FC dev-preview 限制
- iops 生效依赖宿主 cgroup io 控制器暴露设备
- 本地单节点 API 的 read key 可读任意沙盒元数据（多租户需按 team 过滤）

## License

见 [LICENSE](LICENSE)。
