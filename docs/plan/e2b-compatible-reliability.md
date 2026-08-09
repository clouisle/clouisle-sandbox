# E2B 兼容与运行可靠性改造设计文档

## Background & Goals

当前控制面、节点代理、Firecracker、OCI 镜像和持久化状态已经具备最小闭环，但创建路径仍把镜像拉取、rootfs 构建、VMM 启动和 guest 就绪全部放在 HTTP 请求内；`sync` 字段没有改变行为，节点重启后的 VMM/数据库状态也没有统一收敛。部署默认拆成 apiserver、clouisled、PostgreSQL 三套对象，单节点场景运维成本过高；HTTP 路由只覆盖自定义 `/api/v1` 合约，不足以直接使用 E2B SDK。

本改造的目标是一次性完成以下可验收结果：

1. 镜像未缓存时创建请求立即得到可轮询的 `starting`/`pending` 资源，不再被镜像操作占满 300 秒；镜像拉取、层展开、rootfs 构建有超时、失败状态和可重试结果。
2. 提供带任务状态的异步镜像预拉取接口，并把 digest、agent 版本和 rootfs 作为节点本地持久缓存；同一镜像并发只执行一次构建。
3. 默认部署收敛为一个同时拥有 HTTP API 和 Firecracker 的 all-in-one 服务；多节点 gRPC 模式保留为显式扩展，而不是单节点默认依赖。
4. 为状态机、镜像、异步创建、初始化命令、E2B 生命周期、节点/服务重启和故障恢复补齐单元、集成、协议和端到端测试，关键业务逻辑覆盖率目标不低于 80%。
5. 创建请求支持一次性初始化命令；命令在 agent 握手和 secret 注入之后、对外报告 `running` 之前执行，失败会回滚运行时并持久化原因。
6. 提供与 E2B public sandbox API、envd filesystem/process Connect API 兼容的路由、字段、状态码、SSE/Connect JSON 行为，并把 E2B 字段映射到 Clouisle 的资源模型。
7. 沙盒运行时故障可通过自动重启策略和显式恢复接口恢复；无法恢复的实例必须进入可解释的 `error` 终态并释放资源。
8. apiserver、clouisled 和 PostgreSQL 使用同一套持久状态协议；启动扫描、节点心跳和周期 reconciler 自动把历史记录收敛到真实运行态。

## High-Level Design

### 组件与依赖

- `clouisle-core`：扩展 `SandboxSpec`（初始化命令、E2B 生命周期元数据）和状态转换；所有输入在这里完成类型化校验。
- `clouisle-images`：实现带磁盘索引的 OCI digest/rootfs 缓存、per-key single-flight、可取消/可重试的预拉取。
- `clouisle-vmm`：在 `Vmm` trait 上提供镜像就绪检查、预拉取和运行时探测；Firecracker 实现使用持久化 PID/socket 进行重启接管。
- `clouisle-store`：SQLite/PostgreSQL 对齐同一 schema 和迁移；新增状态原因、节点归属、镜像任务和状态收敛所需的更新操作。
- `clouisle-api`：抽出统一的 `provision_sandbox` 服务流程；HTTP 创建只负责 admission 和调度，后台任务负责镜像/VMM/agent；挂载自定义 API 与 E2B 兼容路由，共享鉴权、错误和审计。
- `clouisled`：节点服务使用同一 `Store` trait（生产可直接连接 PostgreSQL），启动时探测并接管存活 Firecracker，心跳携带完整运行实例集合；保留 gRPC 供多节点模式使用。
- `deploy`/`docker-compose.yml`：默认 all-in-one API+Firecracker Pod/容器；PostgreSQL 是唯一外部状态依赖；多节点清单单独标为可选 profile。

### 创建数据流

1. API 验证 E2B 或 Clouisle 请求，写入 `starting` 记录并保留资源 reservation。
2. 检查节点本地 rootfs cache。命中时可在同步窗口内完成启动；未命中时立即创建后台 job，返回 `202` 和可轮询资源。
3. job 执行 `prefetch/pull_and_build → VMM create/start → agent hello → secret materialize → init_command`。每一步写状态、原因、时间和 metrics；成功后原子收敛到 `running`。
4. 失败路径停止 VMM、清理网络、释放 reservation，并写 `error`；对 `on_failure/always` 进入受限次数的恢复队列。
5. 启动扫描和每次 node heartbeat 都通过 `probe_runtime + agent ping` 校正持久状态；节点租约失效时不再把实例伪装为可执行。

### 兼容性边界

E2B compatibility layer 以 E2B public OpenAPI 的 sandbox platform endpoints（create/list/get/connect/delete/pause/resume/timeout/refresh/network）和 envd 的 Filesystem/Process Connect endpoints 为协议目标。`templateID` 映射到 OCI image reference；E2B metadata/envVars/network/timeout 映射到现有 `SandboxSpec`。无需引入 E2B 云端域名或控制面依赖。高级模板构建、团队、计费和云卷管理不属于本地 Firecracker runtime protocol，不伪造云端资源；本地相应能力通过镜像预拉取、sandbox snapshot 和 mount 语义提供。

