# Phase 0：VMM 运行时技术验证 设计文档

**周期**：2-3 周　**前置**：可用的 Linux + KVM 主机（见 ADR-001）
**关联 PRD**：§七 Phase 0、FR-01（部分）、FR-02（部分）、§4.1 性能

---

## 背景与目标

### 要解决的问题

在写任何控制平面代码之前，必须先证明三件事在目标硬件上成立，否则后续架构可能全部返工：

1. Firecracker 能被我们的 Rust 代码可靠地启动、配置、关闭（含异常路径）
2. ADR-005 的「共享 RO base + scratch + guest overlayfs」方案在 guest 内真的能 pivot_root 成功
3. 冷启动时延的**真实分解**是多少——ADR-002 定的 200ms 预算花在哪里，哪一段是瓶颈

### 成功标准

- [ ] `clouislectl vm run --image <rootfs> --exec "echo hi"` 在本地打印 `hi`，全程无手工步骤
- [ ] 同一个 base rootfs 文件被 10 个并发 microVM 同时只读挂载，各自写入互不可见
- [ ] 产出冷启动时延分解表（至少 5 个阶段的 P50/P95，样本 ≥ 200 次）
- [ ] 杀掉 firecracker 进程 / 删除 socket / guest panic 三种异常下，宿主机无残留资源（进程、TAP、挂载点、临时文件）

### 明确不做

不做 API server、不做调度器、不做数据库、不做 OCI 镜像转换（用手工构建的 rootfs）、不做网络隔离（Phase 0 用最简单的 TAP + 静态 IP，甚至可以先不通网）。

---

## 高层设计

### 涉及的 crate

```
clouisle-sandbox/                    # workspace root
├── crates/
│   ├── clouisle-core/               # 领域类型：SandboxSpec, VmSpec, Resources, 状态机
│   ├── clouisle-vmm/                # Vmm trait + FirecrackerVmm + MockVmm
│   │   ├── src/firecracker/api.rs   # HTTP over UDS client（自研，ADR-004）
│   │   ├── src/firecracker/proc.rs  # 进程生命周期 + reaper
│   │   ├── src/firecracker/jailer.rs
│   │   └── src/mock.rs
│   ├── clouisle-proto/              # host <-> guest vsock 协议定义
│   ├── clouisle-agent/              # guest 内二进制（含 --init 模式，PID 1）
│   └── clouislectl/                 # CLI
└── images/                          # 内核 & rootfs 构建脚本（shell + Dockerfile）
```

### 数据流（Phase 0 最小闭环）

```
clouislectl vm run
  │
  ├─1─ 准备 scratch ext4（sparse file + mke2fs）
  ├─2─ 创建 TAP 设备 + 分配 IP
  ├─3─ fork/exec firecracker（jailer 包裹），拿到 UDS 路径
  ├─4─ PUT /boot-source, /drives/{rootfs,scratch}, /network-interfaces/eth0, /vsock, /machine-config
  ├─5─ PUT /actions {InstanceStart}
  │      └─ guest: kernel boot → clouisle-init(PID1) → overlayfs → pivot_root → exec agent
  ├─6─ host 连 vsock UDS，发 CONNECT <port>，等 agent 的 Hello 帧  ← t1 计时点
  ├─7─ 发 Exec 请求，流式收 stdout/stderr/exit_code
  └─8─ SendCtrlAltDel → 等进程退出 → 清理 TAP/scratch/socket/jail 目录
```

---

## 实施计划

### Stage 0.1：Linux 开发环境与 workspace 骨架

- **文件**：`Cargo.toml`（改为 workspace）、`crates/*/Cargo.toml`、`rust-toolchain.toml`、`scripts/setup-host.sh`、`.github/workflows/ci-portable.yml`
- **具体逻辑**：
  - `Cargo.toml` 转为 `[workspace]`，`resolver = "3"`（edition 2024 需要 Rust ≥ 1.85）
  - `scripts/setup-host.sh`：检查 `/dev/kvm` 存在且当前用户可读写（`kvm` 组）、检查 cgroup v2（`/sys/fs/cgroup/cgroup.controllers` 含 `cpu memory io pids`）、检查内核 ≥ 4.14、下载并校验 firecracker + jailer 二进制（**pin 版本 + sha256**）、`modprobe vhost_vsock`
  - 依赖版本全部 pin 到精确版本（不用 `^`）：`tokio`、`hyper`、`hyperlocal`、`serde`、`thiserror`、`tracing`、`nix`、`rusqlite`
