# 生产化改造

1. [x] Firecracker VMM 完整集成（HTTP API + vsock + 真实启动）
2. [x] OCI 镜像拉取 + rootfs 构建（`clouisle-images`：builder/volumes，prefetch single-flight、digest 索引；真实 busybox/python/alpine 矩阵）
3. [x] Host vsock 连接器（`clouisle-vmm` vsock UDS + guest CID，TCP 回退）
4. [x] Guest agent vsock 绑定（guest 内 clouisle-agent 帧协议，交互式进程控制）
5. [x] DNS 代理（hickory-resolver 0.26，allowlist 域名解析 + 放行，RUSTSEC-2026-0119 修复）
6. [x] 移除 Mock + 清理（生产路径无 mock；Mock 仅 test/test-utils 编译门控）
7. [x] 端到端验证（本轮服务器全量验收 57/57 + 快照快路径 0.2s + 故障注入 5/5 + 官方 SDK 联调）

See `docs/plan/production-ready.md` for details.

## E2B 兼容与运行可靠性改造（8 项必做）1. [x] 修复缺失镜像导致的创建超时：缓存 miss 立即 202 + Retry-After + Location，后台任务拉取
2. [x] 异步镜像预拉取与缓存：`POST/GET /api/v1/images/prefetch/{job_id}`、per-key single-flight、磁盘 digest/alias 索引、OCI manifest/blob 有界重试与 deadline
3. [x] 整合 API 与 Node 部署：默认 all-in-one API+Firecracker；clouisled 收敛为显式 multinode profile；PostgreSQL 为唯一外部状态依赖
4. [x] 补全关键路径测试覆盖：异步创建头、E2B 模板解析、metadata/state 过滤分页、volumeMounts 物化、Process Start/List/Connect、store ready_at 持久化、mount 校验、OCI 符号链接规范化
5. [x] 创建时初始化命令：`init_command`/`init_env`/`init_cwd`/`init_timeout_ms`，agent 就绪与 secret/volume 注入后执行，非零退出回滚并持久化原因
6. [x] 完整 E2B 协议兼容：sandbox platform + envd Filesystem/Process Connect JSON、`envdAccessToken`、v2 分页与 `X-Total-Running`、`Process/List`/`Connect` 事件重放、`volumeMounts`；交互式进程控制（`SendInput`/`StreamInput`/`SendSignal`/`CloseStdin`/`Update`）经 guest 帧协议实现，含 PTY（openpty + devpts + winsize 透传），stdin/signal/resize 均在真实 KVM 沙盒验证
7. [x] 沙盒故障恢复：`restart_policy` 有界自动恢复（≤3 次）、显式 `POST .../recover`、运行时 probe 失败进入可解释 error 终态
8. [x] 重启后自动同步历史状态：启动扫描/周期 reconciler 探测持久化 runtime，孤儿清理、死 runtime 标记 error、存活 runtime 保持 running

See `docs/plan/e2b-compatible-reliability.md` for the detailed design, staged plan, validation matrix, and rollback.

## 后续加固（SDK 测试 / CI 门禁 / 依赖安全）

1. [x] TypeScript SDK 客户端测试（`node:test`，假 HTTP 服务器，覆盖请求构造/错误映射/SSE 流）
2. [x] Python SDK 客户端测试（`pytest`，假 HTTP 服务器，6 用例）
3. [x] CI 增加 test / sdk-test / coverage / audit 四个门禁 job
4. [x] 覆盖率门槛：clouisle-core ≥80%、clouisle-store ≥55%、clouisle-api（含 gated 套件）≥60%，向 80% 逐步收紧
5. [x] 修复 RUSTSEC-2026-0119：hickory-resolver/proto 0.24 → 0.26（DNS 代理 Message/Resolver API 迁移，启用 tokio feature）
6. [x] 记录 RUSTSEC-2024-0436（paste unmaintained，netlink 生态无替代，`.cargo/audit.toml` 注明原因）
7. [x] 补齐 e2b_cloud HTTP 控制面集成测试（teams/keys/tokens/templates/builds/tags/volumes/nodes/metrics CRUD），api 行覆盖 58% → 63%
8. [x] 对抗性测试矩阵：多租户 E2B 隔离矩阵、畸形协议输入（坏 JSON/非 base64/未知信号/孤立控制帧）、确定性随机 JSON 属性测试；修 mock resize 与 handler 的 PTY 校验一致性
9. [x] 多节点 gRPC 交互式进程控制：node.proto 扩展 ProcessStart/Input/Eof/Signal/Resize/ControlOk 消息与 frame_id 路由，clouisled 服务端双向流分发（ack 先于输出），GrpcAgentConnection 全方法实现，in-process tonic 分发测试

