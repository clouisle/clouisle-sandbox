# Clouisle Sandbox 实施计划总览

微VM沙盒调度系统（PRD v1.0）的落地索引。本文件**只做索引**：阶段、二级任务清单、状态、详细设计文档链接。所有技术细节在 `docs/plan/` 下的对应文档中。

## 关键前提（必读）

| 项 | 结论 |
|----|------|
| 开发机 | 当前仓库运行在 macOS（darwin）。Firecracker 依赖 Linux + `/dev/kvm`，**在 macOS 上完全无法运行** |
| 分层策略 | 控制平面（API/调度/存储/池）通过 `Vmm` trait + `MockVmm` 后端在 macOS 上开发与测试；VMM/网络/内核相关代码必须在 Linux 宿主机上验证 |
| 必需环境 | 一台 Linux 宿主机（裸金属优先；云上需支持嵌套虚拟化，如 GCP `nested-virt`、AWS `*.metal`），内核 ≥ 5.10，可访问 `/dev/kvm` |
| 语言/版本 | Rust（edition 2024，toolchain ≥ 1.85），Cargo workspace 多 crate |
| 三项 PRD 修正 | 见 [00-architecture-decisions.md](plan/00-architecture-decisions.md) 的 ADR-001 / ADR-002 / ADR-003 |

## 阶段与二级任务清单

### Phase 0 — VMM 运行时技术验证（2-3 周）

详细设计：[docs/plan/01-phase0-vmm-runtime.md](plan/01-phase0-vmm-runtime.md)

- [ ] 0.1 Linux 验证环境与 workspace 骨架搭建
- [ ] 0.2 Guest 内核构建（uncompressed vmlinux + 精简 config）
- [ ] 0.3 base rootfs 构建（OCI 镜像 → ext4 只读基镜像）
- [ ] 0.4 `clouisle-vmm`：Firecracker 进程封装 + HTTP-over-UDS 客户端
- [ ] 0.5 `clouisle-init`：PID 1 初始化（overlayfs / 网络 / 拉起 agent）
- [ ] 0.6 `clouisle-agent`：vsock gRPC 服务端 + exec 能力
- [ ] 0.7 `MockVmm` 后端（macOS 可开发的前提）
- [ ] 0.8 启动时延与资源开销基准（`bench/boot.rs`）

**里程碑**：命令行工具在 Linux 上启动一个 microVM，通过 vsock 执行 `echo hello` 并回收全部资源；同一套代码在 macOS 上以 Mock 后端跑通全部单测。

### Phase 1 — 控制平面 MVP（4-6 周）

详细设计：[docs/plan/02-phase1-control-plane-mvp.md](plan/02-phase1-control-plane-mvp.md)

- [ ] 1.1 领域模型与状态机（`clouisle-core`）
- [ ] 1.2 状态存储（SQLite + `Store` trait + 迁移）
- [ ] 1.3 沙盒生命周期 API（FR-01）
- [ ] 1.4 命令执行 API：一次性 + 流式（FR-02）
- [ ] 1.5 单机资源核算与准入调度器（FR-03 / FR-04 基础）
- [ ] 1.6 cgroup v2 + Jailer 资源限制（FR-04）
- [ ] 1.7 基础可观测性：结构化日志 + Prometheus 指标（FR-10 部分）
- [ ] 1.8 `clouislectl` CLI
- [ ] 1.9 孤儿进程回收与崩溃恢复（reconciler）

**里程碑**：`POST /api/v1/sandboxes` → `POST /exec` → `DELETE` 全链路可用；杀死 apiserver 后重启能收敛真实状态。


### Phase 2 — 沙盒增强功能（4-6 周）

详细设计：[docs/plan/03-phase2-sandbox-enhancements.md](plan/03-phase2-sandbox-enhancements.md)