- **验证**：`scripts/setup-host.sh --check` 在缺 KVM 的机器上以非 0 退出并打印具体缺失项；`cargo build --workspace` 在 macOS 与 Linux 均通过（`clouisle-vmm` 的 firecracker 后端在 macOS 上被 `cfg` 排除）
- **测试用例**：ENV-001、ENV-002

### Stage 0.2：Guest 内核与 base rootfs 构建

- **文件**：`images/kernel/build-kernel.sh`、`images/kernel/microvm.config`、`images/rootfs/Dockerfile.base`、`images/rootfs/build-rootfs.sh`
- **具体逻辑**：
  - 内核：pin 一个 LTS 版本（如 6.1.x），配置基于 Firecracker 官方 microvm config，**必须开**：`VIRTIO_MMIO`、`VIRTIO_BLK`、`VIRTIO_NET`、`VIRTIO_VSOCK`、`HW_RANDOM_VIRTIO`、`OVERLAY_FS`、`EXT4_FS`、`CGROUPS`；**必须关**：模块化（全部 `=y`，无 initrd）、`CONFIG_MODULES`、ACPI、USB、声卡、framebuffer。产出**未压缩 `vmlinux`**（ELF，Firecracker 直接加载，比 bzImage 省一次解压）
  - rootfs：`docker build` 一个最小 Debian/Alpine → `docker export` → 解包到目录 → 放入 `clouisle-agent` 静态二进制到 `/sbin/clouisle-init` → `mke2fs -d <dir> -t ext4 -b 4096 base.ext4 <size>`（**关键：`mke2fs -d` 不需要 root，也不需要 loop 挂载**）
  - agent 用 `x86_64-unknown-linux-musl` 静态编译，避免 glibc 依赖
- **验证**：`file vmlinux` 显示 ELF 64-bit executable；`dumpe2fs base.ext4` 无错误；`debugfs -R "stat /sbin/clouisle-init" base.ext4` 能找到文件且 size > 0
- **测试用例**：IMG-001、IMG-002

### Stage 0.3：Firecracker HTTP client（`clouisle-vmm/src/firecracker/api.rs`）

- **文件**：`crates/clouisle-vmm/src/firecracker/api.rs`、`types.rs`、`error.rs`
- **具体逻辑**：
  - `hyper` + `hyperlocal` 走 Unix socket；所有请求体/响应用 `serde` 强类型，**不用 `serde_json::Value`**
  - 实现 ADR-004 列出的全部端点，每个端点一个方法，返回 `Result<T, FcApiError>`
  - `FcApiError` 区分：`SocketNotReady`（需重试）、`BadRequest{fault_message}`（Firecracker 返回的 4xx，含具体原因）、`Io`
  - socket 就绪等待：启动后 socket 文件不会立即出现，用**指数退避 + 上限 500ms** 轮询 `GET /`，而非固定 sleep
- **验证**：单元测试用 `tokio::net::UnixListener` 起一个假 Firecracker，断言我们发出的 HTTP 请求方法/路径/JSON body 完全符合 Firecracker OpenAPI spec（把官方 `firecracker.yaml` 里的示例 body 作为 golden file）
- **测试用例**：VMM-001 ~ VMM-006

### Stage 0.4：进程生命周期与 jailer（`proc.rs`、`jailer.rs`）

- **文件**：`crates/clouisle-vmm/src/firecracker/proc.rs`、`jailer.rs`、`cleanup.rs`
- **具体逻辑**：
  - `jailer --id <vm-id> --exec-file <fc> --uid <n> --gid <n> --chroot-base-dir <dir> --cgroup-version 2 --cgroup cpu.max=... --cgroup memory.max=... -- --api-sock /run/fc.sock`
  - **非 root 运行**（SR-03）：jailer 自己需要 root 来做 chroot/setuid，但 firecracker 落到非特权 uid。开发环境提供 `--no-jailer` 逃生开关（仅 debug build）
  - 进程回收：`tokio::process::Child` + 显式 `wait()`；**必须** 有一个 `Drop` 保底路径把 SIGKILL 发出去，防止 tokio task 被 cancel 时留下孤儿进程
  - `cleanup.rs`：`CleanupGuard` 结构，按 LIFO 注册待清理资源（jail 目录、TAP、scratch 文件、cgroup 目录），`Drop` 时逐个执行并**记录失败但不 panic**
  - 启动前检查：如果 `<chroot-base>/<vm-id>` 已存在（上次崩溃残留），先清理再建
