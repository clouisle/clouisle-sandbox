# Phase 2：沙盒增强功能 设计文档

**周期**：4-6 周　**前置**：Phase 1 里程碑达成
**关联 PRD**：FR-04（完整）、FR-05、FR-06、FR-07、FR-08、FR-09

---

## 背景与目标

Phase 1 是「能跑」，Phase 2 是「可用于生产的沙盒」：支持任意 OCI 镜像、网络隔离、warm pool、文件传输、快照。

### 成功标准

- [ ] `image: "python:3.11-slim"` 创建沙盒，guest 内能 `import requests`（网络白名单 `pypi.org`）
- [ ] 10 个同镜像沙盒并发，各自写 `/tmp/x` 互不可见；base.ext4 mtime 不变
- [ ] Warm pool P95 分配 < 50 ms（含 post-restore hook 全程）
- [ ] `POST /files/upload` 后 guest `cat /workspace/file.txt` 读到正确内容
- [ ] 沙盒 1 的出站 TCP 在沙盒 2 的 netns 中完全不可见

---

## 实施计划

### Stage 2.1：OCI 镜像管道（FR-06）

- **文件**：`crates/clouisle-images/src/{puller.rs, unpack.rs, builder.rs, cache.rs}`
- **具体逻辑**：
  - 拉取：用 `oci-distribution` crate（或 `containers-image` Rust 绑定）；支持 `docker.io`、`ghcr.io`、私有 registry；凭据从 `~/.docker/config.json` 读（`docker-credential-helpers` 协议）
  - Unpack：按 media type 解各层（tar.gz / zstd）到临时目录；处理 whiteout（`.wh.` 前缀文件 → 删除对应条目；`.wh..wh..opq` → 整目录覆盖）；原地 apply，最终得到合并后的目录树
  - ext4 构建：`mke2fs -d <merged_dir> -t ext4 -b 4096 -L <image_ref_hash> <output>.ext4 <size_mb>M`（**不需要 root**）；size = 解包后目录大小 × 1.3（留余量）+ 64 MB
  - 缓存：镜像 ref（含 digest）→ `<cache_dir>/<digest>.ext4`；再次用到同 digest 直接返回路径，**不重建**。缓存目录配额（LRU 淘汰）
  - API 语义（ADR-002）：`POST /sandboxes` 若镜像未缓存，立即返回 `202` + `status: pending_image`；后台拉取完成后更新状态，支持 `GET /sandboxes/{id}/image-pull-progress`（SSE）
- **验证**：`python:3.11-slim` 成功构建 ext4；第二次创建同镜像沙盒直接命中缓存（< 1 ms）；corrupted layer tar 返回明确错误，不留半成品文件
- **测试用例**：IMG-003 ~ IMG-010

### Stage 2.2：快照预热池（FR-08）

- **文件**：`crates/clouisle-pool/src/{pool.rs, reaper.rs, restore.rs}`、`crates/clouisle-vmm/src/snapshot.rs`
- **具体逻辑**：
  - 预热流程（每个 pool slot）：冷启动一个 VM → 等 agent ready → **调用 ADR-008 的 post_restore_hook 基线版本（写入已知熵）** → `PATCH /vm` 暂停 → `PUT /snapshot/create {kind: Full}` → **杀进程**（不留运行中的 VM）→ 把快照路径入池
  - Restore 流程：从 pool 取快照路径 → `PUT /snapshot/load {mem_backend: {type: File}, resume_vm: true}` → 等 agent hello → 执行 post_restore_hook（注入新熵、同步时钟、flush conntrack、重写 machine-id） → 标记 READY，t1 计时
  - 内存文件 mmap（`mem_backend.type: Uffd`，Phase 3 再上，Phase 2 先用 File 模式验证逻辑）
  - 池管理：`Arc<Mutex<VecDeque<SnapshotSlot>>>`；后台 task 维持 `min_ready` 个就绪快照；补充速率限制（防止批量重建时 CPU 峰值）
  - 池按 `(image_digest, resources_hash)` 分桶，不同配置的 VM 用不同池
  - 快照大小估算：128 MB RAM 的 VM 快照内存文件约 20-80 MB（取决于脏页率），state 文件 < 1 MB
