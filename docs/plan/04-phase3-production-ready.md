# Phase 3：生产就绪 设计文档

**周期**：4-6 周　**前置**：Phase 2 里程碑达成
**关联 PRD**：FR-10（完整）、FR-11、FR-12、SR-03、SR-05、SR-06、SR-07、AR-01 ~ AR-04、ER-01 ~ ER-03

---

## 背景与目标

Phase 2 结束时系统在单机可用。Phase 3 解决三件事：**多机调度**、**控制平面高可用**、**可审计的安全加固**。

### 成功标准

- [ ] 3 台宿主机组成集群，1000 个沙盒按 least-loaded 策略分布，无单点
- [ ] 杀死任一 apiserver 实例，进行中的 API 请求失败率 < 0.1%，新请求正常
- [ ] 杀死 `clouisled`，其上沙盒被标记 `unknown` 并在 30s 内 reconcile
- [ ] 审计日志哈希链可离线验证，篡改任一条被检出
- [ ] 单机 100 并发沙盒下 P95 冷启动仍 < 200 ms

---

## 高层设计：单机 → 多机

```
Phase 1/2（单进程）              Phase 3（控制平面 + 节点代理）
┌──────────────┐                ┌─────────────────────────────┐
│ clouisle-    │                │ clouisle-apiserver × N      │  无状态，LB 后
│  apiserver   │                │  (API + Scheduler)          │
│  ├ API       │      ═══>      └──────────┬──────────────────┘
│  ├ Scheduler │                    PostgreSQL（唯一状态源）
│  ├ Pool      │                           │ gRPC (mTLS)
│  └ Vmm(local)│                ┌──────────┴──────────────────┐
└──────────────┘                │ clouisled × M （每宿主机）   │
                                │  ├ Vmm runtime              │
                                │  ├ Pool manager (本机)      │
                                │  ├ Net manager (本机)       │
                                │  └ eBPF collector           │
                                └─────────────────────────────┘
```

**关键约束**：`clouisled` 是本机沙盒的**唯一权威**。apiserver 不直接操作 VMM，只下发意图（desired state），由 `clouisled` reconcile。这样 apiserver 才能真正无状态、可水平扩展（ER-01）。

---

## 实施计划

### Stage 3.1：抽出 `clouisled` 节点代理（FR-11 前置）

- **文件**：新增 crate `crates/clouisled/src/{main.rs, server.rs, reconciler.rs, registry.rs}`；`crates/clouisle-proto/proto/node.proto`
- **具体逻辑**：
  - `node.proto` 定义：`Register(NodeInfo) -> NodeId`、`Heartbeat(stream NodeStatus) -> stream Command`（双向流）、`CreateSandbox(SandboxSpec) -> SandboxHandle`、`DeleteSandbox(id)`、`Exec(stream) -> stream`（代理转发到 guest vsock）
  - `clouisled` 启动时向任一 apiserver 注册：上报 `{hostname, total_vcpu, total_memory_mb, total_disk_mb, kvm_available, kernel_version, firecracker_version, labels}`；获得 `node_id` 持久化到本地 `/var/lib/clouisle/node_id`（重启后复用）
  - Heartbeat：每 3 s 上报 `{allocated_vcpu, allocated_memory, running_sandboxes[], pool_ready_counts, load_avg}`；apiserver 15 s 未收到 → 标记 node `unreachable`，其沙盒标记 `unknown`
  - Reconciler：每 10 s 对比「DB 中属于本节点的沙盒」与「本机实际 Firecracker 进程」，处理三种漂移：DB 有进程无（重建或标记 error）、进程有 DB 无（孤儿，杀掉）、状态不符（以本机实际为准，回写 DB）
  - Exec 转发：apiserver 收到 `POST /exec` → 查 DB 得知沙盒在哪个 node → 通过该 node 的 gRPC 流转发 → `clouisled` 再转到 guest vsock。**双跳延迟预算**：apiserver→clouisled 1-3 ms，clouisled→guest 2-5 ms，仍在 100 ms SLO 内
- **验证**：`clouisled` 重启后重新注册并保留 node_id；手动 `kill -9` 一个 Firecracker 进程，10 s 内 DB 状态变为 `error`；手动起一个野 Firecracker 进程（带 clouisle 命名规则），被 reconciler 杀掉
- **测试用例**：NODE-001 ~ NODE-012、CHAOS-003 ~ CHAOS-006

