# Changelog

## 0.1.0 (unreleased)

- 初始版本：微VM 沙盒调度系统
- 基于 Firecracker + KVM 的 microVM 隔离
- HTTP API（沙盒 CRUD / 命令执行 / 文件传输）
- 认证（API key + 租户隔离 + scope 校验）
- 审计日志（哈希链 + Ed25519 签名）
- 网络隔离（per-sandbox netns + nftables 默认 drop + 出站白名单）
- 资源准入（Semaphore RAII，无超卖）
- 资源准入在 API 与 clouisled 节点边界正确隔离，并支持重启恢复
- 沙盒 TTL 从 Running 状态开始计时，并在 SQLite/PostgreSQL 中持久化
- Kubernetes liveness and readiness probes are explicitly unauthenticated
- 多节点调度（Filter + Score 两阶段）
- 控制平面（PostgreSQL 存储，HA 就绪）
- 节点代理（clouisled gRPC 服务 + 心跳 + reconciler）
- Docker 部署（docker-compose + Dockerfile）
- K8s 部署（DaemonSet + RBAC + NetworkPolicy）
- CI/CD（GitHub Actions：lint / test / build / release）
- Runtime delivery is container-only: releases publish OCI images, and operator CLI commands execute through Docker Compose.
- The static guest agent configures its management NIC through netlink, so OCI application images do not need `ip` or `ifconfig` binaries.