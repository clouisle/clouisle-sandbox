# Clouisle Sandbox

微VM 沙盒调度系统 —— 基于 Firecracker 的高密度、快速启动的隔离计算环境。

每个沙盒是一个真正的 microVM（Firecracker + KVM），拥有独立内核与根文件系统，通过沙盒专用 TCP 网络通道与 guest agent 通信，支持命令执行、文件传输、多租户、审计与网络隔离。

## 架构

```
                    ┌──────────────────────────────────────────┐
                    │           控制平面 (control plane)         │
                    │                                          │
                    │  clouisle-apiserver (HTTP API, axum)     │
                    │    ├─ 沙盒生命周期 / 命令执行 / 文件传输  │
                    │    ├─ 认证 (API key + 租户隔离)           │
                    │    ├─ 审计哈希链 (Ed25519 签名)           │
                    │    ├─ 资源准入 + 分布式调度               │
                    │    └─ 存储 → PostgreSQL / SQLite          │
                    │                                          │
                    │  clouisled (节点代理, gRPC)               │
                    │    ├─ 节点注册 / 心跳上报 (3s)            │
                    │    ├─ 本机 VMM 生命周期管理               │
                    │    ├─ Reconciler 漂移收敛 (10s)           │
                    │    └─ 防火墙：netns + nftables + DNS      │
                    └──────────────────┬───────────────────────┘
                                       │ gRPC
                    ┌──────────────────▼───────────────────────┐
                    │           数据平面 (data plane)           │
                    │                                          │
                    │  FirecrackerVmm (Firecracker + KVM)      │
                    │    ├─ 进程组管理 (killpg, 无孤儿残留)     │
                    │    ├─ seccomp / jailer / cgroup v2       │
                    │    └─ guest-agent TCP（host ↔ guest:5201） │
                    │                                          │
                    │  每沙盒独立：                             │
                    │    ├─ netns (clo-<hash>)                 │
                    │    ├─ TAP (10.0.0.2/30) + veth pair      │
                    │    ├─ nftables 默认 drop 入站            │
                    │    └─ 出站白名单动态集 (@allowed_v4)     │
                    └──────────────────────────────────────────┘
```

## 部署

### 模式一：单机 Docker 部署（推荐）

适合开发测试、单机生产。一个 `docker-compose up` 拉起全部服务。

**依赖**：宿主机需 `/dev/kvm` + 内核镜像 `/opt/clouisle/vmlinux` + 基础 rootfs。

```bash
# 1. 准备 guest 内核与 rootfs（按需，Phase 0 构建脚本）
#    images/kernel/build-kernel.sh → /opt/clouisle/vmlinux
#    images/rootfs/build-rootfs.sh → /opt/clouisle/rootfs/base.ext4

# 2. 构建并启动
docker compose up -d --build

# 3. 验证
curl localhost:8080/health
# → {"status":"ok","store":"ok","version":"0.1.0"}

# 4. 创建沙盒测试
docker compose exec apiserver clouislectl create --image alpine --vcpu 1 --memory-mb 256

# 5. 查看日志
docker compose logs -f apiserver

# 6. 停止
docker compose down
```

**镜像架构**（`Dockerfile` 多阶段构建）：

```
Stage 1: rust:1-slim-bookworm → 编译 Rust 二进制
Stage 2: debian:bookworm-slim → 装 Firecracker + 复制二进制
```

**重要配置**：

| 参数 | 说明 |
|------|------|
| `privileged: true` | 容器内需访问 `/dev/kvm` + 创建 netns |
| `network_mode: host` | `netns`/`nftables` 需要宿主网络栈 |
| `/dev/kvm` 挂载 | 必须，否则 firecracker 无法启动 |
| `vmlinux` / `rootfs` | 需预先放置到宿主机 `/opt/clouisle/` |
| `CLOUISLE_API_KEYS` | `clouisle-apiserver` 必填；格式为逗号分隔的 `key:tenant:read\|full`。应存入 Secret，严禁提交生产 key。 |

**存储模式切换**：

```bash
# SQLite 单机（快速启动）
docker compose up -d

# PostgreSQL（HA 就绪，docker-compose.yml 默认已配）
# 自动检测 postgres:// 连接串 → 使用 PostgresStore
```

