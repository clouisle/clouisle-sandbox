//! clouisle-vmm: VMM 抽象层（ADR-004）。
//!
//! 后端：
//! - [`FirecrackerVmm`]：Linux + KVM 生产后端
//!
//! 控制平面通过 [`Vmm`] trait 抽象，目前仅支持 Linux + KVM 平台。

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use clouisle_core::{Result, SandboxSpec};

#[cfg(target_os = "linux")]
pub mod docker_dev;
#[cfg(target_os = "linux")]
pub mod docker_engine;
pub mod error;
#[cfg(target_os = "linux")]
pub mod firecracker;

#[cfg(target_os = "linux")]
pub use docker_dev::{DockerDevConfig, DockerDevVmm};
#[cfg(target_os = "linux")]
pub use firecracker::{FirecrackerConfig, FirecrackerVmm};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VmHandle {
    pub id: String,
    /// VMM 后端类型
    pub backend: String,
    /// Stable node owner for remote runtimes.
    #[serde(default)]
    pub owner_id: Option<String>,
    /// 进程 PID
    pub pid: Option<u64>,
    /// API socket 路径
    pub api_socket: Option<String>,
    /// vsock socket 路径
    pub vsock_socket: Option<String>,
    /// Firecracker 分配的 guest CID
    pub vsock_cid: Option<u64>,
    /// 显式网段（快照预热继承路径）；None = 按 sandbox_id 派生。
    #[serde(default)]
    pub subnet: Option<(u16, u16)>,
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
    /// `sandbox_id` 由控制平面统一生成（用于 netns 名、设备名一致性）。
    async fn create(&self, sandbox_id: &str, spec: &SandboxSpec) -> Result<VmHandle>;

    /// 用显式子网创建 VM（快照预热等需要固定网段的场景）。
    /// 默认委托 [`Vmm::create`]（id 派生网段）。
    async fn create_in_subnet(
        &self,
        sandbox_id: &str,
        spec: &SandboxSpec,
        _subnet: (u16, u16),
    ) -> Result<VmHandle> {
        self.create(sandbox_id, spec).await
    }

    /// Return whether the image is already usable without registry I/O.
    async fn image_cache_hit(&self, _spec: &SandboxSpec) -> Result<bool> {
        Ok(true)
    }

    /// Pull and materialize an image before sandbox creation.
    async fn prefetch_image(&self, _spec: &SandboxSpec) -> Result<()> {
        Ok(())
    }

    /// Probe a persisted runtime after a control-plane restart.
    async fn probe(&self, _handle: &VmHandle) -> Result<bool> {
        Ok(true)
    }

    /// Discover runtimes that survived a controller restart. Implementations
    /// return only handles they can safely probe and stop.
    async fn discover(&self) -> Result<Vec<VmHandle>> {
        Ok(Vec::new())
    }

    /// Whether a VM created before its final sandbox ID can safely be leased
    /// later. Backends whose network namespace and guest identity are tied to
    /// `sandbox_id` must return false and use image-cache warming instead.
    fn supports_detached_warm_pool(&self) -> bool {
        true
    }

    /// Whether the backend has a guest agent that must answer before the
    /// control plane can report a running sandbox.
    fn requires_guest_agent(&self) -> bool {
        true
    }

    /// 启动 guest（InstanceStart）。
    async fn start(&self, h: &VmHandle) -> Result<()>;

    /// 暂停。
    async fn pause(&self, h: &VmHandle) -> Result<()>;

    /// 恢复。
    async fn resume(&self, h: &VmHandle) -> Result<()>;

    /// 创建快照。
    async fn snapshot(&self, h: &VmHandle, kind: SnapshotKind, out: &SnapshotPaths) -> Result<()>;

    /// 从快照恢复到由调用方预先创建网络隔离的 sandbox ID。
    async fn restore(
        &self,
        sandbox_id: &str,
        spec: &SandboxSpec,
        from: &SnapshotPaths,
    ) -> Result<VmHandle>;

    /// 停止。
    async fn stop(&self, h: &VmHandle, mode: StopMode) -> Result<()>;

    /// 统计。
    async fn stats(&self, h: &VmHandle) -> Result<VmStats>;

    /// 能力声明。
    fn capabilities(&self) -> VmmCapabilities;
}
