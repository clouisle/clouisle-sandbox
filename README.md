# Clouisle Sandbox

A micro-VM sandbox scheduling system based on Firecracker — high-density, fast-boot, and fully isolated compute environments.

Each sandbox is a real microVM (Firecracker + KVM) with its own kernel and root filesystem. The host communicates with its guest agent over the sandbox's dedicated TCP network channel. It supports command execution, file transfer, multi-tenancy, auditing, and network isolation.

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
                                       │ gRPC
                    ┌──────────────────▼───────────────────────┐
                    │            Data Plane                    │
                    │                                          │
                    │  FirecrackerVmm (Firecracker + KVM)      │
                    │    ├─ Process group mgmt (killpg)        │
                    │    ├─ seccomp / jailer / cgroup v2       │
                    │    └─ guest-agent TCP (host ↔ guest:5201) │
                    │                                          │
                    │  Per-sandbox isolation:                  │
                    │    ├─ netns (clo-<hash>)                 │
                    │    ├─ TAP (10.0.0.2/30) + veth pair      │
                    │    ├─ nftables default drop ingress      │
                    │    └─ DNS-resolved egress allowlist       │
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
Stage 1: rust:1-slim-bookworm → compile Rust binaries
Stage 2: debian:bookworm-slim → install Firecracker + copy binaries
```

**Key configuration**:

| Setting | Description |
|---------|-------------|
| `privileged: true` | Container needs `/dev/kvm` access + netns creation |
| `network_mode: host` | `netns`/`nftables` need the host network stack |
| `/dev/kvm` mount | Required, or firecracker cannot start |
| `vmlinux` / `rootfs` | Must be pre-placed in host `/opt/clouisle/` |
| `CLOUISLE_API_KEYS` | Required by `clouisle-apiserver`; comma-separated `key:tenant:read\|full` entries. Store in a secret, never commit a production key. |

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

### Runtime boundary

Clouisle is a **container-only runtime**. Do not start `clouisle-api`, `clouisled`, `clouislectl`, Firecracker, or the guest agent as host processes. The host supplies Docker, Linux KVM, and mounted guest assets; all Clouisle processes run inside Docker containers or Kubernetes Pods.

### Requirements

| Component | Requirement |
|-----------|-------------|
| Host OS | **Linux** (only supported runtime platform) |
| Container runtime | Docker Engine with Docker Compose v2 |
| Virtualization | `/dev/kvm` available (bare metal or nested virt) |
| Guest assets | Kernel and rootfs/cache mounted beneath `/opt/clouisle/` |

Firecracker and the statically linked guest agent are built into the OCI image. Rust is needed only to modify source or run CI checks, never to operate the runtime.

### CLI (inside Compose)

```bash
# Health check
docker compose exec apiserver clouislectl health

# Create sandbox (1 vCPU / 256 MB)
docker compose exec apiserver clouislectl create --image alpine:latest --vcpu 1 --memory-mb 256

# List sandboxes
docker compose exec apiserver clouislectl list

# Exec command in microVM
docker compose exec apiserver clouislectl exec <sandbox-id> echo hello

# Delete sandbox
docker compose exec apiserver clouislectl delete <sandbox-id>
```

### Direct HTTP API

```bash
# All /api/v1/* endpoints require a Bearer API key. The checked-in Compose
# development value is local-development-key; replace it before deployment.
export CLOUISLE_API_KEY=local-development-key

# Create sandbox
curl -X POST localhost:8080/api/v1/sandboxes \
  -H "Authorization: Bearer $CLOUISLE_API_KEY" \
  -H 'Content-Type: application/json' \
  -d '{"image":{"reference":"alpine"},"resources":{"vcpu":1,"memory_mb":256,"disk_mb":512}}'

# Exec in microVM
curl -X POST localhost:8080/api/v1/sandboxes/<id>/exec \
  -H "Authorization: Bearer $CLOUISLE_API_KEY" \
  -H 'Content-Type: application/json' \
  -d '{"argv":["uname","-a"],"timeout_ms":10000}'

# Delete sandbox
curl -X DELETE localhost:8080/api/v1/sandboxes/<id> \
  -H "Authorization: Bearer $CLOUISLE_API_KEY"

