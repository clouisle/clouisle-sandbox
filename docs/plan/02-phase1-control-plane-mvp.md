# Phase 1：控制平面 MVP 设计文档

**周期**：4-6 周　**前置**：Phase 0 里程碑达成（`MockVmm` 可先行）
**关联 PRD**：FR-01、FR-02、FR-03、FR-04（基础）、FR-10（基础）、AR-02、AR-03

---

## 背景与目标

### 要解决的问题

Phase 0 证明了单个 microVM 的端到端可行性；Phase 1 构建可以**用 API 调用的服务**，具备：
- 完整的沙盒生命周期管理（FR-01）
- 命令执行（FR-02）
- 单机多沙盒并发资源管理（FR-03/FR-04）
- 服务重启后能收敛真实状态（AR-02 的基础）
- Prometheus 指标 + 结构化日志（FR-10 基础）

### 成功标准

- [ ] `POST /api/v1/sandboxes` → `POST .../exec` → `DELETE` 全链路走通
- [ ] 同时创建 20 个沙盒（`--test-threads=4`，Mock VMM 下），每个 exec 独立，无串扰
- [ ] kill apiserver，再起，调用 `GET /sandboxes/{id}` 返回正确状态，不会凭空捏造不存在的沙盒
- [ ] Prometheus `/metrics` 能被 `promtool check metrics` 通过

---

## 高层设计

### crate 职责

| crate | 职责 |
|-------|------|
| `clouisle-core` | 领域类型、状态机、错误类型；**无 I/O**，纯逻辑，全平台可测 |
| `clouisle-store` | `Store` trait + SQLite 实现（`rusqlite`，WAL 模式） |
| `clouisle-scheduler` | 单机资源核算 + 准入控制；依赖 `Store` trait，不依赖具体 VMM |
| `clouisle-api` | Axum HTTP server；依赖 `scheduler` + `vmm` trait |
| `clouisle-vmm` | Phase 0 已有；Phase 1 补全 `MockVmm` |
| `clouislectl` | CLI（`clap`）；thin wrapper over HTTP client |

### 沙盒状态机

```
                 ┌──────────────┐
         create  │   Pending    │ ←── 镜像未就绪（Phase 2 场景）
    ─────────────►              │
                 └──────┬───────┘
                        │ start（or auto-start）
                 ┌──────▼───────┐
      stop/error │   Starting   │
    ◄────────────┤              ├──► Error（timeout, VMM fail）
                 └──────┬───────┘
                        │ agent Hello received
                 ┌──────▼───────┐
  stop/delete    │   Running    ├──► Error（agent 心跳超时）
    ─────────────►              │
                 └──────┬───────┘
                        │ stop API / timeout
                 ┌──────▼───────┐
                 │   Stopping   │
                 └──────┬───────┘
                        │ VMM process exited
                 ┌──────▼───────┐
                 │   Stopped    │──► delete
                 └─────────────-┘
```

所有状态转换在 `clouisle-core/src/sandbox/state.rs` 中实现为 `enum + transition()` 函数，拒绝非法转换并返回 `InvalidTransition` 错误。

---

## 实施计划

### Stage 1.1：领域模型与状态机（`clouisle-core`）

- **文件**：`crates/clouisle-core/src/sandbox/{state.rs, spec.rs, model.rs}`, `execution.rs`, `resources.rs`, `error.rs`
- **具体逻辑**：
  - `SandboxStatus` enum；`SandboxSpec`（image, resources, network_config, timeout, env, mounts）；`Sandbox`（id + spec + status + timestamps + vmm_meta）
  - `Resources`：`vcpu: u16`、`memory_mb: u32`、`disk_mb: u32`、`bandwidth_mbps: Option<u32>`；实现 `Validate` trait（vcpu ≥ 1、memory_mb ≥ 64、disk_mb ≥ 64）
  - `ExecutionSpec`：`argv: Vec<String>`、`env: HashMap`、`cwd: Option<PathBuf>`、`timeout: Duration`；`ExecutionResult`：`exit_code: i32`、`stdout: Bytes`、`stderr: Bytes`、`duration: Duration`
  - 状态机：`transition(&self, event: Event) -> Result<Self, InvalidTransition>`；确保 `Running → Running`（重复 start）返回 `InvalidTransition`
- **验证**：全部状态转换的合法/非法路径均有单测，`cargo test -p clouisle-core`；无 I/O 依赖，macOS 可跑
- **测试用例**：UNIT-001 ~ UNIT-015

### Stage 1.2：状态存储（`clouisle-store`）

