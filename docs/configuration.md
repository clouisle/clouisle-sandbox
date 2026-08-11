# 配置参考

## clouisle-api（控制平面）

### CLI 参数

| 参数 | 默认 | 说明 |
|---|---|---|
| `--addr` | `0.0.0.0:8080` | 监听地址 |
| `--db` | `clouisle.db` | SQLite 路径 或 `postgres://`/`postgresql://` 连接串（自动选择 Store） |
| `--kernel` | `/opt/clouisle/vmlinux` | guest 内核（FC 路径；快照需兼容内核，见下） |
| `--images-dir` | `/opt/clouisle/images` | OCI rootfs 缓存目录 |
| `--api-socket-dir` | `/run/clouisle/firecracker` | FC API socket 目录 |
| `--backend` | `firecracker` | 执行后端：`firecracker` / `docker-dev` |
| `--node-endpoint` | — | 远程 clouisled gRPC 端点（如 `http://node:9090`） |
| `--cluster-scheduling` | false | 按 heartbeat 注册表调度（与 `--node-endpoint` 互斥） |

互斥：`--backend docker-dev` 与 `--node-endpoint` / `--cluster-scheduling` 冲突。

### 环境变量

| 变量 | 说明 |
|---|---|
| `CLOUISLE_API_KEYS` | 注册 API key，多 key 逗号分隔：`key:tenant:scope`（scope=`full`/`read`）。不设置=开发模式 |
| `CLOUISLE_WARM_POOL_MIN_IDLE` | 快照预热池最小空闲数（0=关闭预热） |
| `CLOUISLE_ADMIN_TOKEN` | 管理令牌（部分 e2b_cloud 管理端点） |

## clouisled（节点守护）

| 参数 | 默认 | 说明 |
|---|---|---|
| `--addr` | `0.0.0.0:9090` | gRPC 监听 |
| `--db` | `/data/clouisled.db` | 本地元数据（SQLite） |
| `--node-id` | 空 | 稳定节点 ID（空=自动） |
| `--hostname` | 空 | 上报的控制面主机名 |
| `--kernel` | `/opt/clouisle/vmlinux` | guest 内核 |
| `--images-dir` | `/opt/clouisle/images` | rootfs 缓存 |
| `--api-socket-dir` | `/run/clouisle/firecracker` | FC socket 目录（**须与 API 独立**） |
| `--control-plane` | — | API HTTP 基址（持久化注册 + heartbeat） |
| `--control-plane-key` | — | 注册用 full key（需 `--control-plane`） |

## clouislectl（CLI 工具）

| 参数 | 说明 |
|---|---|
| `--api <url>` | API 地址（默认 `http://127.0.0.1:8080`） |
| `--key <key>` / `CLOUISLE_API_KEY` | API key（无 key 时受保护 API 返回 401） |

子命令：`health`、`create --image <ref> [--vcpu N] [--memory-mb N]`、`list [--status S]`、`delete <id>`、`exec <id> cmd...`。

## clouisle-agent（guest 内）

| 参数 | 说明 |
|---|---|
| （无参数） | guest PID 1（Firecracker，配置静态网络） |
| `serve` | 服务模式（FC 网络配置） |
| `serve --skip-network-config` | Docker 开发容器模式（跳过网络配置，仅 TCP 5201） |

## 后端差异速查

| 能力 | firecracker | docker-dev | 远程节点 |
|---|---|---|---|
| 快照 / restore | ✅ | ❌（明确拒绝） | ✅ |
| vsock | ✅ | ❌ | ✅ |
| egress allowlist | ✅（nftables） | ❌（拒绝非空） | ✅ |
| bandwidth / iops | ✅（tc / io.max） | ❌（拒绝） | ✅ |
| pids_max | ✅（guest cgroup） | ✅（Docker PidsLimit） | ✅ |
| mounts | ✅（共享） | ✅（mount_root 内只读） | ✅ |
| rootfs 隔离 | ✅（每沙盒副本） | ✅（容器） | ✅ |
| Docker socket | 无 | 有（dev 专属） | 无 |

## 内核要求（快照预热）

FC v1.10 snapshot 为 dev-preview：自定义 7.0 内核恢复后确定性崩溃（`BUG: TASK stack guard page`）。启用快照预热须使用兼容内核（实测 `/opt/clouisle/vmlinux-vsock` 4.14.193，恢复后稳定 ≥65s）。纯冷创建不受影响。

## Docker Compose

### 生产（KVM，docker-compose.yml）
- API + PostgreSQL + Firecracker（`/dev/kvm`、host network、privileged）。
- 多节点 overlay：`deploy/multinode/`（API + daemonset 节点）。

### 开发（Docker Desktop/WSL2，docker-compose.dev.yml）
```bash
docker compose -f docker-compose.dev.yml up --build
# API: http://localhost:18080  key: e2b_dev_...（dev:full）
```
- 挂载 `/var/run/docker.sock`（开发专用）；API 连 `clouisle-dev-mgmt`（internal）+ default（端口发布）。
- 沙盒为容器（注入 agent），不支持快照/iops/带宽/allowlist。

## 配置示例

### 单节点生产（SQLite）
```bash
CLOUISLE_API_KEYS="prod-key:team-a:full" clouisle-api \
  --addr 0.0.0.0:8080 --db /data/clouisle.db \
  --kernel /opt/clouisle/vmlinux-vsock --images-dir /opt/clouisle/images \
  --api-socket-dir /run/clouisle/firecracker
```

### HA（PostgreSQL + 预热池）
```bash
CLOUISLE_API_KEYS="prod-key:team-a:full" \
CLOUISLE_WARM_POOL_MIN_IDLE=2 \
clouisle-api --db "postgres://clouisle:pass@pg:5432/clouisle" \
  --kernel /opt/clouisle/vmlinux-vsock --images-dir /opt/clouisle/images
```

### 多节点
```bash
# 节点（每台 KVM 主机）
clouisled --addr 0.0.0.0:9090 --node-id node-1 \
  --kernel /opt/clouisle/vmlinux-vsock --images-dir /opt/clouisle/images \
  --api-socket-dir /run/clouisle/firecracker

# 控制面（调度到节点）
clouisle-api --cluster-scheduling --db "postgres://..." --kernel /opt/clouisle/vmlinux-vsock
```

### Docker 开发（macOS/Windows）
```bash
docker compose -f docker-compose.dev.yml up --build
export E2B_API_KEY="e2b_dev_00000000000000000000000000000000000000"
curl localhost:18080/health
```