## Implementation Plan

### Stage 1: 状态与持久化契约

- **Files modified**: `crates/clouisle-core/src/sandbox/spec.rs`, `crates/clouisle-core/src/sandbox/model.rs`, `crates/clouisle-core/src/sandbox/state.rs`, `crates/clouisle-store/src/store_trait.rs`, `crates/clouisle-store/src/sqlite.rs`, `crates/clouisle-store/src/postgres.rs`。
- **Specific logic**: 增加 `init_command`/初始化超时、E2B metadata/lifecycle 字段；增加可恢复状态和 terminal message 的明确转换；为 Store 增加更新状态原因、node ownership、expiry、镜像任务/幂等查询所需操作；SQLite/PostgreSQL schema 与序列化字段保持同构并兼容旧记录。
- **Validation**: 先写并运行状态转换、旧 JSON 反序列化、两种 Store round-trip 和并发幂等测试；非法初始化命令、超时和不合法生命周期必须返回 4xx/typed error。

### Stage 2: OCI 缓存与异步镜像任务

- **Files modified**: `crates/clouisle-images/src/builder.rs`, `crates/clouisle-vmm/src/lib.rs`, `crates/clouisle-vmm/src/firecracker.rs`, `crates/clouisle-api/src/state.rs`, `crates/clouisle-api/src/handlers/images.rs`, `crates/clouisle-api/src/router.rs`。
- **Specific logic**: 为 image manager 增加 reference→digest 磁盘索引、agent fingerprint 参与的原子 rootfs 文件、per-key single-flight 和失败重试；VMM 暴露 `image_cache_hit/prefetch_image/probe`；新增 `POST /api/v1/images/prefetch` 和 `GET /api/v1/images/prefetch/{job_id}`，job 状态为 queued/running/succeeded/failed。
- **Validation**: 使用 fake registry/client 验证 layer 只拉一次、重启后命中磁盘缓存、失败可重试、不同 agent digest 不复用旧 rootfs；API 测试确认预拉取立即返回 202 并可观察最终状态。

### Stage 3: 非阻塞创建与初始化命令

- **Files modified**: `crates/clouisle-api/src/handlers/sandbox.rs`, `crates/clouisle-api/src/state.rs`, `crates/clouisled/src/agent.rs`, `crates/clouisle-agent/src/serve.rs`, SDK request types and lifecycle tests。
- **Specific logic**: 抽出单一 `provision_sandbox` 流程；缓存 miss 或 `sync=false` 时 HTTP 只做 admission/store 后 `spawn` job，返回 `202 + Retry-After + Location`；缓存 hit 的同步创建保持 `201`。secret 注入后执行 `init_command`，继承 sandbox env、cwd 和受控 timeout；失败统一 stop/network cleanup/store error/release reservation。
- **Validation**: fake VMM/agent 测试覆盖 cache hit 201、cache miss 202、job 失败、重复 job、初始化成功/非零退出/超时；压力测试证明请求线程不等待 registry。

### Stage 4: API 与 Node 部署收敛

- **Files modified**: `crates/clouisle-api/src/main.rs`, `crates/clouisled/src/main.rs`, `crates/clouisled/src/server.rs`, `docker-compose.yml`, `deploy/01-apiserver.yaml`, `deploy/02-daemonset.yaml`, `Dockerfile`。
- **Specific logic**: all-in-one 模式由 API 进程直接拥有 Firecracker、镜像 cache 和网络权限；默认 Compose/Kubernetes 只部署 API+PostgreSQL，Node DaemonSet 作为显式 multi-node profile；Node 仍可使用同一 Store DSN，避免节点本地 SQLite 与控制面分裂；提供统一 health/readiness、优雅关闭和 prefetch 配置。
- **Validation**: `docker compose config`、Kubernetes schema/render、单容器启动 smoke、API→Firecracker 本地生命周期和可选 gRPC 节点模式回归；校验权限、挂载和探针。

### Stage 5: E2B platform 与 envd 协议兼容

- **Files modified**: `crates/clouisle-api/src/e2b.rs`, `crates/clouisle-api/src/handlers/e2b.rs`, `crates/clouisle-api/src/handlers/files.rs`, `crates/clouisle-api/src/handlers/exec.rs`, `crates/clouisle-api/src/router.rs`, `README.zh-CN.md`, `README.md`。
- **Specific logic**: 注册 E2B `/sandboxes`、`/v2/sandboxes`、connect/delete/pause/resume/timeout/refresh/network、`/files` 和 `/filesystem.Filesystem/*`、`/process.Process/*` 路由；实现 `X-API-Key`、`X-Access-Token`、`E2b-Sandbox-Id`/port 和 Connect JSON/SSE 内容协商；统一 E2B camelCase schema、HTTP code 和 error body；E2B `cmd`、`envs`、`cwd` 映射到 exec/init；缺失模板按 OCI reference 明确返回 validation error，不返回假成功。
- **Validation**: contract tests compare method/path/status/header/body against pinned E2B OpenAPI examples; Python/TypeScript E2B SDK smoke covers create→connect→exec stream→files→pause/resume→kill。

