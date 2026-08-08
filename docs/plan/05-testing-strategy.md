# 测试策略与测试用例目录

本文件是全量测试用例的规范性来源。每个用例有唯一 ID，与各 phase 设计文档中的引用对应。

**约定**：
- `[macOS]` = 无需 KVM，在 macOS / 普通 Linux CI 上可跑
- `[KVM]` = 必须在有 `/dev/kvm` 的 Linux 宿主机上跑
- `[Cluster]` = 至少 2 个节点的完整集群
- **粗体**行 = 安全或隔离相关，优先级最高

---

## ENV — 环境配置类

| ID | 名称 | 前提 | 步骤 | 预期结果 | 环境 |
|----|------|------|------|---------|------|
| ENV-001 | KVM 可用性检查 | 裸机或嵌套虚拟化主机 | 运行 `scripts/setup-host.sh --check` | 0 退出码，输出「kvm: ok, cgroup v2: ok」 | [KVM] |
| ENV-002 | 缺 KVM 时友好报错 | 无 `/dev/kvm` 的机器 | 运行 `scripts/setup-host.sh --check` | 非 0 退出码，输出「缺少 /dev/kvm，请加入 kvm 组」，无 panic | [macOS] |
| ENV-003 | Firecracker 版本校验 | - | 放入错误版本 fc 二进制，运行 `clouislectl version` | 报「firecracker version mismatch: got X.Y.Z, need A.B.C」 | [KVM] |
| ENV-004 | cgroup v1 机器上的错误提示 | cgroup v1 机器 | 启动 apiserver | 启动失败，日志含「cgroup v2 required」 | [KVM] |

---

## IMG — 镜像构建类

| ID | 名称 | 前提 | 步骤 | 预期结果 | 环境 |
|----|------|------|------|---------|------|
| IMG-001 | vmlinux 格式校验 | 内核构建完成 | `file vmlinux` | 输出含「ELF 64-bit LSB executable」，**不含** compressed | [KVM] |
| IMG-002 | base rootfs 完整性 | rootfs 构建完成 | `debugfs -R "stat /sbin/clouisle-init" base.ext4` | 文件存在，size > 500KB | [macOS] |
| IMG-003 | OCI 镜像拉取并构建 ext4 | 网络可达 docker.io | 调用 image builder：`python:3.11-slim` | 生成 `<digest>.ext4`，`dumpe2fs` 无错误，`ls /usr/local/bin/python3` 可找到 | [macOS] |
| IMG-004 | 同 digest 命中缓存 | IMG-003 已跑 | 第二次请求同 digest 的镜像 | 耗时 < 5 ms，不重新下载 | [macOS] |
| IMG-005 | Whiteout 处理 | 有含 whiteout 的测试镜像 | 用含删除层的镜像构建 ext4 | 被删除的文件在 ext4 中不存在 | [macOS] |
| IMG-006 | .wh..wh..opq 整目录覆盖 | - | 测试镜像含 opaque whiteout | 父目录中父层文件不可见，只有当前层文件可见 | [macOS] |
| IMG-007 | 损坏 tar 层返回错误 | - | 构造 checksun 错误的 layer tar | 返回 `LayerChecksumMismatch` 错误，无残留临时文件 | [macOS] |
| IMG-008 | 镜像 ext4 大小合理 | IMG-003 已跑 | 检查生成的 ext4 文件大小 | `ext4_size ≤ docker_image_compressed_size × 3`（合理上限） | [macOS] |
| IMG-009 | 私有 registry 认证 | 有私有 registry 凭据 | 配置 docker config.json，拉取私有镜像 | 成功构建 ext4 | [macOS] |
| IMG-010 | 镜像拉取进度 SSE | 大镜像（>500MB） | `GET /sandboxes/{id}/image-pull-progress`（SSE） | 收到 progress 事件，最终 `{"status":"ready"}` | [macOS] |

---

## UNIT — 单元测试（纯逻辑，无 I/O）

| ID | 名称 | 测试目标 | 验证点 | 环境 |
|----|------|---------|--------|------|
| UNIT-001 | 合法状态转换 | `SandboxStatus::transition()` | `Running → Stopping` 返回 Ok | [macOS] |
| UNIT-002 | 非法状态转换拒绝 | 同上 | `Running → Starting` 返回 `InvalidTransition` | [macOS] |
| UNIT-003 | 全状态转换矩阵 | 同上 | 枚举所有 8 × 8 组合，合法/非法与期望一致 | [macOS] |
| UNIT-004 | Resources 校验通过 | `Resources::validate()` | 合法配置返回 Ok | [macOS] |
| UNIT-005 | Resources vcpu=0 拒绝 | 同上 | 返回 `ValidationError {field: "vcpu"}` | [macOS] |
| UNIT-006 | Resources memory < 64MB 拒绝 | 同上 | 返回 `ValidationError {field: "memory_mb"}` | [macOS] |
| UNIT-007 | vsock 帧编解码往返 | `clouisle-proto` | `encode → decode` 所有帧类型结果一致 | [macOS] |
| UNIT-008 | vsock 帧截断后报错 | 同上 | 传入截断字节序列返回 `FrameTruncated` | [macOS] |
| UNIT-009 | 调度器并发准入无超售 | `ResourcePool` | 100 goroutine 同时 admit 1 vCPU，总池 50 vCPU → 50 成功 50 失败 | [macOS] |
| UNIT-010 | Reservation drop 后资源释放 | 同上 | drop Reservation → admit 再次成功 | [macOS] |
| UNIT-011 | CleanupGuard LIFO 顺序 | `cleanup.rs` | 注册 A B C，drop 时 C B A 顺序执行 | [macOS] |
| UNIT-012 | CleanupGuard 失败不 panic | 同上 | 其中一个 cleanup fn 返回 Err → 其余仍执行，不 panic | [macOS] |
| UNIT-013 | 审计哈希链构建 | `chain.rs` | 10 条记录，每条 prev_hash == 前一条 hash | [macOS] |
| UNIT-014 | 审计哈希链篡改检出 | 同上 | 修改第 5 条 payload → verify 返回 `ChainBroken {seq: 5}` | [macOS] |
| UNIT-015 | SLO 枚举与字符串互转 | `core::timing` | `POOL_ALLOC.to_string()` == `"pool_alloc"` | [macOS] |

---

## STORE — 存储类

| ID | 名称 | 步骤 | 预期结果 | 环境 |
|----|------|------|---------|------|
| STORE-001 | 创建后读取 | `create_sandbox` → `get_sandbox` | 字段完全一致 | [macOS] |
| STORE-002 | 状态更新持久化 | `update_sandbox_status` → 重开连接 → `get_sandbox` | 新状态正确 | [macOS] |
| STORE-003 | 删除后找不到 | `delete_sandbox` → `get_sandbox` | 返回 `NotFound` | [macOS] |
| STORE-004 | 并发写无数据竞争 | 50 个 tokio task 同时 `save_execution` | 全部成功，总条数正确 | [macOS] |
| STORE-005 | 迁移幂等 | 运行两次迁移 | 第二次无报错，schema 不变 | [macOS] |
| STORE-006 | WAL 模式生效 | `PRAGMA journal_mode` | 返回 `wal` | [macOS] |
| STORE-007 | `list_sandboxes` 状态过滤 | 创建 Running × 3 + Stopped × 2 → `list(status=Running)` | 返回 3 条 | [macOS] |
| STORE-008 | 执行记录超大 stdout 截断 | 存 2MB stdout 的执行记录 | 取出时 stdout 截断到 1MB，`truncated: true` | [macOS] |

