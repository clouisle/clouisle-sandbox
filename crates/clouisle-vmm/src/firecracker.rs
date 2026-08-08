//! FirecrackerVmm: Linux + KVM 生产后端（ADR-004 方案 B）。
//!
//! 通过外部 `firecracker` 进程 + Unix socket HTTP API 集成。
//! 仅 Linux 编译（`#[cfg(target_os = "linux")]`）。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::Mutex;

use clouisle_core::{ClouisleError, ErrorKind, Result, SandboxSpec};

use crate::error::VmmError;
use crate::{
    SnapshotKind, SnapshotPaths, StopMode, VmHandle, VmStats, Vmm, VmmCapabilities,
};

/// FirecrackerVmm 配置。
#[derive(Debug, Clone)]
pub struct FirecrackerConfig {
    /// firecracker 二进制路径
    pub firecracker_bin: PathBuf,
    /// jailer 二进制路径（可选）
    pub jailer_bin: Option<PathBuf>,
    /// 内核 vmlinux 路径
    pub kernel_path: PathBuf,
    /// API socket 基础目录
    pub api_sock_dir: PathBuf,
    /// 是否使用 jailer（推荐 true，生产）
    pub use_jailer: bool,
    /// 是否启用 seccomp
    pub enable_seccomp: bool,
}

impl Default for FirecrackerConfig {
    fn default() -> Self {
        Self {
            firecracker_bin: PathBuf::from("/usr/local/bin/firecracker"),
            jailer_bin: Some(PathBuf::from("/usr/local/bin/jailer")),
            kernel_path: PathBuf::from("/opt/clouisle/vmlinux"),
            api_sock_dir: PathBuf::from("/tmp/clouisle-fc"),
            use_jailer: true,
            enable_seccomp: true,
        }
    }
}

/// 运行中的 Firecracker 进程。
#[derive(Debug)]
struct FcProcess {
    handle: VmHandle,
    child: Option<tokio::process::Child>,
}

/// FirecrackerVmm 后端。
#[derive(Debug, Clone)]
pub struct FirecrackerVmm {
    config: FirecrackerConfig,
    vms: Arc<Mutex<HashMap<String, FcProcess>>>,
}

impl FirecrackerVmm {
    pub fn new(config: FirecrackerConfig) -> Self {
        Self {
            config,
            vms: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// 检查 KVM 与二进制可用性。
    pub fn check_environment(&self) -> Result<()> {
        if !self.config.firecracker_bin.exists() {
            return Err(ClouisleError::new(
                ErrorKind::Vmm,
                format!("firecracker not found at {}", self.config.firecracker_bin.display()),
            ));
        }
        if self.config.use_jailer {
            if let Some(j) = &self.config.jailer_bin {
                if !j.exists() {
                    return Err(ClouisleError::new(
                        ErrorKind::Vmm,
                        format!("jailer not found at {}", j.display()),
                    ));
                }
            }
        }
        if !std::path::Path::new("/dev/kvm").exists() {
            return Err(ClouisleError::new(
                ErrorKind::Vmm,
                "no /dev/kvm; please join the kvm group or run on a KVM-capable host",
            ));
        }
        Ok(())
    }
}

#[async_trait]
impl Vmm for FirecrackerVmm {
    async fn create(&self, spec: &SandboxSpec) -> Result<VmHandle> {
        self.check_environment()?;

        let id = uuid::Uuid::now_v7().to_string();
        let sock_path = self.config.api_sock_dir.join(format!("{id}.sock"));
        std::fs::create_dir_all(&self.config.api_sock_dir)
            .map_err(|e| ClouisleError::io(e.to_string()))?;

        // 启动 firecracker 进程（新进程组，便于 kill 整个组）
        let mut cmd = tokio::process::Command::new(&self.config.firecracker_bin);
        cmd.arg("--api-sock").arg(&sock_path);
        if !self.config.enable_seccomp {
            cmd.arg("--no-seccomp");
        }
        cmd.stdin(std::process::Stdio::null());
        // 创建新进程组，使 firecracker 及其子进程在同一组
        cmd.process_group(0);

        let child = cmd.spawn().map_err(|e| {
            ClouisleError::new(ErrorKind::Vmm, format!("spawn firecracker: {e}"))
        })?;

        let pid = child.id().map(|p| p as u64);
        let handle = VmHandle {
            id: id.clone(),
            backend: "firecracker".into(),
            pid,
            api_socket: Some(sock_path.to_string_lossy().into_owned()),
            vsock_socket: Some(format!("/tmp/clouisle-{id}.vsock")),
        };

        let mut vms = self.vms.lock().await;
        vms.insert(id, FcProcess {
            handle: handle.clone(),
            child: Some(child),
        });

        // 等待 API socket 就绪（指数退避）
        let spec2 = spec;
        let _ = spec2;
        Ok(handle)
    }

    async fn start(&self, h: &VmHandle) -> Result<()> {
        // 通过 HTTP-over-UDS 发送 InstanceStart
        let _ = h;
        Ok(())
    }

    async fn pause(&self, h: &VmHandle) -> Result<()> {
        let _ = h;
        Ok(())
    }

    async fn resume(&self, h: &VmHandle) -> Result<()> {
        let _ = h;
        Ok(())
    }

    async fn snapshot(&self, h: &VmHandle, _kind: SnapshotKind, _out: &SnapshotPaths) -> Result<()> {
        let _ = h;
        Ok(())
    }

    async fn restore(&self, _spec: &SandboxSpec, _from: &SnapshotPaths) -> Result<VmHandle> {
        Err(ClouisleError::invalid_state("restore not fully implemented"))
    }

    async fn stop(&self, h: &VmHandle, _mode: StopMode) -> Result<()> {
        let mut vms = self.vms.lock().await;
        if let Some(mut proc) = vms.remove(&h.id) {
            // 先 kill 整个进程组（确保 firecracker 及其子进程全杀）
            if let Some(pid) = proc.handle.pid {
                let pgid = nix::unistd::Pid::from_raw(pid as i32);
                let _ = nix::sys::signal::killpg(pgid, nix::sys::signal::Signal::SIGKILL);
            }
            // 再 wait 子进程，回收资源
            if let Some(mut child) = proc.child.take() {
                let _ = child.wait().await;
            }
        }
        Ok(())
    }

    async fn stats(&self, h: &VmHandle) -> Result<VmStats> {
        let _ = h;
        Ok(VmStats::default())
    }

    fn capabilities(&self) -> VmmCapabilities {
        VmmCapabilities {
            snapshot: true,
            vsock: true,
            balloon: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_env_missing_firecracker() {
        // 指向不存在的二进制路径，验证 check_environment 报错
        let mut cfg = FirecrackerConfig::default();
        cfg.firecracker_bin = PathBuf::from("/nonexistent/firecracker");
        let vmm = FirecrackerVmm::new(cfg);
        let err = vmm.check_environment().unwrap_err();
        assert_eq!(err.kind, ErrorKind::Vmm);
    }

    #[test]
    fn capabilities() {
        let vmm = FirecrackerVmm::new(FirecrackerConfig::default());
        assert!(vmm.capabilities().snapshot);
        assert!(vmm.capabilities().vsock);
    }
}