### Stage 3.2：PostgreSQL 后端 + 分布式调度（FR-11、ER-01）

- **文件**：`crates/clouisle-store/src/postgres.rs`、`crates/clouisle-scheduler/src/{placement.rs, filters.rs, scorers.rs}`
- **具体逻辑**：
  - Postgres 实现 `Store` trait（ADR-005 已定义），schema 与 SQLite 共享（用 `sqlx` 的 `AnyConnection` 或分别写 query，倾向后者以用上 Postgres 特性）
  - 调度改为两阶段（借鉴 K8s，但简化）：
    - **Filter**：排除 `unreachable` 节点、资源不足节点、labels 不匹配节点（`node_selector`）、反亲和冲突节点
    - **Score**：`least_loaded`（默认，`1 - allocated/total` 加权 CPU:mem = 1:1）、`best_fit`（装箱，提高密度）、`spread`（跨节点分散）；策略可配（ER-03）
  - **乐观并发**：`UPDATE nodes SET allocated_vcpu = allocated_vcpu + $1 WHERE id = $2 AND allocated_vcpu + $1 <= total_vcpu RETURNING *`；返回 0 行 → 该节点已满 → 换下一个候选。避免锁竞争，天然支持多 apiserver 并发调度
  - Warm pool 变为**分层**：apiserver 维护「每节点每 bucket 的期望池大小」，`clouisled` 负责本机实际预热；apiserver 按全局需求预测调整各节点配额
  - 连接池：`sqlx::PgPool` max 32/实例；准备语句缓存；`pgbouncer` 可选（transaction 模式）
- **验证**：起 3 个 apiserver 实例，并发提交 200 个创建请求，无超卖（`SELECT SUM(allocated) <= total`），无死锁；杀掉 1 个 apiserver，剩余 2 个继续正常服务
- **测试用例**：SCHED-001 ~ SCHED-014、HA-001 ~ HA-008

### Stage 3.3：可观测性完整实现（FR-10）

- **文件**：`crates/clouisle-obs/src/{metrics.rs, tracing.rs, logs.rs}`
- **具体逻辑**：
  - **Metrics**（Prometheus，`/api/v1/metrics`）：
    - `clouisle_sandbox_boot_duration_seconds`（histogram，label: `mode=cold|warm|pool`）— 直接对应 SLO 表
    - `clouisle_sandboxes_total{state}`、`clouisle_pool_ready{bucket}`、`clouisle_pool_miss_total`
    - `clouisle_node_allocated_vcpu{node}`、`clouisle_node_allocated_memory_bytes{node}`
    - `clouisle_exec_duration_seconds`、`clouisle_api_request_duration_seconds{method,path,status}`
    - 从 Firecracker metrics FIFO 抓 per-VM 指标（block/net/vcpu），转成 `clouisle_vm_*{sandbox_id}`；**注意 cardinality**：sandbox_id label 在高流水场景会爆，用 recording rule 聚合 + 只对长生命周期沙盒打 label
  - **Tracing**（OpenTelemetry，`tracing` + `tracing-opentelemetry`）：trace 贯穿 `API → scheduler → clouisled → vmm → vsock → guest agent`；guest agent 通过 vsock 帧携带 traceparent，实现跨 VM 边界的链路
  - **Logs**：结构化 JSON（`tracing-subscriber` json layer）；guest stdout/stderr 从 Firecracker 串口 + vsock exec 流两路采集；输出到 stdout（由 Loki/Vector 采集）+ 可选文件
- **验证**：Grafana 面板显示启动延迟直方图；一次创建+exec 请求在 Jaeger 中呈现完整 span 树（含 guest 内 span）
- **测试用例**：OBS-001 ~ OBS-010

### Stage 3.4：审计日志与哈希链签名（SR-05，ADR-003 落地）