---

## VMM — VMM 层类

| ID | 名称 | 步骤 | 预期结果 | 环境 |
|----|------|------|---------|------|
| VMM-001 | PUT /boot-source 请求格式 | 假 FC + HTTP 断言 | method=PUT, path=/boot-source, body 含 `kernel_image_path` | [macOS] |
| VMM-002 | PUT /drives/rootfs 请求格式 | 假 FC | body 含 `path_on_host`, `is_read_only: true` | [macOS] |
| VMM-003 | PUT /vsock 请求格式 | 假 FC | body 含 `guest_cid`, `uds_path` | [macOS] |
| VMM-004 | PUT /actions InstanceStart | 假 FC | body 含 `action_type: "InstanceStart"` | [macOS] |
| VMM-005 | socket 就绪等待重试 | 延迟 200ms 出现 socket 的假 FC | 最终成功，总等待时间 200-300ms | [macOS] |
| VMM-006 | socket 等待超时（500ms） | 500ms 内不出现 socket | 返回 `SocketNotReady` 错误 | [macOS] |
| VMM-007 | BadRequest 解析 | 假 FC 返回 400 含 fault_message | `FcApiError::BadRequest {fault_message}` | [macOS] |
| VMM-008 | 单机完整生命周期 | `create → start → exec echo → stop` | 全程成功，无残留进程/文件 | [KVM] |
| VMM-009 | 10 个并发 VM 独立 | 同时起 10 VM，各自 exec `echo $RANDOM` | 10 个不同输出，无串扰 | [KVM] |
| VMM-010 | kill FC 后资源清理 | 起 VM → `kill -9 <fc_pid>` | 5s 内 TAP 消失，jail 目录消失，cgroup 消失 | [KVM] |
| VMM-011 | 非法 kernel 路径 | 传入不存在的 kernel 路径 | 明确错误，无残留 TAP / scratch 文件 | [KVM] |
| VMM-012 | firecracker 二进制缺失 | 删除 fc 二进制 | 启动前检测失败，报「firecracker not found at <path>」 | [KVM] |
| VMM-013 | 旧 jail 目录残留（上次崩溃） | 手动创建 `<chroot-base>/<vm-id>` 目录 | 启动时自动清理后继续，日志记录「清理残留 jail」 | [KVM] |

---

## GUEST — Guest 初始化类

| ID | 名称 | 步骤 | 预期结果 | 环境 |
|----|------|------|---------|------|
| GUEST-001 | overlayfs 挂载成功 | 启动 VM，检查串口日志 | 含「pivot_root ok」，无「mount failed」 | [KVM] |
| GUEST-002 | base ext4 只读（共享） | 起 2 个同镜像 VM，各自在 `/tmp` 写文件 | base.ext4 mtime 不变；两个 VM 的 `/tmp` 互不可见 | [KVM] |
| GUEST-003 | scratch 磁盘配额 | 创建 `disk_mb: 100` 的沙盒，guest 内写 120 MB | 写到 100 MB 后报 ENOSPC | [KVM] |
| GUEST-004 | init 故障安全（overlay 失败） | 注入 `clouisle.fail_at=overlay` 内核 cmdline | VM 在 timeout 内退出，宿主机清理完毕，无 hang | [KVM] |
| GUEST-005 | SIGPWR 优雅关闭 | FC `SendCtrlAltDel` → 等待 | guest 调用 `reboot(RB_POWER_OFF)`，进程退出码 0 | [KVM] |
| GUEST-006 | 僵尸子进程回收 | guest 内运行 `sh -c "bash &"` | guest agent（PID 1）定期 waitpid，无僵尸积累 | [KVM] |

---

## EXEC — 命令执行类

| ID | 名称 | 步骤 | 预期结果 | 环境 |
|----|------|------|---------|------|
| EXEC-001 | 基本 exec，stdout | `echo hello` | stdout=`hello\n`，exit_code=0 | [KVM] |
| EXEC-002 | stderr 分流 | `sh -c 'echo err >&2'` | stderr=`err\n`，stdout=`` | [KVM] |
| EXEC-003 | 非零退出码 | `exit 7` | exit_code=7 | [KVM] |
| EXEC-004 | 超时强杀 | `sleep 100`，timeout=2s | 2s 后返回超时错误，guest 内无残留 sleep 进程 | [KVM] |
| EXEC-005 | 超时后子进程清理 | `sh -c 'sleep 100 &'`，timeout=2s | 超时后 `sleep 100` 后台进程也被杀（killpg） | [KVM] |
| EXEC-006 | 环境变量注入 | exec `env`，传入 `{"FOO":"bar"}` | stdout 含 `FOO=bar` | [KVM] |
| EXEC-007 | 工作目录（cwd） | exec `pwd`，cwd=`/tmp` | stdout=`/tmp\n` | [KVM] |
| EXEC-008 | 流式 SSE 输出 | `for i in 1..10; do echo $i; sleep 0.1; done`，流式模式 | SSE 事件逐行到达，chunk 间隔约 100ms（而非最后一次性收到） | [KVM] |
| EXEC-009 | Stopped 沙盒 exec | 先 stop，再 exec | HTTP 409 Conflict | [macOS] |
| EXEC-010 | 并发 exec 无串流 | 2 个沙盒同时流式 exec，各自 1000 行输出 | 每个 SSE 流中无对方的行 | [KVM] |
| EXEC-011 | 大输出（100MB）截断 | exec `dd if=/dev/urandom bs=1M count=100 \| base64` | 响应 stdout 截断到上限，含 `truncated: true` | [KVM] |
| EXEC-012 | exec history 查询 | exec 后 `GET .../exec/{exec_id}` | 返回记录，stdout/stderr/exit_code 正确 | [macOS] |

---

## API — REST API 接口类

