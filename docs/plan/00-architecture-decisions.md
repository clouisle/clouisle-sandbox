# 架构决策记录（ADR）

本文件记录项目的关键技术决策，特别是**对原 PRD 的修正**。每条决策包含背景、决策、后果。

---

## ADR-001：开发环境必须是 Linux + KVM，本机 macOS 只能开发控制平面

**状态**：已接受

### 背景

原 PRD 未提及开发环境约束。当前工作机为 macOS（Darwin 25.6.0）。Firecracker：

- 只能在 Linux 上运行，依赖 `/dev/kvm` 字符设备（`ioctl(KVM_CREATE_VM)`）
- 官方支持 x86_64 与 aarch64 Linux，**不提供 macOS 版本**
- `jailer` 依赖 Linux namespace、cgroup v2、`pivot_root`、seccomp-bpf

也就是说，本项目**约 40% 的代码（数据平面）无法在本机运行或测试**。

### 决策

> **2026-08-11 修订（ADR-DEV-01）**：历史 `DockerVmm`/`MockVmm` 生产/降级后端描述已废弃。
> 生产唯一后端为 `FirecrackerVmm`（无自动降级）；开发后端为显式
> `DockerDevVmm`（`--backend docker-dev`，仅本地开发、Docker socket 等价宿主权限、
> 明确不支持快照/iops/带宽/allowlist）；`MockVmm` 仅测试门控。详见
> `docs/plan/cross-platform-docker-dev-vmm.md`。

1. 全部 VMM 交互抽象为 `Vmm` trait（见 ADR-004），提供后端：
   - `FirecrackerVmm`——生产唯一后端，Linux only，`#[cfg(target_os = "linux")]`
   - `DockerDevVmm`——开发后端（`--backend docker-dev`），容器内注入静态 agent 复用帧协议
   - `MockVmm`——测试用状态机（test/test-utils 门控）
2. CI 分两条流水线：
   - `ci-portable`：macOS + Linux runner，跑 `cargo test`（含 `mock-vmm` feature）
   - `ci-kvm`：**self-hosted Linux 裸金属或支持嵌套虚拟化的云主机**，跑 `--features kvm-integration`
3. 推荐开发主机：
   - 首选裸金属（Equinix Metal / Hetzner AX 系列 / 自有服务器）
   - 次选支持嵌套虚拟化的云主机：GCP N2（`enable-nested-virtualization`）、AWS `*.metal`、Azure Dv3
   - **不推荐**：Apple Silicon 上的 Lima/UTM 跑 Linux 再跑 Firecracker（嵌套虚拟化在 ARM macOS 上不可用，无 `/dev/kvm`）

### 后果

- 正向：控制平面（API、调度器、状态存储、warm pool 逻辑）可在 macOS 上完整 TDD
- 负向：Phase 0 第一件事是准备 Linux 环境，否则整个数据平面阻塞
- 负向：所有性能数字必须在目标硬件上测，本机数据无参考价值

---

## ADR-002：统一启动时延 SLO 定义（修正 PRD 内部矛盾）

**状态**：已接受

### 背景

原 PRD 存在互相矛盾的时延指标：

| 位置 | 原文 | 问题 |
|------|------|------|
| §1.2 | 冷启动 < 200ms，预热启动 < 100ms | 未定义计时起止点 |
| §4.1 | 冷启动 < 200ms，预热启动 < 100ms | 同上 |
| FR-08 验收 | 「从池中分配沙盒的时间 < 50ms」 | 与「预热启动 < 100ms」是什么关系？ |
| FR-08 验收 | 「池中沙盒的预热启动冷启动 < 200ms」 | 语句自相矛盾，无法验收 |

此外「冷启动 < 200ms」若包含首次 OCI 镜像拉取（网络 IO，数百 MB），**物理上不可能达成**。

### 决策

