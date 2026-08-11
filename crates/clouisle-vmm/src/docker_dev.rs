//! Docker 开发后端（DockerDevVmm）。
//!
//! 仅供 macOS/Windows 开发者本地工作流使用（`--backend docker-dev`）：
//! 在 Docker 容器中运行 OCI 应用镜像 + 注入的静态 clouisle-agent（PID 1），
//! 复用帧协议提供 exec/文件/secret 语义。**不是生产后端**：
//! 快照/恢复、vsock、域级 allowlist、IOPS/带宽限速均明确不支持。

use async_trait::async_trait;

use clouisle_core::{ClouisleError, ErrorKind, Resources, Result, SandboxSpec};

use crate::docker_engine::{BollardDockerEngine, ContainerState, DevContainerOpts, DockerEngine};
use crate::{StopMode, VmHandle, VmStats, Vmm, VmmCapabilities};

/// DockerDevVmm 配置。
#[derive(Debug, Clone)]
pub struct DockerDevConfig {
    /// 静态 agent 二进制路径（注入容器）。
    pub agent_binary: std::path::PathBuf,
    /// 内部管理网络名（仅 agent 5201 流量）。
    pub mgmt_network: String,
    /// 可选本地出口网络名（network.enabled 时附加）。
    pub egress_network: String,
    /// 允许的宿主挂载根目录（mounts 校验）。
    pub mount_root: std::path::PathBuf,
}

impl Default for DockerDevConfig {
    fn default() -> Self {
        Self {
            agent_binary: std::path::PathBuf::from("/usr/local/bin/clouisle-agent"),
            mgmt_network: "clouisle-dev-mgmt".into(),
            egress_network: "clouisle-dev-egress".into(),
            mount_root: std::path::PathBuf::from("/tmp/clouisle-dev-mounts"),
        }
    }
}

/// Docker 开发容器名（确定性，mgmt 网络内可解析为 hostname）。
pub fn dev_container_name(sandbox_id: &str) -> String {
    format!("clouisle-dev-{}", &sandbox_id[..sandbox_id.len().min(24)])
}

/// 构建 agent 注入 tar 归档：固定 `clouisle-dev-agent` 位于归档根。
fn agent_tar(agent_path: &std::path::Path) -> std::result::Result<Vec<u8>, ClouisleError> {
    use std::io::Write;
    let data = std::fs::read(agent_path).map_err(|e| {
        ClouisleError::io(format!("read agent binary {}: {e}", agent_path.display()))
    })?;
    let name = "clouisle-dev-agent";
    let mut tar = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_size(data.len() as u64);
    header.set_mode(0o755);
    header.set_entry_type(tar::EntryType::Regular);
    header.set_cksum();
    tar.append_data(&mut header, name, std::io::Cursor::new(&data))
        .map_err(|e| ClouisleError::io(format!("tar agent: {e}")))?;
    let mut out = tar
        .into_inner()
        .map_err(|e| ClouisleError::io(format!("tar finish: {e}")))?;
    out.write_all(&[0u8; 1024])
        .map_err(|e| ClouisleError::io(format!("tar pad: {e}")))?;
    Ok(out)
}

/// docker-dev 预检：不支持的资源/策略在创建前拒绝。
pub fn validate_dev_spec(spec: &SandboxSpec) -> std::result::Result<(), ClouisleError> {
    if !spec.network.allow_egress.is_empty() {
        return Err(ClouisleError::validation(
            "docker-dev backend cannot enforce egress allowlists; use network.enabled with empty allow_egress",
        ));
    }
    let r = &spec.resources;
    if r.iops.is_some() || r.bandwidth_mbps.is_some() {
        return Err(ClouisleError::validation(
            "docker-dev backend does not support iops or bandwidth_mbps limits",
        ));
    }
    match spec.restart_policy {
        clouisle_core::RestartPolicy::Never => {}
        _ => {
            return Err(ClouisleError::validation(
                "docker-dev backend supports restart_policy=never only",
            ));
        }
    }
    Ok(())
}