- **文件**：`crates/clouisle-audit/src/{chain.rs, signer.rs, sink.rs, verify.rs}`、`crates/clouisle-ebpf/`（aya）
- **具体逻辑**：
  - **哈希链**：每条 `AuditRecord` 含 `{seq, prev_hash, timestamp, node_id, sandbox_id, source: Host|Guest, event, payload}`；`hash = SHA256(seq || prev_hash || canonical_json(record))`；每 N 条（或每 5 s）对当前 head hash 做 Ed25519 签名，形成 checkpoint
  - **签名密钥**：Phase 3 用文件系统密钥（0400，root only）+ 明确 TODO 迁移到 TPM/KMS；签名操作在 `clouisled` 内（不出网），checkpoint 上报 apiserver 存 DB
  - **eBPF 采集（宿主机侧，ADR-003 的 Tier 1）**：
    - `tracepoint/syscalls/*` 挂在 Firecracker PID 上 → VMM 的 syscall 序列（检测 jailer/seccomp 之外的异常调用）
    - `kprobe/tun_net_xmit` + netns cookie → per-sandbox 网络流量五元组
    - `cgroup/skb` 程序 → 出站连接记录（与 nftables 白名单交叉验证）
    - `tracepoint/kvm/kvm_exit` → 异常 exit reason 统计（VMM 逃逸尝试的信号）
  - **Guest 侧（Tier 2，标 `trust: advisory`）**：guest agent 记录每次 exec 的 argv/env/cwd/exit_code；**不**做 guest 内 eBPF（Phase 4 可选）
  - **离线校验工具**：`clouislectl audit verify --from <seq> --to <seq> --pubkey <path>`，重算哈希链并验签，输出第一处不匹配的 seq
- **验证**：手动 UPDATE 数据库中一条审计记录的 payload → `audit verify` 报出该 seq 断链；eBPF 采到的出站 IP 集合 ⊆ nftables 白名单集合
- **测试用例**：AUDIT-001 ~ AUDIT-012、SEC-012 ~ SEC-014

### Stage 3.5：安全加固（SR-03、SR-06、SR-07）

- **文件**：`crates/clouisle-vmm/src/firecracker/jailer.rs`（Phase 0 已有，此处强化）、`crates/clouisle-images/src/scan.rs`、`crates/clouisle-api/src/auth.rs`
- **具体逻辑**：
  - **Jailer 完整启用**（SR-03）：每个 VM 独立 `--uid/--gid`（从 uid 池分配，避免复用）、`--chroot-base-dir`、`--cgroup-version 2` + cgroup 限制、`--new-pid-ns`、`--netns <path>`；chroot 内只 bind mount 必需文件（kernel、drives、vsock uds）；Firecracker 默认 seccomp filter 保持启用（**禁止** `--no-seccomp`）
  - **API 认证**（PRD 未明确要求但生产必需，主动补）：API key（`Authorization: Bearer <key>`）+ 可选 mTLS；key 存 DB（argon2 哈希），带 scope（`sandbox:create`、`sandbox:exec`、`admin`）；所有请求的 actor 写入审计
  - **镜像扫描**（SR-07）：集成 `trivy`（子进程调用，CVE DB 本地缓存）；策略可配：`block_on: [CRITICAL]` / `warn_only` / `disabled`；扫描结果缓存（按 digest）
  - **密钥注入**（SR-06）：`secrets: [{name, value}]` 通过 vsock 传给 guest agent，agent 写入 tmpfs（`/run/secrets/<name>`，0400），**不落 overlay 磁盘、不进环境变量、不进审计 payload**（审计只记 name）
  - **网络默认拒绝已在 Phase 2 落地**（SR-04）
- **验证**：`ps aux` 确认 Firecracker 进程非 root；无 API key 请求返回 401；含 CRITICAL CVE 的镜像被拒；`secrets` 值在审计日志中显示为 `[REDACTED]`
- **测试用例**：SEC-001 ~ SEC-004、SEC-015 ~ SEC-022、AUTH-001 ~ AUTH-008

### Stage 3.6：高可用与滚动升级（AR-01 ~ AR-04）

- **文件**：`crates/clouisle-api/src/{shutdown.rs, health.rs}`、`deploy/`（systemd units、Helm chart 可选）
- **具体逻辑**：
  - **优雅关闭**（AR-04）：SIGTERM → `/health/ready` 返回 503（LB 摘流）→ 等进行中请求完成（最长 30 s）→ 退出。**不销毁沙盒**（沙盒生命周期由 `clouisled` 管，apiserver 无状态）
  - **`clouisled` 升级**：沙盒是独立 Firecracker 进程，`clouisled` 重启不影响运行中的 VM；重启后通过 reconciler 重新接管（从 DB + `/proc` 扫描恢复 handle）。这是「无中断升级 VMM 层」的关键——**新 VM 用新版 Firecracker，老 VM 继续跑老版本**
  - **健康检查**：`/health/live`（进程存活）、`/health/ready`（DB 可达 + 至少 1 个 node ready）
  - **优雅降级**（AR-03）：全局并发上限 + 每 tenant 配额；超限返回 `429` + `Retry-After`；**永不** OOM-kill 自己（内存水位监控，超 80% 拒绝新建）
  - **自动恢复**（AR-02）：沙盒异常退出 → `clouisled` 检测（waitpid）→ 按 `restart_policy: never|on_failure|always` 处理；默认 `never`（沙盒语义上是一次性的），事件写审计 + 指标