统一定义计时点：**t0 = API server 收到请求的第一个字节；t1 = 沙盒状态转为 `READY` 且 guest agent 完成一次 vsock health ping**。

三档 SLO：

| SLO 名称 | 定义 | P50 | P95 | 对应 PRD |
|---------|------|-----|-----|---------|
| **POOL_ALLOC** | 从 warm pool 取一个已就绪沙盒并绑定给请求 | ≤ 20 ms | ≤ 50 ms | FR-08 验收项 1 |
| **WARM_START** | 从内存快照 restore 一个新沙盒（无池命中） | ≤ 60 ms | ≤ 100 ms | §4.1「预热启动」 |
| **COLD_START** | 完整 Firecracker boot，**镜像已在本地缓存**、rootfs 已构建 | ≤ 120 ms | ≤ 200 ms | §4.1「冷启动」 |
| *（新增，非 SLO）* | `IMAGE_PULL`：首次拉取镜像 + 构建 rootfs | 不设 SLO，异步任务 + 进度上报 | — | 补 PRD 缺口 |

**镜像拉取从启动路径中剥离**：`POST /sandboxes` 若镜像未缓存，立即返回 `202` + `status: pending_image`，客户端轮询或 watch。这也解决了 PRD §8 的「镜像拉取慢」风险。

### 后果

- 验收标准变得可测量，每档 SLO 对应独立的 benchmark（见测试用例 PERF-001~004）
- 需要 API 语义支持异步创建（PRD FR-01 已提到「同步创建与异步创建两种模式」，此处对齐）

---

## ADR-003：审计采集改为「宿主机可信 + Guest 辅助」双层模型（修正 PRD FR-10）

**状态**：已接受

### 背景

PRD FR-10 与 §6.3 要求：「每次执行的**系统调用**、网络连接、文件访问记录（eBPF 采集，宿主机侧签名）」。

这在 microVM 架构下**不成立**：

- 每个 microVM 运行**自己的 Linux 内核**。Guest 内进程的 `openat()`、`connect()` 由 guest kernel 处理，**不产生宿主机 syscall**，宿主机 eBPF（kprobe/tracepoint/LSM）完全看不到
- 宿主机 eBPF 能看到的只是：`firecracker` 进程自身的 syscall（主要是 `ioctl(KVM_RUN)`、`read`/`write` on virtio queues）、TAP 设备收发的**网络包**、cgroup 事件、block device IO
- 这与容器（共享内核，宿主机 eBPF 可见全部 guest syscall）根本不同——PRD 此处疑似照搬了容器沙盒的审计设计

### 决策

分两层，**信任级别不同**：

**Layer H（宿主机侧，可信 / tamper-evident）**
| 数据源 | 采集方式 | 得到什么 |
|-------|---------|---------|
| VMM 进程 syscall | eBPF tracepoint `raw_syscalls:sys_enter` 过滤 pid | VMM 自身行为，可检测逃逸尝试（如非预期 `openat`、`socket`） |
| 网络流 | eBPF TC classifier on TAP / netns | 五元组、字节数、DNS 查询明文，**guest 无法伪造** |
| Block IO | eBPF tracepoint `block:block_rq_issue` | rootfs / scratch 读写量 |
| cgroup 事件 | `memory.events`、`cpu.stat`、OOM kill | 资源触限记录 |
| KVM exit | `kvm:kvm_exit` tracepoint（可选，高开销，采样） | 异常 exit reason，逃逸检测信号 |

**Layer G（Guest 侧，非可信 / advisory）**
| 数据源 | 采集方式 | 信任说明 |
|-------|---------|---------|
| exec 事件 | guest agent 自己记录 command/exit_code | agent 由我们分发，但 guest root 可篡改 |
| guest 内文件/网络 | 可选 guest 侧 eBPF 或 seccomp notify | 仅作辅助，**不作为安全判定依据** |

Guest 侧数据通过 vsock 上报，落库时打 `trust_level: advisory` 标记。

