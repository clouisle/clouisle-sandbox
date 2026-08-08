# Clouisle Sandbox

微VM 沙盒调度系统 —— 基于 Firecracker 的高密度、快速启动的隔离计算环境。

每个沙盒是一个真正的 microVM（Firecracker + KVM），拥有独立内核与根文件系统，
通过 vsock 与宿主机通信，支持命令执行、文件传输、多租户、审计与网络隔离。

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
                                       │ gRPC (mTLS)
                    ┌──────────────────▼───────────────────────┐
                    │           数据平面 (data plane)           │
                    │                                          │
                    │  FirecrackerVmm (Firecracker + KVM)      │
                    │    ├─ 进程组管理 (killpg, 无孤儿残留)     │
                    │    ├─ seccomp / jailer / cgroup v2       │
                    │    └─ vsock 通道 (host ↔ guest)          │
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
Stage 1: rust:1.85-slim → 编译 Rust 二进制
Stage 2: debian:bookworm-slim → 装 Firecracker + 复制二进制
```

**重要配置**：

| 参数 | 说明 |
|------|------|
| `privileged: true` | 容器内需访问 `/dev/kvm` + 创建 netns |
| `network_mode: host` | `netns`/`nftables` 需要宿主网络栈 |
| `/dev/kvm` 挂载 | 必须，否则 firecracker 无法启动 |
| `vmlinux` / `rootfs` | 需预先放置到宿主机 `/opt/clouisle/` |

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

### 环境要求

| 组件 | 要求 |
|------|------|
| 操作系统 | **Linux**（唯一支持的运行平台） |
| 虚拟化 | `/dev/kvm` 可用（裸金属或嵌套虚拟化） |
| Firecracker | v1.10.1（`/usr/local/bin/firecracker`） |
| Rust | ≥ 1.85（edition 2024） |

> macOS / Windows 仅可编译控制平面相关 crate（`clouisle-core`、`clouisle-store` 等），
> `FirecrackerVmm` 通过 `#[cfg(target_os = "linux")]` 门控，非 Linux 平台不可用。

### 使用 CLI（clouislectl）

```bash
# 健康检查
cargo run -p clouislectl -- health

# 创建沙盒 (1 vCPU / 256 MB)
cargo run -p clouislectl -- create --image alpine:latest --vcpu 1 --memory-mb 256

# 列出沙盒
cargo run -p clouislectl -- list

# 在 microVM 内执行命令
cargo run -p clouislectl -- exec <sandbox-id> echo hello

# 删除沙盒
cargo run -p clouislectl -- delete <sandbox-id>
```

### 直接使用 HTTP API

```bash
# 创建沙盒
curl -X POST localhost:8080/api/v1/sandboxes \
  -H 'Content-Type: application/json' \
  -d '{"image":{"reference":"alpine"},"resources":{"vcpu":1,"memory_mb":256,"disk_mb":512}}'

# 在 microVM 中执行命令
curl -X POST localhost:8080/api/v1/sandboxes/<id>/exec \
  -H 'Content-Type: application/json' \
  -d '{"argv":["uname","-a"],"timeout_ms":10000}'

# 删除沙盒
curl -X DELETE localhost:8080/api/v1/sandboxes/<id>

# 健康检查
curl localhost:8080/health
curl localhost:8080/health/live
curl localhost:8080/health/ready

# Prometheus 指标
curl localhost:8080/metrics
```

## API 端点

### 沙盒生命周期

| 方法 | 路径 | 说明 |
|------|------|------|
| POST | `/api/v1/sandboxes` | 创建沙盒 |
| GET | `/api/v1/sandboxes` | 列出沙盒（`?status=&limit=&offset=`） |
| GET | `/api/v1/sandboxes/{id}` | 查询单个沙盒 |
| DELETE | `/api/v1/sandboxes/{id}` | 删除沙盒 |

### 命令执行

| 方法 | 路径 | 说明 |
|------|------|------|
| POST | `/api/v1/sandboxes/{id}/exec` | 同步执行命令 |
| POST | `/api/v1/sandboxes/{id}/exec/stream` | 流式执行（SSE，stdout/stderr 逐行推送） |
| GET | `/api/v1/sandboxes/{id}/exec` | 执行历史记录 |
| GET | `/api/v1/sandboxes/{id}/exec/{exec_id}` | 单条执行记录 |

