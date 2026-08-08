//! NodeAgent：节点代理核心逻辑。
//!
//! 管理本机 VMM、维护资源核算、生成心跳上报。
//! 与 apiserver 的传输层（gRPC）在 Phase 3 后期接入；此处提供可测试的核心。

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::RwLock;

use clouisle_core::{
    ClouisleError, ErrorKind, NodeInfo, Resources, Sandbox, SandboxEvent, SandboxSpec,
    SandboxStatus,
};
use clouisle_scheduler::ResourcePool;
use clouisle_store::Store;
use clouisle_vmm::{StopMode, VmHandle, Vmm};

use crate::node::HeartbeatReport;

/// 节点代理配置。
#[derive(Debug, Clone)]
pub struct NodeAgentConfig {
    pub node_id: String,
    pub hostname: String,
    pub total_vcpu: u16,
    pub total_memory_mb: u64,
    pub total_disk_mb: u64,
    pub kvm_available: bool,
    pub kernel_version: String,
    pub firecracker_version: String,
    pub labels: HashMap<String, String>,
    /// 心跳周期（秒）
    pub heartbeat_secs: u64,
}

impl NodeAgentConfig {
    pub fn node_info(&self) -> NodeInfo {
        NodeInfo {
            node_id: self.node_id.clone(),
            hostname: self.hostname.clone(),
            total_vcpu: self.total_vcpu,
            total_memory_mb: self.total_memory_mb,
            total_disk_mb: self.total_disk_mb,
            kvm_available: self.kvm_available,
            kernel_version: self.kernel_version.clone(),
            firecracker_version: self.firecracker_version.clone(),
            labels: self.labels.clone(),
        }
    }
}

/// 节点代理。持有本机所有沙盒 handle 与资源池。
#[derive(Clone)]
pub struct NodeAgent {
    pub config: NodeAgentConfig,
    pub vmm: Arc<dyn Vmm>,
    pub pool: Arc<ResourcePool>,
    /// 本机沙盒（id → sandbox）
    pub sandboxes: Arc<RwLock<HashMap<String, Sandbox>>>,
    /// 活跃沙盒的 reservation（防止 RAII 释放）
    reservations: Arc<tokio::sync::Mutex<HashMap<String, clouisle_scheduler::Reservation>>>,
}

impl NodeAgent {
    pub fn new(config: NodeAgentConfig, vmm: Arc<dyn Vmm>) -> Self {
        let capacity = Resources {
            vcpu: config.total_vcpu,
            memory_mb: config.total_memory_mb as u32,
            disk_mb: config.total_disk_mb as u32,
            ..Resources::default()
        };
        let pool = Arc::new(ResourcePool::new(capacity, 200));
        Self {
            config,
            vmm,
            pool,
            sandboxes: Arc::new(RwLock::new(HashMap::new())),
            reservations: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        }
    }

    /// 注册载荷。
    pub fn registration(&self) -> crate::node::NodeRegistration {
        crate::node::NodeRegistration {
            node: self.config.node_info(),
        }
    }

    /// 生成本机心跳报告。
    pub async fn heartbeat(&self) -> HeartbeatReport {
        let sb = self.sandboxes.read().await;
        let running: Vec<String> = sb
            .values()
            .filter(|s| s.status.is_active())
            .map(|s| s.id.clone())
            .collect();
        HeartbeatReport {
            node_id: self.config.node_id.clone(),
            allocated_vcpu: sb.values().map(|s| s.spec.resources.vcpu).sum(),
            allocated_memory_mb: sb.values().map(|s| s.spec.resources.memory_mb as u64).sum(),
            running_sandboxes: running,
            pool_ready: HashMap::new(),
            load_avg: [0.0; 3],
        }
    }

    /// 在本机创建沙盒（admit → vmm.create → start → 等 ready）。
    pub async fn create_sandbox(
        &self,
        spec: SandboxSpec,
        store: &dyn Store,
    ) -> Result<Sandbox, ClouisleError> {
        // 准入
        let reservation = self.pool.admit(&spec).await?;

        let id = uuid::Uuid::now_v7().to_string();
        let mut sandbox = Sandbox::new(id.clone(), spec);
        sandbox.node_id = Some(self.config.node_id.clone());
        sandbox.transition(SandboxEvent::Start)?;
        store.create_sandbox(&sandbox).await?;

        // VMM
        let handle = match self.vmm.create(&id, &sandbox.spec).await {
            Ok(h) => h,
            Err(e) => {
                sandbox.transition(SandboxEvent::Failed).ok();
                store
                    .update_sandbox_status(&id, &SandboxStatus::Error)
                    .await
                    .ok();
                return Err(e);
            }
        };
        sandbox.vmm_meta = clouisle_core::VmmMeta {
            backend: handle.backend.clone(),
            pid: handle.pid,
            api_socket: handle.api_socket.clone(),
            vsock_socket: handle.vsock_socket.clone(),
            vsock_cid: handle.vsock_cid,
            vmm_id: Some(handle.id.clone()),
            extra: Default::default(),
        };

        if let Err(e) = self.vmm.start(&handle).await {
            sandbox.transition(SandboxEvent::Failed).ok();
            store
                .update_sandbox_status(&id, &SandboxStatus::Error)
                .await
                .ok();
            return Err(e);
        }
        sandbox.transition(SandboxEvent::AgentHello)?;
        store
            .update_sandbox_status(&id, &SandboxStatus::Running)
            .await?;

        self.sandboxes
            .write()
            .await
            .insert(id.clone(), sandbox.clone());
        self.reservations
            .lock()
            .await
            .insert(id.clone(), reservation);
        Ok(sandbox)
    }