### 模式二：Kubernetes（生产级）

每节点一个 clouisled DaemonSet Pod，pod 内直管 firecracker 进程。
apiserver 多副本，PostgreSQL 共享状态，gRPC 转发到 clouisled。

```
┌────────────────────── K8s 集群 ──────────────────────┐
│                                                       │
│  Deployment: apiserver (副本×2, 无状态)               │
│    └─ HTTP API / 调度 / 存储 → PostgreSQL              │
│                                                       │
│  DaemonSet: clouisled (每节点一个 Pod)                 │
│    └─ Pod 内: [clouisled] + [firecracker 进程们]       │
│        沙盒 A  沙盒 B  沙盒 C  (Pod 内多进程)          │
│                                                       │
│  StatefulSet: postgres (控制平面共享状态)               │
│                                                       │
│  NetworkPolicy: 默认拒绝跨命名空间                      │
└───────────────────────────────────────────────────────┘
```

**部署步骤**：

```bash
# 1. 创建命名空间 + RBAC
kubectl apply -f deploy/00-rbac.yaml

# 2. 启动 PostgreSQL
kubectl apply -f deploy/03-postgres.yaml
kubectl -n clouisle wait --for=condition=ready pod -l app=postgres

# 3. 启动 apiserver（多副本）
kubectl apply -f deploy/01-apiserver.yaml

# 4. 启动 clouisled（DaemonSet）
kubectl apply -f deploy/02-daemonset.yaml

# 5. 应用网络策略
kubectl apply -f deploy/04-networkpolicy.yaml
```

**关键安全配置**：

| 配置 | 说明 |
|------|------|
| `automountServiceAccountToken: false` | 沙盒 Pod 不持有 K8s token，逃逸后无法操作集群 |
| `privileged: true` | 仅 clouisled Pod 需要，用于访问 `/dev/kvm` + 创建 netns |
| `hostNetwork: true` | 需要宿主网络栈创建 netns / nftables |
| Role 最小权限 | 只读 Pod（`get/list`），不创建/删除/修改 |

**部署清单**（`deploy/` 目录）：

| 文件 | 内容 |
|------|------|
| `00-rbac.yaml` | 命名空间 + ServiceAccount + Role + RoleBinding |
| `01-apiserver.yaml` | apiserver Deployment + Service + Secret |
| `02-daemonset.yaml` | clouisled DaemonSet（hostNetwork + /dev/kvm 透传） |
| `03-postgres.yaml` | PostgreSQL StatefulSet + Service |
| `04-networkpolicy.yaml` | 默认拒绝 + 必要端口的白名单 |

### 模式三：高可用（HA）

| 组件 | 高可用方式 |
|------|-----------|
| apiserver | Deployment 多副本，PostgreSQL 共享状态，**无状态**（不持有 VMM 引用） |
| clouisled | DaemonSet 每节点一个，心跳超时（15s）标记节点 unreachable |
| 存储 | PostgreSQL StatefulSet 或云 RDS（`PostgresStore` 代码已有，连接串自动检测） |
| 资源调度 | 乐观锁：`UPDATE nodes SET allocated=... WHERE ... RETURNING`（防止多实例超卖） |
| 健康检查 | `/health/live`（存活）+ `/health/ready`（就绪），K8s readinessProbe 自动摘流 |
| 优雅关闭 | SIGTERM → `/health/ready` 返回 503 → LB 摘流 → 等 30s → 退出，不销毁沙盒 |

## 快速开始

### 运行时边界

Clouisle 是**仅容器运行时**。不得在宿主机直接启动 `clouisle-api`、`clouisled`、`clouislectl`、Firecracker 或 guest agent。宿主机只提供 Docker、Linux KVM 和挂载的 guest 资源；所有 Clouisle 进程都在 Docker 容器或 Kubernetes Pod 内运行。

### 环境要求

| 组件 | 要求 |
|------|------|
| 宿主机操作系统 | **Linux**（唯一支持的运行平台） |
| 容器运行时 | Docker Engine + Docker Compose v2 |
| 虚拟化 | `/dev/kvm` 可用（裸金属或嵌套虚拟化） |
| Guest 资源 | 内核及 rootfs/cache 挂载在 `/opt/clouisle/` 下 |