- **文件**：`crates/clouisle-store/src/{lib.rs, trait.rs, sqlite.rs, migrations/}`
- **具体逻辑**：
  - `Store` trait：`create_sandbox`、`get_sandbox`、`update_sandbox_status`、`list_sandboxes`、`delete_sandbox`、`save_execution`、`get_execution`；全部 `async`，`Send + Sync`
  - SQLite schema（`migrations/001_init.sql`）：`sandboxes` 表（`id TEXT PK`、`spec_json TEXT`、`status TEXT`、`vmm_meta_json TEXT`、`created_at INTEGER`、`updated_at INTEGER`）；`executions` 表
  - **必须**：WAL 模式（`PRAGMA journal_mode=WAL`）、`PRAGMA synchronous=NORMAL`、`PRAGMA foreign_keys=ON`；连接池用 `deadpool-sqlite` 或 `tokio-rusqlite`（不阻塞 tokio runtime）
  - 迁移：启动时自动运行所有未跑的 migration 文件（按文件名排序）
  - `InMemoryStore` 实现（用 `tokio::sync::RwLock<HashMap>`）：用于单元测试，**不写文件**
- **验证**：集成测试起真实 SQLite，写 100 条记录后重开连接验证持久化；多个 tokio task 并发写，无死锁；SQLite 文件删除后重启能自动重建
- **测试用例**：STORE-001 ~ STORE-008

### Stage 1.3：沙盒生命周期 API（FR-01）

- **文件**：`crates/clouisle-api/src/{router.rs, handlers/sandbox.rs, middleware.rs, error.rs}`
- **具体逻辑**：
  - 框架：`axum` 0.8.x；`tower` middleware 栈：请求 ID 注入、`tracing` span、请求计时（`histogram`）
  - `POST /api/v1/sandboxes`：validate spec → scheduler.admit() → store.create() → vmm.create() + vmm.start() → 等待 agent hello（超时 = `start_timeout`，默认 10s）→ 返回 `201 Created` 含完整 Sandbox JSON；若 spec 非法返回 `400` 含 `errors: [{field, message}]`；若资源不足返回 `507 Insufficient Storage` 含当前可用量
  - `DELETE /api/v1/sandboxes/{id}`：vmm.stop(force=true) → store.update → 返回 `204`；若 id 不存在返回 `404`
  - 错误格式统一：`{"error": {"code": "RESOURCE_EXHAUSTED", "message": "...", "details": {...}}}`
  - 请求 ID：从 `X-Request-Id` header 读取，或自动生成 UUID v7，贯穿 tracing span 和响应 header
- **验证**：
  - 正向：创建后 GET 能拿到 status=running；DELETE 后再 GET 返回 404
  - 负向：`vcpu: 0` → 400；spec JSON 缺 image → 400 含 field 名；id 不存在 → 404
  - 并发：100 goroutine 同时 POST，`MockVmm` 下每个都成功（无竞争条件）
- **测试用例**：API-001 ~ API-012

### Stage 1.4：命令执行 API（FR-02）

- **文件**：`crates/clouisle-api/src/handlers/exec.rs`、`crates/clouisle-vmm/src/vsock.rs`（复用）
- **具体逻辑**：
  - `POST /api/v1/sandboxes/{id}/exec`：同步模式（命令跑完才返回），body 含 argv/env/cwd/timeout
  - 流式模式：SSE（`text/event-stream`）响应，事件类型 `stdout`、`stderr`、`exit`；用 `axum::response::Sse`
  - 超时：先 exec timeout（guest 内 SIGKILL），再 HTTP response timeout；两个超时独立，前者 ≤ 后者
  - `GET /api/v1/sandboxes/{id}/exec/{exec_id}`：查历史执行记录（从 store）；stdout/stderr 截断到 1MB，超出在响应中标记 `truncated: true`
  - 沙盒状态检查：exec 前校验 status == Running，否则 `409 Conflict`
- **验证**：
  - `echo hello` → stdout 含 `hello`，exit_code 0
  - `sleep 100 &timeout=1s` → 返回超时错误，exec_id 标记 `timeout: true`
  - 沙盒 Stopped 时 exec → 409
  - 流式：用 curl --no-buffer 验证 chunk 逐行到达
- **测试用例**：EXEC-001 ~ EXEC-012

### Stage 1.5：单机资源核算调度器（`clouisle-scheduler`）

- **文件**：`crates/clouisle-scheduler/src/{lib.rs, admission.rs, accounting.rs}`
- **具体逻辑**：
  - 启动时从 `/proc/cpuinfo` 读物理核数，从 `/proc/meminfo` 读 MemAvailable，从 cgroup `cpu.max` 读当前容器限制（若在容器内跑）——**三选最小**作为可用资源上限
  - `admit(spec: &SandboxSpec) -> Result<Reservation>`：原子性地检查并预留资源（`tokio::sync::Mutex<ResourcePool>`）；成功返回 `Reservation`（RAII，drop 时自动释放）
  - 资源池初始化时从 store 读取所有 Running 状态的沙盒，恢复预留（**重启恢复**）
  - 强制上限：单机最大 200 个沙盒（可配置）；单沙盒最大 4 vCPU / 8 GB
- **验证**：资源池刚好满时第 N+1 个 admit 返回 `ResourceExhausted`；drop Reservation 后资源立即可用；并发 100 个 admit（Mock），无超额分配
- **测试用例**：SCHED-001 ~ SCHED-008

### Stage 1.6：cgroup v2 + Jailer 资源限制（FR-04）