| ID | 名称 | 步骤 | 预期结果 | 环境 |
|----|------|------|---------|------|
| API-001 | 创建沙盒（最小 spec） | `POST /api/v1/sandboxes {image, vcpu:1, memory_mb:256}` | 201，body 含 id + status | [macOS] |
| API-002 | 创建后立即查询 | POST → GET | status=running，spec 字段一致 | [macOS] |
| API-003 | vcpu=0 拒绝 | POST vcpu=0 | 400，errors[0].field=vcpu | [macOS] |
| API-004 | 缺少 image 字段 | POST 无 image | 400，errors[0].field=image | [macOS] |
| API-005 | 资源超宿主机上限 | POST vcpu=9999 | 507，body 含当前可用量 | [macOS] |
| API-006 | 删除运行中沙盒 | DELETE running sandbox | 204，VMM 进程随后退出 | [KVM] |
| API-007 | 删除不存在的 id | DELETE `{"id":"nonexistent"}` | 404 | [macOS] |
| API-008 | 请求 ID 透传 | 请求头带 `X-Request-Id: test-123` | 响应头含 `X-Request-Id: test-123` | [macOS] |
| API-009 | 无请求 ID 自动生成 | 不带 X-Request-Id | 响应头含自动生成的 UUID v7 | [macOS] |
| API-010 | 列表过滤（status） | 创建 3 个 running + 2 个 stopped → `GET /sandboxes?status=running` | 返回 3 条 | [macOS] |
| API-011 | 分页（limit/offset） | 10 个沙盒 → `GET /sandboxes?limit=3&offset=0` | 返回 3 条，含 total=10 | [macOS] |
| API-012 | 并发 100 创建（MockVmm） | 100 goroutine 同时 POST | 全部 201，总沙盒数 = 100 | [macOS] |

---

## NET — 网络隔离类

| ID | 名称 | 步骤 | 预期结果 | 环境 |
|----|------|------|---------|------|
| NET-001 | 允许白名单域名出站 | `allow_egress: ["pypi.org"]`，guest `curl pypi.org` | 连接成功 | [KVM] |
| NET-002 | 拒绝非白名单域名 | `allow_egress: ["pypi.org"]`，guest `curl google.com` | DNS 返回 NXDOMAIN | [KVM] |
| **NET-003** | **拒绝直接 IP（绕过 DNS）** | 手动构造 IP 字面量 TCP 连接到未在白名单中的 IP | nftables drop，连接超时 | [KVM] |
| NET-004 | 空白名单 = 完全离线 | `allow_egress: []`，guest `curl anything` | 所有出站均失败 | [KVM] |
| **NET-005** | **沙盒间网络隔离** | 沙盒 A 监听 TCP，沙盒 B 尝试连接 A 的 guest IP | 连接失败（不通）| [KVM] |
| NET-006 | 入站默认拒绝 | 宿主机向沙盒 guest IP 发 TCP SYN | 无响应（drop） | [KVM] |
| NET-007 | netns 清理 | 删除沙盒后检查 `ip netns list` | 该沙盒的 netns 不再存在 | [KVM] |
| NET-008 | CNAME 链展开 | 白名单含 `pypi.org`（实际 CNAME 多跳） | 最终 A 记录 IP 进入 `@allowed_egress`，curl 成功 | [KVM] |
| NET-009 | DNS TTL 过期后重新解析 | 设 TTL 极短（1s），等待 TTL 过期后再 curl | 自动重新解析，nftables set 更新，连接仍成功 | [KVM] |
| NET-010 | 出站带宽限制 | `bandwidth_mbps: 10`，iperf3 测速 | 实测 ≤ 10 Mbps（允许 ±20% 误差） | [KVM] |
| NET-011 | veth / TAP 清理 | 删除沙盒后 `ip link show \| grep <sbx-id>` | 无相关 veth 残留 | [KVM] |
| NET-012 | 50 沙盒并发，各自 curl 各自白名单域名 | 50 沙盒不同白名单，同时发请求 | 无跨沙盒流量泄漏（用 tcpdump 在各 TAP 上验证） | [KVM] |

---

## RES — 资源限制类

| ID | 名称 | 步骤 | 预期结果 | 环境 |
|----|------|------|---------|------|
| RES-001 | CPU 上限（1 vCPU） | 创建 vcpu=1，guest `stress-ng --cpu 4` | 宿主机观察到该 VM cgroup 的 cpu.stat 使用率 ≤ 100% | [KVM] |
| RES-002 | 内存硬限制 | memory_mb=256，guest 申请 300MB | Guest OOM，进程被杀；宿主机不受影响 | [KVM] |
| RES-003 | 内存软限制（high） | 内存压力但未到 max | cgroup memory.events 中 high 计数增加 | [KVM] |
| RES-004 | 磁盘 IOPS 限制 | `disk_mb: 1024`，guest `fio --iodepth=32 --rw=randread` | 观测到 IOPS ≤ 配置值（±30%） | [KVM] |
| RES-005 | 进程数上限（fork bomb 防护） | guest 跑 `:() { : \| : & }; :` | cgroup `pids.max` 生效，fork 被拒，宿主机无影响 | [KVM] |
| RES-006 | 内存超限时宿主机无 OOM | 100 个各 256MB 的沙盒同时超用内存 | 各自 guest OOM，宿主机 `/proc/meminfo` MemAvailable 不归零 | [KVM] |
| RES-007 | 资源限制热更新 | 运行中 `POST /sandboxes/{id}/resources {bandwidth_mbps: 5}` | Firecracker `PATCH /network-interfaces/eth0` 被调用，新限速生效 | [KVM] |
| RES-008 | cgroup 目录销毁时清理 | 删除沙盒后检查 `/sys/fs/cgroup/` | 该沙盒的 cgroup slice 不存在 | [KVM] |
| RES-009 | 无 swap（SR-04 相关） | 任意沙盒 | `cat /sys/fs/cgroup/.../memory.swap.max` = 0 | [KVM] |
| RES-010 | 预留资源调度核算 | admit(vcpu=2) × 25，总池 50 vCPU | 第 26 次 admit 返回 507 | [macOS] |
| RES-011 | 释放后可再分配 | 删除 10 个沙盒 → 立即创建 10 个新沙盒 | 全部成功，无「资源不足」 | [macOS] |
| RES-012 | swap 禁止（在 guest 内验证） | guest `swapon -s` | 无 swap 设备 | [KVM] |

---

## POOL — 温池类

| ID | 名称 | 前置条件 | 步骤 | 预期结果 | 环境 | 追溯 |
|----|------|---------|------|---------|------|------|
| POOL-001 | 池预热到目标水位 | 配置 `min_idle: 5` | 启动 clouisled，等待 30s | 池内 5 个 idle 沙盒，状态均为 READY | [KVM] | FR-08 |
| POOL-002 | 命中池的分配延迟 | 池已满水位 | `POST /sandboxes`（规格与池模板一致），测 t0→READY | ≤ 50ms（P95） | [KVM] | FR-08 |
| POOL-003 | 池未命中回落冷启动 | 请求规格与池模板不符（如 vcpu=4） | 创建沙盒 | 走冷启动路径，成功，≤ 200ms | [KVM] | FR-08 |
| POOL-004 | 取用后自动补充 | 池水位 5 | 取用 1 个，等待 10s | 池重新回到 5 | [KVM] | FR-08 |
| POOL-005 | 并发取用不重复分配 | 池水位 5 | 20 并发创建请求 | 5 个命中池且各不相同（VM PID 唯一），15 个冷启动；无一个池实例被分配两次 | [KVM] | FR-08 |
| POOL-006 | **池实例分配前的清洁性** | — | 取用池实例，检查 `/tmp`、环境变量、进程列表、网络状态 | 无上一租户残留；`/tmp` 为空；无非 init 的用户进程 | [KVM] | SR-01 |
| POOL-007 | 池实例超时回收 | `max_idle_secs: 300` | 造一个 idle 超 300s 的实例 | 被销毁，池补新的 | [KVM] | FR-08 |
| POOL-008 | 池实例健康检查失败被剔除 | 手动 kill 一个池内 VM 进程 | 等待一个健康检查周期 | 该实例从池中移除，补充新的，无请求被分配到死实例 | [KVM] | AR-02 |
| POOL-009 | 资源不足时池让位给显式请求 | 资源池接近打满 | 提交显式创建请求 | 池主动缩容释放资源，显式请求成功 | [KVM] | FR-03 |
| POOL-010 | 池在节点关停时清理 | 池水位 5 | `SIGTERM` clouisled | 5 个池实例全部销毁，无孤儿 Firecracker 进程 | [KVM] | AR-01 |
| POOL-011 | 池模板变更后旧实例作废 | 修改镜像 digest | reload 配置 | 旧模板实例被逐步替换，新分配只用新模板 | [KVM] | ER-01 |
| POOL-012 | 池大小为 0 时功能正常 | `min_idle: 0` | 创建沙盒 | 全部走冷启动，无报错 | [KVM] | FR-08 |

