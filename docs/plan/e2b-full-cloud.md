# E2B 完整云控制面兼容设计文档

## Background & Goals

本阶段不再把 E2B 视为只覆盖 Sandbox 生命周期的适配层，而是以官方仓库提交 `cab27aa6fabd53f759189328c4f74df2df1550ad` 为不可变协议基线，覆盖：

- `spec/openapi.yml`：Team、认证凭据、Template/Build、Sandbox、Snapshot、Volume、Node、Admin API；
- `spec/openapi-volumecontent.yml`：Team Volume 内容 API；
- `spec/envd/envd.yaml`：envd REST；
- `spec/envd/filesystem/filesystem.proto`：Filesystem Connect RPC；
- `spec/envd/process/process.proto`：Process Connect RPC。

目标是让官方 Python 同步/异步 SDK 与 TypeScript SDK 在真实 Linux/KVM 环境中完成创建、连接、文件、命令、PTY、暂停/恢复、快照、Fork、网络、Volume、模板和凭据管理闭环，并通过可重复的契约、随机化、故障注入和安全验收。运行时目标包括：通过资源池预置与 snapshot/CoW clone 路径把 warm-start p95 压到 60ms 以内；每个实例使用独立 MicroVM/guest kernel，目标增量开销小于 5MB；默认采用 eBPF/netns 的内核态隔离，L7 proxy 支持域名/path/method 策略与不暴露给 guest 的 credential injection；快照支持高频 checkpoint、回滚和 fork；Volume 通过可插拔 backend trait 跨 Sandbox 共享；CI 和发布产物同时覆盖 amd64 与 arm64。60ms、5MB 和数千实例是必须通过真实硬件 benchmark 证明的目标，不是当前实现的既成事实。

## High-Level Design

### 控制面资源模型

```text
User / Admin identity
        |
        v
Team membership ---- Team API keys / deprecated access tokens
        |
        +---- Templates -> Builds -> OCI digest/rootfs artifacts
        +---- Sandboxes -> runtime/VMM/agent lifecycle
        +---- Snapshots
        +---- Volumes -> volumecontent API -> volumeMounts
        +---- Metrics / audit
```

每个资源都保存 `team_id`。API key 只保存不可逆摘要，原始 key 仅在创建响应中出现一次。现有 `tenant_id` 作为兼容迁移字段保留，但所有 E2B API 通过显式 Team context 做授权。

### 存储

新增控制面 Store 抽象，SQLite/PostgreSQL 使用同一 schema：teams、team_members、api_keys、access_tokens、templates、template_builds、template_tags、snapshots、volumes、volume_files、build_logs、sandbox_logs、team_metrics、audit_events。迁移必须向前兼容现有 sandboxes/executions 表。

### Runtime

Sandbox API 继续复用现有 provision/reconcile/VMM/agent 流程，但创建优先走按 `template digest + resources + architecture` 分桶的 warm pool。可用槽位来自预启动 MicroVM 或 snapshot/CoW clone；clone 必须在资源、网络身份、密钥和 sandbox ID 重新绑定后才对外暴露。快照/Fork 通过 VMM snapshot/restore 能力实现；不支持的底层能力返回官方错误状态，而不是 200 假成功。envd Connect 层使用官方 protobuf 生成的 JSON Connect wire contract，REST 层使用官方 OpenAPI 字段和 status code。

### Isolation and extensibility

- `NetworkBackend` trait 抽象 netns/nftables 与 Linux eBPF backend；策略编译器支持 domain/path/method、allow/deny、egress proxy 和 credential injection，credential 只在代理内存中出现，绝不写入 guest 文件或环境变量。
- `VolumeBackend` trait 抽象本地目录、块设备、对象存储和外部插件；控制面只保存 backend、mount 和 capability metadata，内容 API 通过授权 token 访问。
- `ArchitectureBackend` 明确 amd64/arm64 的 kernel、agent、Firecracker binary、rootfs cache 和 snapshot 兼容性；不同架构禁止复用 snapshot/rootfs。

### Compatibility and security boundaries