- **验证**：起 VM 后 `kill -9` firecracker，断言 5 秒内 TAP 设备消失、jail 目录被删、cgroup 目录被删；用 `lsof -p` 确认无残留 fd
- **测试用例**：VMM-010 ~ VMM-013、CHAOS-001

### Stage 0.5：`clouisle-init` —— guest PID 1

- **文件**：`crates/clouisle-agent/src/init.rs`、`main.rs`、`overlay.rs`
- **具体逻辑**（按顺序，任何一步失败 → 打印到 `/dev/console` 后 `reboot(RB_AUTOBOOT)`，**绝不 hang**）：
  1. 挂 `/proc`、`/sys`、`/dev`（devtmpfs）、`/dev/pts`、`/run`（tmpfs）
  2. ADR-005 的 overlay 流程：`/dev/vda` RO → `/mnt/base`；`/dev/vdb` → `/mnt/scratch`（若未格式化则 `mke2fs`，需内置 `mke2fs` 或用 Rust 的 ext4 库；**Phase 0 简化：scratch 由宿主机预格式化**）
  3. `mount -t overlay`，`pivot_root`，`umount` 老 root
  4. 从内核 cmdline 读配置（`clouisle.vsock_port=`、`clouisle.log_level=`），Phase 2 改为从 MMDS 读
  5. 配网：从 cmdline 读 `ip=`，`ioctl(SIOCSIFADDR)` 设 `eth0`（不依赖 `ip` 命令，减少 rootfs 体积）
  6. `exec` 到 agent 的 serve 模式（**同一个二进制，`--serve`**）；agent 作为 PID 1 需要 `waitpid(-1)` 循环回收僵尸子进程
  7. 注册 SIGTERM/SIGPWR handler（Firecracker `SendCtrlAltDel` 会触发 guest 的 ACPI power button → 需要 `reboot(RB_POWER_OFF)`）
- **验证**：串口日志出现 `clouisle-init: overlay ready, pivot_root ok`；guest 内 `mount | grep overlay` 正确；在 guest 内 `touch /tmp/x` 成功且宿主机 base.ext4 的 mtime 不变
- **测试用例**：GUEST-001 ~ GUEST-005

### Stage 0.6：vsock 通道与 exec

- **文件**：`crates/clouisle-proto/src/lib.rs`、`crates/clouisle-agent/src/exec.rs`、`crates/clouisle-vmm/src/vsock.rs`
- **具体逻辑**：
  - 协议：**长度前缀帧**（`u32 len` + `postcard` 编码的 enum），不上 gRPC——Phase 0 不需要，减少依赖和启动开销。帧类型：`Hello{agent_version}`、`ExecReq{id,argv,env,cwd,timeout}`、`Stdout{id,chunk}`、`Stderr{id,chunk}`、`Exited{id,code}`、`Ping`/`Pong`、`SyncTime{unix_nanos}`
  - host 侧连接方式（Firecracker 的 host-initiated 语义）：`connect(uds_path)` → 写 `CONNECT <port>\n` → 读 `OK <port>\n` → 之后是裸字节流。**这个握手必须实现对，是常见踩坑点**
  - guest 侧：`AF_VSOCK` `bind(VMADDR_CID_ANY, port)` + `listen`
  - exec：`Command` + `pipe`，stdout/stderr 各一个 tokio task 泵到帧；超时用 `tokio::time::timeout` + 进程组 SIGKILL（`killpg`，防止子进程逃逸）
- **验证**：`clouislectl vm run --exec "sh -c 'echo out; echo err >&2; exit 7'"` 打印正确的 stdout/stderr 且退出码为 7；`--exec "sleep 100" --timeout 2` 在 2 秒后返回超时错误且 guest 内无残留进程
- **测试用例**：EXEC-001 ~ EXEC-008