---

## FILE — 文件传输类

| ID | 名称 | 前置条件 | 步骤 | 预期结果 | 环境 | 追溯 |
|----|------|---------|------|---------|------|------|
| FILE-001 | 上传小文件 | 沙盒 RUNNING | `PUT /sandboxes/{id}/files?path=/work/a.txt`，1KB | 200，guest 内容与 sha256 一致 | [KVM] | FR-07 |
| FILE-002 | 下载文件 | guest 内已有 `/work/b.bin` 10MB | `GET /sandboxes/{id}/files?path=/work/b.bin` | 200，sha256 与 guest 侧一致 | [KVM] | FR-07 |
| FILE-003 | 上传大文件（100MB）流式 | — | 上传 100MB | 成功；apiserver RSS 增长 < 50MB（证明未全量缓冲） | [KVM] | FR-07 |
| FILE-004 | 目录打包下载 | guest 有 `/work/dir` 多层结构 | `GET .../files?path=/work/dir&format=tar` | 返回合法 tar，解开后结构一致 | [KVM] | FR-07 |
| FILE-005 | **路径穿越拒绝（上传）** | — | `path=/work/../../etc/passwd` | 400 `INVALID_PATH`，宿主机与 guest 的 `/etc/passwd` 均未变 | [KVM] | SR-01 |
| FILE-006 | **符号链接逃逸拒绝** | guest 内 `ln -s /etc /work/evil` | 上传到 `/work/evil/passwd` | 拒绝或写入被限制在沙盒内，`/etc/passwd` 未变 | [KVM] | SR-01 |
| FILE-007 | 超出配额的上传 | disk_mb=512，已用 500MB | 上传 100MB | 507 或 `ENOSPC` 明确错误，沙盒不崩溃 | [KVM] | FR-04 |
| FILE-008 | 文件权限与属主 | 上传时指定 mode=0755 | 上传后 guest `stat` | mode 为 0755，属主为沙盒运行用户 | [KVM] | FR-07 |
| FILE-009 | 上传到不存在的父目录 | — | `path=/work/nope/a.txt` | 400 明确错误（或按参数自动建目录，行为与文档一致） | [KVM] | FR-07 |
| FILE-010 | 传输中断的清理 | 上传 100MB 中途断连 | 客户端 abort | guest 无半截文件残留（写临时文件后 rename） | [KVM] | FR-07 |
| FILE-011 | 对已停止沙盒的传输 | 沙盒 STOPPED | 上传 | 409 `INVALID_STATE` | [macOS] | FR-07 |
| FILE-012 | 并发上传同一路径 | — | 2 个并发上传写 `/work/x` | 无交错损坏（其一胜出且内容完整） | [KVM] | FR-07 |

---

## SNAP — 快照类

| ID | 名称 | 前置条件 | 步骤 | 预期结果 | 环境 | 追溯 |
|----|------|---------|------|---------|------|------|
| SNAP-001 | 创建快照 | 沙盒 RUNNING，已执行若干命令 | `POST /sandboxes/{id}/snapshots {label: "s1"}` | 202 → 快照目录存在，含 `mem.snap` + `vmstate.snap` | [KVM] | FR-09 |
| SNAP-002 | 从快照恢复 | SNAP-001 完成 | `POST /sandboxes/restore {snapshot_id: "s1"}` | 新沙盒 RUNNING，`history` 显示之前执行的命令 | [KVM] | FR-09 |
| SNAP-003 | 快照后状态一致（文件系统） | 快照前写入 `/work/test.txt` | 恢复后 `cat /work/test.txt` | 内容一致 | [KVM] | FR-09 |
| SNAP-004 | **多次从同一快照恢复（克隆）** | 一个快照 | 并发恢复 5 次 | 5 个沙盒均 RUNNING，互相隔离（写 `/tmp/x` 互不可见） | [KVM] | FR-09, SR-01 |
| SNAP-005 | **多次恢复后 RNG 重新播种** | 同一快照恢复 2 次 | 两个沙盒各读 `/dev/urandom` 128B | 两份字节串不同（确认 virtio-rng 重新播种，见 ADR-003） | [KVM] | SR-03 |
| SNAP-006 | 快照后时钟重新同步 | 快照后休眠 5s 再恢复 | 恢复后 guest `date -u` | 与宿主机偏差 ≤ 1s（ADR-003 约定） | [KVM] | FR-09 |
| SNAP-007 | 快照不含网络状态 | 快照时有建立的 TCP 连接 | 恢复后检查 `/proc/net/tcp` 中该连接 | 连接记录不存在或已 RST（网络连接不跨快照） | [KVM] | FR-09 |
| SNAP-008 | 列出快照 | 已创建 3 个快照 | `GET /sandboxes/{id}/snapshots` | 返回 3 个条目，含 created_at / label / size_bytes | [KVM] | FR-09 |
| SNAP-009 | 删除快照 | 快照存在 | `DELETE /snapshots/{snap_id}` | 204，目录已删除 | [KVM] | FR-09 |
| SNAP-010 | 快照磁盘配额 | `snapshot_max_gb: 5`，尝试超出 | 第 N 个快照让总量超 5GB | 409 `SNAPSHOT_QUOTA_EXCEEDED` | [KVM] | FR-09 |
| SNAP-011 | 内核版本不匹配拒绝恢复 | 快照 A 用 kernel-v6.1；当前节点 kernel-v6.6 | 恢复 A | 422 `KERNEL_MISMATCH`，明确错误（ADR-003） | [KVM] | FR-09 |
| SNAP-012 | UFFD 共享内存按需加载 | Phase 3，启用 UFFD | 从快照恢复后立即计时到首条命令响应 | ≤ 100ms，内存实际读入量 < 全量 | [KVM] | FR-09 |

---

## SEC — 安全隔离类

