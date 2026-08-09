//! gRPC-backed VMM adapter for a `clouisled` node daemon.
//!
//! The control plane remains the source of metadata while this adapter routes
//! process ownership to the node that has `/dev/kvm` and the guest network
//! namespace.
use std::collections::HashMap;

use crate::agent::{AgentConnection, AgentConnector};
use async_trait::async_trait;
use bytes::Bytes;
use clouisle_core::{ClouisleError, ErrorKind, Result, SandboxSpec};
use clouisle_vmm::{
    SnapshotKind, SnapshotPaths, StopMode, VmHandle, VmStats, Vmm, VmmCapabilities,
};
use clouisled::server::proto::node_service_client::NodeServiceClient;
use clouisled::server::proto::{
    CreateSandboxRequest, FileList, FileRead, FileRequest, FileWrite, SandboxId,
};

#[derive(Debug, Clone)]
pub struct GrpcNodeVmm {
    endpoint: String,
}

impl GrpcNodeVmm {
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
        }
    }

    async fn client(&self) -> Result<NodeServiceClient<tonic::transport::Channel>> {
        NodeServiceClient::connect(self.endpoint.clone())
            .await
            .map_err(|error| {
                ClouisleError::new(
                    ErrorKind::Vmm,
                    format!("connect clouisled {}: {error}", self.endpoint),
                )
            })
    }
}

#[async_trait]
impl Vmm for GrpcNodeVmm {
    async fn create(&self, sandbox_id: &str, spec: &SandboxSpec) -> Result<VmHandle> {
        let spec_json = serde_json::to_string(spec)
            .map_err(|error| ClouisleError::new(ErrorKind::Validation, error.to_string()))?;
        let mut client = self.client().await?;
        let response = client
            .create_sandbox(CreateSandboxRequest {
                sandbox_id: sandbox_id.to_string(),
                spec_json,
            })
            .await
            .map_err(|error| {
                ClouisleError::new(
                    ErrorKind::Vmm,
                    format!("create sandbox on clouisled: {error}"),
                )
            })?
            .into_inner();
        if response.sandbox_id != sandbox_id {
            return Err(ClouisleError::new(
                ErrorKind::Vmm,
                format!(
                    "clouisled returned sandbox id {} for requested {sandbox_id}",
                    response.sandbox_id
                ),
            ));
        }
        Ok(VmHandle {
            id: response.sandbox_id,
            backend: response.backend,
            pid: (response.pid != 0).then_some(response.pid),
            api_socket: (!response.api_socket.is_empty()).then_some(response.api_socket),
            vsock_socket: (!response.vsock_socket.is_empty()).then_some(response.vsock_socket),
            vsock_cid: None,
        })
    }

    /// `CreateSandbox` starts the guest atomically on the owning node.
    async fn start(&self, _handle: &VmHandle) -> Result<()> {
        Ok(())
    }

    async fn pause(&self, _handle: &VmHandle) -> Result<()> {
        Err(ClouisleError::invalid_state(
            "remote pause is not exposed by the node protocol",
        ))
    }

    async fn resume(&self, _handle: &VmHandle) -> Result<()> {
        Err(ClouisleError::invalid_state(
            "remote resume is not exposed by the node protocol",
        ))
    }

    async fn snapshot(
        &self,
        _handle: &VmHandle,
        _kind: SnapshotKind,
        _out: &SnapshotPaths,
    ) -> Result<()> {
        Err(ClouisleError::invalid_state(
            "remote snapshots are not exposed by the node protocol",
        ))
    }

    async fn restore(
        &self,
        _sandbox_id: &str,
        _spec: &SandboxSpec,
        _from: &SnapshotPaths,
    ) -> Result<VmHandle> {
        Err(ClouisleError::invalid_state(
            "remote snapshot restore is not exposed by the node protocol",
        ))
    }

    async fn stop(&self, handle: &VmHandle, _mode: StopMode) -> Result<()> {
        let mut client = self.client().await?;
        let result = client
            .delete_sandbox(SandboxId {
                sandbox_id: handle.id.clone(),
            })
            .await
            .map_err(|error| {
                ClouisleError::new(
                    ErrorKind::Vmm,
                    format!("delete sandbox on clouisled: {error}"),
                )
            })?
            .into_inner();
        if result.ok {
            Ok(())
        } else {
            Err(ClouisleError::new(ErrorKind::Vmm, result.error))
        }
    }

    async fn stats(&self, _handle: &VmHandle) -> Result<VmStats> {
        Err(ClouisleError::invalid_state(
            "remote VMM statistics are not exposed by the node protocol",
        ))
    }

    fn capabilities(&self) -> VmmCapabilities {
        VmmCapabilities {
            snapshot: false,
            vsock: false,
            balloon: false,
        }
    }
}