    /// 停止并删除本机沙盒。
    pub async fn delete_sandbox(&self, id: &str, store: &dyn Store) -> Result<(), ClouisleError> {
        let sandbox = {
            let sb = self.sandboxes.read().await;
            sb.get(id).cloned().ok_or_else(|| {
                ClouisleError::new(
                    ErrorKind::NotFound,
                    format!("sandbox {id} not on this node"),
                )
            })?
        };

        // 停 VMM
        let handle = VmHandle {
            id: sandbox
                .vmm_meta
                .vmm_id
                .clone()
                .unwrap_or_else(|| id.to_string()),
            backend: sandbox.vmm_meta.backend.clone(),
            pid: sandbox.vmm_meta.pid,
            api_socket: sandbox.vmm_meta.api_socket.clone(),
            vsock_socket: sandbox.vmm_meta.vsock_socket.clone(),
            vsock_cid: sandbox.vmm_meta.vsock_cid,
        };
        self.vmm.stop(&handle, StopMode::Force).await?;

        store.delete_sandbox(id).await?;
        self.sandboxes.write().await.remove(id);
        self.reservations.lock().await.remove(id);
        Ok(())
    }

    /// 通过 agent 执行命令（gRPC 转发用）。
    pub async fn exec_command(
        &self,
        sandbox_id: &str,
        argv: Vec<String>,
        env: std::collections::HashMap<String, String>,
        cwd: Option<String>,
        timeout_ms: u64,
    ) -> Result<clouisle_core::execution::ExecutionResult, ClouisleError> {
        // 简化：本地执行（真实实现走 vsock 到 guest agent）
        if argv.is_empty() {
            return Err(ClouisleError::validation("argv empty"));
        }
        let start = std::time::Instant::now();
        let mut cmd = tokio::process::Command::new(&argv[0]);
        cmd.args(&argv[1..])
            .envs(env)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        if let Some(c) = cwd {
            cmd.current_dir(c);
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| ClouisleError::new(ErrorKind::Vmm, format!("spawn {argv:?}: {e}")))?;

        let stdout = child.stdout.take().expect("stdout piped");
        let stderr = child.stderr.take().expect("stderr piped");
        let mut out_buf = Vec::new();
        let mut err_buf = Vec::new();
        let (out_bytes, err_bytes, status, timed_out) = tokio::select! {
            status = child.wait() => {
                use tokio::io::AsyncReadExt;
                let mut o = tokio::io::BufReader::new(stdout);
                let mut e = tokio::io::BufReader::new(stderr);
                let _ = tokio::join!(o.read_to_end(&mut out_buf), e.read_to_end(&mut err_buf));
                (out_buf.clone(), err_buf.clone(), status.ok(), false)
            }
            _ = tokio::time::sleep(std::time::Duration::from_millis(timeout_ms)) => {
                let _ = child.kill().await;
                let _ = child.wait().await;
                (out_buf, err_buf, None, true)
            }
        };
        let duration_ms = start.elapsed().as_millis() as u64;
        let _ = sandbox_id;
        Ok(clouisle_core::execution::ExecutionResult {
            exit_code: if timed_out {
                -1
            } else {
                status.unwrap_or_default().code().unwrap_or(-1)
            },
            stdout: bytes::Bytes::from(out_bytes),
            stderr: bytes::Bytes::from(err_bytes),
            duration_ms,
        })
    }