### 文件传输

| 方法 | 路径 | 说明 |
|------|------|------|
| POST | `/api/v1/sandboxes/{id}/files/upload?path=` | 上传文件（≤50MB） |
| GET | `/api/v1/sandboxes/{id}/files/download?path=` | 下载文件 |
| GET | `/api/v1/sandboxes/{id}/files/ls?path=` | 列目录 |

### 可观测性

| 方法 | 路径 | 说明 |
|------|------|------|
| GET | `/health` | 健康检查 |
| GET | `/health/live` | 存活探针（K8s livenessProbe） |
| GET | `/health/ready` | 就绪探针（K8s readinessProbe） |
| GET | `/metrics` | Prometheus 指标 |

### 请求体结构

#### `SandboxSpec`（创建沙盒）

| 字段 | 类型 | 默认值 | 说明 |
|------|------|--------|------|
| `image` | `{reference, digest?}` | — | 镜像引用，如 `"alpine"` |
| `resources.vcpu` | `u16` | `1` | vCPU 数量（1~4） |
| `resources.memory_mb` | `u32` | `256` | 内存（MiB，≥64） |
| `resources.disk_mb` | `u32` | `512` | 磁盘 scratch（MiB，≥64） |
| `resources.bandwidth_mbps` | `u32?` | `null` | 出站带宽上限 |
| `resources.iops` | `u32?` | `null` | 磁盘 IOPS 上限 |
| `network.enabled` | `bool` | `true` | 是否启用网络 |
| `network.allow_egress` | `[string]` | `[]` | 出站域名白名单，空=禁止全部出站 |
| `mounts` | `[{source,target,readonly}]` | `[]` | 卷挂载 |
| `secrets` | `[{name,value}]` | `[]` | 密钥注入（`/run/secrets/<name>`） |
| `ttl_secs` | `u64?` | `null` | 沙盒租期（秒），到期强制销毁 |
| `start_timeout_secs` | `u64` | `10` | 启动超时（秒） |
| `env` | `{string:string}` | `{}` | 环境变量 |
| `restart_policy` | `"never"` / `"on_failure"` / `"always"` | `"never"` | 重启策略 |

#### `ExecRequest`

| 字段 | 类型 | 默认值 | 说明 |
|------|------|--------|------|
| `argv` | `[string]` | — | 命令及参数，如 `["echo","hello"]` |
| `env` | `{string:string}` | `{}` | 额外环境变量 |
| `cwd` | `string?` | `null` | 工作目录 |
| `timeout_ms` | `u64` | `30000` | 执行超时（毫秒） |
| `stream` | `bool` | `false` | 是否 SSE 流式输出 |

### 错误响应

统一格式：`{ "error": { "code": "...", "message": "...", "details": null } }`

| HTTP 状态码 | `code` | 说明 |
|-------------|--------|------|
| 400 | `VALIDATION` | 请求参数校验失败 |
| 404 | `NOT_FOUND` | 沙盒/执行记录不存在 |
| 409 | `INVALID_STATE` | 状态冲突（如对已停止沙盒执行命令） |
| 507 | `RESOURCE_EXHAUSTED` | 资源不足（CPU/内存/磁盘配额超限） |
| 401 | `UNAUTHENTICATED` | 未提供有效 API key |
| 403 | `FORBIDDEN` | 权限不足（只读 key 尝试写操作） |
| 429 | `QUOTA_EXCEEDED` | 租户/沙盒数配额超限 |
| 500 | `INTERNAL` | 内部错误 |
| 503 | `VMM` | VMM 层错误（Firecracker 不可用等） |

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
  │    │  ├─ @allowed_v4 accept       │
  │    │  ├─ 10.0.0.0/8 accept       │
  │    │  └─ counter drop             │
  │    └─ postrouting: masquerade     │
  └─ 5. DNS proxy (10.0.0.1:53)      │
```

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
| `clouisle-proto` | host↔guest vsock 帧协议（长度前缀 + postcard） |
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