Firecracker 与静态链接的 guest agent 都构建在 OCI 镜像中。Rust 只用于修改源码或 CI 检查，绝不用于运行时运维。

### 使用 CLI（在 Compose 容器内）

```bash
# 健康检查
docker compose exec apiserver clouislectl health

# 创建沙盒 (1 vCPU / 256 MB)
docker compose exec apiserver clouislectl create --image alpine:latest --vcpu 1 --memory-mb 256

# 列出沙盒
docker compose exec apiserver clouislectl list

# 在 microVM 内执行命令
docker compose exec apiserver clouislectl exec <sandbox-id> echo hello

# 删除沙盒
docker compose exec apiserver clouislectl delete <sandbox-id>
```

### 直接使用 HTTP API

```bash
# 全部 /api/v1/* 端点需要 Bearer API key。Compose 中的开发值为
# local-development-key；部署前必须替换。
export CLOUISLE_API_KEY=local-development-key

# 创建沙盒
curl -X POST localhost:8080/api/v1/sandboxes \
  -H "Authorization: Bearer $CLOUISLE_API_KEY" \
  -H 'Content-Type: application/json' \
  -d '{"image":{"reference":"alpine"},"resources":{"vcpu":1,"memory_mb":256,"disk_mb":512}}'

# 在 microVM 中执行命令
curl -X POST localhost:8080/api/v1/sandboxes/<id>/exec \
  -H "Authorization: Bearer $CLOUISLE_API_KEY" \
  -H 'Content-Type: application/json' \
  -d '{"argv":["uname","-a"],"timeout_ms":10000}'

# 删除沙盒
curl -X DELETE localhost:8080/api/v1/sandboxes/<id> \
  -H "Authorization: Bearer $CLOUISLE_API_KEY"

# 以下运维端点刻意不鉴权，供探针与 Prometheus 抓取。
curl localhost:8080/health
curl localhost:8080/health/live
curl localhost:8080/health/ready
curl localhost:8080/metrics
```

## API 端点

### 认证与租户隔离

生产服务必须配置 `CLOUISLE_API_KEYS`，格式为逗号分隔的 `key:tenant:read|full`。所有 `/api/v1/*` 请求需要 `Authorization: Bearer <key>`；`read` key 只能读取，`full` key 才能创建、执行、上传、删除和更新节点租约。认证 key 决定沙盒所属租户；其他租户访问沙盒、执行记录和文件资源一律返回 `404`。`/health`、`/health/live`、`/health/ready` 与 `/metrics` 刻意保持公开。

### 完整 HTTP 接口参考

下表枚举 `clouisle-apiserver` 注册的全部路由。`{id}` 和 `{exec_id}` 均为 UUID 字符串。除明确标注外，所有 `/api/v1/*` 成功响应均为 JSON。

| 方法 | 路径 | 权限 | 请求或查询参数 | 成功响应 |
|------|------|------|----------------|----------|
| POST | `/api/v1/sandboxes` | `full` | `CreateSandboxRequest` JSON | `201` + `Sandbox` |
| GET | `/api/v1/sandboxes` | `read`/`full` | `status`、`limit`、`offset` | `200` + `{items: Sandbox[], total: number}` |
| GET | `/api/v1/sandboxes/{id}` | 所有者 | — | `200` + `Sandbox` |
| DELETE | `/api/v1/sandboxes/{id}` | 所有者 + `full` | — | `204` |
| POST | `/api/v1/sandboxes/{id}/exec` | 所有者 + `full` | `ExecRequest` JSON | `200` + `ExecResponse` |
| POST | `/api/v1/sandboxes/{id}/exec/stream` | 所有者 + `full` | `ExecRequest` JSON | `200` + `text/event-stream`（`stdout`、`stderr`、`exit`、`error`） |
| GET | `/api/v1/sandboxes/{id}/exec` | 所有者 | `limit`（默认 `100`） | `200` + `ExecutionRecord[]` |
| GET | `/api/v1/sandboxes/{id}/exec/{exec_id}` | 所有者 | — | `200` + `ExecutionRecord` |
| POST | `/api/v1/sandboxes/{id}/files/upload` | 所有者 + `full` | 必填 `path` query + 原始字节（≤50 MiB） | `200` + `{ok: true}` |
| GET | `/api/v1/sandboxes/{id}/files/download` | 所有者 | 必填 `path` query | `200` + 原始字节，`application/octet-stream` |
| GET | `/api/v1/sandboxes/{id}/files/ls` | 所有者 | 必填 `path` query | `200` + `{items: DirEntry[]}` |
| POST | `/api/v1/nodes` | `full` | `RegisteredNode` JSON | `204` |
| GET | `/api/v1/nodes` | `read`/`full` | — | `200` + 最近 15 秒有心跳的 `RegisteredNode[]` |
| GET | `/health` | 公开 | — | `200` 或 `503` + `{status, store, version}` |
| GET | `/health/live` | 公开 | — | `200` + `{status: "alive"}` |
| GET | `/health/ready` | 公开 | — | `200` 或 `503` + `{status: "ready"|"not_ready"}` |
| GET | `/metrics` | 公开 | — | `200` Prometheus 文本（`text/plain; version=0.0.4`） |