- API key、Bearer、X-Team-ID、X-Admin-Token、Sandbox `X-Access-Token` 分层校验；
- 所有资源读取、更新、删除必须做 Team scope check；跨 Team 资源返回 404，避免泄露存在性；
- filesystem 路径在规范化、symlink/hardlink 检查和实际访问之间使用安全 root fd / `openat2`（Linux）或等价受限实现；
- OCI layer 解压拒绝绝对路径、`..`、设备文件和超出配额的展开；
- secret 不进入日志、错误、metrics 或 build output；
- network transform/egress proxy 必须实际实现，不能静默忽略；若底层能力不可用，返回明确的能力错误并由 contract test 固定。

## Pinned Contracts

- E2B OpenAPI: `https://raw.githubusercontent.com/e2b-dev/E2B/cab27aa6fabd53f759189328c4f74df2df1550ad/spec/openapi.yml`
- Volume content OpenAPI: `https://raw.githubusercontent.com/e2b-dev/E2B/cab27aa6fabd53f759189328c4f74df2df1550ad/spec/openapi-volumecontent.yml`
- envd REST: `https://raw.githubusercontent.com/e2b-dev/E2B/cab27aa6fabd53f759189328c4f74df2df1550ad/spec/envd/envd.yaml`
- Filesystem protobuf: `https://raw.githubusercontent.com/e2b-dev/E2B/cab27aa6fabd53f759189328c4f74df2df1550ad/spec/envd/filesystem/filesystem.proto`
- Process protobuf: `https://raw.githubusercontent.com/e2b-dev/E2B/cab27aa6fabd53f759189328c4f74df2df1550ad/spec/envd/process/process.proto`

The repository will store checksums and generated contract snapshots under test fixtures; CI rejects accidental upstream drift.

## Implementation Plan

### Stage 1: Contract and schema foundation

- **Files modified**: `docs/plan/e2b-full-cloud.md`, `crates/clouisle-api/src/e2b_cloud.rs`, `crates/clouisle-api/src/state.rs`, `crates/clouisle-api/src/router.rs`, Store implementations, contract fixtures.
- **Specific logic**: Define exact serde models, pagination headers, error mapping, capability matrix, team context and resource IDs. Add schema migrations and in-memory fixtures with the same behavior.
- **Validation**: Parse pinned specs, verify every path/method is mapped, and run golden serialization tests for every public request/response.

### Stage 2: Identity and Team control plane

- **Files modified**: `crates/clouisle-api/src/auth.rs`, new identity handlers/store modules, SQLite/PostgreSQL migrations.
- **Specific logic**: Team listing, user membership, API key create/list/patch/delete, deprecated access-token create/delete, Admin Team kill/cancel/create-key endpoints, hashed secrets, last-used timestamps, scope and team selection.
- **Validation**: key rotation/revocation, duplicate names, wrong Team header, cross-Team 404, admin authorization, audit/redaction and concurrent updates.

### Stage 3: Template and build control plane

- **Files modified**: image manager, template handlers, build task registry, store migrations, router.
- **Specific logic**: v2/v3 template payloads, build creation/status/logs/cancel, upload URL and content validation, tags, aliases, OCI digest linkage and atomic artifact publication.
- **Validation**: real OCI builds for multiple architectures, failed/cancelled builds, retries, corrupted layers, upload limits, alias races and restart recovery.

### Stage 4: Volume control plane

- **Files modified**: new volume service/handlers, volumecontent handlers, Store implementations, sandbox mount mapping.
- **Specific logic**: Team Volume CRUD, one-time content token, path/dir/file APIs, quotas, volumeMount lifecycle, mount authorization and cleanup.
- **Validation**: persistence across Sandbox deletion/restart, concurrent writes, path escape/symlink attacks, quota exhaustion and token revocation.

### Stage 5: Complete Sandbox platform

- **Files modified**: E2B models/handlers/router, VMM snapshot APIs, metrics/logging, sandbox spec mapping.
- **Specific logic**: official create/list/v2 pagination and headers, logs/metrics, pause/resume/connect/timeout/refreshes, snapshots/list/fork, complete network allow/deny/rules/proxy behavior, node/team metrics and official status codes.
- **Validation**: official SDK lifecycle, pagination, state transitions, fork fanout, snapshot restore, network policy precedence and runtime fault recovery.

