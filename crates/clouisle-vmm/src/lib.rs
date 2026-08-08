//! clouisle-vmm: VMM 抽象层（ADR-004）。
//!
//! 后端：
//! - [`FirecrackerVmm`]：Linux + KVM 生产后端
//!
//! 控制平面通过 [`Vmm`] trait 抽象，目前仅支持 Linux + KVM 平台。

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use clouisle_core::{Result, SandboxSpec};

pub mod error;
#[cfg(target_os = "linux")]
pub mod firecracker;

#[cfg(target_os = "linux")]
pub use firecracker::{FirecrackerConfig, FirecrackerVmm};

/// Vmm 句柄：一个已创建 VMM 的引用。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VmHandle {
    pub id: String,
    /// VMM 后端类型
    pub backend: String,
    /// 进程 PID
    pub pid: Option<u64>,
    /// API socket 路径
    pub api_socket: Option<String>,
    /// vsock socket 路径
    pub vsock_socket: Option<String>,
    /// Firecracker 分配的 guest CID
    pub vsock_cid: Option<u64>,
}

/// 停止模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopMode {
    /// 优雅关闭（SendCtrlAltDel）
    Graceful,
    /// 立即终止
    Force,
}

/// 快照类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SnapshotKind {
    /// 全量快照
    Full,
    /// 增量快照
    Diff,
}

/// 快照路径对。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotPaths {
    pub state_path: String,
    pub mem_path: String,
}

/// VMM 统计。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VmStats {
    pub boot_time_us: Option<u64>,
    pub vcpu_usage: Option<f64>,
    pub mem_used_mb: Option<u64>,
    pub rx_bytes: Option<u64>,
    pub tx_bytes: Option<u64>,
}

/// 后端能力声明，用于优雅降级。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct VmmCapabilities {
    pub snapshot: bool,
    pub vsock: bool,
    pub balloon: bool,
}

/// VMM 抽象（ADR-004）。
#[async_trait]
pub trait Vmm: Send + Sync {
    /// 创建 VMM 资源（进程启动、socket 就绪），不启动 guest。
    async fn create(&self, spec: &SandboxSpec) -> Result<VmHandle>;

    /// 启动 guest（InstanceStart）。
    async fn start(&self, h: &VmHandle) -> Result<()>;

    /// 暂停。
    async fn pause(&self, h: &VmHandle) -> Result<()>;

    /// 恢复。
    async fn resume(&self, h: &VmHandle) -> Result<()>;

    /// 创建快照。
    async fn snapshot(&self, h: &VmHandle, kind: SnapshotKind, out: &SnapshotPaths) -> Result<()>;

    /// 从快照恢复。
    async fn restore(&self, spec: &SandboxSpec, from: &SnapshotPaths) -> Result<VmHandle>;

    /// 停止。
    async fn stop(&self, h: &VmHandle, mode: StopMode) -> Result<()>;

    /// 统计。
    async fn stats(&self, h: &VmHandle) -> Result<VmStats>;

    /// 能力声明。
    fn capabilities(&self) -> VmmCapabilities;
}