沙盒列表的 `status` 可取 `pending`、`starting`、`running`、`stopping`、`stopped`、`error`；其他值返回 `400`。`limit` 默认 `100`，`offset` 默认 `0`，传入 `limit=0` 时按 `1` 处理。文件 `path` 必须非空，且不得包含 `..` 或平台路径前缀。

### 请求与响应模型

#### `CreateSandboxRequest`

`POST /api/v1/sandboxes` 将以下字段直接放在 JSON 顶层。

| 字段 | 类型 | 默认值 | 契约 |
|------|------|--------|------|
| `image.reference` | string | 必填 | OCI 镜像引用，不得为空白 |
| `image.digest` | string/null | `null` | 可选的不可变镜像 digest |
| `resources.vcpu` | integer | `1` | 虚拟 CPU 数量，`1..=4` |
| `resources.memory_mb` | integer | `256` | 内存，单位 MiB，范围 `64..=8192` |
| `resources.disk_mb` | integer | `512` | Scratch 磁盘，单位 MiB，至少 `64` |
| `resources.bandwidth_mbps` | integer/null | `null` | 出站带宽上限，单位 Mbps；提供时至少 `1` Mbps |
| `resources.iops` | integer/null | `null` | 磁盘 I/O 每秒操作数；提供时至少 `1` IOPS |
| `resources.pids_max` | integer/null | `512` | guest cgroup 进程数量上限 |
| `network.enabled` | boolean | `true` | `false` 时仍保留管理 agent 通道；公网出站被拒绝 |
| `network.allow_egress` | string[] | `[]` | DNS 域名白名单；空数组拒绝全部公网出站 |
| `mounts` | `{source,target,readonly}`[] | `[]` | 请求的 host 到 guest 挂载 |
| `secrets` | `{name,value}`[] | `[]` | 写入 `/run/secrets/<name>`；名称必须唯一且为普通文件名，响应会脱敏 value |
| `ttl_secs` | integer/null | `null` | 运行期秒数；仅到达 `Running` 后开始计时 |
| `start_timeout_secs` | integer | `10` | agent ready 截止时间，单位秒，范围 `1..=300` |
| `env` | object | `{}` | guest 环境变量 |
| `node_selector` | object | `{}` | 集群调度时所需的节点标签 |
| `restart_policy` | `never`/`on_failure`/`always` | `never` | 持久化重启策略 |
| `tenant_id` | string/null | 忽略 | 会被认证 key 的租户覆盖 |
| `sync` | boolean | `true` | 为 wire compatibility 接受；当前无论其值均等待 guest ready |

create/get/list 返回的 `Sandbox` 含有 `id`、`spec`、`status`、`created_at`、`updated_at`、`ready_at`、`expires_at`、`vmm_meta`、`terminal_message`、`node_id`。时间戳为 RFC 3339 UTC 字符串。`vmm_meta` 含有 `backend`、可选进程 `pid`、`api_socket`、`vsock_socket`、数值 `vsock_cid`、`vmm_id`、`extra`。

#### `ExecRequest`、`ExecResponse` 与执行历史

