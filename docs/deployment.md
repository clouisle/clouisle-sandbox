# 部署指南

## 1. 生产单节点（Linux + KVM）

### 前置
- Linux x86_64/arm64，`/dev/kvm` 可用
- Firecracker v1.10.1、guest 内核（`/opt/clouisle/vmlinux-vsock`，快照兼容）、rootfs 缓存
- Docker（可选：用 Compose 部署控制面）

### 启动
```bash
docker compose up -d            # API + PostgreSQL + Firecracker
curl localhost:8080/health
```
Compose 使用 host network + `/dev/kvm` + privileged（FC 需要 netns/nft 权限）。

### 环境变量（生产必改）
- `CLOUISLE_API_KEYS`：替换内置开发 key（`key:tenant:full`）
- PostgreSQL 密码：替换 `docker-compose.yml` 默认值

## 2. 开发模式（macOS / Windows Docker Desktop）

```bash
docker compose -f docker-compose.dev.yml up --build
# API: http://localhost:18080，key: e2b_dev_...（dev:full）
```
- 沙盒为 Docker 容器（注入静态 agent），复用同一 API 契约。
- **安全边界**：`/var/run/docker.sock` 仅 dev Compose 挂载（宿主等效权限）；生产 Compose/Kubernetes 不含 socket。
- 能力边界：无快照/iops/带宽/allowlist；`restart_policy` 仅 `never`。

## 3. 多节点（gRPC）

```mermaid
flowchart LR
    API[apiserver\n--cluster-scheduling\nPostgreSQL]
    N1[clouisled node-1\nKVM 主机]
    N2[clouisled node-2\nKVM 主机]
    API -->|gRPC Register/Heartbeat| N1
    API -->|gRPC| N2
    API --> PG[(PostgreSQL 共享状态)]
```

```bash
# 每台节点主机
clouisled --addr 0.0.0.0:9090 --node-id node-1 \
  --kernel /opt/clouisle/vmlinux-vsock --images-dir /opt/clouisle/images \
  --api-socket-dir /run/clouisle/firecracker \
  --control-plane http://api:8080 --control-plane-key <full-key>

# 控制面
clouisle-api --cluster-scheduling \
  --db "postgres://clouisle:pass@pg:5432/clouisle" \
  --kernel /opt/clouisle/vmlinux-vsock --images-dir /opt/clouisle/images
```

Kubernetes：`deploy/multinode/`（API + daemonset 节点 overlay）。

**部署约束**：API 与 clouisled 必须使用**独立 `--api-socket-dir`**（共享目录会互相误判孤儿 runtime）。

## 4. HA（PostgreSQL）

- `--db postgres://...` 自动选 PostgresStore；断库时 health `degraded`/`not_ready`，恢复后**自动重连**。
- 多 API 副本共享同一 PostgreSQL（`docker-compose.yml` 默认含 PG）。

## 5. 存储选择

| 形态 | `--db` | 适用 |
|---|---|---|
| 单机 | `clouisle.db`（SQLite，WAL） | 开发/小型 |
| HA | `postgres://user:pass@host:5432/clouisle` | 生产多副本 |
| 多节点 | PostgreSQL（共享调度状态） | 集群 |

## 6. 验证部署

```bash
curl localhost:8080/health                      # ok
curl localhost:8080/metrics | head              # Prometheus 可解析
# 创建 → running → exec → 删除 全链路
```

## 7. 升级与回滚

- 每个版本保留 `docker compose down` 前的数据库备份（SQLite 文件 / PG dump）。
- 后端语义变更（如内核切换）先在验收环境跑 `docs/plan/server-comprehensive-test-plan.md` 全量用例。
- 快照预热依赖内核兼容性：切换内核后重验 `docs/plan/snapshot-warm-pool.md`。