/// Selects a ready node from the durable registry for every new sandbox.
#[derive(Clone)]
pub struct ScheduledNodeVmm {
    store: std::sync::Arc<dyn clouisle_store::Store>,
}

impl ScheduledNodeVmm {
    pub fn new(store: std::sync::Arc<dyn clouisle_store::Store>) -> Self {
        Self { store }
    }

    async fn node_for(&self, spec: &SandboxSpec) -> Result<clouisle_core::RegisteredNode> {
        let nodes = self
            .store
            .list_ready_nodes(chrono::Utc::now().timestamp_millis() - 15_000)
            .await?;
        let infos = nodes
            .iter()
            .map(|node| node.info.clone())
            .collect::<Vec<_>>();
        let allocations = nodes
            .iter()
            .map(|node| clouisle_scheduler::placement::NodeAllocation {
                node_id: node.info.node_id.clone(),
                allocated_vcpu: node.allocated_vcpu,
                allocated_memory_mb: node.allocated_memory_mb,
                sandbox_count: node.running_sandboxes,
            })
            .collect::<Vec<_>>();
        let selected = clouisle_scheduler::placement::place(
            &infos,
            &allocations,
            spec,
            clouisle_scheduler::PlacementStrategy::LeastLoaded,
        )
        .ok_or_else(|| {
            ClouisleError::resource_exhausted("no ready node can satisfy sandbox resources")
        })?;
        nodes
            .into_iter()
            .find(|node| node.info.node_id == selected.node_id)
            .ok_or_else(|| ClouisleError::invalid_state("selected node missing from registry"))
    }
}

#[cfg(test)]
mod scheduled_tests {
    use super::*;
    use clouisle_core::{NodeInfo, NodeStatus, Resources, SandboxSpec};
    use clouisle_store::{InMemoryStore, Store};

    #[tokio::test]
    async fn selects_current_ready_node() {
        let store = std::sync::Arc::new(InMemoryStore::new());
        store
            .upsert_node(&clouisle_core::RegisteredNode {
                info: NodeInfo {
                    node_id: "node-a".into(),
                    hostname: "node-a".into(),
                    total_vcpu: 4,
                    total_memory_mb: 4096,
                    total_disk_mb: 4096,
                    kvm_available: true,
                    kernel_version: "test".into(),
                    firecracker_version: "test".into(),
                    labels: Default::default(),
                },
                endpoint: "http://node-a:9090".into(),
                status: NodeStatus::Ready,
                last_heartbeat_ms: chrono::Utc::now().timestamp_millis(),
                allocated_vcpu: 0,
                allocated_memory_mb: 0,
                running_sandboxes: 0,
            })
            .await
            .unwrap();
        let vmm = ScheduledNodeVmm::new(store);
        let spec = SandboxSpec {
            resources: Resources {
                vcpu: 1,
                memory_mb: 128,
                ..Resources::default()
            },
            ..SandboxSpec::default()
        };
        assert_eq!(
            vmm.node_for(&spec).await.unwrap().endpoint,
            "http://node-a:9090"
        );
    }
}

#[async_trait]
impl Vmm for ScheduledNodeVmm {
    async fn create(&self, sandbox_id: &str, spec: &SandboxSpec) -> Result<VmHandle> {
        let node = self.node_for(spec).await?;
        let mut handle = GrpcNodeVmm::new(&node.endpoint)
            .create(sandbox_id, spec)
            .await?;
        handle.backend = format!("grpc:{}", node.endpoint);
        Ok(handle)
    }
    async fn start(&self, _: &VmHandle) -> Result<()> {
        Ok(())
    }
    async fn pause(&self, _: &VmHandle) -> Result<()> {
        Err(ClouisleError::invalid_state("remote pause unavailable"))
    }
    async fn resume(&self, _: &VmHandle) -> Result<()> {
        Err(ClouisleError::invalid_state("remote resume unavailable"))
    }
    async fn snapshot(&self, _: &VmHandle, _: SnapshotKind, _: &SnapshotPaths) -> Result<()> {
        Err(ClouisleError::invalid_state("remote snapshot unavailable"))
    }
    async fn restore(&self, _: &str, _: &SandboxSpec, _: &SnapshotPaths) -> Result<VmHandle> {
        Err(ClouisleError::invalid_state("remote restore unavailable"))
    }
    async fn stop(&self, handle: &VmHandle, mode: StopMode) -> Result<()> {
        let endpoint = handle
            .backend
            .strip_prefix("grpc:")
            .ok_or_else(|| ClouisleError::invalid_state("missing persisted node endpoint"))?;
        GrpcNodeVmm::new(endpoint).stop(handle, mode).await
    }
    async fn stats(&self, _: &VmHandle) -> Result<VmStats> {
        Err(ClouisleError::invalid_state(
            "remote statistics unavailable",
        ))
    }
    fn capabilities(&self) -> VmmCapabilities {
        VmmCapabilities {
            snapshot: false,
            vsock: false,
            balloon: false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct GrpcAgentConnector {
    endpoint: String,
}

impl GrpcAgentConnector {
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
        }
    }
}

#[async_trait]
impl AgentConnector for GrpcAgentConnector {
    async fn connect_and_hello(
        &self,
        handle: &VmHandle,
        sandbox_id: &str,
    ) -> Result<Box<dyn AgentConnection>> {
        let endpoint = handle
            .backend
            .strip_prefix("grpc:")
            .unwrap_or(&self.endpoint);
        NodeServiceClient::connect(endpoint.to_string())
            .await
            .map_err(|error| {
                ClouisleError::new(ErrorKind::Vmm, format!("connect node: {error}"))
            })?;
        Ok(Box::new(GrpcAgentConnection {
            endpoint: endpoint.to_string(),
            sandbox_id: sandbox_id.to_string(),
        }))
    }
}

struct GrpcAgentConnection {
    endpoint: String,
    sandbox_id: String,
}

#[async_trait]
impl AgentConnection for GrpcAgentConnection {
    fn sandbox_id(&self) -> &str {
        &self.sandbox_id
    }