| 字段 | 类型 | 默认值 | 契约 |
|------|------|--------|------|
| `argv` | string[] | 必填 | 非空的命令及参数数组 |
| `env` | object | `{}` | 覆盖沙盒环境中同名变量 |
| `cwd` | string/null | `null` | guest 工作目录 |
| `timeout_ms` | integer | `30000` | 执行超时，单位毫秒，至少为 `1` ms |
| `stream` | boolean | `false` | 为兼容性接受；以 `/exec` 或 `/exec/stream` 路径选择响应模式 |

`ExecResponse` 为 `{exec_id, exit_code, stdout, stderr, duration_ms, timed_out, stdout_truncated, stderr_truncated}`，其中 `duration_ms` 单位为毫秒。输出以 UTF-8 lossy 文本返回，stdout/stderr 各最多保留 1 MiB，截断会由字段明确标识。`ExecutionRecord` 额外含有 `{id, sandbox_id, spec, started_at, finished_at, node_id}`。流式端点发送 SSE，但不会创建执行历史记录。

#### `RegisteredNode` 与文件响应

`POST /api/v1/nodes` 必须包含下列字段（`labels` 可省略，默认 `{}`），且 `endpoint` 不得为空。`total_memory_mb`、`total_disk_mb`、`allocated_memory_mb` 单位为 MiB；`last_heartbeat_ms` 为 Unix 毫秒；`total_vcpu`、`allocated_vcpu`、`running_sandboxes` 均为数量。

```json
{
  "info": {
    "node_id": "node-a", "hostname": "node-a", "total_vcpu": 16,
    "total_memory_mb": 32768, "total_disk_mb": 102400,
    "kvm_available": true, "kernel_version": "6.8", "firecracker_version": "1.10.1",
    "labels": {"zone": "a"}
  },
  "endpoint": "http://node-a:9090", "status": "ready",
  "last_heartbeat_ms": 1735689600000, "allocated_vcpu": 0,
  "allocated_memory_mb": 0, "running_sandboxes": 0
}
```

`status` 可为 `ready`、`unreachable`、`down`、`draining`。目录项为 `{name, size, mode, mtime, is_dir}`：`size` 单位为字节，`mode` 是数值 Unix 文件模式，`mtime` 为 Unix 秒。下载响应会在 `Content-Disposition` 中提供安全文件名。

### 错误响应

统一格式：`{ "error": { "code": "...", "message": "...", "details": null } }`

| HTTP 状态码 | `code` | 说明 |
|-------------|--------|------|
| 400 | `VALIDATION` | 请求参数校验失败 |
| 401 | `UNAUTHENTICATED` | 未提供或提供了无效 API key |
| 403 | `FORBIDDEN` | 只读 key 尝试执行变更操作 |
| 404 | `NOT_FOUND` | 调用方不可见的沙盒、执行记录或文件资源 |
| 409 | `INVALID_STATE` | 状态冲突，例如对未运行沙盒执行命令 |
| 422 | — | JSON 无法反序列化为该端点请求类型 |
| 429 | `QUOTA_EXCEEDED` | 租户或沙盒数量配额超限 |
| 500 | `INTERNAL`、`VMM`、`IO`、`NETWORK`、`IMAGE`、`TIMEOUT`、`STORE` | 内部或基础设施失败 |
| 503 | — | `/health` 或 `/health/ready` 报告存储不可用 |

## 数据库

### 存储什么

| 表 | 内容 | 说明 |
|----|------|------|
| `sandboxes` | 沙盒元数据（id/spec/status/vmm_meta/node_id） | **不存** rootfs/内核/进程内存 |
| `executions` | 执行记录（argv/exit_code/stdout/stderr） | 命令执行历史与审计 |

### 存储实现

| 实现 | 用途 | 说明 |
|------|------|------|
| `InMemoryStore` | 测试 | 单元/集成测试用 |
| `SqliteStore` | 单机部署（默认） | WAL 模式，零外部依赖 |
| `PostgresStore` | HA 多实例 | 连接串以 `postgres://` 开头自动切换 |

```bash
# SQLite（默认）
./clouisle-api --db /tmp/clouisle.db

# PostgreSQL（HA）
./clouisle-api --db "postgres://user:pass@host:5432/clouisle"
```