| ID | 名称 | 前置条件 | 步骤 | 预期结果 | 环境 | 追溯 |
|----|------|---------|------|---------|------|------|
| **SEC-001** | **逃逸：guest 写宿主机文件系统** | — | guest 尝试 `mount --bind /proc/sysrq-trigger /hostfs` 等各种挂载 | 全部拒绝（seccomp / capabilities 不足） | [KVM] | SR-01 |
| **SEC-002** | **CAP_NET_ADMIN 缺失** | — | guest `ip link add dummy0 type dummy` | Operation not permitted | [KVM] | SR-01 |
| **SEC-003** | **seccomp 阻断危险 syscall** | — | guest 调用 `ptrace(PTRACE_ATTACH, 1, ...)` | EPERM 或 SIGSYS | [KVM] | SR-02 |
| **SEC-004** | **跨沙盒进程不可见** | 两个沙盒 A、B | B 的 guest `cat /proc/<A_pid>/status` | No such file（不同 PID namespace） | [KVM] | SR-01 |
| **SEC-005** | **内存不跨沙盒** | 沙盒 A 写 `/dev/shm/secret` | 沙盒 B 读 `/dev/shm/secret` | ENOENT（不同 IPC ns） | [KVM] | SR-01 |
| **SEC-006** | **网络流量不跨沙盒** | A 监听 8080 | B 直接 curl `<A_guest_IP>:8080`（绕过 DNS 代理） | 连接被 nftables 拒绝 | [KVM] | SR-01 |
| **SEC-007** | **Jailer uid/gid 隔离** | — | `ps aux` 观察 Firecracker 进程 uid | uid ≥ 100000（jailer 分配的子 uid） | [KVM] | SR-02 |
| **SEC-008** | **root 文件系统只读** | — | guest `touch /bin/evil` | Read-only file system | [KVM] | SR-01 |
| **SEC-009** | vsock 命令注入防护 | — | 构造 vsock 帧体为 `{"method":"exec","cmd":"id > /etc/evil"}` 并转义 JSON | 命令执行输出为 literal 字符串，`/etc/evil` 不存在 | [KVM] | SR-01 |
| **SEC-010** | **MMDS 仅 guest 可达** | — | 宿主机 curl `169.254.169.254` | 超时 / 拒绝（iptables 屏蔽宿主机访问 link-local） | [KVM] | SR-02 |
| **SEC-011** | 沙盒租期到期强制销毁 | `ttl_secs: 60` | 创建后 60s 不操作 | 沙盒被销毁，资源释放 | [KVM] | FR-01 |
| **SEC-012** | **cgroup v2 层次完整性** | — | guest `cat /proc/self/cgroup` | 显示在沙盒专属 slice 内（不在 root cgroup） | [KVM] | SR-04 |
| **SEC-013** | **虚拟设备最小化** | — | guest `ls /sys/bus/pci/devices` 与 `lsmod` | 仅 virtio-blk / virtio-net / virtio-vsock / virtio-rng，无其他 | [KVM] | SR-02 |
| **SEC-014** | **宿主机 /dev/kvm 权限** | — | 检查 Firecracker 进程可访问的设备 | 仅 `/dev/kvm`，无 `/dev/mem`、`/dev/kmem` | [KVM] | SR-02 |
| **SEC-015** | 恶意 rootfs 拒绝加载 | 构造声明 size 巨大但实际截断的 ext4 镜像 | 创建沙盒 | 启动失败并给出明确错误，不 panic、不挂起 | [KVM] | SR-02 |

---

## AUTH — 认证授权类

| ID | 名称 | 前置条件 | 步骤 | 预期结果 | 环境 | 追溯 |
|----|------|---------|------|---------|------|------|
| AUTH-001 | 无 token 拒绝 | — | `GET /sandboxes` 不带 Authorization | 401 `UNAUTHENTICATED` | [macOS] | SR-05 |
| AUTH-002 | 无效 token 拒绝 | — | 带 `Bearer garbage` | 401 | [macOS] | SR-05 |
| AUTH-003 | 过期 token 拒绝 | 签发 exp 在过去的 token | 任意请求 | 401 `TOKEN_EXPIRED` | [macOS] | SR-05 |
| **AUTH-004** | **跨租户读取拒绝** | 租户 A 的沙盒 S | 用租户 B 的 token `GET /sandboxes/S` | 404（不泄漏存在性，非 403） | [macOS] | SR-06 |
| **AUTH-005** | **跨租户删除拒绝** | 同上 | 租户 B `DELETE /sandboxes/S` | 404，且 S 仍存在 | [macOS] | SR-06 |
| **AUTH-006** | **跨租户 exec 拒绝** | 同上 | 租户 B `POST /sandboxes/S/exec` | 404 | [macOS] | SR-06 |
| AUTH-007 | 列表按租户过滤 | A 有 3 个、B 有 2 个沙盒 | A `GET /sandboxes` | 仅返回 A 的 3 个 | [macOS] | SR-06 |
| AUTH-008 | 租户配额限制 | 租户 A 配额 10 个沙盒，已有 10 | 创建第 11 个 | 429 `TENANT_QUOTA_EXCEEDED` | [macOS] | SR-06 |
| AUTH-009 | 只读 token 无法变更 | scope = `read` 的 token | `POST /sandboxes` | 403 `INSUFFICIENT_SCOPE` | [macOS] | SR-05 |
| AUTH-010 | token 撤销即时生效 | 撤销某 token | 立即用该 token 请求 | 401（不依赖缓存过期） | [macOS] | SR-05 |
| AUTH-011 | 每租户速率限制 | 租户 A 超过 QPS 上限 | 持续打请求 | 429 + `Retry-After` 头 | [macOS] | SR-06 |

---

## AUDIT — 审计日志类

| ID | 名称 | 前置条件 | 步骤 | 预期结果 | 环境 | 追溯 |
|----|------|---------|------|---------|------|------|
| AUDIT-001 | 生命周期事件落库 | — | create → exec → delete | audit_logs 有 3 类事件，含 tenant / sandbox / actor / ts | [macOS] | FR-10, SR-07 |
| **AUDIT-002** | **哈希链完整** | 已有 100 条日志 | 逐条校验 `hash_n == H(hash_{n-1} \|\| payload_n)` | 全部通过 | [macOS] | SR-07 |
| **AUDIT-003** | **篡改被检出** | 直接改库中第 50 条 payload | 重跑链校验 | 在第 50 条报告断链 | [macOS] | SR-07 |
| **AUDIT-004** | **批次签名可验证** | 已签名批次 | 用公钥验签 | Ed25519 验签通过 | [macOS] | SR-07 |
| **AUDIT-005** | **审计不可删除** | — | 尝试 `DELETE` 审计记录（API 与 SQL 触发器双重） | API 无该端点；DB 触发器拒绝 DELETE/UPDATE | [macOS] | SR-07 |
| AUDIT-006 | 宿主机侧事件标记 trusted | eBPF 采集的 VMM 事件 | 查该事件记录 | `source: "host"`, `trust: "trusted"`（ADR-004） | [KVM] | FR-10 |
| AUDIT-007 | Guest 上报事件标记 advisory | guest agent 上报的进程事件 | 查该事件记录 | `source: "guest"`, `trust: "advisory"` | [KVM] | FR-10 |
| AUDIT-008 | 审计写入不阻塞主流程 | 审计 sink 人为阻塞 5s | 期间正常 create 沙盒 | 创建成功，审计走缓冲队列，无请求超时 | [macOS] | AR-02 |
| AUDIT-009 | 审计队列满时不丢关键事件 | 队列容量打满 | 继续产生安全类事件 | 安全类事件优先保留，普通事件可降级丢弃并计数告警 | [macOS] | SR-07 |
| AUDIT-010 | 审计导出 | 已有若干日志 | `GET /audit?from=&to=&tenant=` | NDJSON 流式导出，字段完整 | [macOS] | SR-07 |

