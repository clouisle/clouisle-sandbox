# 部署架构决策

## 选型：K8s DaemonSet + Pod 内进程

**决策**：clouisled 作为 DaemonSet 部署，Pod 内直管 firecracker 进程。

```
┌────────────────────── K8s 集群 ──────────────────────┐
│                                                       │
│  Deployment: apiserver (默认单体，可扩展副本)           │
│    └─ HTTP API + 本机 Firecracker / 镜像缓存             │
│                                                       │
│  StatefulSet: postgres (共享持久状态)                   │
│                                                       │
│  可选 profile: clouisled DaemonSet（多节点）             │
│    └─ 每节点直管 Firecracker，通过同一 PostgreSQL      │
│                                                       │
│  默认 docker compose / kubectl -k deploy 不启动节点    │
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
| 控制平面存储 | **PostgreSQL**（StatefulSet 或云服务） | 默认单体和多副本共享状态 |
| 默认部署 | **API + Firecracker + PostgreSQL** | 单节点无需维护独立 Node 服务 |
| 多节点扩展 | **可选 clouisled DaemonSet** | 显式启用后通过同一 Store DSN |

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
| apiserver | 默认单体；可扩展 Deployment 多副本 | **已实现**（PostgreSQL 共享状态） |
| clouisled | 可选 DaemonSet 每节点一个 | **已实现**（同一 Store + 周期 reconciler） |
| postgres | StatefulSet 或云服务 | **已实现**（StatefulSet 清单与连接串） |
| 资源调度 | 本地池或节点心跳调度 | **部分实现**（多节点调度路径保留） |
| 健康检查 | `/health/live` + `/health/ready` | **已实现**（已接入默认探针） |