# 安全专项验收报告

- 日期：2026-08-11
- 范围：认证/授权、租户隔离、网络隔离、数据面隔离、注入安全、清理、逃逸边界、依赖安全
- 依据：`docs/plan/server-comprehensive-test-plan.md` 与 `docs/plan/server-acceptance-report.md` 的实测证据

## 1. 认证与授权（11 用例全过）

| 项 | 结果 |
|---|---|
| 无 header / 空 Bearer / 错误 key / Basic scheme | 401 UNAUTHENTICATED |
| 多余空格 / 小写 scheme | 401（严格 header 解析） |
| full key 读写 | 200 |
| read key 读 / 写 | 200 / 403 |
| health/metrics 免认证 | 200（可观测性端点） |
| API key 单向散列存储（e2b_cloud） | 单测验证 |

**缺口**：本地 API 单租户——read key 可读任意 sandbox 元数据（`require_tenant` 仅 e2b_cloud 路径）。多租户部署需按 team 过滤 sandbox 查询。

## 2. 网络隔离（KVM 实测）

| 项 | 结果 |
|---|---|
| 空 allowlist 出站 | 拒绝（wget rc=1） |
| 白名单域名 | 可达（DNS 代理放行） |
| 非白名单域名 / 直接 IP | 拦截（DNS 拒绝 + nft drop） |
| 跨沙盒 guest 互访 | 隔离（A→B 连接失败） |
| host 入站非 agent 端口 | 拒绝 |
| network.enabled=false | 完全离线（FC）/ mgmt-only（docker-dev internal 网络） |
| 删除清理 | netns/veth/nft 零残留 |

## 3. 数据面隔离（修复后实测）

| 项 | 机制 | 验证 |
|---|---|---|
| rootfs | 每沙盒独立 ext4 副本（FC drive 指向副本） | A 填满 rootfs 不影响 B |
| 进程数 | guest cgroup v2 pids.max | fork 被拒（内核日志） |
| 带宽 | host netns tc tbf | 1Mbit qdisc |
| IOPS | host cgroup v2 io.max | 代码生效；本环境 io 控制器不暴露 nvme（受限 cgroup 视图，记录） |
| 密钥 | agent hello 后写 /run/secrets（600） | 注入精确 + 响应 REDACTED 无泄露 |
| 路径安全 | traversal/编码/控制字符拒绝 | 上传下载/镜像引用全矩阵 |

## 4. 注入与输入安全

- `image.reference` 控制字符（NUL）拒绝（修复）
- mount/secret 路径穿越拒绝（`..`、相对、越界）
- init_cwd 绝对路径 + 无穿越校验
- 非 JSON / 数组 / 重复字段 / 未知字段：4xx 或明确 serde 行为，不 panic
- exec 超时后进程组回收

## 5. 清理（零残留）

| 资源 | 删除后 |
|---|---|
| netns / veth / nft 表 | 0 |
| Firecracker 进程 / socket | 0 |
| rootfs 副本 | 0 |
| io cgroup | 0 |
| docker-dev 容器 | 0（标签 `com.clouisle.managed=true` 回收） |

## 6. 逃逸边界

- **FC 沙盒**：独立微VM（独立内核），与宿主隔离；数据面经 netns 隔离；无 docker socket（生产 manifest 扫描确认）
- **docker-dev**：显式开发后端（文档声明 Docker socket = host 等效权限）；沙盒容器非 privileged、无 host PID/IPC/network；mount 仅限配置根目录且只读
- 快照克隆 rootfs 共享：FC dev-preview 已知限制（记录）

## 7. 依赖安全

- RUSTSEC-2026-0119（hickory）：修复升级 0.26
- RUSTSEC-2024-0436（paste）：记录豁免（`.cargo/audit.toml`，netlink 生态无替代）
- CI audit 门禁 job

## 结论

生产路径（FC/KVM）安全边界实测有效；docker-dev 边界文档化（开发专用、非生产语义）。缺口（单租户过滤、io 控制器设备暴露）已记录并有明确修复路径。