/// 资源映射：CPU → NanoCPUs，内存 → bytes，pids → PidsLimit。
fn map_resources(r: &Resources) -> (Option<i64>, Option<i64>, Option<i64>) {
    let nano = Some(r.vcpu as i64 * 1_000_000_000);
    let mem = Some(r.memory_mb as i64 * 1024 * 1024);
    let pids = r.pids_max.map(|p| p as i64);
    (nano, mem, pids)
}

pub struct DockerDevVmm {
    config: DockerDevConfig,
    engine: Box<dyn DockerEngine>,
}

impl DockerDevVmm {
    /// 连接本机 Docker Engine；失败时返回可解释错误（docker-dev 需要 socket）。
    pub async fn new(config: DockerDevConfig) -> std::result::Result<Self, ClouisleError> {
        let engine = BollardDockerEngine::connect().await?;
        Ok(Self {
            config,
            engine: Box::new(engine),
        })
    }

    #[cfg(test)]
    pub fn with_engine(config: DockerDevConfig, engine: Box<dyn DockerEngine>) -> Self {
        Self { config, engine }
    }

    async fn ensure_dev_networks(&self, enabled: bool) -> std::result::Result<(), ClouisleError> {
        self.engine
            .ensure_network(&self.config.mgmt_network, true)
            .await?;
        if enabled {
            self.engine
                .ensure_network(&self.config.egress_network, false)
                .await?;
        }
        Ok(())
    }
}

#[async_trait]
impl Vmm for DockerDevVmm {
    async fn create(&self, sandbox_id: &str, spec: &SandboxSpec) -> Result<VmHandle> {
        validate_dev_spec(spec)?;
        let name = dev_container_name(sandbox_id);

        self.ensure_dev_networks(spec.network.enabled).await?;
        // 精确引用拉取（幂等：已存在时 Docker 跳过）。
        self.engine.pull_image(&spec.image.reference).await?;

        // mounts 校验（仅 canonical 且在 mount_root 内）
        let mut readonly_mounts = Vec::new();
        for mount in &spec.mounts {
            let src = std::fs::canonicalize(&mount.source).map_err(|e| {
                ClouisleError::validation(format!("mount source {}: {e}", mount.source))
            })?;
            if !src.starts_with(&self.config.mount_root) {
                return Err(ClouisleError::validation(format!(
                    "mount source {} outside allowed root {}",
                    src.display(),
                    self.config.mount_root.display()
                )));
            }
            if !mount.target.starts_with('/') || mount.target.contains("..") {
                return Err(ClouisleError::validation(format!(
                    "invalid mount target {}",
                    mount.target
                )));
            }
            readonly_mounts.push((src.to_string_lossy().into_owned(), mount.target.clone()));
        }

        let (nano, mem, pids) = map_resources(&spec.resources);
        let mut networks = vec![self.config.mgmt_network.clone()];
        if spec.network.enabled {
            networks.push(self.config.egress_network.clone());
        }
        let opts = DevContainerOpts {
            name: name.clone(),
            image: spec.image.reference.clone(),
            entrypoint: vec![
                "/clouisle-dev-agent".into(),
                "serve".into(),
                "--skip-network-config".into(),
            ],
            labels: vec![
                ("com.clouisle.managed".into(), "true".into()),
                ("com.clouisle.backend".into(), "docker-dev".into()),
                ("com.clouisle.sandbox".into(), sandbox_id.into()),
            ],
            nano_cpus: nano,
            memory_bytes: mem,
            pids_limit: pids,
            networks,
            readonly_mounts,
        };
        self.engine.create_container(&opts).await?;

        // 注入静态 agent（PID 1）。
        let tar = agent_tar(&self.config.agent_binary)?;
        if let Err(error) = self.engine.upload_archive(&name, "/", tar).await {
            let _ = self.engine.remove(&name).await;
            return Err(error);
        }

        Ok(VmHandle {
            id: name,
            backend: "docker-dev".into(),
            owner_id: None,
            pid: None,
            api_socket: None,
            vsock_socket: None,
            vsock_cid: None,
            subnet: None,
        })
    }