    async fn exec(
        &self,
        argv: Vec<String>,
        env: HashMap<String, String>,
        cwd: Option<String>,
        timeout_ms: u64,
    ) -> Result<clouisle_core::execution::ExecutionResult> {
        use clouisled::server::proto::{ExecRequest, ExecStream, exec_stream};
        use tokio_stream::wrappers::ReceiverStream;

        let (tx, rx) = tokio::sync::mpsc::channel(1);
        tx.send(ExecStream {
            msg: Some(exec_stream::Msg::ExecReq(ExecRequest {
                sandbox_id: self.sandbox_id.clone(),
                argv,
                env,
                cwd: cwd.unwrap_or_default(),
                timeout_ms,
            })),
        })
        .await
        .map_err(|error| ClouisleError::io(format!("queue node exec request: {error}")))?;
        drop(tx);

        let mut client = NodeServiceClient::connect(self.endpoint.clone())
            .await
            .map_err(|error| {
                ClouisleError::new(ErrorKind::Vmm, format!("connect node: {error}"))
            })?;
        let mut stream = client
            .exec(tonic::Request::new(ReceiverStream::new(rx)))
            .await
            .map_err(|error| ClouisleError::new(ErrorKind::Vmm, format!("node exec: {error}")))?
            .into_inner();
        let started = std::time::Instant::now();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let exit_code = loop {
            let message = stream
                .message()
                .await
                .map_err(|error| {
                    ClouisleError::new(ErrorKind::Vmm, format!("read node exec: {error}"))
                })?
                .ok_or_else(|| ClouisleError::io("node exec stream ended before exit"))?;
            match message.msg {
                Some(exec_stream::Msg::ExecStdout(output)) => stdout.extend(output.data),
                Some(exec_stream::Msg::ExecStderr(output)) => stderr.extend(output.data),
                Some(exec_stream::Msg::ExecExit(exit)) => break exit.exit_code,
                Some(exec_stream::Msg::ExecReq(_)) | None => {
                    return Err(ClouisleError::invalid_state(
                        "unexpected node exec response",
                    ));
                }
            }
        };
        Ok(clouisle_core::execution::ExecutionResult {
            exit_code,
            stdout: Bytes::from(stdout),
            stderr: Bytes::from(stderr),
            duration_ms: started.elapsed().as_millis() as u64,
        })
    }

