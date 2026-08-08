# Clouisle Sandbox

A micro-VM sandbox scheduling system based on Firecracker — high-density, fast-boot, and fully isolated compute environments.

Each sandbox is a real microVM (Firecracker + KVM) with its own kernel and root filesystem, communicating with the host via vsock. It supports command execution, file transfer, multi-tenancy, auditing, and network isolation.

> [中文文档 (Chinese README)](README.zh-CN.md)

## Architecture

```
                    ┌──────────────────────────────────────────┐
                    │          Control Plane                    │
                    │                                          │
                    │  clouisle-apiserver (HTTP API, axum)     │
                    │    ├─ Sandbox lifecycle / exec / files   │
                    │    ├─ Auth (API key + tenant isolation)  │
                    │    ├─ Audit hash chain (Ed25519 signed)  │
                    │    ├─ Resource admission + scheduling    │
                    │    └─ Storage → PostgreSQL / SQLite      │
                    │                                          │
                    │  clouisled (node agent, gRPC)            │
                    │    ├─ Node registration / heartbeat (3s) │
                    │    ├─ Local VMM lifecycle management     │
                    │    ├─ Reconciler drift convergence (10s) │
                    │    └─ Firewall: netns + nftables + DNS   │
                    └──────────────────┬───────────────────────┘
                                       │ gRPC (mTLS)
                    ┌──────────────────▼───────────────────────┐
                    │            Data Plane                    │
                    │                                          │
                    │  FirecrackerVmm (Firecracker + KVM)      │
                    │    ├─ Process group mgmt (killpg)        │
                    │    ├─ seccomp / jailer / cgroup v2       │
                    │    └─ vsock channel (host ↔ guest)       │
                    │                                          │
                    │  Per-sandbox isolation:                  │
                    │    ├─ netns (clo-<hash>)                 │
                    │    ├─ TAP (10.0.0.2/30) + veth pair      │
                    │    ├─ nftables default drop ingress      │
                    │    └─ Egress allowlist (@allowed_v4)     │
                    └──────────────────────────────────────────┘
```

## Deployment

### Mode 1: Single-node Docker (recommended)

Ideal for development and single-node production. One `docker compose up` starts everything.

**Prerequisites**: host needs `/dev/kvm`, kernel image `/opt/clouisle/vmlinux`, and base rootfs.

```bash
# 1. Prepare guest kernel & rootfs (optional, Phase 0 scripts)
#    images/kernel/build-kernel.sh → /opt/clouisle/vmlinux
#    images/rootfs/build-rootfs.sh → /opt/clouisle/rootfs/base.ext4

# 2. Build and start
docker compose up -d --build

# 3. Verify
curl localhost:8080/health
# → {"status":"ok","store":"ok","version":"0.1.0"}

# 4. Create a sandbox
docker compose exec apiserver clouislectl create --image alpine --vcpu 1 --memory-mb 256

# 5. View logs
docker compose logs -f apiserver

# 6. Stop
docker compose down
```

**Image structure** (`Dockerfile` multi-stage build):

```
Stage 1: rust:1.85-slim → compile Rust binaries
Stage 2: debian:bookworm-slim → install Firecracker + copy binaries
```

**Key configuration**:

| Setting | Description |
|---------|-------------|
| `privileged: true` | Container needs `/dev/kvm` access + netns creation |
| `network_mode: host` | `netns`/`nftables` need the host network stack |
| `/dev/kvm` mount | Required, or firecracker cannot start |
| `vmlinux` / `rootfs` | Must be pre-placed in host `/opt/clouisle/` |

**Storage mode switch**:

```bash
# SQLite (fast startup)
docker compose up -d

# PostgreSQL (HA-ready, default in docker-compose.yml)
# auto-detects postgres:// connection string → uses PostgresStore
```

### Mode 2: Kubernetes (production-grade)

One clouisled DaemonSet Pod per node, directly managing firecracker processes inside the Pod.
apiserver runs multi-replica, sharing state via PostgreSQL, forwarding via gRPC to clouisled.