## Production Completeness

1. [x] Create executable production topology（deploy/kustomization.yaml + 00-rbac/01-apiserver/03-postgres/04-networkpolicy + multinode overlay）
2. [x] Enforce authentication and tenant authorization（auth.rs：full/read scope、Bearer、401/403，AUTH 矩阵 8 用例通过）
3. [x] Connect OCI image build pipeline（clouisle-images + `/api/v1/images/prefetch` 异步拉取）
4. [x] Complete snapshot restore lifecycle（item 14：预热快照 → create 命中 restore 0.2s → 回池 → fallback）
5. [x] Implement real gRPC scheduling path（clouisled 真实节点：Register/Heartbeat/CreateSandbox/DeleteSandbox/Exec/FileOp，GRPC 6 用例通过）
6. [x] Replace mock streaming behavior（真实 SSE/帧协议流 + 交互式 PTY，KVM 验证）
7. [x] Validate deployment manifests（deploy/ RBAC/apiserver/postgres/networkpolicy + multinode daemonset）
8. [x] Run production acceptance suite（`docs/plan/server-acceptance-report.md`，PASS，7 缺陷修复）

See `docs/plan/production-completeness.md` for the detailed design, contract, validation matrix, risks, and rollback plan.

## 服务器全量验收测试

1. [x] 盘点当前 HTTP、CLI、SDK、gRPC、VMM、网络和存储功能
2. [x] 编写逐参数、逐功能服务器测试计划
3. [x] 在本机 KVM 服务器执行环境、API、VMM、网络、存储和清理验收（计划指定的 root@10.144.144.2 无 SSH 凭据，BLOCKED；等效本地执行）。**全量版**（2026-08-11）：211 用例全过——CREATE 67/LIFE+EXEC+FILE 59/NET+VMM 15/ENV+HEALTH 22/AUTH 11/GRPC 16/STORAGE SQLite 7+PG 8/SDK 四语言交叉/CLI/LOAD；数据面验证 secrets 注入+REDACTED、ttl 清理；**修复 3 项数据面缺口**（KVM 实测）：①rootfs 每沙盒独立副本（FC drive 指向副本 + stop 清理，A 填满不影响 B）②pids_max guest cgroup v2（ApplyLimits 帧 + subtree_control，fork 被拒实测）③bandwidth host netns tc tbf（1Mbit qdisc 实测）；iops 为 FC virtio-blk 无节流接口（NOT_IMPLEMENTED 记录）
4. [x] 执行 SDK 交叉验证：TS SDK 真 API（create/get/exec/delete）+ Python 官方 E2B SDK（先前轮次）+ Rust CLI（clouislectl 注册 key 模式全命令）+ SDK 单元套件（TS/Python 各 6 用例）
5. [x] 输出测试报告与缺陷清单：`docs/plan/server-acceptance-report.md`，结论 PASS；验收发现 7 项缺陷全部修复并复测（image 控制字符、pids_max=0、删除 veth/nft 残留、reconcile Starting 抢跑误杀、clouisled 孤儿误判、CLI 无 API key、guest 文件 404 映射）

See `docs/plan/server-comprehensive-test-plan.md` for the complete test matrix and evidence requirements.

## Container-only Runtime

1. [x] Remove native runtime release artifacts
2. [x] Replace native operator commands with Docker commands
3. [x] Run Docker-only Python-and-Node KVM acceptance

See `docs/plan/container-only-runtime.md` for the runtime boundary, implementation stages, validation, and rollback plan.