## 网络隔离（防火墙）

每沙盒创建时自动配置独立网络隔离环境，删除时自动清理。

```
沙盒创建                         沙盒删除
  │                                │
  ├─ 1. netns: netns add clo-<hash>  ├─ 1. nftables: delete table
  ├─ 2. TAP: tap0 10.0.0.2/30       ├─ 2. netns: delete clo-<hash>
  ├─ 3. veth: vn-<hash> 10.0.0.1/30  │
  ├─ 4. nftables ruleset:             │
  │    ├─ input: default drop         │
  │    │  ├─ iif "lo" accept          │
  │    │  ├─ iif "tap0" accept        │
  │    │  ├─ udp dport 53 accept      │
  │    │  └─ ct state established accept
  │    ├─ forward: default drop       │
  │    │  ├─ private/agent/DNS accept │
  │    │  ├─ 已解析白名单 IP 放行       │
  │    │  └─ counter drop             │
  │    └─ postrouting: masquerade     │
  └─ 5. host-veth 出站 guard + DNS proxy（gateway:53） │
```

host-veth guard 会阻断直接访问公网 IP。DNS 代理只为 `network.allow_egress` 中的域名返回记录，并动态放行其解析出的 IPv4 地址；空白名单会拒绝全部公网出站。

## 安全设计

| 层 | 措施 |
|----|------|
| **进程隔离** | 每沙盒独立 microVM（独立内核），Firecracker 进程组统一回收 |
| **网络隔离** | 每沙盒独立 netns + nftables 默认 drop 入站 + 出站白名单动态集 |
| **沙盒 K8s 权限** | `automountServiceAccountToken: false`，逃逸后无法操作集群 |
| **认证** | API key（Bearer token）+ 租户隔离 + scope 校验（`read` / `full`） |
| **审计** | 哈希链（SHA-256 逐条链接）+ Ed25519 批次签名，篡改可检出 |
| **文件传输** | 路径穿越防护（`..` 拒绝），写入限制在沙盒根内 |
| **资源限制** | vcpu / 内存 / 磁盘 Semaphore 准入，无超卖 |

## gRPC 协议（clouisled ↔ apiserver）

`clouisled` 节点代理通过 gRPC 与 `apiserver` 通信（`proto/node.proto`）：

```protobuf
service NodeService {
    rpc Register(NodeInfo) returns (NodeId);                    // 节点注册
    rpc Heartbeat(stream HeartbeatReport) returns (stream ...);  // 双向心跳
    rpc CreateSandbox(CreateSandboxRequest) returns (SandboxHandle);
    rpc DeleteSandbox(SandboxId) returns (DeleteResult);
    rpc Exec(stream ExecStream) returns (stream ExecStream);    // 双向 exec
}
```

**心跳周期**：3s。**超时判定**：15s 未收到 → 节点标记 `unreachable`。

## 沙盒状态机

```
Pending → Starting → Running → Stopping → Stopped → (delete)
             │          │
             ▼          ▼
           Error      Error
```

## SDK 客户端

官方 SDK，覆盖主流语言。**全部强类型**，公开 API 无 `any` / `Any` / `Value`。

| 语言 | 包名 | 位置 | 状态 |
|------|------|------|------|
| **Rust** | `clouisle-sdk` | [`sdk/rust/`](sdk/rust) | ✅ 异步，`reqwest` |
| **Python** | `clouisle-sandbox` | [`sdk/python/`](sdk/python) | ✅ `httpx` + `dataclass` 类型 |
| **TypeScript** | `@clouisle/sdk` | [`sdk/typescript/`](sdk/typescript) | ✅ `axios` + `.d.ts`，编译出 JS |

### Rust

```rust
use clouisle_sdk::{Client, SandboxSpec, ExecRequest};

let client = Client::new("http://localhost:8080", "my-api-key");

// 创建沙盒
let sb = client.create_sandbox(&SandboxSpec {
    image: ImageRef { reference: "alpine:latest".into(), digest: None },
    ..SandboxSpec::default()
}).await.unwrap();

// 执行命令
let result = client.exec_cmd(&sb.id, vec!["echo", "hello"], 5000).await.unwrap();
println!("exit: {}", result.exit_code);
```