```
┌────────────────────── K8s Cluster ──────────────────────┐
│                                                         │
│  Deployment: apiserver (replicas×2, stateless)          │
│    └─ HTTP API / scheduling / storage → PostgreSQL       │
│                                                         │
│  DaemonSet: clouisled (one Pod per node)                │
│    └─ Pod: [clouisled] + [firecracker processes]        │
│        sandbox A  sandbox B  sandbox C                  │
│                                                         │
│  StatefulSet: postgres (shared control-plane state)     │
│                                                         │
│  NetworkPolicy: default deny cross-namespace            │
└─────────────────────────────────────────────────────────┘
```

**Deploy steps**:

```bash
# 1. Create namespace + RBAC
kubectl apply -f deploy/00-rbac.yaml

# 2. Start PostgreSQL
kubectl apply -f deploy/03-postgres.yaml
kubectl -n clouisle wait --for=condition=ready pod -l app=postgres

# 3. Start apiserver (multi-replica)
kubectl apply -f deploy/01-apiserver.yaml

# 4. Start clouisled (DaemonSet)
kubectl apply -f deploy/02-daemonset.yaml

# 5. Apply network policies
kubectl apply -f deploy/04-networkpolicy.yaml
```

**Key security settings**:

| Setting | Description |
|---------|-------------|
| `automountServiceAccountToken: false` | Sandbox Pods hold no K8s token; escaping cannot touch the cluster |
| `privileged: true` | Only clouisled Pods need it (for `/dev/kvm` + netns) |
| `hostNetwork: true` | Need host network stack for netns / nftables |
| Minimal Role | Read-only Pods (`get/list`), no create/delete/modify |

**Manifests** (`deploy/` directory):

| File | Contents |
|------|----------|
| `00-rbac.yaml` | Namespace + ServiceAccount + Role + RoleBinding |
| `01-apiserver.yaml` | apiserver Deployment + Service + Secret |
| `02-daemonset.yaml` | clouisled DaemonSet (hostNetwork + /dev/kvm passthrough) |
| `03-postgres.yaml` | PostgreSQL StatefulSet + Service |
| `04-networkpolicy.yaml` | Default deny + required port allowlist |

### Mode 3: High Availability (HA)

| Component | HA approach |
|-----------|-------------|
| apiserver | Deployment multi-replica, PostgreSQL shared state, **stateless** (no VMM refs) |
| clouisled | DaemonSet per node, heartbeat timeout (15s) marks node unreachable |
| Storage | PostgreSQL StatefulSet or cloud RDS (`PostgresStore` code ready, auto-detected) |
| Scheduling | Optimistic lock: `UPDATE nodes SET allocated=... WHERE ... RETURNING` |
| Health | `/health/live` (liveness) + `/health/ready` (readiness), K8s probes auto-drain |
| Graceful shutdown | SIGTERM → `/health/ready` 503 → LB drain → wait 30s → exit, no sandbox teardown |

## Quick Start

### Requirements

| Component | Requirement |
|-----------|-------------|
| OS | **Linux** (only supported runtime platform) |
| Virtualization | `/dev/kvm` available (bare metal or nested virt) |
| Firecracker | v1.10.1 (`/usr/local/bin/firecracker`) |
| Rust | ≥ 1.85 (edition 2024) |

> macOS / Windows can only compile control-plane crates (`clouisle-core`, `clouisle-store`, etc.).
> `FirecrackerVmm` is gated by `#[cfg(target_os = "linux")]`, unavailable on non-Linux.

### Build

```bash
cargo build --workspace
```

### CLI (clouislectl)

```bash
# Health check
cargo run -p clouislectl -- health

# Create sandbox (1 vCPU / 256 MB)
cargo run -p clouislectl -- create --image alpine:latest --vcpu 1 --memory-mb 256

# List sandboxes
cargo run -p clouislectl -- list

# Exec command in microVM
cargo run -p clouislectl -- exec <sandbox-id> echo hello

# Delete sandbox
cargo run -p clouislectl -- delete <sandbox-id>
```

### Direct HTTP API

```bash
# Create sandbox
curl -X POST localhost:8080/api/v1/sandboxes \
  -H 'Content-Type: application/json' \
  -d '{"image":{"reference":"alpine"},"resources":{"vcpu":1,"memory_mb":256,"disk_mb":512}}'

# Exec in microVM
curl -X POST localhost:8080/api/v1/sandboxes/<id>/exec \
  -H 'Content-Type: application/json' \
  -d '{"argv":["uname","-a"],"timeout_ms":10000}'

# Delete sandbox
curl -X DELETE localhost:8080/api/v1/sandboxes/<id>

# Health checks
curl localhost:8080/health
curl localhost:8080/health/live
curl localhost:8080/health/ready

# Prometheus metrics
curl localhost:8080/metrics
```

