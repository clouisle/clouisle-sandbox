# 生产化改造设计文档

## 背景
当前 Firecracker VMM 集成是空壳（create/start 等均无操作），Agent 通信使用 Mock，镜像管道返回假路径，DNS 代理为空占位。需要全部实现真实后端。

## 架构

```
clouisle-api (host)
  ├── Firecracker VMM ──HTTP-over-UDS──► firecracker 进程
  │                                  └─► vsock device (guest CID)
  ├── VsockAgentConnector ──AF_VSOCK──► guest agent (clouisle-agent --serve)
  ├── OCI ImageBuilder ──► registry ──► ext4 rootfs ──► firecracker drive
  └── FirewallManager ──► netns + nftables + DNS proxy (UDP :53)
```

## 共享契约

### VmHandle 扩展
```rust
pub struct VmHandle {
    pub id: String,
    pub backend: String,
    pub pid: Option<u64>,
    pub api_socket: Option<String>,    // Firecracker UDS
    pub vsock_socket: Option<String>,  // Host-side vsock UDS path
    pub vsock_cid: Option<u64>,        // Guest CID (Firecracker 分配)
}
```

### AgentConnector trait（不变）
```rust
pub trait AgentConnector: Send + Sync {
    async fn connect_and_hello(&self, handle: &VmHandle, sandbox_id: &str) -> Result<Box<dyn AgentConnection>>;
}
```

### Frame 协议（不变，已定义）
- 长度前缀帧: [u32 BE len | postcard Frame]
- 类型: Hello, ExecReq, Stdout, Stderr, Exited, Ping, Pong, WriteFile, ReadFile, ...

## 实现计划

### Stage 1: Firecracker VMM 完整集成
- 实现 HTTP-over-UDS 客户端
- create: 配置 machine-config, boot-source, drives, vsock, network
- start: InstanceStart
- 等待 guest 就绪（vsock 连接探测）
- stop: 支持 graceful (SendCtrlAltDel) + force (SIGKILL)

### Stage 2: OCI 镜像拉取 + rootfs 构建
- 使用 oci-client 或 oci-distribution 拉取 OCI 镜像
- 解压到 ext4 文件系统
- 注入 clouisle-agent 二进制
- 缓存管理

### Stage 3: Host vsock 连接器
- 使用 tokio-vsock 或 raw AF_VSOCK
- 连接 guest CID
- Frame 协议编解码
- 替换 MockAgentConnector

### Stage 4: Guest agent vsock 绑定
- 绑定 vsock 端口
- 接受连接
- 使用现有 process_frames 处理帧

### Stage 5: DNS 代理
- UDP 监听 10.0.0.1:53
- 使用 hickory-resolver 解析
- 白名单过滤

### Stage 6: 移除 Mock + 清理
- 删除 MockAgentConnector, MockFsBackend, MockAgentConnection
- 删除测试中的 MockVmm
- 更新 main.rs 使用真实连接器