- **验证**：连续 restore 10 个同快照的 VM，抽查各自生成的 UUID 全不同（ADR-008 安全校验）；restore P95 < 100 ms；pool 被耗尽时降级到冷启动，不报错
- **测试用例**：POOL-001 ~ POOL-010、SEC-010、SEC-011

### Stage 2.3：Per-sandbox 网络命名空间（FR-05，ADR-006 Phase 2 方案）

- **文件**：`crates/clouisle-net/src/{netns.rs, tap.rs, route.rs, dns_proxy.rs, nftables.rs}`
- **具体逻辑**：
  - 创建沙盒时：`unshare(CLONE_NEWNET)` 创建 netns，用 `netns_path = /run/clouisle/netns/<sbx-id>`（持久 bind mount）；在 netns 内创建 TAP `tap0`，guest 侧 IP 固定 `10.0.0.2/30`，host 侧网关 `10.0.0.1/30`；veth pair 桥接到宿主机（`veth-<sbx-id>-in/out`），宿主机侧做 SNAT
  - nftables 规则（netns 内）：`table ip filter { chain forward { type filter hook forward priority 0; default drop; ip daddr @allowed_egress accept } }`；默认拒绝入站；允许 DNS 查询到 `10.0.0.1:53`
  - DNS 代理（`dns_proxy.rs`）：绑定 `10.0.0.1:53`（host netns 中但在 netns 外用 `SO_BINDTODEVICE`，或直接在 netns 内 fork 子进程）；按 `allow_egress` 列表解析，成功 → 写 nftables dynamic set `@allowed_egress`（`nft add element`）并设 TTL；CNAME 链展开全部 A 记录
  - **`allow_egress: []`（空）**：不起 DNS 代理，nftables 拒绝全部出站——纯离线沙盒
  - 清理：sandbox 销毁时删除 veth pair、netns bind mount（内核自动回收 netns 中所有资源）
- **验证**：沙盒 A 内 `curl pypi.org` 成功；沙盒 B 内 `curl 8.8.8.8` 失败（若不在白名单）；宿主机 tcpdump 在沙盒 A 的 TAP 上不见沙盒 B 的流量
- **测试用例**：NET-001 ~ NET-012、SEC-005 ~ SEC-008

### Stage 2.4：文件传输 API（FR-07）

- **文件**：`crates/clouisle-api/src/handlers/files.rs`、`crates/clouisle-agent/src/files.rs`
- **具体逻辑**：
  - `POST /api/v1/sandboxes/{id}/files/upload?path=/workspace/file.txt`：multipart/form-data 或 raw body；通过 vsock 发 `WriteFile{path, mode, content}` 帧；guest agent 写文件（若目录不存在则 `mkdir -p`）；**大小限制**：单次 50 MB（可配置）
  - `GET /api/v1/sandboxes/{id}/files/download?path=/workspace/output.txt`：guest 读文件通过 vsock 流式返回；`Content-Disposition: attachment`；**大小限制**：单次 100 MB
  - `GET .../files/ls?path=/workspace`：返回 `[{name, size, mode, mtime, is_dir}]`；不递归（客户端自行遍历）
  - vsock 协议扩展：新增 `WriteFile`、`ReadFile{path, offset, length}`、`ListDir{path}` 帧类型
- **验证**：上传 → exec cat → 内容一致；下载宿主机没有的文件 → guest 返回 ENOENT → API 返回 404；上传 51MB 文件 → 413
- **测试用例**：FILE-001 ~ FILE-008

### Stage 2.5：资源限制完整实现（FR-04）