## API Endpoints

### Sandbox Lifecycle

| Method | Path | Description |
|--------|------|-------------|
| POST | `/api/v1/sandboxes` | Create sandbox |
| GET | `/api/v1/sandboxes` | List sandboxes (`?status=&limit=&offset=`) |
| GET | `/api/v1/sandboxes/{id}` | Get single sandbox |
| DELETE | `/api/v1/sandboxes/{id}` | Delete sandbox |

### Command Execution

| Method | Path | Description |
|--------|------|-------------|
| POST | `/api/v1/sandboxes/{id}/exec` | Sync exec command |
| POST | `/api/v1/sandboxes/{id}/exec/stream` | Streaming exec (SSE, per-line stdout/stderr) |
| GET | `/api/v1/sandboxes/{id}/exec` | Execution history |
| GET | `/api/v1/sandboxes/{id}/exec/{exec_id}` | Single execution record |

### File Transfer

| Method | Path | Description |
|--------|------|-------------|
| POST | `/api/v1/sandboxes/{id}/files/upload?path=` | Upload file (≤50MB) |
| GET | `/api/v1/sandboxes/{id}/files/download?path=` | Download file |
| GET | `/api/v1/sandboxes/{id}/files/ls?path=` | List directory |

### Observability

| Method | Path | Description |
|--------|------|-------------|
| GET | `/health` | Health check |
| GET | `/health/live` | Liveness probe (K8s livenessProbe) |
| GET | `/health/ready` | Readiness probe (K8s readinessProbe) |
| GET | `/metrics` | Prometheus metrics |

### Request Bodies

#### `SandboxSpec` (create sandbox)

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `image` | `{reference, digest?}` | — | Image reference, e.g. `"alpine"` |
| `resources.vcpu` | `u16` | `1` | vCPU count (1~4) |
| `resources.memory_mb` | `u32` | `256` | Memory (MiB, ≥64) |
| `resources.disk_mb` | `u32` | `512` | Disk scratch (MiB, ≥64) |
| `resources.bandwidth_mbps` | `u32?` | `null` | Egress bandwidth cap |
| `resources.iops` | `u32?` | `null` | Disk IOPS cap |
| `network.enabled` | `bool` | `true` | Enable networking |
| `network.allow_egress` | `[string]` | `[]` | Egress domain allowlist, empty = deny all egress |
| `mounts` | `[{source,target,readonly}]` | `[]` | Volume mounts |
| `secrets` | `[{name,value}]` | `[]` | Secret injection (`/run/secrets/<name>`) |
| `ttl_secs` | `u64?` | `null` | Sandbox TTL (seconds), force destroy on expiry |
| `start_timeout_secs` | `u64` | `10` | Start timeout (seconds) |
| `env` | `{string:string}` | `{}` | Environment variables |
| `restart_policy` | `"never"` / `"on_failure"` / `"always"` | `"never"` | Restart policy |

#### `ExecRequest`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `argv` | `[string]` | — | Command and args, e.g. `["echo","hello"]` |
| `env` | `{string:string}` | `{}` | Extra environment vars |
| `cwd` | `string?` | `null` | Working directory |
| `timeout_ms` | `u64` | `30000` | Exec timeout (ms) |
| `stream` | `bool` | `false` | SSE streaming output |

### Error Responses

Unified format: `{ "error": { "code": "...", "message": "...", "details": null } }`

| HTTP Status | `code` | Description |
|-------------|--------|-------------|
| 400 | `VALIDATION` | Request validation failed |
| 404 | `NOT_FOUND` | Sandbox/execution record not found |
| 409 | `INVALID_STATE` | State conflict (e.g. exec on stopped sandbox) |
| 507 | `RESOURCE_EXHAUSTED` | Insufficient resources (CPU/mem/disk quota) |
| 401 | `UNAUTHENTICATED` | Missing/invalid API key |
| 403 | `FORBIDDEN` | Insufficient scope (read-only key on write op) |
| 429 | `QUOTA_EXCEEDED` | Tenant/sandbox quota exceeded |
| 500 | `INTERNAL` | Internal error |
| 503 | `VMM` | VMM layer error (Firecracker unavailable, etc.) |