**签名**：只对 Layer H 事件做 hash-chain（每条记录含前一条的 SHA-256）+ 每批 Ed25519 签名，私钥存宿主机 TPM 2.0 或云 KMS。Layer G 数据不签名，但纳入 chain 的 payload 哈希以便检测事后删改。

修正 §6.3 `AuditLog` 表结构：`eBPF_events` 字段拆为 `host_events`（jsonb）+ `guest_events`（jsonb）+ `trust_level`。

### 后果

- 正向：安全边界清晰，不会因「以为审计到了 guest syscall」而产生错误的安全假设
- 负向：若客户强需求 guest 内 syscall 级审计，需在 guest 镜像内置 eBPF/auditd，成本转移到镜像构建，且**对恶意 guest 无效**
- 负向：SR-05「审计可追溯」的实际语义需要向利益方重新说明

---

## ADR-004：VMM 抽象为 trait，Firecracker 以外部进程 + HTTP API 方式集成

**状态**：已接受

### 背景

PRD §3.2.1 建议「使用 rust-vmm 生态或直接集成 Firecracker」，ER-02 要求支持多后端。两条路：

| 方案 | 优点 | 缺点 |
|------|------|------|
| A. 链接 rust-vmm crates 自建 VMM | 单进程、无 IPC 开销、完全可控 | 自己维护设备模型/seccomp/CPU template，安全责任全揽，工作量 10x |
| B. 外部 `firecracker` 进程 + Unix socket HTTP API | AWS 生产验证、CVE 有上游修复、jailer 现成 | 每沙盒一个进程（PRD §3.2.1 本就如此设计）、需管理进程生命周期 |

### 决策

选 **B**。定义 trait：

```rust
#[async_trait]
pub trait Vmm: Send + Sync {
    async fn create(&self, spec: &VmSpec) -> Result<VmHandle>;
    async fn start(&self, h: &VmHandle) -> Result<()>;
    async fn pause(&self, h: &VmHandle) -> Result<()>;
    async fn resume(&self, h: &VmHandle) -> Result<()>;
    async fn snapshot(&self, h: &VmHandle, kind: SnapshotKind, out: &SnapshotPaths) -> Result<()>;
    async fn restore(&self, spec: &VmSpec, from: &SnapshotPaths) -> Result<VmHandle>;
    async fn stop(&self, h: &VmHandle, mode: StopMode) -> Result<()>;
    async fn stats(&self, h: &VmHandle) -> Result<VmStats>;
    fn capabilities(&self) -> VmmCapabilities; // 声明是否支持 snapshot / vsock / balloon
}
```

Firecracker HTTP client 自研（`hyper` + `hyperlocal`），因为没有成熟的 Rust crate 覆盖全部 API。用到的端点：

- `PUT /boot-source`、`PUT /drives/{id}`、`PUT /network-interfaces/{id}`、`PUT /vsock`
- `PUT /machine-config`（`vcpu_count`、`mem_size_mib`、`track_dirty_pages`）
- `PUT /actions`（`InstanceStart`、`SendCtrlAltDel`、`FlushMetrics`）
- `PATCH /vm`（`Paused` / `Resumed`）
- `PUT /snapshot/create`、`PUT /snapshot/load`
- `PATCH /drives/{id}`、`PATCH /network-interfaces/{id}`（rate limiter 热更新）
- `GET /` （instance-info，用于 health）

`capabilities()` 让上层优雅降级：`DockerVmm` 声明 `snapshot: false`，warm pool 自动退回「保持已启动实例」模式。

### 后果

- 正向：安全责任大部分外包给上游 Firecracker，CVE 跟随升级即可
- 正向：Phase 4 接 Kata / gVisor / libkrun 只需实现 trait
- 负向：需要自研 HTTP client（约 600 行，含类型定义）
- 负向：进程管理复杂度（僵尸进程、socket 清理、OOM 后残留），需要 reaper

---

