# 本地开发

## 环境要求

- Rust（edition 2021）、`cargo`；Linux 需要 `/dev/kvm` + Firecracker（FC 验证）
- `rustup target add x86_64-unknown-linux-musl`（guest agent 交叉构建）
- Docker（docker-dev 后端验证）

## 构建

```bash
cargo build --workspace                    # 全部二进制（debug）
cargo build --release --workspace          # 发布

# guest agent（musl 静态，注入 rootfs）
cargo build --target x86_64-unknown-linux-musl -p clouisle-agent
cp target/x86_64-unknown-linux-musl/debug/clouisle-agent /usr/local/bin/clouisle-agent

# 交叉编译（arm64 验证）
cargo build --workspace --target aarch64-unknown-linux-gnu
```

## 运行

```bash
# 单节点（firecracker，需 KVM）
CLOUISLE_API_KEYS="dev-key:dev:full" \
cargo run -p clouisle-api -- --addr 127.0.0.1:8080 \
  --db /tmp/clouisle.db \
  --kernel /opt/clouisle/vmlinux-vsock --images-dir /opt/clouisle/images \
  --api-socket-dir /tmp/clouisle-fc

# docker-dev 后端（需 Docker daemon + /var/run/docker.sock）
CLOUISLE_API_KEYS="dev-key:dev:full" \
cargo run -p clouisle-api -- --backend docker-dev --addr 127.0.0.1:8080 \
  --db /tmp/clouisle.db --api-socket-dir /tmp/clouisle-fc

# 节点守护
cargo run -p clouisled -- --addr 0.0.0.0:9090 --node-id node-1 \
  --kernel /opt/clouisle/vmlinux-vsock --images-dir /opt/clouisle/images
```

## 测试

```bash
cargo test --workspace                 # 全量单元测试（206 个）
cargo test -p clouisle-api --features test-utils --tests   # API gated 套件（79 个）
cargo clippy --workspace --all-targets # 零警告
cargo fmt --all                        # 格式化

# SDK 套件
cd sdk/typescript && npm test
cd sdk/python && .venv/bin/python -m pytest tests -q

# 服务器全量验收（KVM）——见 docs/plan/server-comprehensive-test-plan.md
# 快照/Fork API 验证、故障注入：scripts/kvm-fault-injection.sh
```

## 代码结构

```
crates/
  clouisle-api/     控制平面（handlers/state/auth/e2b/e2b_cloud/agent/node_client）
  clouisle-agent/   guest agent（serve 帧协议 / init 网络 / limits cgroup）
  clouisle-vmm/     Vmm 抽象 + firecracker + docker_dev + docker_engine
  clouisle-net/     netns / nftables / DNS 代理 / 子网分配
  clouisle-images/  OCI 拉取 / rootfs 构建 / agent 注入
  clouisle-store/   SQLite / PostgreSQL
  clouisle-proto/   帧协议 codec
  clouisle-scheduler/ 资源池
  clouisle-pool/    预热池
  clouisled/        节点守护（gRPC + 本地 FC 管理）
  clouislectl/      CLI
sdk/
  rust/ python/ typescript/   官方 SDK
deploy/                       生产 K8s manifest（RBAC/apiserver/postgres/networkpolicy/multinode）
scripts/                      e2b-sdk-acceptance.py / kvm-fault-injection.sh / gen-openapi.py
spec/openapi.json             OpenAPI 3.0（脚本生成）
```

## 工作流提示

- **帧协议**（agent ↔ API）：`clouisle-proto` 的 `Frame`；新增消息要同步 `codec` roundtrip 测试。
- **新增后端**：实现 `Vmm` + `AgentConnector`，`main.rs` 选择逻辑，`state.rs` 网络归属。
- **新增 spec 字段**：`clouisle-core` 定义 + `validate` 校验 + `handlers` 透传 + 验收矩阵用例。
- **数据库变更**：SQLite/PostgreSQL 两份 schema（`clouisle-store`）同步 + 迁移向前兼容。
- **变更验证**：跑全量回归（上表）+ 对应 `docs/plan/` 验收脚本；不要跳过 clippy。

## 故障排查

| 现象 | 检查 |
|---|---|
| 创建 507 RESOURCE_EXHAUSTED | 资源池被旧沙盒占满；`GET /api/v1/sandboxes` 清理 |
| agent hello timeout | 内核/镜像是否就绪；netns/tap 是否建立；`start_timeout_secs` 是否过短 |
| 快照恢复后崩溃 | 换快照兼容内核（vmlinux-vsock）；见 docs/plan/snapshot-warm-pool.md |
| docker-dev 连不上容器 | API 容器必须在 `clouisle-dev-mgmt` 网络内 |
| 端口发布无效 | internal 网络禁止 -p；API 需同时连 default 网络 |