- **文件**：`crates/clouisle-vmm/src/firecracker/rate_limiter.rs`、`crates/clouisle-net/src/shaper.rs`
- **具体逻辑**：
  - CPU 精确配额：Phase 1 通过 jailer `cpu.max`；Phase 2 补充 Firecracker 的 balloon device 用于内存动态调整（Phase 3 warm pool 用，先实现）
  - 磁盘 IO：Firecracker rate_limiter for block devices（`bandwidth: {size, one_time_burst, refill_time}` + `ops: {...}`）；Phase 2 target：磁盘 IO 500 IOPS / 50 MB/s（可配置）
  - 网络带宽：Firecracker rate_limiter for network interfaces；`bandwidth_mbps` 从 spec 读，换算为 token bucket 参数
  - **实时热更新**：`PATCH /drives/{id}` 和 `PATCH /network-interfaces/{id}` 支持运行中更新限速（Firecracker 支持），暴露为 `POST /sandboxes/{id}/resources`
- **验证**：guest 内 `dd if=/dev/urandom of=/dev/null bs=1M count=1000 | pv` 观察到 ≤ 配置的带宽；网络 `iperf3` 观察到 ≤ `bandwidth_mbps`
- **测试用例**：RES-007 ~ RES-012

### Stage 2.6：快照与恢复 API（FR-09）

- **文件**：`crates/clouisle-api/src/handlers/snapshot.rs`、`crates/clouisle-store/src/snapshots.rs`
- **具体逻辑**：
  - `POST /api/v1/sandboxes/{id}/snapshots`：pause → `PUT /snapshot/create {kind: Full}` → 存元数据（snapshot_id, sandbox_id, created_at, state_path, mem_path, size） → resume → 返回 snapshot 对象；`kind: Diff`（增量）在 Phase 3 补全
  - `POST /api/v1/sandboxes/{id}/restore`：从 snapshot_id 恢复；**clone 语义**（原沙盒继续运行，restore 出新沙盒）vs **in-place**（原沙盒回滚）；Phase 2 实现 clone 语义
  - 状态约束：`Paused` 期间拒绝 exec 请求（返回 409 with `Retry-After` header）
  - 快照存储路径：`<snapshot_base_dir>/<snapshot_id>/{vm.snapshot, vm.mem}`；默认在宿主机本地（Phase 3 支持远程存储）
- **验证**：沙盒内写文件 → 快照 → exec 删除文件 → restore → 文件还在；快照元数据持久化，apiserver 重启后仍可 restore
- **测试用例**：SNAP-001 ~ SNAP-008

---

## 测试策略

**回归范围**：Phase 1 全部 API-*、EXEC-* 用例在引入网络命名空间后必须全部重跑（网络配置变化可能影响 vsock 路由）。

**安全测试重点**：
- NET-008：沙盒内直连 IP 字面量（绕过 DNS）→ 被 nftables drop
- SEC-010~011：两个 restore 自同一快照的 VM，UUID / random bytes 不同
- SEC-005：沙盒间网络完全隔离

---

## 风险与缓解

| 风险 | 缓解 |
|------|------|
| `mke2fs -d` 构建大镜像（>1GB）耗时长 | 后台异步构建 + 进度 SSE；考虑 `mkfs.ext4` + loopback 并行构建优化 |
| Firecracker 快照的 UFFD 在不同内核版本行为不一致 | Phase 2 先用 File 模式（简单稳定），UFFD 做 Phase 3 可选优化，测试时 pin 内核版本 |
| DNS 代理被 guest 绕过（直接写 `/etc/resolv.conf`） | guest 内 resolv.conf 由 init 在 pivot_root 后立即写死，overlay upper 中写入；guest 进程可以修改，但修改是在 scratch 里，只影响自己；DNS 代理在**宿主机侧**做 nftables 规则，guest 绕过 DNS 后 IP 不在 `@allowed_egress` 里，连接仍被 drop |
| netns + nftables 操作需要 CAP_NET_ADMIN | apiserver 以 `cap_net_admin` + `cap_net_raw` 运行（不需要 root），Phase 3 用 jailer 进一步限制 |