- **验证**：滚动重启 3 个 apiserver（逐个），全程 `hey -z 60s` 压测无 5xx；升级 `clouisled` 二进制并重启，运行中沙盒的 exec 在重启窗口后仍可用
- **测试用例**：HA-001 ~ HA-012、CHAOS-001 ~ CHAOS-010

### Stage 3.7：持久化存储挂载（FR-12）

- **文件**：`crates/clouisle-vmm/src/drives.rs`、`crates/clouisle-api/src/handlers/volumes.rs`
- **具体逻辑**：
  - `mounts: [{source, target, readonly}]`（PRD §5.2 已定义）实现：宿主机目录 → 不能直接 bind mount 进 VM（无共享文件系统时）。三个选项：
    1. **额外 block device**：把 host 目录预先打成 ext4 镜像挂为第三块盘 —— 简单，但不支持 host 侧实时可见
    2. **virtio-fs** —— Firecracker **不支持**（这是 Cloud Hypervisor / krun 的特性）
    3. **agent 侧同步**：创建时把 source 内容通过 vsock 推入 guest，销毁时（可选）拉回 —— 语义清晰，适合小数据集
  - **Phase 3 决策**：`readonly: true` 用方案 1（打 ext4 只读挂载，多 VM 可共享同一镜像文件）；`readonly: false` 用方案 3 + 明确文档说明「非实时同步，销毁时回写」。**在 API 文档中显式声明此语义差异**，避免用户误期待 POSIX 共享语义
  - 超过 100 MB 的可写 mount 拒绝（返回 400 + 建议用只读或对象存储）
- **验证**：只读 mount 在 guest 内可读、写入报 EROFS；可写 mount 在销毁时内容回写到 host source
- **测试用例**：VOL-001 ~ VOL-008

---

## 测试策略

**回归范围**：Phase 3 重构了执行路径（单进程 → apiserver + clouisled 双跳），**Phase 1/2 的全部用例必须重跑**，特别关注：
- EXEC-* 的延迟 SLO（双跳后是否仍 < 100 ms）
- POOL-* 的分配延迟（池管理下移到 clouisled 后）
- 所有 API-* 的语义等价性

**新增测试类型**：
- **HA 测试**：进程级故障注入（kill apiserver / clouisled / postgres）
- **Chaos 测试**：磁盘写满、网络分区、时钟跳变、OOM
- **Soak 测试**：72 h 持续负载，观察 FD 泄漏、内存增长、快照目录膨胀

---

## 风险与缓解

| 风险 | 缓解 |
|------|------|
| 单机 → 多机重构引入大面积回归 | Stage 3.1 先在**单节点上跑 apiserver + clouisled 双进程**，验证等价性后再上多节点；`Store` / `Vmm` trait 在 Phase 1 已隔离，重构面可控 |
| Postgres 成为吞吐瓶颈（1000 req/s） | 读走连接池 + 只读副本；写批量化；SLO 声明中区分读写（ADR-004）；实测不达标时引入 Redis 做热状态缓存 |
| eBPF 程序在不同内核版本编译失败 | 用 CO-RE（aya + BTF），CI 覆盖 5.10 / 6.1 / 6.6 三个内核；eBPF 采集**降级可用**——加载失败只告警，不阻塞沙盒创建 |
| 审计日志量过大（每沙盒每秒数百条网络事件） | eBPF 侧聚合（per-flow 而非 per-packet）；采样策略可配；冷数据归档到对象存储 |
| uid 池耗尽 / uid 复用导致跨沙盒权限泄漏 | uid 池按 [100000, 165535] 分配，销毁后延迟 60 s 才回收（等内核清理完 chroot）；池耗尽时拒绝新建而非复用 |