    async fn exec_stream(
        &self,
        argv: Vec<String>,
        env: HashMap<String, String>,
        cwd: Option<String>,
        timeout_ms: u64,
        events: tokio::sync::mpsc::Sender<crate::agent::ExecStreamEvent>,
    ) -> Result<()> {
        use clouisled::server::proto::{ExecRequest, ExecStream, exec_stream};
        use tokio_stream::wrappers::ReceiverStream;
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        tx.send(ExecStream {
            msg: Some(exec_stream::Msg::ExecReq(ExecRequest {
                sandbox_id: self.sandbox_id.clone(),
                argv,
                env,
                cwd: cwd.unwrap_or_default(),
                timeout_ms,
            })),
        })
        .await
        .map_err(|error| ClouisleError::io(format!("queue node exec request: {error}")))?;
        drop(tx);
        let mut client = NodeServiceClient::connect(self.endpoint.clone())
            .await
            .map_err(|error| {
                ClouisleError::new(ErrorKind::Vmm, format!("connect node: {error}"))
            })?;
        let mut stream = client
            .exec(tonic::Request::new(ReceiverStream::new(rx)))
            .await
            .map_err(|error| ClouisleError::new(ErrorKind::Vmm, format!("node exec: {error}")))?
            .into_inner();
        while let Some(message) = stream.message().await.map_err(|error| {
            ClouisleError::new(ErrorKind::Vmm, format!("read node exec: {error}"))
        })? {
            let event = match message.msg {
                Some(exec_stream::Msg::ExecStdout(output)) => {
                    crate::agent::ExecStreamEvent::Stdout(Bytes::from(output.data))
                }
                Some(exec_stream::Msg::ExecStderr(output)) => {
                    crate::agent::ExecStreamEvent::Stderr(Bytes::from(output.data))
                }
                Some(exec_stream::Msg::ExecExit(exit)) => {
                    let _ = events
                        .send(crate::agent::ExecStreamEvent::Exit(exit.exit_code))
                        .await;
                    return Ok(());
                }
                Some(exec_stream::Msg::ExecReq(_)) | None => {
                    return Err(ClouisleError::invalid_state(
                        "unexpected node exec response",
                    ));
                }
            };
            if events.send(event).await.is_err() {
                return Ok(());
            }
        }
        Err(ClouisleError::io("node exec stream ended before exit"))
    }
    async fn write_file(&self, path: &str, content: Bytes, mode: u32) -> Result<()> {
        let mut client = NodeServiceClient::connect(self.endpoint.clone())
            .await
            .map_err(|error| {
                ClouisleError::new(ErrorKind::Vmm, format!("connect node: {error}"))
            })?;
        let response = client
            .file_op(FileRequest {
                sandbox_id: self.sandbox_id.clone(),
                op: Some(clouisled::server::proto::file_request::Op::Write(
                    FileWrite {
                        path: path.to_string(),
                        content: content.to_vec(),
                        mode,
                    },
                )),
            })
            .await
            .map_err(|error| {
                ClouisleError::new(ErrorKind::Vmm, format!("node write file: {error}"))
            })?
            .into_inner();
        match response.result {
            Some(clouisled::server::proto::file_response::Result::WriteOk(true)) => Ok(()),
            _ => Err(ClouisleError::invalid_state(
                "unexpected node write response",
            )),
        }
    }

    async fn read_file(&self, path: &str) -> Result<Bytes> {
        let mut client = NodeServiceClient::connect(self.endpoint.clone())
            .await
            .map_err(|error| {
                ClouisleError::new(ErrorKind::Vmm, format!("connect node: {error}"))
            })?;
        let response = client
            .file_op(FileRequest {
                sandbox_id: self.sandbox_id.clone(),
                op: Some(clouisled::server::proto::file_request::Op::Read(FileRead {
                    path: path.to_string(),
                })),
            })
            .await
            .map_err(|error| {
                ClouisleError::new(ErrorKind::Vmm, format!("node read file: {error}"))
            })?
            .into_inner();
        match response.result {
            Some(clouisled::server::proto::file_response::Result::Content(content)) => {
                Ok(Bytes::from(content))
            }
            _ => Err(ClouisleError::invalid_state(
                "unexpected node read response",
            )),
        }
    }

    async fn list_dir(&self, path: &str) -> Result<Vec<clouisle_core::DirEntry>> {
        let mut client = NodeServiceClient::connect(self.endpoint.clone())
            .await
            .map_err(|error| {
                ClouisleError::new(ErrorKind::Vmm, format!("connect node: {error}"))
            })?;
        let response = client
            .file_op(FileRequest {
                sandbox_id: self.sandbox_id.clone(),
                op: Some(clouisled::server::proto::file_request::Op::List(FileList {
                    path: path.to_string(),
                })),
            })
            .await
            .map_err(|error| {
                ClouisleError::new(ErrorKind::Vmm, format!("node list directory: {error}"))
            })?
            .into_inner();
        match response.result {
            Some(clouisled::server::proto::file_response::Result::Entries(entries)) => Ok(entries
                .entries
                .into_iter()
                .map(|entry| clouisle_core::DirEntry {
                    name: entry.name,
                    size: entry.size,
                    mode: entry.mode,
                    mtime: entry.mtime,
                    is_dir: entry.is_dir,
                })
                .collect()),
            _ => Err(ClouisleError::invalid_state(
                "unexpected node list response",
            )),
        }
    }

    async fn ping(&self) -> Result<()> {
        NodeServiceClient::connect(self.endpoint.clone())
            .await
            .map(|_| ())
            .map_err(|error| ClouisleError::new(ErrorKind::Vmm, format!("connect node: {error}")))
    }
}