## ADR-005：rootfs 采用「共享只读 base + 每沙盒 scratch + guest 内 overlayfs」

**状态**：已接受

### 背景

每沙盒需要可写根文件系统（FR-06）。候选：

| 方案 | 密度 | 复杂度 | 备注 |
|------|------|--------|------|
| A. 每沙盒完整复制 ext4 | 差（100 沙盒 × 500MB = 50GB） | 低 | 100 沙盒场景不可接受 |
| B. reflink 复制（XFS/Btrfs CoW） | 中（磁盘省，page cache 不共享） | 低 | 依赖特定文件系统 |
| C. 宿主机 overlayfs 后转 ext4 镜像 | 差 | 中 | 每次仍需生成镜像文件 |
| D. **共享 RO base drive + per-VM scratch drive + guest 内 overlayfs** | **优**（base 在宿主机 page cache 中被 100 个 VM 共享） | 中 | firecracker-containerd 同类做法 |

### 决策

选 **D**：

- Firecracker 挂两个 block device：
  - `/dev/vda` = base rootfs ext4，`is_read_only: true`，**所有同镜像沙盒共享同一文件**（宿主机 page cache 只缓存一份）
  - `/dev/vdb` = per-sandbox sparse ext4 scratch，大小 = `resources.disk_mb`（即 FR-04 的磁盘限制）
- `clouisle-init`（guest PID 1）挂载：
  ```
  mount -o ro /dev/vda /mnt/base
  mount /dev/vdb /mnt/scratch          # 首次由 init 内 mke2fs 或预格式化
  mkdir -p /mnt/scratch/upper /mnt/scratch/work
  mount -t overlay overlay -o lowerdir=/mnt/base,upperdir=/mnt/scratch/upper,workdir=/mnt/scratch/work /mnt/root
  # pivot_root 到 /mnt/root
  ```

base rootfs 由 OCI 镜像转换生成（Phase 2）：拉取 → 按序解包各 layer（处理 whiteout）→ `mke2fs -d <dir>`（**无需 root**，这是选 `mke2fs -d` 而非 loop mount 的关键原因）。

### 后果

- 正向：100 个 python:3.11 沙盒的 base 只占一份宿主机内存缓存，直接支撑 FR-03 的密度目标
- 正向：磁盘配额天然由 scratch 镜像大小实现
- 负向：guest 内需要 overlayfs 支持（内核配置 `CONFIG_OVERLAY_FS=y`）
- 负向：`init` 逻辑变复杂，需要在 Phase 0 就做对，否则后面全部返工

---

## ADR-006：网络分两阶段——Phase 1 桥接，Phase 2 per-sandbox netns

**状态**：已接受

### 背景

FR-05 要求每沙盒独立网络命名空间；SR-04 要求默认拒绝入站。同时 warm pool 快照恢复要求**所有克隆出来的 VM 可以有相同的 guest IP**（否则快照无法复用）。

### 决策

**Phase 1（够用即可）**：单个宿主机 bridge `clo0` + 每 VM 一个 TAP，guest IP 从 `10.200.0.0/16` 分配，唯一。nftables 做 SNAT + 默认 drop 入站。简单，先跑通。

**Phase 2（正式方案）**：每沙盒一个 netns：

```
netns "clo-<sbx-id>":
  tap0     10.0.0.2/30  (guest 侧固定 IP，所有沙盒相同 → 快照可复用)
  veth-in  192.168.<a>.<b>/30  ─┐
                                │ 宿主机 root netns
  veth-out 192.168.<a>.<b+1>/30 ┘ + SNAT/DNAT 改写
```

netns 内做 DNAT/SNAT 将固定的 `10.0.0.2` 映射到宿主机侧唯一的 `192.168.x.y`。这是 Firecracker 官方推荐的 clone 网络方案。

**出站域名白名单**（FR-05 `allow_egress: ["*.python.org"]`）：nftables 无法匹配域名。方案：

