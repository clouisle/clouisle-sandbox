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
    /// Whether this node owns netns/TAP/firewall lifecycle for its sandboxes.
    pub manage_network: bool,
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

/// Guest command output emitted in arrival order for gRPC forwarding.
#[cfg(target_os = "linux")]
pub enum NodeExecEvent {
    Stdout(bytes::Bytes),
    Stderr(bytes::Bytes),
    Exit(i32),
}

/// 节点代理。持有本机所有沙盒 handle 与资源池。
#[derive(Clone)]
pub struct NodeAgent {
    pub config: NodeAgentConfig,
    pub vmm: Arc<dyn Vmm>,
    pub pool: Arc<ResourcePool>,
    /// 本机沙盒（id → sandbox）。
    pub sandboxes: Arc<RwLock<HashMap<String, Sandbox>>>,
    reservations: Arc<tokio::sync::Mutex<HashMap<String, clouisle_scheduler::Reservation>>>,
    #[cfg(target_os = "linux")]
    firewall: Option<Arc<clouisle_net::FirewallManager>>,
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
            config: config.clone(),
            vmm,
            pool,
            sandboxes: Arc::new(RwLock::new(HashMap::new())),
            reservations: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            #[cfg(target_os = "linux")]
            firewall: config
                .manage_network
                .then(|| Arc::new(clouisle_net::FirewallManager::new())),
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

    /// Create a sandbox using a control-plane assigned ID.
    pub async fn create_sandbox(
        &self,
        spec: SandboxSpec,
        store: &dyn Store,
    ) -> Result<Sandbox, ClouisleError> {
        self.create_sandbox_with_id(uuid::Uuid::now_v7().to_string(), spec, store)
            .await
    }