---

## OBS — 可观测性类

| ID | 名称 | 前置条件 | 步骤 | 预期结果 | 环境 | 追溯 |
|----|------|---------|------|---------|------|------|
| OBS-001 | Prometheus 指标暴露 | clouisled 启动 | `curl /metrics` | 返回 text/plain Prometheus 格式，含 `sandbox_active_total` | [macOS] | FR-10 |
| OBS-002 | 沙盒计数指标准确 | 创建 10 个沙盒 | 查 `sandbox_active_total` | = 10 | [macOS] | FR-10 |
| OBS-003 | 执行计数与状态分布 | 执行 5 成功 / 2 超时 | 查 `execution_total{status="ok"}` / `execution_total{status="timeout"}` | 分别为 5 / 2 | [KVM] | FR-10 |
| OBS-004 | 启动延迟 histogram | 10 次冷启动 | 查 `sandbox_boot_duration_seconds` P99 bucket | bucket 分布合理，P99 值与实测接近 | [KVM] | FR-10 |
| OBS-005 | 日志包含 trace_id | 发起一个请求并记录 `X-Request-Id` | 检索日志 | 所有相关日志行均含该 trace_id | [macOS] | FR-10 |
| OBS-006 | 健康检查端点 | clouisled 正常 | `GET /healthz` | 200 `{"status":"ok"}` | [macOS] | AR-02 |
| OBS-007 | 就绪检查端点 | 节点正常接受调度 | `GET /readyz` | 200 `{"ready":true}` | [macOS] | AR-02 |
| OBS-008 | 就绪检查：资源耗尽时变 not-ready | 资源打满（RES-010 场景） | `GET /readyz` | 200 `{"ready":false,"reason":"resource_exhausted"}` | [macOS] | AR-02 |
| OBS-009 | Metrics 不含租户私密信息 | — | 全量 `/metrics` 导出 | 无沙盒 ID 以外的任何租户标识符或命令内容 | [macOS] | SR-06 |
| OBS-010 | 执行超时被记录到 metrics | `timeout_ms: 100`，guest sleep 1s | 查 metrics | `execution_total{status="timeout"}` +1 | [KVM] | FR-02 |
| OBS-011 | 结构化日志 JSON 格式 | — | 启动 + 创建一个沙盒 | 日志行可被 `jq` 解析，含 `level`/`ts`/`msg` 字段 | [macOS] | FR-10 |

---

## SCHED — 调度类

| ID | 名称 | 前置条件 | 步骤 | 预期结果 | 环境 | 追溯 |
|----|------|---------|------|---------|------|------|
| SCHED-001 | 最小负载节点优先 | 2 节点，A 空 B 半满 | 提交请求 | 调度到 A | [Cluster] | FR-11 |
| SCHED-002 | 资源不足时 507 | 全部节点打满 | 提交请求 | 507 `INSUFFICIENT_CAPACITY` | [Cluster] | FR-03 |
| SCHED-003 | affinity：指定节点 | 2 节点 | 请求带 `node_selector: {node: "nodeB"}` | 调度到 B | [Cluster] | FR-11 |
| SCHED-004 | anti-affinity：沙盒分散 | 3 节点，已各有 1 个同 group | 请求带 `anti_affinity_group: "g1"` | 调度到拥有该 group 沙盒最少的节点 | [Cluster] | FR-11 |
| SCHED-005 | 节点下线后调度绕开 | 标记 nodeA `cordoned` | 提交请求 | 调度到非 cordoned 节点 | [Cluster] | FR-11 |
| SCHED-006 | 并发调度无双重分配 | 50 vCPU 总资源，vcpu=2 的请求 × 30 并发 | 观察 scheduler 日志 | admit 25 个，其余 507；无一个节点分配超额 | [Cluster] | FR-03 |
| SCHED-007 | 节点资源上报刷新 | 删除沙盒后 | 立即查节点可用资源 | 资源已回收（≤ 1 个心跳周期） | [Cluster] | FR-11 |
| SCHED-008 | 调度决策持久化（control plane 重启后） | 调度若干沙盒 | 重启 apiserver | 已调度沙盒位置不变，路由正确 | [Cluster] | AR-03 |

---

## NODE — 节点管理类

| ID | 名称 | 前置条件 | 步骤 | 预期结果 | 环境 | 追溯 |
|----|------|---------|------|---------|------|------|
| NODE-001 | 节点注册 | 2 节点集群 | 启动 nodeB | nodeB 出现在 `GET /nodes`，状态 Ready | [Cluster] | FR-11 |
| NODE-002 | 节点注销 | 空节点 | 关停 nodeB | 状态变 Offline | [Cluster] | FR-11 |
| NODE-003 | 节点心跳超时 | nodeB 正常 | kill 心跳进程后等待 3× 心跳间隔 | nodeB 状态变 Unhealthy | [Cluster] | AR-01 |
| NODE-004 | 故障节点上的沙盒标记错误 | nodeB Unhealthy | 查 nodeB 上的沙盒 | 状态变 ERROR | [Cluster] | AR-01 |
| NODE-005 | cordon 阻止新调度 | nodeA cordon | 提交新请求 | 不调度到 nodeA | [Cluster] | FR-11 |
| NODE-006 | drain 迁移存量沙盒 | nodeA 有 5 个沙盒 | drain nodeA | 5 个沙盒被停止并迁移或重建（视迁移策略），nodeA 无 RUNNING | [Cluster] | FR-11 |
| NODE-007 | 节点容量上报 | — | `GET /nodes/{id}` | 含 cpu_total / mem_total / cpu_avail / mem_avail | [Cluster] | FR-11 |

---

## HA — 高可用类