1. netns 内跑轻量 DNS 代理（监听 `10.0.0.1:53`，guest 的 resolv.conf 指向它）
2. 代理只解析白名单域名，其余返回 NXDOMAIN
3. 解析成功时把结果 IP 写入 nftables 动态 set `allowed_v4`（带 TTL）
4. nftables 规则：`ip daddr @allowed_v4 accept; drop`——**防止 guest 绕过 DNS 直接连 IP 字面量**

### 后果

- 正向：Phase 2 方案与快照恢复兼容，是 warm pool 的前置条件
- 负向：DNS 代理是新组件，需处理 CNAME 链、TTL 抖动、DoH 绕过（对策：drop 出站 443 到非 allowed_v4 的流量，DoH 自然失效）
- 负向：netns 创建约 5-15 ms，计入 COLD_START 预算，需在预热时提前建好

---

## ADR-007：状态存储 SQLite 起步，Phase 3 切 Postgres

**状态**：已接受

### 背景

PRD §3.2.4 建议 etcd 或 PostgreSQL。Phase 1 单机场景引入 etcd 是过度设计。

### 决策

定义 `Store` trait（sandbox / execution / audit / node 四类实体的 CRUD + watch）。

- Phase 1-2：`SqliteStore`（`rusqlite` bundled，WAL 模式，无外部依赖，单二进制可跑）
- Phase 3：`PostgresStore`（`sqlx`），多控制平面实例共享；leader election 用 Postgres advisory lock（`pg_try_advisory_lock`），**不引入 etcd/raft**
- 审计日志走独立表 + 独立 WAL 落盘（append-only 文件 + 定期批量入库），避免审计写入拖慢主路径

### 后果

- 正向：Phase 1 零运维依赖，`cargo run` 即可起服务，利于开发和测试
- 负向：需要两套 SQL（SQLite / Postgres 方言差异），用 `sqlx` 的 query macro 需要分别验证；对策是 SQL 保守写法 + 两套集成测试
- 负向：ADR-002 的 API 吞吐 ≥ 1000 req/s 目标，SQLite 单写者是瓶颈 → 写操作批量提交（见 Phase 3 §3.8）

---

## ADR-008：快照复用的安全约束（PRD 未提及的坑）

**状态**：已接受

### 背景

Warm pool 用「同一个内存快照 restore 多次」实现 < 100 ms 启动。这引入了 PRD 完全没有提到的安全问题。

### 决策

从同一快照 restore 出的多个 VM，以下状态是**重复的**，必须处理：

| 问题 | 后果 | 对策 |
|------|------|------|
| 内核熵池 / RNG 状态相同 | 多个沙盒生成**相同的**「随机」数（TLS 私钥、session token、UUID） | restore 后立即通过 virtio-rng 注入新熵；guest init 在 resume 后 `write /dev/urandom` + 触发 `RNDADDENTROPY` |
| 系统时钟停在快照时刻 | 证书校验失败、日志时间错乱 | resume 后 guest agent 立即从宿主机 vsock 拉时间并 `clock_settime` |
| 已建立的 TCP 连接失效 | 快照前的连接在 restore 后是幽灵连接 | 快照必须在「网络静默」状态创建；restore 后 flush conntrack |
| MMDS token 失效 | metadata 服务不可用 | restore 后重新 PUT mmds 配置 |
| 相同 machine-id / hostname | 日志混淆、某些软件行为异常 | resume 后由 agent 重写 `/etc/machine-id`、hostname |

这些全部实现在 `clouisle-init` 的 `post_restore_hook` 中，并有专门的测试用例（SEC-010、SEC-011）验证「两个从同一快照恢复的沙盒生成的随机数不同」。

### 后果

- 正向：避免了一类极难排查、后果严重的安全漏洞
- 负向：`post_restore_hook` 增加 restore 后的固定开销（实测目标 < 10 ms），计入 WARM_START 预算