    /// 恢复：从 store 加载本节点沙盒（重启后接管）。
    pub async fn reconcile_from_store(&self, store: &dyn Store) -> usize {
        let all = store.list_sandboxes(None).await.unwrap_or_default();
        let mine: Vec<Sandbox> = all
            .into_iter()
            .filter(|s| s.node_id.as_deref() == Some(self.config.node_id.as_str()))
            .filter(|s| s.status.is_active())
            .collect();
        let count = mine.len();
        let specs: Vec<SandboxSpec> = mine.iter().map(|s| s.spec.clone()).collect();
        self.pool.restore(&specs).await;
        let mut sb = self.sandboxes.write().await;
        for s in mine {
            sb.insert(s.id.clone(), s);
        }
        count
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use clouisle_store::InMemoryStore;
    use clouisle_vmm::{
        SnapshotKind, SnapshotPaths, StopMode, VmHandle, VmStats, Vmm, VmmCapabilities,
    };

    #[derive(Clone)]
    struct TestVmm;

    #[async_trait]
    impl Vmm for TestVmm {
        async fn create(
            &self,
            _: &str,
            _: &clouisle_core::SandboxSpec,
        ) -> clouisle_core::Result<VmHandle> {
            Ok(VmHandle {
                id: uuid::Uuid::now_v7().to_string(),
                backend: "test".into(),
                pid: None,
                api_socket: None,
                vsock_socket: None,
                vsock_cid: None,
            })
        }
        async fn start(&self, _: &VmHandle) -> clouisle_core::Result<()> {
            Ok(())
        }
        async fn pause(&self, _: &VmHandle) -> clouisle_core::Result<()> {
            Ok(())
        }
        async fn resume(&self, _: &VmHandle) -> clouisle_core::Result<()> {
            Ok(())
        }
        async fn snapshot(
            &self,
            _: &VmHandle,
            _k: SnapshotKind,
            _o: &SnapshotPaths,
        ) -> clouisle_core::Result<()> {
            Ok(())
        }
        async fn restore(
            &self,
            _: &clouisle_core::SandboxSpec,
            _: &SnapshotPaths,
        ) -> clouisle_core::Result<VmHandle> {
            Ok(VmHandle {
                id: uuid::Uuid::now_v7().to_string(),
                backend: "test".into(),
                pid: None,
                api_socket: None,
                vsock_socket: None,
                vsock_cid: None,
            })
        }
        async fn stop(&self, _: &VmHandle, _m: StopMode) -> clouisle_core::Result<()> {
            Ok(())
        }
        async fn stats(&self, _: &VmHandle) -> clouisle_core::Result<VmStats> {
            Ok(VmStats::default())
        }
        fn capabilities(&self) -> VmmCapabilities {
            VmmCapabilities {
                snapshot: true,
                vsock: true,
                balloon: false,
            }
        }
    }

    fn config() -> NodeAgentConfig {
        NodeAgentConfig {
            node_id: "node-1".into(),
            hostname: "host1".into(),
            total_vcpu: 16,
            total_memory_mb: 32768,
            total_disk_mb: 102400,
            kvm_available: true,
            kernel_version: "6.1".into(),
            firecracker_version: "1.4".into(),
            labels: HashMap::new(),
            heartbeat_secs: 3,
        }
    }

    #[tokio::test]
    async fn registration_has_node_id() {
        let agent = NodeAgent::new(config(), Arc::new(TestVmm));
        let reg = agent.registration();
        assert_eq!(reg.node.node_id, "node-1");
    }

    #[tokio::test]
    async fn create_sandbox_updates_heartbeat() {
        let agent = NodeAgent::new(config(), Arc::new(TestVmm));
        let store = InMemoryStore::new();
        let sb = agent
            .create_sandbox(SandboxSpec::default(), &store)
            .await
            .unwrap();
        assert_eq!(sb.status, SandboxStatus::Running);
        assert_eq!(sb.node_id.as_deref(), Some("node-1"));

        let hb = agent.heartbeat().await;
        assert_eq!(hb.running_sandboxes.len(), 1);
        assert_eq!(hb.allocated_vcpu, 1);
    }

    #[tokio::test]
    async fn delete_releases_resources() {
        let agent = NodeAgent::new(config(), Arc::new(TestVmm));
        let store = InMemoryStore::new();
        let sb = agent
            .create_sandbox(SandboxSpec::default(), &store)
            .await
            .unwrap();
        agent.delete_sandbox(&sb.id, &store).await.unwrap();
        let hb = agent.heartbeat().await;
        assert!(hb.running_sandboxes.is_empty());
        assert_eq!(hb.allocated_vcpu, 0);
    }

    #[tokio::test]
    async fn delete_unknown_id_not_found() {
        let agent = NodeAgent::new(config(), Arc::new(TestVmm));
        let store = InMemoryStore::new();
        let err = agent.delete_sandbox("nope", &store).await.unwrap_err();
        assert_eq!(err.kind, ErrorKind::NotFound);
    }

    #[tokio::test]
    async fn reconcile_restores_running() {
        let agent = NodeAgent::new(config(), Arc::new(TestVmm));
        let store = InMemoryStore::new();
        let sb = agent
            .create_sandbox(SandboxSpec::default(), &store)
            .await
            .unwrap();

        // 模拟重启：新 agent 实例
        let agent2 = NodeAgent::new(config(), Arc::new(TestVmm));
        let n = agent2.reconcile_from_store(&store).await;
        assert_eq!(n, 1);
        assert_eq!(agent2.sandboxes.read().await.len(), 1);
        let _ = sb;
    }

    #[tokio::test]
    async fn heartbeat_empty() {
        let agent = NodeAgent::new(config(), Arc::new(TestVmm));
        let hb = agent.heartbeat().await;
        assert!(hb.running_sandboxes.is_empty());
        assert_eq!(hb.node_id, "node-1");
    }
}