    async fn start(&self, h: &VmHandle) -> Result<()> {
        self.engine.start(&h.id).await
    }

    async fn pause(&self, h: &VmHandle) -> Result<()> {
        self.engine.pause(&h.id).await
    }

    async fn resume(&self, h: &VmHandle) -> Result<()> {
        self.engine.unpause(&h.id).await
    }

    async fn snapshot(
        &self,
        _h: &VmHandle,
        _kind: crate::SnapshotKind,
        _out: &crate::SnapshotPaths,
    ) -> Result<()> {
        Err(ClouisleError::new(
            ErrorKind::Vmm,
            "docker-dev backend does not support snapshots",
        ))
    }

    async fn restore(
        &self,
        _sandbox_id: &str,
        _spec: &SandboxSpec,
        _from: &crate::SnapshotPaths,
    ) -> Result<VmHandle> {
        Err(ClouisleError::new(
            ErrorKind::Vmm,
            "docker-dev backend does not support restore",
        ))
    }

    async fn stop(&self, h: &VmHandle, mode: StopMode) -> Result<()> {
        let result = match mode {
            StopMode::Graceful => self.engine.stop(&h.id).await,
            StopMode::Force => self.engine.kill(&h.id).await,
        };
        // 幂等清理：容器已不存在视为成功。
        if result.is_err() && !self.engine.container_exists(&h.id).await.unwrap_or(false) {
            return Ok(());
        }
        result?;
        let _ = self.engine.remove(&h.id).await;
        Ok(())
    }

    async fn stats(&self, h: &VmHandle) -> Result<VmStats> {
        match self.engine.inspect_state(&h.id).await {
            Ok(ContainerState::Running | ContainerState::Paused) => Ok(VmStats::default()),
            Ok(_) => Err(ClouisleError::invalid_state(format!(
                "container {} not running",
                h.id
            ))),
            Err(e) => Err(e),
        }
    }

    fn capabilities(&self) -> VmmCapabilities {
        VmmCapabilities {
            snapshot: false,
            vsock: false,
            balloon: false,
        }
    }

    async fn probe(&self, h: &VmHandle) -> Result<bool> {
        match self.engine.inspect_state(&h.id).await {
            Ok(ContainerState::Running | ContainerState::Paused) => Ok(true),
            _ => Ok(false),
        }
    }

