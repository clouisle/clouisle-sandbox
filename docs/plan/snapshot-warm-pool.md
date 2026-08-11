# 快照预热 Fast Path 设计文档

## Background & Goals

冷启动（镜像拉取 → rootfs 构建 → Firecracker 引导 → agent 就绪）在镜像已缓存时
仍需要数秒。目标：对常用模板**预建 FC 快照**，创建时走 `/snapshot/load` 快路径，
跳过内核引导与 agent 冷启动。

## 障碍（已实证）

- guest IP（`10.{a}.{b}.2`）固化在快照内存中；FC 后端的
  `supports_detached_warm_pool` 因"预创建 VM 无法安全重分配给其他沙盒身份"
  返回 `false`（firecracker.rs:848 注释）。
- 现网段由 `sandbox_id` SHA-256 哈希派生（`netns.rs::sandbox_subnet`），
  快照恢复到一个新 sandbox 会使用不同网段 → guest IP 与 netns 不匹配。

## 方案：快照继承子网

1. **子网分配池**（`clouisle-net::SubnetAllocator`）：顺序分配 `10.{a}.{b}.0/30`
   （a/b 从 10 递增），替换"id 哈希派生"仅用于快照预热路径；普通冷创建保持
   id 派生（向后兼容）。
2. **快照预热**（API 层后台任务）：为模板分配子网 →
   `vmm.create_in_subnet`（boot cmdline 用该子网）→ start → agent hello →
   pause → `snapshot(Full)` → stop → 记录 `{pool_key, paths, subnet}`。
   快照与子网一对一绑定。
3. **创建快路径**：无 warm slot 且快照池命中 → 占用快照 → 新 sandbox 的
   netns 使用**继承子网**（而非 id 派生）→ `restore` → 网络 setup 与
   agent 连接均用继承子网。快照→sandbox 一对一，无并发冲突。
4. **释放**：sandbox 删除 → 快照回池（busy=false）或丢弃；子网随快照生命周期。

## 改动面

- `clouisle-net`：`SubnetAllocator`（allocate/release）。
- `clouisle-vmm`：`Vmm::create_in_subnet`（trait 默认委托 `create`）；
  Firecracker 实现用显式子网构造 boot args。
- `clouisle-api`：`state.snapshots` 预热池；
  `provision_sandbox` 快照命中分支（继承子网网络 + agent 连接）；
  删除释放；`warm_persisted_templates` 增强为快照预热。
- `crates/clouisle-api/src/agent.rs` / firewall：guest_ip/gateway 改用
  继承子网（经 sandbox 的 vmm_meta.extra["subnet"] 或显式传递）。

## 验证

- 单元：SubnetAllocator 不重、create_in_subnet 默认委托。
- KVM：预热快照 → 创建 → running + exec；同模板并发两实例（不同快照子网）
  网络互不干扰；删除释放后再次创建可用。
- 回归：冷创建路径（id 派生网段）行为不变。

## 实现状态（2026-08-11）

已实现并通过单测/回归：

- `clouisle-net::SubnetAllocator`：顺序子网分配（`netns.rs`），单元测试覆盖递增/回绕。
- `Vmm::create_in_subnet`（trait 默认委托 `create`）；Firecracker 用显式子网构造
  boot cmdline（`boot_args_for_subnet`）。
- `FirewallManager::create_network_in_subnet`、`netns::create_netns_in_subnet`。
- `VmHandle.subnet` 字段（serde default），`VsockAgentConnector` 优先用显式子网，
  `meta_to_handle` 从 `vmm_meta.extra["subnet"]` 恢复。
- AppState 快照预热池：`warm_snapshot`（create_in_subnet → start → hello →
  断开 → 静默 5s（guest 进入安静期，早期快照恢复出崩溃内核栈）→ pause →
  Full 快照 → stop → 清理临时 netns）、`claim_snapshot`、`release_snapshot`。
- `provision_sandbox` 快路径：无 warm slot 时认领快照、继承子网建 netns、restore
  （`/snapshot/load` 前不配置任何设备，FC v1.10 会拒绝）、跳过重复 `start`。
- reconcile 构造 handle 时从 `vmm_meta.extra["subnet"]` 恢复继承子网
  （guest probe 用正确 IP，避免误标 Error 后重跑 provision 冲突）。
- 预热 VM 不带 vsock（FC 恢复无法重配 vsock 设备，快照内固化路径多 clone 冲突；
  agent 走 TCP 不依赖 vsock）。
- `warm_persisted_templates` 对持久化模板同步预热快照。
- sandbox 删除释放快照回池。

## 已验证障碍（KVM，FC v1.10.1）——决定性复现与突破

自定义 guest 内核（`/opt/clouisle/vmlinux`，7.0.0-28）恢复后确定性崩溃：

```
BUG: TASK stack guard page was hit at ffffc900001cbff8
Oops: stack guard page: 0000 [#1] SMP NOPTI
RIP: 0010:error_entry+0x17/0x140
```

**最小复现（firecracker CLI 直连，绕开 clouisle 代码）**：完整 netns（br0 +
tap0 + veth）克隆场景——恢复后 agent TCP 3s 内可达（`OK`），+23s 内核崩溃。
结论：FC v1.10 snapshot（dev-preview）与自定义 7.0 内核不兼容，与 clouisle
代码无关（恢复、网络、vsock 路径均已单独验证正确）。

**突破**：改用 `/opt/clouisle/vmlinux-vsock`（Linux 4.14.193，FC 时代早期
内核）后，同一最小复现恢复后 +65s agent TCP 稳定、无崩溃。CPU 模板路线
不可行（FC v1.10 中 restore 前任何 API 配置——含 machine-config——都被拒）。

## KVM 端到端验证（2026-08-11，4.14 内核）

- 预热：模板持久化后 `warm_snapshot`（agent TCP 就绪 → 断开 → 静默 5s →
  pause → Full 快照 → stop → 临时 netns 清理）成功。
- 快路径：create 201 **0.19-0.21s**（冷创建 ~30x 加速）；`vmm_meta.extra.subnet=10.10`
  （继承预热子网）；恢复后 exec 连续成功（+25s/+45s 均 exit 0），guest 稳定。
- 快照回池：删除释放快照 → 下次 create 复用（0.19s）。
- fallback：快照被占用时 create 正确走冷创建（vsock 路径 1.22s）。
- 修复的并发 bug：reconcile 构造 handle 时未从 `vmm_meta.extra["subnet"]` 恢复
  继承子网 → guest probe 连错 IP 误标 Error → restart policy 重置 Starting →
  重跑 provision veth 冲突 → 沙盒被毁。已修复（`state.rs` reconcile handle 恢复
  subnet），修复后恢复沙盒跨 reconcile 周期存活（exec+45s 验证）。

**前提**：快照预热必须使用与 FC snapshot 兼容的 guest 内核（4.14 经实测稳定；
7.0 自定义内核存在 FC dev-preview 恢复崩溃 bug）。冷创建路径完全不受影响
（回归 199+79 测试全绿，clippy 0，TS/Python SDK 全过）。
