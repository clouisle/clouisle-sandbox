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