## Database

### What is stored

| Table | Contents | Notes |
|-------|----------|-------|
| `sandboxes` | Sandbox metadata (id/spec/status/vmm_meta/node_id) | Does **not** store rootfs/kernel/process memory |
| `executions` | Execution records (argv/exit_code/stdout/stderr) | Command history & audit |

### Store Implementations

| Implementation | Use case | Notes |
|----------------|----------|-------|
| `InMemoryStore` | Testing | Unit/integration tests |
| `SqliteStore` | Single-node (default) | WAL mode, zero external deps |
| `PostgresStore` | HA multi-instance | Auto-switched on `postgres://` prefix |

```bash
# SQLite (default)
./clouisle-api --db /tmp/clouisle.db

# PostgreSQL (HA)
./clouisle-api --db "postgres://user:pass@host:5432/clouisle"
```

## Network Isolation (Firewall)

Each sandbox gets an isolated network environment at creation, cleaned up at deletion.

```
Sandbox create                      Sandbox delete
  │                                    │
  ├─ 1. netns: netns add clo-<hash>     ├─ 1. nftables: delete table
  ├─ 2. TAP: tap0 10.0.0.2/30          ├─ 2. netns: delete clo-<hash>
  ├─ 3. veth: vn-<hash> 10.0.0.1/30    │
  ├─ 4. nftables ruleset:              │
  │    ├─ input: default drop          │
  │    │  ├─ iif "lo" accept           │
  │    │  ├─ iif "tap0" accept         │
  │    │  ├─ udp dport 53 accept       │
  │    │  └─ ct state established accept
  │    ├─ forward: default drop        │
  │    │  ├─ @allowed_v4 accept        │
  │    │  ├─ 10.0.0.0/8 accept        │
  │    │  └─ counter drop              │
  │    └─ postrouting: masquerade      │
  └─ 5. DNS proxy (10.0.0.1:53)        │
```

## Security Design

| Layer | Measure |
|-------|---------|
| **Process isolation** | Per-sandbox microVM (own kernel), Firecracker process-group cleanup |
| **Network isolation** | Per-sandbox netns + nftables default drop ingress + egress allowlist |
| **Sandbox K8s perms** | `automountServiceAccountToken: false`, cant touch cluster after escape |
| **Auth** | API key (Bearer token) + tenant isolation + scope check (`read` / `full`) |
| **Audit** | SHA-256 hash chain + Ed25519 batch signing, tamper-evident |
| **File transfer** | Path traversal protection (`..` rejected), writes confined to sandbox root |
| **Resource limits** | vcpu/mem/disk Semaphore admission, no oversubscription |

## gRPC Protocol (clouisled ↔ apiserver)

`clouisled` node agent communicates with `apiserver` via gRPC (`proto/node.proto`):

```protobuf
service NodeService {
    rpc Register(NodeInfo) returns (NodeId);                    // node registration
    rpc Heartbeat(stream HeartbeatReport) returns (stream ...);  // bidi heartbeat
    rpc CreateSandbox(CreateSandboxRequest) returns (SandboxHandle);
    rpc DeleteSandbox(SandboxId) returns (DeleteResult);
    rpc Exec(stream ExecStream) returns (stream ExecStream);    // bidi exec
}
```

**Heartbeat period**: 3s. **Timeout threshold**: 15s without heartbeat → node marked `unreachable`.

## Sandbox State Machine

```
Pending → Starting → Running → Stopping → Stopped → (delete)
             │          │
             ▼          ▼
           Error      Error
```

## SDKs

Official SDKs for all major languages. Fully typed — no `any` / `Any` / `Value` in the public API.

| Language | Package | Location | Status |
|----------|---------|----------|--------|
| **Rust** | `clouisle-sdk` | [`sdk/rust/`](sdk/rust) | ✅ async, `reqwest` |
| **Python** | `clouisle-sandbox` | [`sdk/python/`](sdk/python) | ✅ `httpx` + `dataclass` types |
| **TypeScript** | `@clouisle/sdk` | [`sdk/typescript/`](sdk/typescript) | ✅ `axios` + `.d.ts`, compiles to JS |

### Rust