### Stage 0.7：启动时延分解基准

- **文件**：`benches/boot_latency.rs`、`crates/clouisle-core/src/timing.rs`、`docs/bench/phase0-results.md`
- **具体逻辑**：
  - `timing.rs`：`BootTrace` 结构，在 7 个点打时间戳：`t_request`、`t_scratch_ready`、`t_tap_ready`、`t_proc_spawned`、`t_api_configured`、`t_instance_start`、`t_agent_hello`
  - Firecracker 自己的 `--metrics-path` 也要采（含 `api_server`、`vmm`、`boot` 计时），与我们的埋点交叉验证
  - 跑 200 次，输出 P50/P95/P99 + 每阶段占比表；同时记录 guest 内核的 boot 耗时（从 guest dmesg 时间戳）
  - 输出必须是 markdown 表格，直接进 `docs/bench/phase0-results.md`
- **验证**：分解表各阶段之和 ≈ 总耗时（误差 < 5%）；若 P95 > 200 ms，在文档中标注瓶颈阶段与优化方向（不在 Phase 0 优化）
- **测试用例**：PERF-001

---

## 测试策略

| 层级 | 范围 | 运行环境 | 命令 |
|------|------|---------|------|
| 单元 | HTTP client 请求构造、协议编解码、状态机、cleanup LIFO 顺序 | macOS / Linux | `cargo test --workspace` |
| 组件 | 假 Firecracker（UnixListener）驱动完整 create→start→stop 流程 | macOS / Linux | `cargo test --features mock-vmm` |
| 集成 | 真实 microVM 启动 + exec + 清理 | **Linux + KVM** | `cargo test --features kvm-integration -- --test-threads=1` |
| 基准 | 启动时延分解 | **Linux + KVM 裸金属** | `cargo bench --features kvm-integration` |

**负向测试（重点，Phase 0 必须覆盖）**：
- 传入不存在的 kernel 路径 → 明确错误信息，非 panic，无残留
- Firecracker 二进制缺失 / 版本不匹配 → 启动前检测并报错
- `/dev/kvm` 无权限 → 报错文本包含「加入 kvm 组」的可执行建议
- guest init 故意 panic（注入 `clouisle.fail_at=overlay` cmdline）→ 宿主机在 timeout 后判定失败并清理
- vsock 握手时 guest 未就绪 → host 侧重试逻辑生效，超时后清理

**回归范围**：Phase 0 是新建，无回归。但 Stage 0.5 的 init 逻辑一旦变更，必须重跑 GUEST-* 全部用例。

---

## 风险与缓解

| 风险 | 概率 | 影响 | 缓解 |
|------|------|------|------|
| 无 Linux + KVM 主机，Phase 0 无法开工 | **高** | 阻塞全部数据平面 | Stage 0.1 第一天就确认主机；同时 `MockVmm` 让控制平面可并行开发 |
| guest overlayfs / pivot_root 调试困难（无 shell、无日志） | 高 | Stage 0.5 拖期 | 强制 `/dev/console` 串口日志 + 每步打点；准备一个带 busybox shell 的 debug rootfs 变体用于人工排查 |
| 内核配置漏项导致设备不可见（如忘开 VIRTIO_VSOCK） | 中 | 症状隐晦（vsock 连不上但无报错） | `microvm.config` 加注释说明每项用途；写一个 `scripts/verify-kernel-config.sh` 断言必需项 |
| 冷启动 P95 超 200 ms | 中 | ADR-002 的 SLO 不达标 | Phase 0 只测量不优化；把分解数据带入 Phase 3 性能优化阶段（届时手段：vmlinux 裁剪、`quiet` cmdline、跳过 udev、snapshot restore） |
| Firecracker 版本升级破坏 API 兼容 | 低 | client 代码需改 | 版本 pin + sha256；golden file 测试会在升级时立即失败提示 |

**回滚方案**：Phase 0 无生产影响。若 Firecracker 路线验证失败（如目标硬件 KVM 不可用），退到 `DockerVmm` 后端交付一个隔离性较弱的版本，同时向上反馈硬件需求——但这是重大降级，需决策层确认。