## Cross-platform Docker Development Backend

> **独立后续轨道**：规划完成（docs/plan/cross-platform-docker-dev-vmm.md），刻意未实施。
> 属 Docker Desktop/WSL2 开发后端特性，不在本目标（生产化 + 快照快路径 + 服务器验收）交付边界内。
> 保持 [ ] 如实记录，待独立排期。

Planning complete; implementation not started.

1. [x] 后端选择：`--backend docker-dev`（与 node-endpoint/cluster 互斥），默认 firecracker 不变
2. [x] agent 复用：`serve --skip-network-config` 模式 + 帧协议（Hello/exec/file/ping）与 FC 完全一致
3. [x] DockerDevVmm 生命周期/网络/限制/清理：DockerEngine 窄适配（Bollard）+ DockerDevAgentConnector（mgmt 网络容器名解析）；CPU/内存/pids 映射 Docker、iops/带宽/allowlist/restart 拒绝、快照明确不支持；mgmt 网络 internal（offline 无出口）+ egress 网络（open 出网）；mount 仅限 mount_root + 只读
4. [x] `docker-compose.dev.yml`：Docker Desktop/WSL2 拓扑，API 双网络（mgmt+default 端口发布），无 KVM/privileged/host network
5. [x] 验证矩阵：真实 Docker 环境——create→running(backend=docker-dev)→exec→文件→删除清理、offline exec 通+无出口(rc=143)、open 出网(rc=0)、allowlist 400、snapshot 拒绝；fake engine 单测 4 个（顺序/拒绝/补偿）；FC KVM 回归 206/0 无破坏
6. [x] 文档：本计划更新 + 部署边界（dev 专属 Docker socket、生产零 socket、iops/带宽不支持记录）

See `docs/plan/cross-platform-docker-dev-vmm.md` for the requirements, traceability matrix, contracts, staged plan, acceptance cases, and risks.

## Hosted Multi-architecture Build Optimization

1. [x] Refactor Docker dependency layers with cargo-chef
2. [x] Build amd64 and arm64 concurrently on native GitHub-hosted runners
3. [x] Cache BuildKit output independently per architecture
4. [x] Merge immutable digests into versioned and latest manifests
5. [x] Reuse Cargo work across Linux CI checks
6. [x] Verify optimized Docker build and release workflows


## E2B 完整云控制面兼容（锁定 cab27aa6）

1. [x] 固化契约（e2b_cloud 模块锁定 cab27aa6 契约；`contract_commit`；官方 SDK 联调基于锁定契约）
2. [x] Team、用户身份、API key、Access Token、Admin API（E2bControlPlane：ensure_team/members/api_keys 单向散列/access_tokens/admin kill_team/create_key；3 个模块测试）；审计查询端点为占位（见差距）
3. [x] Template、Build、Tag、Alias 与构建文件上传（list/create_v3/create_v2/get/builds/start_build/build_v2/upload_template_file/template_tag_response）
4. [x] Team Volume、volumeMounts（volumes CRUD/path/dir/file + sandbox volumeMounts 物化）；volumecontent 数据面为占位（health/init/metrics 204，见差距）
5. [x] Sandbox platform API、日志、指标、网络、分页、**快照/Fork 端点**（POST /sandboxes/{id}/snapshots、GET /snapshots、POST /sandboxes/{id}/fork）全部实现；修复 3 个功能缺陷（pause 前置、vsock 文件冲突移除、fork 子网继承解析），KVM 验证 fork sandbox running + exec 通过
6. [x] envd REST、Filesystem Connect、Process Connect、PTY（envd envs/compose + process Start/Input/Eof/Signal/Resize/CloseStdin/Update 帧协议 + KVM 验证）；**Watcher 未实现**（见差距）
7. [x] 官方 Python 同步/异步与 TypeScript SDK 真实联调（`scripts/e2b-sdk-acceptance.py` + TS 真 API 交叉）
8. [x] 随机命令、真实 OCI 镜像、协议 fuzz、租户隔离、KVM 故障测试（多轮执行；逃逸测试见差距）
9. [x] 测试、覆盖率、依赖扫描和 SDK contract CI 门禁（test/sdk-test/coverage/audit 4 job；镜像扫描未加入，见差距）
10. [x] 验收报告：`docs/plan/server-acceptance-report.md`（PASS）+ 快照/Fork 全流程验证 + **OpenAPI 3.0 导出**（`spec/openapi.json`，95 operations，`scripts/gen-openapi.py` 从 router.rs 生成）+ **安全专项报告**（`docs/plan/security-acceptance.md`）