    async fn discover(&self) -> Result<Vec<VmHandle>> {
        // 开发后端不接管既有容器；reconcile 由 API 层按标签扫描。
        Ok(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::docker_engine::DockerEngine;
    use clouisle_core::{NetworkConfig, RestartPolicy, SandboxSpec};
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct FakeEngine {
        calls: Arc<Mutex<Vec<String>>>,
        pull_fail: bool,
        start_fail: bool,
    }

    #[async_trait]
    impl DockerEngine for FakeEngine {
        async fn pull_image(&self, reference: &str) -> std::result::Result<(), ClouisleError> {
            self.calls.lock().unwrap().push(format!("pull:{reference}"));
            if self.pull_fail {
                return Err(ClouisleError::io("pull failed"));
            }
            Ok(())
        }
        async fn ensure_network(
            &self,
            name: &str,
            internal: bool,
        ) -> std::result::Result<(), ClouisleError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("net:{name}:internal={internal}"));
            Ok(())
        }
        async fn create_container(
            &self,
            opts: &DevContainerOpts,
        ) -> std::result::Result<(), ClouisleError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("create:{}", opts.name));
            assert!(opts.entrypoint.contains(&"/clouisle-dev-agent".to_string()));
            Ok(())
        }
        async fn upload_archive(
            &self,
            container: &str,
            _p: &str,
            _t: Vec<u8>,
        ) -> std::result::Result<(), ClouisleError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("upload:{container}"));
            Ok(())
        }
        async fn start(&self, container: &str) -> std::result::Result<(), ClouisleError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("start:{container}"));
            if self.start_fail {
                return Err(ClouisleError::io("start failed"));
            }
            Ok(())
        }
        async fn pause(&self, c: &str) -> std::result::Result<(), ClouisleError> {
            self.calls.lock().unwrap().push(format!("pause:{c}"));
            Ok(())
        }
        async fn unpause(&self, c: &str) -> std::result::Result<(), ClouisleError> {
            self.calls.lock().unwrap().push(format!("unpause:{c}"));
            Ok(())
        }
        async fn stop(&self, c: &str) -> std::result::Result<(), ClouisleError> {
            self.calls.lock().unwrap().push(format!("stop:{c}"));
            Ok(())
        }
        async fn kill(&self, c: &str) -> std::result::Result<(), ClouisleError> {
            self.calls.lock().unwrap().push(format!("kill:{c}"));
            Ok(())
        }
        async fn remove(&self, c: &str) -> std::result::Result<(), ClouisleError> {
            self.calls.lock().unwrap().push(format!("remove:{c}"));
            Ok(())
        }
        async fn inspect_state(
            &self,
            c: &str,
        ) -> std::result::Result<ContainerState, ClouisleError> {
            self.calls.lock().unwrap().push(format!("inspect:{c}"));
            Ok(ContainerState::Running)
        }
        async fn container_exists(&self, c: &str) -> std::result::Result<bool, ClouisleError> {
            self.calls.lock().unwrap().push(format!("exists:{c}"));
            Ok(false)
        }
    }

    fn dev_vmm(engine: FakeEngine) -> DockerDevVmm {
        let cfg = DockerDevConfig {
            agent_binary: std::path::PathBuf::from("/bin/echo"),
            mount_root: std::path::PathBuf::from("/tmp"),
            ..Default::default()
        };
        DockerDevVmm::with_engine(cfg, Box::new(engine))
    }

    fn spec() -> SandboxSpec {
        SandboxSpec {
            image: clouisle_core::ImageRef::new("docker.io/library/alpine:latest"),
            network: NetworkConfig {
                enabled: false,
                allow_egress: vec![],
                deny_egress: vec![],
            },
            restart_policy: RestartPolicy::Never,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn create_uploads_agent_and_returns_dev_handle() {
        let engine = FakeEngine::default();
        let calls = engine.calls.clone();
        let vmm = dev_vmm(engine);
        let handle = vmm.create("sbx-1234567890abcdef", &spec()).await.unwrap();
        assert_eq!(handle.backend, "docker-dev");
        assert!(handle.id.starts_with("clouisle-dev-"));
        let calls = calls.lock().unwrap().clone();
        assert!(calls.iter().any(|c| c.starts_with("pull:")));
        assert!(
            calls
                .iter()
                .any(|c| c.starts_with("net:clouisle-dev-mgmt:internal=true"))
        );
        assert!(calls.iter().any(|c| c.starts_with("create:")));
        assert!(calls.iter().any(|c| c.starts_with("upload:")));
    }

    #[tokio::test]
    async fn allowlist_rejected() {
        let mut s = spec();
        s.network.allow_egress = vec!["example.com".into()];
        let engine = FakeEngine::default();
        let vmm = dev_vmm(engine);
        assert!(vmm.create("sbx-1", &s).await.is_err());
    }

    #[tokio::test]
    async fn unsupported_resources_rejected() {
        let mut s = spec();
        s.resources.iops = Some(100);
        let vmm = dev_vmm(FakeEngine::default());
        assert!(vmm.create("sbx-1", &s).await.is_err());
        let mut s = spec();
        s.resources.bandwidth_mbps = Some(1);
        assert!(vmm.create("sbx-1", &s).await.is_err());
    }

    #[tokio::test]
    async fn snapshot_unsupported() {
        let vmm = dev_vmm(FakeEngine::default());
        let h = vmm.create("sbx-1", &spec()).await.unwrap();
        assert!(
            vmm.snapshot(
                &h,
                crate::SnapshotKind::Full,
                &crate::SnapshotPaths {
                    state_path: "s".into(),
                    mem_path: "m".into()
                }
            )
            .await
            .is_err()
        );
        assert!(!vmm.capabilities().snapshot);
    }
}