# These operational endpoints are intentionally unauthenticated for probes
# and Prometheus scraping.
curl localhost:8080/health
curl localhost:8080/health/live
curl localhost:8080/health/ready
curl localhost:8080/metrics
```


## API Endpoints

### Authentication and tenant isolation

`CLOUISLE_API_KEYS` is required by the production server. Its format is a comma-separated list of `key:tenant:read|full` entries. Every `/api/v1/*` request needs `Authorization: Bearer <key>`; `read` keys may only read, while `full` keys may create, execute, upload, delete, and update node leases. The authenticated key determines the sandbox tenant, and a different tenant receives `404` for sandbox, execution, and file resources. `/health`, `/health/live`, `/health/ready`, and `/metrics` are deliberately public.

### Complete HTTP reference

This table enumerates every route registered by `clouisle-apiserver`. `{id}` and `{exec_id}` are UUID strings. All `/api/v1/*` responses use JSON unless noted otherwise.

| Method | Path | Scope | Request or query | Success response |
|--------|------|-------|------------------|------------------|
| POST | `/api/v1/sandboxes` | `full` | `CreateSandboxRequest` JSON | `201` + `Sandbox` |
| GET | `/api/v1/sandboxes` | `read`/`full` | `status`, `limit`, `offset` | `200` + `{items: Sandbox[], total: number}` |
| GET | `/api/v1/sandboxes/{id}` | owner | — | `200` + `Sandbox` |
| DELETE | `/api/v1/sandboxes/{id}` | owner + `full` | — | `204` |
| POST | `/api/v1/sandboxes/{id}/exec` | owner + `full` | `ExecRequest` JSON | `200` + `ExecResponse` |
| POST | `/api/v1/sandboxes/{id}/exec/stream` | owner + `full` | `ExecRequest` JSON | `200` + `text/event-stream` (`stdout`, `stderr`, `exit`, `error`) |
| GET | `/api/v1/sandboxes/{id}/exec` | owner | `limit` (default `100`) | `200` + `ExecutionRecord[]` |
| GET | `/api/v1/sandboxes/{id}/exec/{exec_id}` | owner | — | `200` + `ExecutionRecord` |
| POST | `/api/v1/sandboxes/{id}/files/upload` | owner + `full` | required `path` query + raw request bytes (≤50 MiB) | `200` + `{ok: true}` |
| GET | `/api/v1/sandboxes/{id}/files/download` | owner | required `path` query | `200` + raw bytes, `application/octet-stream` |
| GET | `/api/v1/sandboxes/{id}/files/ls` | owner | required `path` query | `200` + `{items: DirEntry[]}` |
| POST | `/api/v1/nodes` | `full` | `RegisteredNode` JSON | `204` |
| GET | `/api/v1/nodes` | `read`/`full` | — | `200` + `RegisteredNode[]` with heartbeats from the last 15 seconds |
| GET | `/health` | public | — | `200` or `503` + `{status, store, version}` |
| GET | `/health/live` | public | — | `200` + `{status: "alive"}` |
| GET | `/health/ready` | public | — | `200` or `503` + `{status: "ready"|"not_ready"}` |
| GET | `/metrics` | public | — | `200` Prometheus text (`text/plain; version=0.0.4`) |

For sandbox listing, `status` accepts `pending`, `starting`, `running`, `stopping`, `stopped`, or `error`; an unknown value returns `400`. `limit` defaults to `100`, `offset` defaults to `0`, and a supplied `limit=0` is coerced to `1`. File paths must be non-empty and must not contain `..` or a platform prefix.

### Request and response models

#### `CreateSandboxRequest`

`POST /api/v1/sandboxes` flattens the following fields at the JSON top level.

| Field | Type | Default | Contract |
|-------|------|---------|----------|
| `image.reference` | string | required | OCI image reference; cannot be blank |
| `image.digest` | string/null | `null` | Optional immutable image digest |
| `resources.vcpu` | integer | `1` | Virtual CPU count, `1..=4` |
| `resources.memory_mb` | integer | `256` | Memory in MiB, `64..=8192` |
| `resources.disk_mb` | integer | `512` | Scratch disk in MiB, at least `64` |
| `resources.bandwidth_mbps` | integer/null | `null` | Egress bandwidth cap in Mbps; when supplied, at least `1` Mbps |
| `resources.iops` | integer/null | `null` | Disk I/O operations per second; when supplied, at least `1` IOPS |
| `resources.pids_max` | integer/null | `512` | Guest cgroup process-count limit |
| `network.enabled` | boolean | `true` | `false` still retains the management agent channel; public egress is denied |
| `network.allow_egress` | string[] | `[]` | DNS domain allowlist; empty denies public egress |
| `mounts` | `{source,target,readonly}`[] | `[]` | Requested host-to-guest mounts |
| `secrets` | `{name,value}`[] | `[]` | Materialized as `/run/secrets/<name>`; names are unique bare filenames and responses redact values |
| `ttl_secs` | integer/null | `null` | Runtime lifetime in seconds; starts only after `Running` |
| `start_timeout_secs` | integer | `10` | Agent-ready deadline in seconds, `1..=300` |
| `env` | object | `{}` | Guest environment variables |
| `node_selector` | object | `{}` | Required node labels when cluster scheduling is enabled |
| `restart_policy` | `never`/`on_failure`/`always` | `never` | Persisted restart policy |
| `tenant_id` | string/null | ignored | Replaced with the tenant from the authenticated key |
| `sync` | boolean | `true` | Accepted for wire compatibility; creation currently waits for guest readiness regardless of its value |

The `Sandbox` returned by create/get/list contains `id`, `spec`, `status`, `created_at`, `updated_at`, `ready_at`, `expires_at`, `vmm_meta`, `terminal_message`, and `node_id`. Timestamps are RFC 3339 UTC strings. `vmm_meta` contains `backend`, optional process `pid`, `api_socket`, `vsock_socket`, numeric `vsock_cid`, `vmm_id`, and `extra`.

#### `ExecRequest`, `ExecResponse`, and execution history

| Field | Type | Default | Contract |
|-------|------|---------|----------|
| `argv` | string[] | required | Non-empty command and argument vector |
| `env` | object | `{}` | Overrides keys from the sandbox environment |
| `cwd` | string/null | `null` | Guest working directory |
| `timeout_ms` | integer | `30000` | Execution timeout in milliseconds; must be at least `1` ms |
| `stream` | boolean | `false` | Accepted for compatibility; select `/exec` or `/exec/stream` to choose response mode |

`ExecResponse` is `{exec_id, exit_code, stdout, stderr, duration_ms, timed_out, stdout_truncated, stderr_truncated}`; `duration_ms` is milliseconds. Output is UTF-8-lossy text and each stream is retained up to 1 MiB; truncation is explicit. `ExecutionRecord` adds `{id, sandbox_id, spec, started_at, finished_at, node_id}`. The streaming endpoint emits SSE but does not create an execution-history record.

#### `RegisteredNode` and file responses

`POST /api/v1/nodes` requires the fields shown below (`labels` may be omitted and defaults to `{}`); `endpoint` must not be empty. `total_memory_mb`, `total_disk_mb`, and `allocated_memory_mb` use MiB; `last_heartbeat_ms` is Unix milliseconds; `total_vcpu`, `allocated_vcpu`, and `running_sandboxes` are counts.

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

`status` is one of `ready`, `unreachable`, `down`, or `draining`. A directory entry is `{name, size, mode, mtime, is_dir}` where `size` is bytes, `mode` is the numeric Unix file mode, and `mtime` is Unix seconds. Downloads include a safe filename in `Content-Disposition`.

### Error Responses

Unified format: `{ "error": { "code": "...", "message": "...", "details": null } }`

| HTTP Status | `code` | Description |
|-------------|--------|-------------|
| 400 | `VALIDATION` | Request validation failed |
| 401 | `UNAUTHENTICATED` | Missing or invalid API key |
| 403 | `FORBIDDEN` | Read-only key attempted a mutation |
| 404 | `NOT_FOUND` | Sandbox, execution, or file resource is not visible to the caller |
| 409 | `INVALID_STATE` | State conflict, such as exec on a non-running sandbox |
| 422 | — | JSON cannot be deserialized into the endpoint request type |
| 429 | `QUOTA_EXCEEDED` | Tenant or sandbox quota exceeded |
| 500 | `INTERNAL`, `VMM`, `IO`, `NETWORK`, `IMAGE`, `TIMEOUT`, or `STORE` | Internal or infrastructure failure |
| 503 | — | `/health` or `/health/ready` reports unavailable storage |

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
  │    │  ├─ private/agent/DNS accept  │
  │    │  ├─ resolved allowlist IPs    │
  │    │  └─ counter drop              │
  │    └─ postrouting: masquerade      │
  └─ 5. host-veth egress guard + DNS proxy (gateway:53) │
```

The host-veth guard blocks direct public-IP egress. The DNS proxy returns answers only for `network.allow_egress` domains and dynamically allows the resolved IPv4 destinations; an empty allowlist denies all public egress.

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
| `clouisle-proto` | host↔guest framed TCP protocol (length-prefix + postcard) |
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