- [ ] 2.1 OCI 镜像拉取与 rootfs 构建管道（FR-06）
- [ ] 2.2 快照预热池（Warm Pool）——基于内存快照 + UFFD（FR-08）
- [ ] 2.3 每沙盒网络命名空间 + TAP + nftables（FR-05）
- [ ] 2.4 出站域名白名单：DNS 代理 + nftables 动态集（FR-05）
- [ ] 2.5 文件传输 API——上传 / 下载 / ls（FR-07）
- [ ] 2.6 资源限制增强——cgroup v2 全维度 + Firecracker token bucket（FR-04）
- [ ] 2.7 快照 & 恢复 API（FR-09）

**里程碑**：从 Docker Hub 任意镜像创建沙盒；warm pool 下 P95 分配延迟 < 50 ms；沙盒间网络严格隔离验证通过。

### Phase 3 — 生产就绪（4-6 周）

详细设计：[docs/plan/04-phase3-production-ready.md](plan/04-phase3-production-ready.md)

- [ ] 3.1 多节点：`clouisled` 节点代理 + 控制平面心跳（FR-11）
- [ ] 3.2 跨节点调度策略（最少负载 / best-fit / 反亲和）
- [ ] 3.3 高可用控制平面：Postgres 后端 + 多实例（AR-01）
- [ ] 3.4 宿主机侧 eBPF 审计（VMM syscall + TAP 流量 + cgroup 事件）（FR-10）
- [ ] 3.5 审计日志 hash-chain 签名（SR-05）
- [ ] 3.6 Jailer + seccomp-bpf 安全加固（SR-03）
- [ ] 3.7 镜像漏洞扫描集成（SR-07）
- [ ] 3.8 性能调优：启动 P95 < 200ms，并发 ≥ 100（达标验收）
- [ ] 3.9 滚动升级与优雅降级（AR-04 / AR-03）

**里程碑**：3 节点集群，单控制平面重启无感；eBPF 审计链路端到端验证；全性能指标达到 PRD §4.1 目标值。

### Phase 4 — 扩展优化（持续）

- [ ] 4.1 多 VMM 后端（Kata Containers、gVisor 通过 `Vmm` trait 接入）
- [ ] 4.2 macOS dev 后端（libkrun/Hypervisor.framework）
- [ ] 4.3 AMD SEV-SNP 机密计算支持
- [ ] 4.4 实时迁移（CRIU + Firecracker live migration）
- [ ] 4.5 自动扩缩容（节点级别 HPA）

## 测试策略总览

全量测试用例目录：[docs/plan/05-testing-strategy.md](plan/05-testing-strategy.md)

| 测试层级 | 工具 | 前提 | 覆盖的需求 |
|---------|------|------|-----------|
| 单元测试 | `cargo test` | macOS 可跑 | 状态机、配置验证、调度算法、工具函数 |
| 组件测试（Mock VMM）| `cargo test --features mock-vmm` | macOS 可跑 | FR-01/02/03/04/08/10 全路径，不含 KVM |
| 集成测试（真实 VMM） | `cargo test --features kvm-integration` | Linux + `/dev/kvm` | FR-01~08, FR-09, FR-10 全量 |
| E2E API 测试 | `hurl` / `k6` | Linux + 完整集群 | 所有 API 端点 |
| 性能基准 | `criterion` + `k6` | Linux + 完整集群 | NFR §4.1 |
| 安全测试 | 手动 + automated | Linux + 完整集群 | SR-01~07 |
| 混沌测试 | 自定义脚本 | Linux + 完整集群 | AR-01~04 |

## 成功标准核查表（与 PRD §9 对齐）

- [ ] 冷启动 P95 < 200 ms（排除镜像首次拉取，cached image）
- [ ] 池分配 P95 < 50 ms
- [ ] 单机并发沙盒 ≥ 100（2 vCPU / 256 MB 配置）
- [ ] API 可用性 ≥ 99.95%（滚动窗口 30 天）
- [ ] 无已知严重安全漏洞（CVSS ≥ 7.0）
- [ ] 审计链路覆盖：宿主机侧可追溯每次 exec 的 VMM syscall + 网络流
- [ ] 全测试覆盖率：核心 crate 行覆盖率 ≥ 80%