### Python

```python
from clouisle import Client, SandboxSpec, ImageRef, ExecRequest

client = Client("http://localhost:8080", "my-api-key")

# 创建沙盒
sb = client.create_sandbox(SandboxSpec(
    image=ImageRef(reference="alpine:latest"),
))

# 执行命令
result = client.exec_cmd(sb.id, ["echo", "hello"])
print(f"exit: {result.exit_code}, stdout: {result.stdout}")
```

### TypeScript / JavaScript

```ts
import { Client } from "@clouisle/sdk";

const client = new Client("http://localhost:8080", "my-api-key");

// 创建沙盒
const sb = await client.createSandbox({
  image: { reference: "alpine:latest" },
  resources: { vcpu: 1, memory_mb: 256, disk_mb: 512 },
});

// 执行命令
const result = await client.execCmd(sb.id, ["echo", "hello"]);
console.log("exit:", result.exit_code, "stdout:", result.stdout);
```

## Workspace 结构

| Crate | 职责 |
|-------|------|
| `clouisle-core` | 领域模型、状态机、SLO 定义（纯逻辑，无 I/O） |
| `clouisle-vmm` | `Vmm` trait + `FirecrackerVmm`（进程组管理、HTTP-over-UDS 客户端） |
| `clouisle-store` | `Store` trait + SQLite / InMemory / PostgreSQL 实现 |
| `clouisle-scheduler` | 资源准入（Semaphore RAII）+ 多节点放置策略 |
| `clouisle-api` | Axum HTTP 服务（沙盒 CRUD / exec / 文件 / 健康 / 指标） |
| `clouisle-proto` | host↔guest 的分帧 TCP 协议（长度前缀 + postcard） |
| `clouisle-agent` | guest 内二进制（PID 1 init + serve） |
| `clouislectl` | 命令行工具 |
| `clouisled` | 节点代理（gRPC 服务 + 注册/心跳/reconciler） |
| `clouisle-net` | netns / nftables / DNS 白名单代理 / 防火墙编排器 |
| `clouisle-pool` | 快照预热池（FR-08） |
| `clouisle-images` | OCI 镜像拉取 + 卷管理 |
| `clouisle-audit` | 审计日志哈希链 + Ed25519 签名（SR-05） |
| `clouisle-obs` | Prometheus 指标 / tracing 日志 |
| `benches` | Criterion 性能基准 |
| `sdk/rust` | Rust SDK (`clouisle-sdk`) |
| `sdk/python` | Python SDK (`clouisle-sandbox`) |
| `sdk/typescript` | TypeScript/JS SDK (`@clouisle/sdk`)

## 测试

```bash
cargo test --workspace     # 全量测试（151+ 测试）
cargo bench -p clouisle-bench  # 性能基准（需 Linux + KVM）
```

| 测试层级 | 说明 | 运行平台 |
|---------|------|---------|
| 单元测试 | 状态机、调度、存储、协议编解码 | 全平台 |
| 集成测试（HTTP） | 沙盒生命周期 / exec / 文件 / 健康 | 全平台（TestVmm 夹具） |
| 端到端（Linux+KVM） | 真实 Firecracker microVM 创建→exec→删除→零残留 | Linux + `/dev/kvm` |

## 配置

### 服务启动参数

| 参数 | 默认值 | 说明 |
|------|--------|------|
| `--addr` | `0.0.0.0:8080` | 监听地址 |
| `--db` | `clouisle.db` | SQLite 路径 或 `postgres://` 连接串 |

### FirecrackerVmm 配置

（`crates/clouisle-vmm/src/firecracker.rs`）

| 字段 | 默认值 | 说明 |
|------|--------|------|
| `firecracker_bin` | `/usr/local/bin/firecracker` | Firecracker 二进制路径 |
| `jailer_bin` | `/usr/local/bin/jailer` | Jailer 路径（可选） |
| `kernel_path` | `/opt/clouisle/vmlinux` | Guest 内核 |
| `use_jailer` | `true` | 是否使用 Jailer（生产推荐） |
| `enable_seccomp` | `true` | 是否启用 seccomp |

## License

MIT