| ID | 名称 | 前置条件 | 步骤 | 预期结果 | 环境 | 追溯 |
|----|------|---------|------|---------|------|------|
| HA-001 | 主控平面崩溃恢复 | 单节点 HA + SQLite WAL | kill -9 apiserver，立即重启 | 重启后已有沙盒信息仍可查，新请求成功 | [macOS] | AR-03 |
| HA-002 | Postgres HA：主库切换 | Phase 3 双控平面 | 停止 Postgres 主库 | 备库晋升，apiserver 重连后服务恢复（≤ 30s） | [Cluster] | AR-03 |
| HA-003 | 控制平面领导选举 | 2 个 apiserver 实例 | 同时启动 | 仅一个持有 Postgres advisory lock，另一个等待 | [Cluster] | AR-03 |
| HA-004 | 领导故障后重新选举 | HA-003 环境 | kill 当前主 | 备接管，advisory lock 换主，服务恢复 | [Cluster] | AR-03 |
| HA-005 | WAL 并发写不丢数据 | 高并发写 SQLite WAL 模式 | 100 并发事务，同时提交 | 所有事务提交，无丢失；WAL checkpoint 正常 | [macOS] | AR-03 |
| HA-006 | 节点重启后沙盒自愈 | 沙盒 RUNNING | 重启 clouisled | 启动时扫描 `/proc`，重建状态；RUNNING 的恢复为 RUNNING 或 ERROR（按 policy） | [KVM] | AR-01 |
| HA-007 | apiserver 滚动升级无中断 | 2 个 apiserver | 升级其中一个 | 升级期间请求由另一个处理，无 5xx | [Cluster] | AR-01 |

---

## VOL — 持久化存储类

| ID | 名称 | 前置条件 | 步骤 | 预期结果 | 环境 | 追溯 |
|----|------|---------|------|---------|------|------|
| VOL-001 | 创建卷 | — | `POST /volumes {size_mb: 512}` | 201，返回 volume_id；宿主机生成对应 sparse 文件 | [KVM] | FR-12 |
| VOL-002 | 挂载卷到沙盒 | 已有卷 | 创建沙盒带 `volumes: [{id, mount_path: "/data"}]` | Guest 内 `/data` 可读写 | [KVM] | FR-12 |
| VOL-003 | 数据跨沙盒生命周期持久 | VOL-002 写入 `/data/x` | 删除沙盒 → 新沙盒挂载同卷 | `/data/x` 内容不变 | [KVM] | FR-12 |
| VOL-004 | 卷独占挂载 | 卷已挂载到沙盒 A | 尝试同时挂载到沙盒 B | 409 `VOLUME_IN_USE` | [KVM] | FR-12 |
| VOL-005 | 卷释放后可再挂载 | 删除沙盒 A | 挂载到沙盒 B | 成功，数据保留 | [KVM] | FR-12 |
| VOL-006 | 卷容量上限 | 卷 512MB | Guest 内 `dd` 写 600MB | 写入在 512MB 处失败（ENOSPC），宿主机磁盘不被写穿 | [KVM] | FR-12, SR-04 |
| VOL-007 | 删除卷 | 卷未挂载 | `DELETE /volumes/{id}` | 204；宿主机文件被删除 | [KVM] | FR-12 |
| VOL-008 | 删除已挂载卷被拒 | 卷正在使用 | `DELETE /volumes/{id}` | 409 `VOLUME_IN_USE` | [KVM] | FR-12 |
| VOL-009 | 跨租户卷隔离 | 租户 A 的卷 | 租户 B 尝试挂载 | 404（不泄漏存在性） | [KVM] | SR-06 |
| VOL-010 | 卷快照与沙盒快照独立 | 沙盒有卷 + 打快照 | 恢复快照 | 卷按当前状态挂载（不回滚），语义在文档中明确 | [KVM] | FR-09, FR-12 |

---

## RECOVER — 崩溃与恢复类

| ID | 名称 | 前置条件 | 步骤 | 预期结果 | 环境 | 追溯 |
|----|------|---------|------|---------|------|------|
| RECOVER-001 | Firecracker 进程崩溃 | 沙盒 RUNNING | `kill -9 <firecracker-pid>` | 沙盒状态在 ≤ 5s 内变 ERROR，资源被回收 | [KVM] | AR-01 |
| RECOVER-002 | Guest kernel panic | 沙盒 RUNNING | guest 内 `echo c > /proc/sysrq-trigger` | 检测到 VM 退出，状态变 ERROR，宿主机资源释放 | [KVM] | AR-01 |
| RECOVER-003 | Guest Agent 崩溃但 VM 存活 | 沙盒 RUNNING | guest 内 kill agent | 健康检查失败，状态变 UNHEALTHY；exec 返回 503 | [KVM] | AR-01 |
| RECOVER-004 | clouisled 崩溃，VM 存活 | 3 个沙盒 RUNNING | `kill -9 clouisled`，重启 | 重建对 3 个存活 VM 的管理（socket 重连成功），状态保持 RUNNING | [KVM] | AR-01 |
| RECOVER-005 | 孤儿 VM 清理 | 手工起一个无记录的 firecracker | 启动 clouisled | 检测为孤儿，按 policy 终止并记审计日志 | [KVM] | AR-01 |
| RECOVER-006 | 宿主机断电模拟（残留文件） | 沙盒目录含 socket / pid 残留 | 启动 clouisled | 清理陈旧 socket / cgroup / netns / TAP，不影响新沙盒创建 | [KVM] | AR-01 |
| RECOVER-007 | 数据库损坏检测 | 破坏 SQLite 文件头 | 启动 apiserver | 快速失败并给出明确错误，**不**静默重建丢数据 | [macOS] | AR-03 |
| RECOVER-008 | 磁盘写满 | 宿主机数据盘填至 100% | 创建沙盒 | 507 或 500 明确错误，无部分创建的僵尸资源 | [KVM] | AR-01 |
| RECOVER-009 | 创建过程中途失败的回滚 | 注入故障：drive 配置后失败 | 创建沙盒 | 已分配的 TAP / netns / cgroup / 文件全部回滚，无泄漏 | [KVM] | AR-01 |
| RECOVER-010 | 删除幂等 | 已删除的沙盒 | 再次 `DELETE` | 204 或 404，无 panic，无重复资源释放 | [macOS] | FR-01 |

---

## CHAOS — 混沌与压力类