```rust
use clouisle_sdk::{Client, SandboxSpec, ExecRequest};

let client = Client::new("http://localhost:8080", "my-api-key");

// Create sandbox
let sb = client.create_sandbox(&SandboxSpec {
    image: ImageRef { reference: "alpine:latest".into(), digest: None },
    ..SandboxSpec::default()
}).await.unwrap();

// Exec command
let result = client.exec_cmd(&sb.id, vec!["echo", "hello"], 5000).await.unwrap();
println!("exit: {}", result.exit_code);
```

### Python

```python
from clouisle import Client, SandboxSpec, ImageRef, ExecRequest

client = Client("http://localhost:8080", "my-api-key")

# Create sandbox
sb = client.create_sandbox(SandboxSpec(
    image=ImageRef(reference="alpine:latest"),
))

# Exec command
result = client.exec_cmd(sb.id, ["echo", "hello"])
print(f"exit: {result.exit_code}, stdout: {result.stdout}")
```

### TypeScript / JavaScript

```ts
import { Client } from "@clouisle/sdk";

const client = new Client("http://localhost:8080", "my-api-key");

// Create sandbox
const sb = await client.createSandbox({
  image: { reference: "alpine:latest" },
  resources: { vcpu: 1, memory_mb: 256, disk_mb: 512 },
});

// Exec command
const result = await client.execCmd(sb.id, ["echo", "hello"]);
console.log("exit:", result.exit_code, "stdout:", result.stdout);
```

## Workspace Structure

| Crate | Responsibility |
|-------|----------------|
| `clouisle-core` | Domain models, state machine, SLO definitions (pure logic, no I/O) |
| `clouisle-vmm` | `Vmm` trait + `FirecrackerVmm` (process-group mgmt, HTTP-over-UDS client) |
| `clouisle-store` | `Store` trait + SQLite / InMemory / PostgreSQL |
| `clouisle-scheduler` | Resource admission (Semaphore RAII) + multi-node placement |
| `clouisle-api` | Axum HTTP service (sandbox CRUD / exec / files / health / metrics) |
| `clouisle-proto` | host↔guest vsock frame protocol (length-prefix + postcard) |
| `clouisle-agent` | Guest binary (PID 1 init + serve) |
| `clouislectl` | CLI tool |
| `clouisled` | Node agent (gRPC service + register/heartbeat/reconciler) |
| `clouisle-net` | netns / nftables / DNS allowlist proxy / firewall orchestrator |
| `clouisle-pool` | Snapshot warm pool (FR-08) |
| `clouisle-images` | OCI image pull + volume management |
| `clouisle-audit` | Audit hash chain + Ed25519 signing (SR-05) |
| `clouisle-obs` | Prometheus metrics / tracing logs |
| `benches` | Criterion benchmarks |
| `sdk/rust` | Rust SDK (`clouisle-sdk`) |
| `sdk/python` | Python SDK (`clouisle-sandbox`) |
| `sdk/typescript` | TypeScript/JS SDK (`@clouisle/sdk`)

## Testing

```bash
cargo test --workspace     # full test suite (151+ tests)
cargo bench -p clouisle-bench  # benchmarks (requires Linux + KVM)
```

| Test level | Description | Platform |
|-----------|-------------|----------|
| Unit tests | state machine, scheduling, storage, protocol codec | All |
| Integration (HTTP) | sandbox lifecycle / exec / files / health | All (TestVmm fixture) |
| E2E (Linux+KVM) | real Firecracker microVM create→exec→delete→zero residue | Linux + `/dev/kvm` |

## Configuration

### Server flags

| Flag | Default | Description |
|------|---------|-------------|
| `--addr` | `0.0.0.0:8080` | Listen address |
| `--db` | `clouisle.db` | SQLite path or `postgres://` connection string |

### FirecrackerVmm config

(`crates/clouisle-vmm/src/firecracker.rs`)

| Field | Default | Description |
|-------|---------|-------------|
| `firecracker_bin` | `/usr/local/bin/firecracker` | Firecracker binary path |
| `jailer_bin` | `/usr/local/bin/jailer` | Jailer path (optional) |
| `kernel_path` | `/opt/clouisle/vmlinux` | Guest kernel |
| `use_jailer` | `true` | Use Jailer (recommended for prod) |
| `enable_seccomp` | `true` | Enable seccomp |

## License

MIT