### Stage 6: 故障恢复与自动状态收敛

- **Files modified**: `crates/clouisle-vmm/src/firecracker.rs`, `crates/clouisle-api/src/main.rs`, `crates/clouisled/src/agent.rs`, `crates/clouisled/src/server.rs`, `crates/clouisled/proto/node.proto`, store implementations。
- **Specific logic**: Firecracker probe persisted PID/socket/API；NodeAgent startup adoption 只接管真实存活实例，孤儿进程清理并记录原因；reconciler 周期执行 DB↔runtime↔agent 三方比对；`restart_policy` 限制重启次数和退避，显式 `POST .../recover` 触发恢复，达到上限进入 error；node heartbeat 携带 running IDs，租约过期或缺失实例自动更新历史状态并释放资源。
- **Validation**: 注入 VMM crash、agent hello timeout、API 重启、Node 重启、PostgreSQL 重连、孤儿 runtime 和重复 heartbeat；确认 Running/Paused/Error/Stopped 最终状态、资源计数、网络清理和重试上限。

### Stage 7: 覆盖率与发布验收

- **Files modified**: `crates/*/src/**`, `crates/clouisle-api/tests/**`, `sdk/python/tests/**`, `sdk/typescript/**`, `.github/workflows/**`, `docs/IMPLEMENTATION_PLAN.md`。
- **Specific logic**: 补齐测试 fixtures、fake registry、fake VMM/agent、协议 golden cases、重启恢复测试和真实 Linux/KVM acceptance；CI 分层运行 unit/integration/contract/e2e，并设置关键业务 lines/branches/functions 80% 门槛。
- **Validation**: RED→GREEN TDD checkpoints；最后执行 fmt/check/clippy/test/coverage、Compose/K8s render、Docker smoke、E2B client matrix 和安全扫描，输出逐项证据。

## Testing Strategy

### Happy paths

- 缓存命中同步创建、缓存 miss 异步创建、预拉取后快速同步创建。
- 初始化命令成功后才进入 running；exec、stream exec、文件读写、目录列表和执行历史完整闭环。
- E2B create/list/get/connect/pause/resume/timeout/refresh/delete 以及 envd files/process Connect JSON。
- API+Node 单体部署和多节点调度部署都能完成同一 sandbox 生命周期。

### Error paths and boundaries

- OCI 不存在、鉴权失败、digest 不匹配、registry 超时、层损坏、rootfs 构建失败、缓存文件损坏。
- 资源不足、空 argv、非法路径、初始化命令非零/超时、agent 不响应、VMM socket/PID 消失、node lease 过期。
- 重复请求、重复 heartbeat、重复 prefetch、服务中断后重试、旧数据库 schema 和未知状态字段。
- 输出上限、超时下进程组清理、secret 脱敏、跨租户 404 和 E2B/自定义协议错误码映射。

### Regression scope

现有 Rust workspace tests、Python/TypeScript SDK tests、认证/文件安全测试、OCI multi-arch ignored acceptance、Docker/K8s manifest checks 均必须保持通过；Linux/KVM-only cases 在无 KVM 环境仅作为明确标记的 acceptance，不得以 mock 结果冒充生产验证。

## Risks & Mitigation

- **镜像下载仍可能很慢**：所有未缓存创建默认走后台任务；任务状态持久化，前台只返回 admission；registry 使用 deadline、重试和 single-flight。
- **状态误判导致运行实例被杀**：先 probe PID、socket、VMM API 和 agent，再执行状态转换；设置 grace window，保留审计原因。
- **多进程→单体迁移风险**：保留 `clouisled`/gRPC profile，默认部署只切换拓扑；可通过旧清单回滚，不改变核心 HTTP 数据模型。
- **E2B 协议持续演进**：锁定仓库内的 OpenAPI fixture 和兼容版本，新增字段只向前兼容；不对模板/计费等云端控制面伪造本地实现。
- **恢复重复启动**：基于 sandbox ID 的状态/锁和运行时探测做幂等；每次重试最多一次 active runtime，失败释放 reservation。
- **数据库连接断开**：所有状态更新显式重试并记录；readiness 反映 Store 状态，重连后再次执行 reconciler。

## Rollback Plan

1. 保留 `/api/v1` 旧路由和现有数据库字段读取能力，所有新字段有 serde 默认值。
2. 通过配置关闭异步创建、恢复策略和 E2B 路由，不删除旧缓存文件或旧部署对象。
3. 单体部署失败时回退到当前 API Deployment + DaemonSet 清单；数据迁移使用向前兼容的 `ALTER TABLE`，不做破坏性删除。