| ID | 名称 | 前置条件 | 步骤 | 预期结果 | 环境 | 追溯 |
|----|------|---------|------|---------|------|------|
| CHAOS-001 | 随机 kill VMM | 100 沙盒运行中 | 每 10s 随机 kill 一个 firecracker，持续 30 分钟 | 每次都被正确标记 ERROR + 资源回收；无资源泄漏累积 | [KVM] | AR-01 |
| CHAOS-002 | 随机 kill clouisled | 50 沙盒运行中 | 每 5 分钟 kill 一次并自动重启，持续 1 小时 | 状态最终一致，无孤儿 VM，无重复计费/计数 | [KVM] | AR-01 |
| CHAOS-003 | 网络分区（节点 ↔ 控制平面） | 集群 | iptables DROP 控制平面流量 60s 后恢复 | 分区期间节点保持本地沙盒运行；恢复后状态重新同步一致 | [Cluster] | AR-01 |
| CHAOS-004 | 数据库连接抖动 | Phase 3 | 每 30s 断开 Postgres 连接 5s | apiserver 自动重连；请求失败返回 503 而非 panic | [Cluster] | AR-03 |
| CHAOS-005 | 磁盘 I/O 变慢 | 用 cgroup 限制宿主机 I/O | 创建沙盒 | 启动变慢但不失败；超时后返回明确错误 | [KVM] | AR-01 |
| CHAOS-006 | Soak：24 小时持续创建/删除 | 单节点 | 循环创建-执行-删除，速率 5/s，持续 24h | 无内存泄漏（RSS 平稳）、无 fd 泄漏、无 netns/TAP 残留、成功率 ≥ 99.9% | [KVM] | AR-01, AR-04 |
| CHAOS-007 | Soak：长生命周期沙盒 | 10 个沙盒运行 24h | 每分钟 exec 一次 | 全程 exec 成功；guest 无内存增长；时钟无显著漂移 | [KVM] | AR-04 |
| CHAOS-008 | 突发流量 | 空闲状态 | 瞬间提交 500 个创建请求 | 无 panic；超容量部分返回 507；已 admit 的全部成功 | [KVM] | AR-04 |
| CHAOS-009 | 快照并发恢复风暴 | 有快照 | 同时恢复 100 个 | 全部成功或明确 507；无 UFFD handler 崩溃 | [KVM] | FR-09 |
| CHAOS-010 | 时钟跳变 | 沙盒运行中 | 宿主机 `date -s` 前跳 1 小时 | 超时计算基于单调时钟，不误判超时 | [KVM] | AR-01 |

---

## PERF — 性能类

所有 PERF 用例以 [ADR-002](00-architecture-decisions.md) 归一化后的 SLO 为准：t0 = API 请求到达，t1 = 沙盒 READY 且 agent 可响应。

| ID | 名称 | 前置条件 | 步骤 | 目标 | 环境 | 追溯 |
|----|------|---------|------|------|------|------|
| PERF-001 | 池分配延迟 | 温池有空闲 | 100 次创建，取 P50/P99 | P99 ≤ 50ms | [KVM] | FR-08 |
| PERF-002 | 快照恢复延迟 | 已有快照 | 100 次恢复，取 P99 | P99 ≤ 100ms | [KVM] | FR-09 |
| PERF-003 | 冷启动延迟（镜像已缓存） | 无池、镜像本地已有 | 100 次创建，取 P99 | P99 ≤ 200ms | [KVM] | FR-01 |
| PERF-004 | Firecracker 裸启动基线 | — | 直接起 firecracker 到 guest init 完成 | ≤ 125ms（Phase 0 基线，用于回归比对） | [KVM] | — |
| PERF-005 | 单节点密度 | 128GB / 32 核宿主机 | 持续创建 256MB 沙盒直到失败 | ≥ 200 个并发运行 | [KVM] | FR-03 |
| PERF-006 | 控制平面吞吐（只读） | — | wrk 压 `GET /sandboxes` | ≥ 1000 req/s，P99 ≤ 50ms | [macOS] | AR-04 |
| PERF-007 | 控制平面吞吐（写） | SQLite WAL + 写批处理 | wrk 压创建/删除混合 | ≥ 200 req/s（写路径目标，见 ADR-002 说明） | [KVM] | AR-04 |
| PERF-008 | exec 往返延迟 | 沙盒 READY | 执行 `true` 1000 次，取 P99 | P99 ≤ 20ms（vsock 往返 + 进程创建） | [KVM] | FR-02 |
| PERF-009 | 大输出吞吐 | 沙盒 READY | guest 输出 100MB 到 stdout | 吞吐 ≥ 50 MB/s，内存占用有界（流式，不全量缓冲） | [KVM] | FR-02 |
| PERF-010 | 文件上传吞吐 | 沙盒 READY | 上传 1GB 文件 | ≥ 100 MB/s；宿主机内存占用有界 | [KVM] | FR-07 |
| PERF-011 | 内存共享效率（页缓存） | 100 个同镜像沙盒 | 对比总 RSS 与单个 × 100 | 因共享只读基础盘，实际占用显著低于线性（记录实测比例作为回归基线） | [KVM] | FR-03 |
| PERF-012 | 启动延迟随并发退化 | 并发 1 / 10 / 50 / 100 同时创建 | 各取 P99 | P99 随并发增长但不超过 SLO 的 3 倍；记录曲线 | [KVM] | AR-04 |

---

## 执行方式与 CI 矩阵

### 三条流水线

| 流水线 | 触发 | 包含 | 时长目标 |
|--------|------|------|---------|
| **fast** | 每次 push / PR | UNIT、STORE、以及所有 `[macOS]` 用例（mock VMM 后端） | ≤ 5 分钟 |
| **kvm** | PR 合入 main、每日定时 | 所有 `[KVM]` 用例（GUEST / EXEC / NET / RES / POOL / FILE / SNAP / SEC / VOL / RECOVER） | ≤ 40 分钟 |
| **heavy** | 每周 + 发布前 | `[Cluster]`、CHAOS、PERF、soak（CHAOS-006/007 各 24h） | 按需 |

### 环境要求

- **fast**：任意机器，`cargo test --workspace`。VMM 使用 mock 后端，无需 `/dev/kvm`。
- **kvm**：裸金属 Linux 优先；云上需支持嵌套虚拟化（GCP `nested-virt`、AWS `*.metal`）。KVM 用例通过 feature gate 隔离：
  ```bash
  cargo test --workspace --features kvm-integration -- --test-threads=1
  ```
  `--test-threads=1` 是必需的：网络命名空间、cgroup 层级、宿主机端口都是全局资源。
- **heavy**：≥ 2 节点集群 + 独立 Postgres。

### 前置守卫

`[KVM]` 流水线在跑任何用例前先执行 ENV 类用例作为 preflight。ENV 任一失败即整条流水线快速失败，不再继续 —— 避免把环境问题误报成功能回归。

### 判定规则

- **阻塞发布**：任一 UNIT / STORE / VMM / GUEST / EXEC / API 失败，或任一 **SEC / AUTH / AUDIT** 用例失败。安全类无「已知失败」豁免。
- **需记录不阻塞**：PERF 未达标时记录实测值并开 issue；连续两次发布未达标则升级为阻塞。
- **禁止 flaky 容忍**：不引入自动重试掩盖不稳定。用例不稳定时先定位根因（通常是清理不彻底或缺同步点），修复后再入库。

### 覆盖率与追溯

- 覆盖率工具 `cargo-llvm-cov`。目标：核心状态机与配置校验模块行覆盖 ≥ 85%，整体 ≥ 70%。覆盖率不作为硬门禁，仅作趋势观察。
- 每个 FR / SR / AR / ER 至少被一个用例的「追溯」列引用。CI 中用脚本校验：解析 PRD 需求 ID 清单与本文件的追溯列，缺失即失败。这保证需求不会在实现中被静默丢弃。

### 用例编号与新增约定

- ID 单调递增，永不复用。删除用例时保留 ID 并标注 `(已废弃，原因)`，避免历史报告中的 ID 指向错误用例。
- 新增功能必须同时新增对应用例并填写追溯列，作为 PR review 的检查项。
