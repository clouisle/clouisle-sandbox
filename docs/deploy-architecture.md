# 部署架构决策

## 选型：K8s DaemonSet + Pod 内进程

**决策**：clouisled 作为 DaemonSet 部署，Pod 内直管 firecracker 进程。

```
┌────────────────────── K8s 集群 ──────────────────────┐
│                                                       │
│  Deployment: apiserver (副本×2, 无状态)               │
│    └─ HTTP API / 调度 / 存储 → PostgreSQL              │
│                                                       │
│  DaemonSet: clouisled (每节点一个 Pod)                 │
│    └─ Pod 内: [clouisled] + [firecracker 进程们]       │
│        沙盒 A  沙盒 B  沙盒 C  (Pod 内多进程)          │
│                                                       │
│  StatefulSet: postgres (控制平面共享状态)               │
│                                                       │
│  NetworkPolicy: 默认拒绝跨命名空间                      │
└───────────────────────────────────────────────────────┘
```

## 关键决策

| 决策 | 选型 | 原因 |
|------|------|------|
| 沙盒管理方式 | **Pod 内进程**（clouisled 直管 firecracker） | 保持 ~150ms 冷启动延迟，单机 100+ 密度 |
| 部署形态 | **DaemonSet** | 每节点一个 Pod，天然亲和宿主机设备 |
| KVM 透传 | **privileged + hostPath** | `/dev/kvm` 直通，firecracker 必要 |
| 沙盒 K8s 权限 | **`automountServiceAccountToken: false`** | 沙盒无 K8s token，逃逸后无法操作集群 |
| 控制平面存储 | **PostgreSQL**（StatefulSet） | 多实例 apiserver 共享状态源 |
| 控制平面状态 | **无状态** | 通过 gRPC 转发到 clouisled，不持有 VMM 引用 |
| 节点发现 | **clouisled 心跳上报** | 每 3s 上报资源/沙盒数，apiserver 维护节点列表 |

## 节点代理架构

```
clouisled (DaemonSet Pod)
  │
  ├─ gRPC Server (监听 clouisled:9090)
  │   ├─ Register(NodeInfo) → NodeId
  │   ├─ Heartbeat(stream NodeStatus) → stream Command  (双向流)
  │   ├─ CreateSandbox(SandboxSpec) → SandboxHandle
  │   ├─ DeleteSandbox(id) → ()
  │   └─ Exec(stream ExecReq) → stream ExecResp
  │
  ├─ FirecrackerVmm (本机)
  │   ├─ create / start / stop / stats
  │   └─ 进程组管理 (killpg 无孤儿)
  │
  ├─ Reconciler (每 10s)
  │   ├─ DB 有、进程无 → 标记 error
  │   ├─ 进程有、DB 无 → 孤儿，杀掉
  │   └─ 状态不符 → 回写 DB
  │
  └─ FirewallManager (本机)
      ├─ 创建 netns + nftables 规则
      └─ 删除时清理
```

## 安全设计

| 层 | 措施 |
|----|------|
| **Pod 权限** | `privileged` 仅 clouisled Pod，firecracker 进程无 K8s token |
| **沙箱隔离** | 独立 microVM（seccomp + jailer + cgroup + netns） |
| **控制平面** | API key（Bearer token）+ 租户隔离 + scope 校验 |
| **审计** | 哈希链 + Ed25519 签名，篡改可检出 |
| **网络** | NetworkPolicy 默认拒绝跨命名空间 |

## 高可用架构

| 组件 | 高可用方式 | 状态 |
|------|-----------|------|
| apiserver | Deployment 多副本，PostgreSQL 共享状态 | **待实现**（当前用 SQLite + 内存池） |
| clouisled | DaemonSet 每节点一个，节点故障时该节点沙盒标记 error | **待实现**（心跳超时未接入） |
| postgres | StatefulSet 或云服务（RDS） | **待实现**（`PostgresStore` 代码已有） |
| 资源调度 | 乐观锁：`UPDATE nodes ... WHERE ... RETURNING` | **待实现**（当前进程内 Semaphore） |
| 健康检查 | `/health/live` + `/health/ready` | **已实现**（未接入 K8s 探针） |