### Stage 6: Complete envd protocol

- **Files modified**: `crates/clouisle-agent/src/serve.rs`, `crates/clouisle-api/src/handlers`, `crates/clouisle-proto`, generated contract bindings and VMM/agent connection code.
- **Specific logic**: envd REST `/health`, `/metrics`, `/init`, `/freeze`, `/unfreeze`, `/collapse`, `/fsfreeze`, `/fsthaw`, `/envs`, `/files`, `/files/compose`; Filesystem Stat/MakeDir/Move/ListDir/Remove/WatchDir/Watcher RPCs; Process List/Connect/Start/Update/StreamInput/SendInput/SendSignal/CloseStdin; PTY and keepalive.
- **Validation**: official Connect JSON/binary clients, streaming cancellation, backpressure, stdin ordering, signal delivery, watcher events, metadata and malformed frames.

### Stage 7: SDK interoperability

- **Files modified**: `tests/sdk-python`, `tests/sdk-typescript`, CI workflows and SDK fixtures.
- **Specific logic**: Pin official SDK source/package versions compatible with the contract commit. Configure custom API/sandbox URLs and run sync Python, async Python and TypeScript against the local API plus real envd.
- **Validation**: no SDK-side mocks for acceptance; create → commands → files → PTY → pause/resume → snapshot/Fork → volume → network → kill, including MCP/IAM/secure options.

### Stage 8: Adversarial and security testing

- **Files modified**: Rust property/fuzz tests, integration fixtures, security workflows.
- **Specific logic**: Random command argv/env/cwd, random valid/invalid JSON and Connect frames, real image references and malformed OCI layers, multi-tenant authorization matrix, filesystem and registry/network attacks, resource exhaustion and fault injection.
- **Validation**: deterministic seeds recorded as artifacts; no secret leakage; fuzz corpus minimized and replayed on every CI run.

### Stage 9: CI and release gates

- **Files modified**: `.github/workflows/ci.yml`, new workflows, Cargo manifests, SDK test manifests, Docker scanning configuration.
- **Specific logic**: unit/integration/contract/SDK/KVM lanes, `cargo llvm-cov --fail-under-lines 80`, `cargo audit`/`cargo deny`, image CVE/SBOM scan, static analysis, reproducible contract artifacts.
- **Validation**: a pull request cannot merge with any failed lane, ignored acceptance test, missing fixture checksum or coverage regression.

## Testing Strategy

### Required test classes

1. Golden contract tests for all OpenAPI paths/methods/status/body/header combinations.
2. Official Python synchronous and asynchronous SDK tests.
3. Official TypeScript SDK tests.
4. Property-based command, JSON, path, metadata, network and pagination tests.
5. Real OCI image matrix with digest pinning and multi-arch cases.
6. Real Linux/KVM Firecracker lifecycle tests.
7. Security tests for auth, tenancy, path escape, OCI extraction, SSRF, DoS, secret leakage and network isolation.
8. Restart/fault tests for API, node, VMM, agent, store and registry failure.

### Definition of complete

Completion requires every pinned path to have an implementation or a deliberately specified official `x-not-implemented` behavior with matching response and test. “Pass” from a mock test alone does not count for runtime, SDK or security acceptance.

## Risks & Mitigation

- **Scope drift**: pin the upstream commit and fail contract checks on changes.
- **Cloud-only identity dependencies**: implement local Team/User/Admin providers with equivalent wire semantics and explicit deployment configuration; do not silently downgrade auth.
- **Snapshot portability**: expose only snapshots that can be restored by the selected VMM backend.
- **Volume data loss**: write-ahead metadata and atomic file operations; test restart at every mutation boundary.
- **SDK transport mismatch**: use official generated clients and capture raw request/response artifacts.
- **Security regressions**: adversarial tests are mandatory CI gates, not optional acceptance.

## Rollback Plan

Use additive schema migrations and keep existing `/api/v1` routes. E2B cloud routes are feature-gated only during migration; once contract acceptance passes, enable them by default. A failed migration can disable the new router while preserving existing sandbox records and rootfs caches.