- **文件**：`crates/clouisle-vmm/src/firecracker/cgroup.rs`
- **具体逻辑**：
  - Jailer 参数构造：`--cgroup cpu.max="{quota} {period}"` 其中 quota = `vcpu * period * 0.9`（留 10% 给 VMM 本身），period = 100000（100ms）
  - 内存：`--cgroup memory.max={memory_mb * 1024 * 1024}`、`--cgroup memory.swap.max=0`（禁止 swap，PRD SR-04 的一部分）
  - pids：`--cgroup pids.max=512`（防止 fork bomb）
  - IO：通过 Firecracker 的 rate_limiter（`PATCH /drives/{id}`）设置 IOPS 和带宽上限，cgroup 侧用 `io.max`（需要知道 `/dev/vd*` 的 major:minor）
  - 验证方法：启动后 `cat /sys/fs/cgroup/<jail-dir>/cpu.max` 与期望值对比
- **验证**：创建 1 vCPU 沙盒，guest 内跑 `stress-ng --cpu 4`，宿主机观察到 cpu 占用 ≤ 100%（即 1 核）；内存超限时 guest OOM 而非宿主机 OOM
- **测试用例**：RES-001 ~ RES-006

### Stage 1.7：可观测性基础（FR-10 部分）

- **文件**：`crates/clouisle-api/src/metrics.rs`、`crates/clouisle-core/src/telemetry.rs`
- **具体逻辑**：
  - 指标（`metrics` crate + `metrics-exporter-prometheus`）：
    - `sandbox_total`（gauge，by status）
    - `sandbox_create_duration_seconds`（histogram，buckets: .05 .1 .2 .5 1 2）
    - `sandbox_exec_duration_seconds`（histogram）
    - `sandbox_resource_vcpu_reserved`、`_memory_mb_reserved`（gauge）
    - `api_requests_total`（counter, by method + path + status）
    - `api_request_duration_seconds`（histogram）
  - 日志：`tracing-subscriber`，JSON 格式（`tracing-subscriber::fmt::json()`），含 `request_id`、`sandbox_id` 字段；级别可通过 `RUST_LOG` 或 `CLOUISLE_LOG` 控制
  - **`GET /api/v1/health`**：返回 `{"status": "ok", "store": "ok", "version": "..."}` 或 503
  - **`GET /metrics`**：Prometheus text format，`Content-Type: text/plain; version=0.0.4`
- **验证**：`promtool check metrics <(curl -s localhost:8080/metrics)` 通过；`RUST_LOG=debug` 下日志含 request_id 且格式合法 JSON；健康检查在 store 不可写时返回 503
- **测试用例**：OBS-001 ~ OBS-005

### Stage 1.8：孤儿进程回收与崩溃恢复（Reconciler）

- **文件**：`crates/clouisle-vmm/src/reconciler.rs`
- **具体逻辑**：
  - 启动时扫描 store 中所有 `status IN (Starting, Running, Stopping)` 的沙盒：
    - 检查 `vmm_meta.pid` 是否存在（`kill(pid, 0)` + 检查 `/proc/<pid>/cmdline` 含 `firecracker`）
    - 若进程已死：更新 status = `Error`，清理资源，记录 audit log
    - 若进程存活：尝试重建 vsock 连接验证 agent 健康；若 agent 无响应且超过 `recovery_timeout`（默认 30s），强制停止
  - 运行时 heartbeat：每 30s 向每个 Running 沙盒的 agent 发 Ping；连续 3 次失败 → 状态转 Error + 清理
  - 设计原则：Reconciler 的每个操作必须是**幂等的**（多次跑结果相同）
- **验证**：手动 kill firecracker 进程，5 秒内 GET 沙盒返回 status=error；重启 apiserver，store 里是 Running 但进程实际已死的记录，5 秒内被修正
- **测试用例**：CHAOS-001 ~ CHAOS-005、RECOVER-001 ~ RECOVER-004

---

## 测试策略

**Happy path**：
- 创建 → exec → 查询历史执行 → 停止 → 删除
- 同时创建 10 个沙盒（MockVmm），并发 exec，各自输出不串流
- 超出资源限制时收到 507，池释放后再创建成功

**Error path**：
- 非法 spec（缺字段、超出范围）
- 对 Stopped 沙盒 exec
- 对不存在的 id 操作
- exec 超时

**回归范围**：Phase 1 新增 API，Phase 0 的 GUEST-*、EXEC-* 用例需在真实 VMM 下回归。

---

## 风险与缓解

| 风险 | 缓解 |
|------|------|
| SQLite 并发写性能不足（>200 req/s 写操作） | WAL 模式；写操作批量提交（每 5ms 一批）；Phase 3 切 Postgres |
| Reconciler 与正常流程产生竞争（同时操作同一沙盒） | 所有沙盒级操作通过 per-sandbox `Mutex` 串行；Reconciler 也持同一把锁 |
| Agent 心跳超时误杀正常沙盒（宿主机高负载） | 超时阈值可配置；连续 3 次 miss 才触发，非单次 |
