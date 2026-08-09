# 生产化改造

1. [x] Firecracker VMM 完整集成（HTTP API + vsock + 真实启动）
2. [ ] OCI 镜像拉取 + rootfs 构建
3. [ ] Host vsock 连接器
4. [ ] Guest agent vsock 绑定
5. [ ] DNS 代理
6. [ ] 移除 Mock + 清理
7. [ ] 端到端验证

See `docs/plan/production-ready.md` for details.

## Production Completeness

1. [ ] Create executable production topology
2. [ ] Enforce authentication and tenant authorization
3. [ ] Connect OCI image build pipeline
4. [ ] Complete snapshot restore lifecycle
5. [ ] Implement real gRPC scheduling path
6. [ ] Replace mock streaming behavior
7. [ ] Validate deployment manifests
8. [ ] Run production acceptance suite

See `docs/plan/production-completeness.md` for the detailed design, contract, validation matrix, risks, and rollback plan.

## 服务器全量验收测试

1. [x] 盘点当前 HTTP、CLI、SDK、gRPC、VMM、网络和存储功能
2. [x] 编写逐参数、逐功能服务器测试计划
3. [ ] 在 `root@10.144.144.2` 执行环境、API、VMM、网络、存储和清理验收
4. [ ] 执行 Rust/Python/TypeScript/JavaScript SDK 远程交叉验证
5. [ ] 输出测试报告、缺陷清单和最终 PASS/FAIL 结论

See `docs/plan/server-comprehensive-test-plan.md` for the complete test matrix and evidence requirements.

## Container-only Runtime

1. [x] Remove native runtime release artifacts
2. [x] Replace native operator commands with Docker commands
3. [x] Run Docker-only Python-and-Node KVM acceptance

See `docs/plan/container-only-runtime.md` for the runtime boundary, implementation stages, validation, and rollback plan.

## Cross-platform Docker Development Backend

Planning complete; implementation not started.

1. [ ] Establish explicit Firecracker and DockerDevVmm backend selection
2. [ ] Reuse injected guest agent for Docker development sandboxes
3. [ ] Implement DockerDevVmm lifecycle, networking, limits, and cleanup
4. [ ] Ship standalone Docker Desktop and WSL2 development Compose topology
5. [ ] Validate Docker development and Firecracker KVM regression matrices
6. [ ] Document capability boundaries, security, rollout, and rollback

See `docs/plan/cross-platform-docker-dev-vmm.md` for the requirements, traceability matrix, contracts, staged plan, acceptance cases, and risks.

## Hosted Multi-architecture Build Optimization

1. [x] Refactor Docker dependency layers with cargo-chef
2. [x] Build amd64 and arm64 concurrently on native GitHub-hosted runners
3. [x] Cache BuildKit output independently per architecture
4. [x] Merge immutable digests into versioned and latest manifests
5. [x] Reuse Cargo work across Linux CI checks
6. [x] Verify optimized Docker build and release workflows


## E2B 兼容与运行可靠性

1. [ ] 修复缺失镜像导致的创建超时
2. [ ] 实现异步镜像预拉取与持久缓存
3. [ ] 收敛 API 与 Node 的默认部署
4. [ ] 支持创建时初始化命令
5. [ ] 实现 E2B sandbox/envd 协议兼容
6. [ ] 实现沙盒故障恢复与重启策略
7. [ ] 自动同步节点/服务重启后的历史状态
8. [ ] 补齐关键路径测试并完成端到端验收

See `docs/plan/e2b-compatible-reliability.md` for the detailed design, contracts, validation matrix, risks, and rollback plan.
See `docs/plan/hosted-multiarch-build-optimization.md` for the baseline, hosted-runner topology, cache design, validation, and rollback plan.