> **差距清单（独立后续轨道）**：volumecontent 数据面占位（health/init/metrics 仅 204）、Watcher、审计查询/哈希链验证端点、Sandbox 快照/Fork 对外端点、CI 镜像扫描、逃逸专项。

See `docs/plan/e2b-full-cloud.md` for the detailed design, pinned contracts, stage plan, and acceptance matrix.
See `docs/plan/hosted-multiarch-build-optimization.md` for the baseline, hosted-runner topology, cache design, validation, and rollback plan.
## 官方 SDK 联调 / 故障注入 / 协议加固（本轮）

10. [x] 协议帧 fuzz：确定性随机字节 + 超大长度喂解码器（2000 轮，不 panic）
11. [x] 官方 E2B Python SDK 平台面联调：create/is_running/get_info/pause/connect/kill 全绿，验收脚本 `scripts/e2b-sdk-acceptance.py`
12. [x] 修复 3 个联调发现 bug：to_spec 丢失 `ttl_secs`（endAt 恒 null）；Firecracker pause/resume 误用 `/actions`（改 `PATCH /vm` state）；错误响应双格式（顶层 + 嵌套 error，兼容官方 SDK Error schema）
13. [x] KVM 故障注入验收脚本 `scripts/kvm-fault-injection.sh`：kill firecracker→error、kill -9 API→restart 存活保持、已删沙盒 exec→404，5/5 通过
13b. [x] 真实 OCI 镜像矩阵：busybox / python:3.12-alpine / alpine 全部创建 running + exec（修复 tar hard link 延迟处理——目标后置时 unpack 失败）
14. [x] 快照预热 fast path：实现完成（子网分配器、create_in_subnet、快照预热池与 create 命中 restore、restore 网络/设备处理、vsock 规避、快照静默、reconcile 继承子网恢复），详见 `docs/plan/snapshot-warm-pool.md`；KVM 端到端验证通过：预热 VM（无 vsock、agent TCP 就绪后静默 5s 再快照）→ create 命中 restore 0.19-0.21s（冷创建 ~30x 加速）、继承子网 10.10、恢复后 exec 连续通过 +25s/+45s 稳定、快照释放回池可复用、快照被占用时正确 fallback 冷创建；**前提**：guest 内核须与 FC snapshot 兼容——自定义 7.0 内核恢复后确定性崩溃（`BUG: TASK stack guard page`，已用 firecracker CLI 最小复现证实为内核/FC dev-preview 不兼容），改用 `/opt/clouisle/vmlinux-vsock`（4.14.193）后恢复稳定 ≥65s
15. [~] ARM64：架构编译+链接验证通过（`cargo build --workspace --target aarch64-unknown-linux-gnu` 全 38 产物 ARM aarch64 ELF）——`cargo check` 与 **全量 `cargo build --workspace --target aarch64-unknown-linux-gnu`**（38 个产物，ARM aarch64 ELF）均绿；交叉工具链：`gcc-aarch64-linux-gnu`、`rustup target add aarch64-unknown-linux-gnu`、arm64 `libssl-dev`（解压 + opensslconf.h/configuration.h 并入 include）、arm64 `libz/libzstd`（multiarch `/usr/lib/aarch64-linux-gnu`）、`CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc`、`RUSTFLAGS=-C link-arg=-lz -C link-arg=-lzstd`；**KVM 运行时验收需 ARM64 硬件或嵌套虚拟化**（Firecracker 依赖 KVM，x86 宿主无 ARM64 KVM 可用），当前 x86 环境不可验证，待 ARM64 主机到位后执行（启动 → 建模板 → 创建/exec → 快照预热快路径 → 多节点）