    /// In cluster mode the API supplies this ID before forwarding the create
    /// request, keeping API metadata, node state, and VMM handles aligned.
    pub async fn create_sandbox_with_id(
        &self,
        id: String,
        spec: SandboxSpec,
        store: &dyn Store,
    ) -> Result<Sandbox, ClouisleError> {
        if id.is_empty() {
            return Err(ClouisleError::validation("sandbox id is required"));
        }
        let reservation = self.pool.admit(&spec).await?;
        let mut sandbox = Sandbox::new(id.clone(), spec);
        sandbox.node_id = Some(self.config.node_id.clone());
        sandbox.transition(SandboxEvent::Start)?;

        store.create_sandbox(&sandbox).await?;
        #[cfg(target_os = "linux")]
        if let Some(firewall) = &self.firewall
            && let Err(error) = firewall.create_network(&id).await
        {
            store
                .update_sandbox_status(&id, &SandboxStatus::Error)
                .await
                .ok();
            return Err(error);
        }

        let handle = match self.vmm.create(&id, &sandbox.spec).await {
            Ok(handle) => handle,
            Err(error) => {
                sandbox.transition(SandboxEvent::Failed).ok();
                store
                    .update_sandbox_status(&id, &SandboxStatus::Error)
                    .await
                    .ok();
                #[cfg(target_os = "linux")]
                if let Some(firewall) = &self.firewall {
                    let _ = firewall.teardown_sandbox_network(&id).await;
                }
                return Err(error);
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

        if let Err(error) = self.vmm.start(&handle).await {
            sandbox.transition(SandboxEvent::Failed).ok();
            store
                .update_sandbox_status(&id, &SandboxStatus::Error)
                .await
                .ok();
            let _ = self.vmm.stop(&handle, StopMode::Force).await;
            #[cfg(target_os = "linux")]
            if let Some(firewall) = &self.firewall {
                let _ = firewall.teardown_sandbox_network(&id).await;
            }
            return Err(error);
        }

        #[cfg(target_os = "linux")]
        if let Some(firewall) = &self.firewall {
            let gateway = format!("{}/30", clouisle_net::netns::gateway_ip(&id));
            let allow = if sandbox.spec.network.enabled {
                sandbox.spec.network.allow_egress.clone()
            } else {
                Vec::new()
            };
            if let Err(error) = firewall.setup_sandbox_network(&id, &gateway, &allow).await {
                sandbox.transition(SandboxEvent::Failed).ok();
                store
                    .update_sandbox_status(&id, &SandboxStatus::Error)
                    .await
                    .ok();
                let _ = self.vmm.stop(&handle, StopMode::Force).await;
                let _ = firewall.teardown_sandbox_network(&id).await;
                return Err(error);
            }
        }
        sandbox.transition(SandboxEvent::AgentHello)?;
        store
            .update_sandbox_status(&id, &SandboxStatus::Running)
            .await?;
        store.update_sandbox_expiry(&id, sandbox.expires_at).await?;

        self.sandboxes
            .write()
            .await
            .insert(id.clone(), sandbox.clone());
        self.reservations.lock().await.insert(id, reservation);
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
        #[cfg(target_os = "linux")]
        if let Some(firewall) = &self.firewall
            && let Err(error) = firewall.teardown_sandbox_network(id).await
        {
            tracing::warn!(sandbox_id = id, error = %error, "network teardown failed");
        }
        Ok(())
    }

    #[cfg(target_os = "linux")]
    pub async fn file_op(
        &self,
        sandbox_id: &str,
        request: clouisle_proto::Frame,
    ) -> Result<clouisle_proto::Frame, ClouisleError> {
        use clouisle_proto::codec::{read_frame, write_frame};
        let sandbox = self
            .sandboxes
            .read()
            .await
            .get(sandbox_id)
            .cloned()
            .ok_or_else(|| {
                ClouisleError::not_found(format!("sandbox {sandbox_id} not on this node"))
            })?;
        if !sandbox.is_executable() {
            return Err(ClouisleError::invalid_state(format!(
                "sandbox {sandbox_id} is not running (status={})",
                sandbox.status
            )));
        }
        let address = format!("{}:5201", clouisle_net::netns::guest_ip(sandbox_id))
            .parse::<std::net::SocketAddr>()
            .map_err(|error| ClouisleError::io(format!("invalid guest address: {error}")))?;
        let stream = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            tokio::net::TcpStream::connect(address),
        )
        .await
        .map_err(|_| ClouisleError::timeout("guest agent connect timed out"))?
        .map_err(|error| ClouisleError::io(format!("connect guest agent: {error}")))?;
        let mut stream = tokio::io::BufStream::new(stream);
        write_frame(
            &mut stream,
            &clouisle_proto::Frame::Hello {
                agent_version: env!("CARGO_PKG_VERSION").to_string(),
            },
        )
        .await
        .map_err(|error| ClouisleError::io(format!("send guest hello: {error}")))?;
        if !matches!(
            read_frame(&mut stream)
                .await
                .map_err(|error| ClouisleError::io(format!("read guest hello: {error}")))?,
            clouisle_proto::Frame::Hello { .. }
        ) {
            return Err(ClouisleError::invalid_state("guest did not return Hello"));
        }
        write_frame(&mut stream, &request)
            .await
            .map_err(|error| ClouisleError::io(format!("send guest file request: {error}")))?;
        let response = read_frame(&mut stream)
            .await
            .map_err(|error| ClouisleError::io(format!("read guest file response: {error}")))?;
        if let clouisle_proto::Frame::Error { message, .. } = &response {
            return Err(ClouisleError::new(
                ErrorKind::Vmm,
                format!("guest file operation: {message}"),
            ));
        }
        Ok(response)
    }

    #[cfg(target_os = "linux")]
    /// Execute through the sandbox guest agent, never on the node host.
    pub async fn exec_command(
        &self,
        sandbox_id: &str,
        argv: Vec<String>,
        env: std::collections::HashMap<String, String>,
        cwd: Option<String>,
        timeout_ms: u64,
    ) -> Result<clouisle_core::execution::ExecutionResult, ClouisleError> {
        use clouisle_proto::Frame;
        use clouisle_proto::codec::{read_frame, write_frame};

        if argv.is_empty() {
            return Err(ClouisleError::validation("argv empty"));
        }
        let sandbox = self
            .sandboxes
            .read()
            .await
            .get(sandbox_id)
            .cloned()
            .ok_or_else(|| {
                ClouisleError::not_found(format!("sandbox {sandbox_id} not on this node"))
            })?;
        if !sandbox.is_executable() {
            return Err(ClouisleError::invalid_state(format!(
                "sandbox {sandbox_id} is not running (status={})",
                sandbox.status
            )));
        }

        let guest_ip = clouisle_net::netns::guest_ip(sandbox_id);
        let address = format!("{guest_ip}:5201")
            .parse::<std::net::SocketAddr>()
            .map_err(|error| ClouisleError::io(format!("invalid guest address: {error}")))?;
        let stream = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            tokio::net::TcpStream::connect(address),
        )
        .await
        .map_err(|_| ClouisleError::timeout("guest agent connect timed out"))?
        .map_err(|error| ClouisleError::io(format!("connect guest agent: {error}")))?;
        let mut stream = tokio::io::BufStream::new(stream);
        write_frame(
            &mut stream,
            &Frame::Hello {
                agent_version: env!("CARGO_PKG_VERSION").to_string(),
            },
        )
        .await
        .map_err(|error| ClouisleError::io(format!("send guest hello: {error}")))?;
        if !matches!(
            read_frame(&mut stream)
                .await
                .map_err(|error| ClouisleError::io(format!("read guest hello: {error}")))?,
            Frame::Hello { .. }
        ) {
            return Err(ClouisleError::invalid_state("guest did not return Hello"));
        }

        let execution_id = uuid::Uuid::now_v7().to_string();
        write_frame(
            &mut stream,
            &Frame::ExecReq {
                id: execution_id.clone(),
                argv,
                env,
                cwd,
                timeout_ms,
            },
        )
        .await
        .map_err(|error| ClouisleError::io(format!("send guest ExecReq: {error}")))?;

        let started = std::time::Instant::now();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let exit_code = loop {
            let frame = read_frame(&mut stream)
                .await
                .map_err(|error| ClouisleError::io(format!("read guest exec output: {error}")))?;
            match frame {
                Frame::Stdout { id, chunk } if id == execution_id => {
                    stdout.extend_from_slice(&chunk)
                }
                Frame::Stderr { id, chunk } if id == execution_id => {
                    stderr.extend_from_slice(&chunk)
                }
                Frame::Exited { id, code } if id == execution_id => break code,
                Frame::Error { message, .. } => {
                    return Err(ClouisleError::new(
                        ErrorKind::Vmm,
                        format!("guest exec: {message}"),
                    ));
                }
                _ => return Err(ClouisleError::invalid_state("unexpected guest exec frame")),
            }
        };
        Ok(clouisle_core::execution::ExecutionResult {
            exit_code,
            stdout: bytes::Bytes::from(stdout),
            stderr: bytes::Bytes::from(stderr),
            duration_ms: started.elapsed().as_millis() as u64,
        })
    }
    #[cfg(target_os = "linux")]
    pub async fn exec_command_stream(
        &self,
        sandbox_id: &str,
        argv: Vec<String>,
        env: std::collections::HashMap<String, String>,
        cwd: Option<String>,
        timeout_ms: u64,
        events: tokio::sync::mpsc::Sender<std::result::Result<NodeExecEvent, tonic::Status>>,
    ) -> Result<(), ClouisleError> {
        use clouisle_proto::Frame;
        use clouisle_proto::codec::{read_frame, write_frame};
        if argv.is_empty() {
            return Err(ClouisleError::validation("argv empty"));
        }
        let sandbox = self
            .sandboxes
            .read()
            .await
            .get(sandbox_id)
            .cloned()
            .ok_or_else(|| {
                ClouisleError::not_found(format!("sandbox {sandbox_id} not on this node"))
            })?;
        if !sandbox.is_executable() {
            return Err(ClouisleError::invalid_state(format!(
                "sandbox {sandbox_id} is not running"
            )));
        }
        let address = format!("{}:5201", clouisle_net::netns::guest_ip(sandbox_id))
            .parse::<std::net::SocketAddr>()
            .map_err(|error| ClouisleError::io(format!("invalid guest address: {error}")))?;
        let stream = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            tokio::net::TcpStream::connect(address),
        )
        .await
        .map_err(|_| ClouisleError::timeout("guest agent connect timed out"))?
        .map_err(|error| ClouisleError::io(format!("connect guest agent: {error}")))?;
        let mut stream = tokio::io::BufStream::new(stream);
        write_frame(
            &mut stream,
            &Frame::Hello {
                agent_version: env!("CARGO_PKG_VERSION").to_string(),
            },
        )
        .await
        .map_err(|error| ClouisleError::io(format!("send guest hello: {error}")))?;
        if !matches!(
            read_frame(&mut stream)
                .await
                .map_err(|error| ClouisleError::io(format!("read guest hello: {error}")))?,
            Frame::Hello { .. }
        ) {
            return Err(ClouisleError::invalid_state("guest did not return Hello"));
        }
        let id = uuid::Uuid::now_v7().to_string();
        write_frame(
            &mut stream,
            &Frame::ExecReq {
                id: id.clone(),
                argv,
                env,
                cwd,
                timeout_ms,
            },
        )
        .await
        .map_err(|error| ClouisleError::io(format!("send guest ExecReq: {error}")))?;
        loop {
            match read_frame(&mut stream)
                .await
                .map_err(|error| ClouisleError::io(format!("read guest exec output: {error}")))?
            {
                Frame::Stdout {
                    id: frame_id,
                    chunk,
                } if frame_id == id => {
                    if events.send(Ok(NodeExecEvent::Stdout(chunk))).await.is_err() {
                        return Ok(());
                    }
                }
                Frame::Stderr {
                    id: frame_id,
                    chunk,
                } if frame_id == id => {
                    if events.send(Ok(NodeExecEvent::Stderr(chunk))).await.is_err() {
                        return Ok(());
                    }
                }
                Frame::Exited { id: frame_id, code } if frame_id == id => {
                    let _ = events.send(Ok(NodeExecEvent::Exit(code))).await;
                    return Ok(());
                }
                Frame::Error { message, .. } => {
                    return Err(ClouisleError::new(
                        ErrorKind::Vmm,
                        format!("guest exec: {message}"),
                    ));
                }
                _ => return Err(ClouisleError::invalid_state("unexpected guest exec frame")),
            }
        }
    }

    #[cfg(not(target_os = "linux"))]
    pub async fn exec_command(
        &self,
        _sandbox_id: &str,
        _argv: Vec<String>,
        _env: std::collections::HashMap<String, String>,
        _cwd: Option<String>,
        _timeout_ms: u64,
    ) -> Result<clouisle_core::execution::ExecutionResult, ClouisleError> {
        Err(ClouisleError::invalid_state(
            "guest execution requires Linux network namespaces",
        ))
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
        let mut reservations = self.reservations.lock().await;
        let mut sandboxes = self.sandboxes.write().await;
        for sandbox in mine {
            match self.pool.admit(&sandbox.spec).await {
                Ok(reservation) => {
                    reservations.insert(sandbox.id.clone(), reservation);
                    sandboxes.insert(sandbox.id.clone(), sandbox);
                }
                Err(error) => {
                    tracing::error!(sandbox_id = %sandbox.id, %error, "cannot restore sandbox reservation")
                }
            }
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
    struct TestVmm {
        probe_alive: bool,
    }

    impl Default for TestVmm {
        fn default() -> Self {
            Self { probe_alive: true }
        }
    }

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
        async fn probe(&self, _: &VmHandle) -> clouisle_core::Result<bool> {
            Ok(self.probe_alive)
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
            _: &str,
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
            manage_network: false,
            heartbeat_secs: 3,
        }
    }

    #[tokio::test]
    async fn registration_has_node_id() {
        let agent = NodeAgent::new(config(), Arc::new(TestVmm::default()));
        let reg = agent.registration();
        assert_eq!(reg.node.node_id, "node-1");
    }

    #[tokio::test]
    async fn create_sandbox_updates_heartbeat() {
        let agent = NodeAgent::new(config(), Arc::new(TestVmm::default()));
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
        let agent = NodeAgent::new(config(), Arc::new(TestVmm::default()));
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
        let agent = NodeAgent::new(config(), Arc::new(TestVmm::default()));
        let store = InMemoryStore::new();
        let err = agent.delete_sandbox("nope", &store).await.unwrap_err();
        assert_eq!(err.kind, ErrorKind::NotFound);
    }

    #[tokio::test]
    async fn reconcile_restores_running() {
        let agent = NodeAgent::new(config(), Arc::new(TestVmm::default()));
        let store = InMemoryStore::new();
        let sb = agent
            .create_sandbox(SandboxSpec::default(), &store)
            .await
            .unwrap();

        // 模拟重启：新 agent 实例
        let agent2 = NodeAgent::new(config(), Arc::new(TestVmm::default()));
        let n = agent2.reconcile_from_store(&store).await;
        assert_eq!(n, 1);
        assert_eq!(agent2.sandboxes.read().await.len(), 1);
        let _ = sb;
    }

    #[tokio::test]
    async fn heartbeat_empty() {
        let agent = NodeAgent::new(config(), Arc::new(TestVmm::default()));
        let hb = agent.heartbeat().await;
        assert!(hb.running_sandboxes.is_empty());
        assert_eq!(hb.node_id, "node-1");
    }
    #[tokio::test]
    async fn reconcile_marks_dead_runtime_error() {
        let store = InMemoryStore::new();
        let creator = NodeAgent::new(config(), Arc::new(TestVmm::default()));
        let sandbox = creator
            .create_sandbox(SandboxSpec::default(), &store)
            .await
            .unwrap();

        let restarted = NodeAgent::new(
            config(),
            Arc::new(TestVmm {
                probe_alive: false,
            }),
        );
        assert_eq!(restarted.reconcile_from_store(&store).await, 0);
        let persisted = store.get_sandbox(&sandbox.id).await.unwrap();
        assert_eq!(persisted.status, SandboxStatus::Error);
        assert!(persisted.terminal_message.is_some());
    }
}

