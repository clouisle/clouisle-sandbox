//! clouisled gRPC 服务端：接收 apiserver 的转发请求。
//!
//! 通过 tonic 实现，将 NodeService 的 RPC 接到 NodeAgent 的真实逻辑。

use std::sync::Arc;

use tonic::{Request, Response, Status, Streaming};

use clouisle_core::{ClouisleError, ErrorKind, SandboxSpec};
use clouisle_store::Store;

use crate::agent::NodeAgent;

// 生成的 proto 代码
pub mod proto {
    tonic::include_proto!("clouisle");
}

use proto::node_service_server::{NodeService, NodeServiceServer};
use proto::{
    CreateSandboxRequest, DeleteResult, ExecStream, FileRequest, FileResponse, HeartbeatCommand,
    HeartbeatReport, NodeId, NodeInfo, SandboxHandle, SandboxId,
};

/// 将 ClouisleError 转为 tonic Status。
#[allow(clippy::result_large_err)]
fn to_status(e: ClouisleError) -> Status {
    let code = match e.kind {
        ErrorKind::NotFound => tonic::Code::NotFound,
        ErrorKind::Validation => tonic::Code::InvalidArgument,
        ErrorKind::ResourceExhausted => tonic::Code::ResourceExhausted,
        ErrorKind::InvalidState => tonic::Code::FailedPrecondition,
        _ => tonic::Code::Internal,
    };
    Status::new(code, e.message)
}

/// 交互式控制结果 → gRPC ack 帧（成功）或 Status（失败）。
#[allow(clippy::result_large_err)]
#[cfg(target_os = "linux")]
fn control_response(
    frame_id: &str,
    result: std::result::Result<(), ClouisleError>,
) -> std::result::Result<ExecStream, Status> {
    match result {
        Ok(()) => Ok(ExecStream {
            msg: Some(proto::exec_stream::Msg::ProcessOk(
                proto::ProcessControlOk {
                    frame_id: frame_id.to_string(),
                },
            )),
        }),
        Err(error) => Err(to_status(error)),
    }
}

/// NodeService gRPC 实现。
pub struct NodeServiceImpl {
    agent: NodeAgent,
    store: Arc<dyn Store>,
}

impl NodeServiceImpl {
    pub fn new(agent: NodeAgent, store: Arc<dyn Store>) -> Self {
        Self { agent, store }
    }

    /// 启动 gRPC 服务。
    pub async fn serve(self, addr: &str) -> Result<(), Box<dyn std::error::Error>> {
        let addr: std::net::SocketAddr = addr.parse()?;
        let svc = NodeServiceServer::new(self);
        tracing::info!(addr = %addr, "clouisled gRPC listening");
        tonic::transport::Server::builder()
            .add_service(svc)
            .serve(addr)
            .await?;
        Ok(())
    }
}

#[tonic::async_trait]
impl NodeService for NodeServiceImpl {
    /// 节点注册。
    async fn register(&self, request: Request<NodeInfo>) -> Result<Response<NodeId>, Status> {
        let info = request.into_inner();
        let node_id = if info.node_id.is_empty() {
            "auto".to_string()
        } else {
            info.node_id.clone()
        };
        tracing::info!(node_id = %node_id, hostname = %info.hostname, vcpu = info.total_vcpu, "node registered");
        Ok(Response::new(NodeId { node_id }))
    }

    /// 心跳（双向流，简化：单次上报 + 回空命令）。
    type HeartbeatStream = tokio_stream::wrappers::ReceiverStream<Result<HeartbeatCommand, Status>>;

    async fn heartbeat(
        &self,
        request: Request<Streaming<HeartbeatReport>>,
    ) -> Result<Response<Self::HeartbeatStream>, Status> {
        let mut stream = request.into_inner();
        let mut reports = Vec::new();

        // 读取所有上报（简化：等第一个）
        if let Some(report) = stream
            .message()
            .await
            .map_err(|e| Status::internal(e.to_string()))?
        {
            reports.push(report);
        }

        if let Some(r) = reports.first() {
            tracing::debug!(node_id = %r.node_id, vcpu = r.allocated_vcpu, "heartbeat received");
        }

        // 返回空命令集（无排空指令）
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        let _ = tx.try_send(Ok(HeartbeatCommand { cmd: None }));
        Ok(Response::new(tokio_stream::wrappers::ReceiverStream::new(
            rx,
        )))
    }

    /// 创建沙盒。
    async fn create_sandbox(
        &self,
        request: Request<CreateSandboxRequest>,
    ) -> Result<Response<SandboxHandle>, Status> {
        let req = request.into_inner();
        if req.sandbox_id.is_empty() {
            return Err(Status::invalid_argument("sandbox_id is required"));
        }
        let spec: SandboxSpec = serde_json::from_str(&req.spec_json)
            .map_err(|e| Status::invalid_argument(format!("bad spec_json: {e}")))?;

        let sandbox = self
            .agent
            .create_sandbox_with_id(req.sandbox_id, spec, self.store.as_ref())
            .await
            .map_err(to_status)?;

        Ok(Response::new(SandboxHandle {
            sandbox_id: sandbox.id,
            backend: sandbox.vmm_meta.backend,
            pid: sandbox.vmm_meta.pid.unwrap_or(0),
            api_socket: sandbox.vmm_meta.api_socket.unwrap_or_default(),
            vsock_socket: sandbox.vmm_meta.vsock_socket.unwrap_or_default(),
        }))
    }

    /// 删除沙盒。
    async fn delete_sandbox(
        &self,
        request: Request<SandboxId>,
    ) -> Result<Response<DeleteResult>, Status> {
        let req = request.into_inner();
        match self
            .agent
            .delete_sandbox(&req.sandbox_id, self.store.as_ref())
            .await
        {
            Ok(_) => Ok(Response::new(DeleteResult {
                ok: true,
                error: String::new(),
            })),
            Err(e) => Ok(Response::new(DeleteResult {
                ok: false,
                error: e.message,
            })),
        }
    }

    /// Execute commands as a true output stream.
    type ExecStream =
        std::pin::Pin<Box<dyn futures::Stream<Item = Result<ExecStream, Status>> + Send>>;

    // tonic fixes the stream error type to Status; boxing it is incompatible with its generated trait.
    #[allow(clippy::result_large_err)]
    async fn exec(
        &self,
        request: Request<Streaming<ExecStream>>,
    ) -> Result<Response<Self::ExecStream>, Status> {
        #[cfg(target_os = "linux")]
        let mut stream = request.into_inner();

        #[cfg(not(target_os = "linux"))]
        {
            let _ = request;
            return Err(Status::unimplemented("guest execution requires Linux"));
        }

        #[cfg(target_os = "linux")]
        {
            use crate::agent::{NodeExecEvent, ProcessControlOp};

            let (tx, rx) = tokio::sync::mpsc::channel(32);
            let mut sandbox_id = String::new();
            let mut legacy_exec: Option<crate::server::proto::ExecRequest> = None;

            // 交互式进程：一个 gRPC 双向流服务一个进程会话。ProcessStart 建立
            // 会话并启动输出泵；控制消息（stdin/EOF/信号/resize）同步执行并回 ack。
            let agent = self.agent.clone();
            let mut started_any = false;
            while let Some(msg) = stream
                .message()
                .await
                .map_err(|e| Status::internal(e.to_string()))?
            {
                match msg.msg {
                    Some(proto::exec_stream::Msg::ExecReq(req)) => {
                        sandbox_id = req.sandbox_id.clone();
                        legacy_exec = Some(req);
                        break;
                    }
                    Some(proto::exec_stream::Msg::ProcessStart(req)) => {
                        sandbox_id = req.sandbox_id.clone();
                        let frame_id = req.frame_id.clone();
                        let result = agent
                            .process_start_stream(
                                &sandbox_id,
                                &frame_id,
                                req.argv,
                                req.env,
                                if req.cwd.is_empty() {
                                    None
                                } else {
                                    Some(req.cwd)
                                },
                                req.timeout_ms,
                                req.stdin,
                                req.pty.map(|pty| clouisle_proto::PtyConfig {
                                    cols: pty.cols as u16,
                                    rows: pty.rows as u16,
                                }),
                            )
                            .await;
                        match result {
                            Ok((pid, mut event_rx)) => {
                                // ack 必须先于任何输出：输出转接任务在 ack 之后启动。
                                tx.send(Ok(ExecStream {
                                    msg: Some(proto::exec_stream::Msg::ProcessStarted(
                                        proto::ProcessStarted {
                                            frame_id: frame_id.clone(),
                                            pid,
                                        },
                                    )),
                                }))
                                .await
                                .map_err(|e| Status::internal(e.to_string()))?;
                                let forward_tx = tx.clone();
                                let forward_sandbox = sandbox_id.clone();
                                tokio::spawn(async move {
                                    while let Some(event) = event_rx.recv().await {
                                        let msg = event.map(|event| match event {
                                            NodeExecEvent::Stdout(data) => ExecStream {
                                                msg: Some(proto::exec_stream::Msg::ExecStdout(
                                                    proto::ExecOutput {
                                                        sandbox_id: forward_sandbox.clone(),
                                                        data: data.to_vec(),
                                                        frame_id: frame_id.clone(),
                                                    },
                                                )),
                                            },
                                            NodeExecEvent::Stderr(data) => ExecStream {
                                                msg: Some(proto::exec_stream::Msg::ExecStderr(
                                                    proto::ExecOutput {
                                                        sandbox_id: forward_sandbox.clone(),
                                                        data: data.to_vec(),
                                                        frame_id: frame_id.clone(),
                                                    },
                                                )),
                                            },
                                            NodeExecEvent::Exit(exit_code) => ExecStream {
                                                msg: Some(proto::exec_stream::Msg::ExecExit(
                                                    proto::ExecExit {
                                                        sandbox_id: forward_sandbox.clone(),
                                                        exit_code,
                                                        frame_id: frame_id.clone(),
                                                    },
                                                )),
                                            },
                                        });
                                        if forward_tx.send(msg).await.is_err() {
                                            break;
                                        }
                                    }
                                });
                            }
                            Err(error) => {
                                tx.send(Err(to_status(error)))
                                    .await
                                    .map_err(|e| Status::internal(e.to_string()))?;
                            }
                        }
                        started_any = true;
                    }
                    Some(proto::exec_stream::Msg::ProcessInput(req)) => {
                        let result = agent
                            .process_control(
                                &sandbox_id,
                                &req.frame_id,
                                ProcessControlOp::Stdin(bytes::Bytes::from(req.data)),
                            )
                            .await;
                        tx.send(control_response(&req.frame_id, result))
                            .await
                            .map_err(|e| Status::internal(e.to_string()))?;
                    }
                    Some(proto::exec_stream::Msg::ProcessEof(req)) => {
                        let result = agent
                            .process_control(&sandbox_id, &req.frame_id, ProcessControlOp::StdinEof)
                            .await;
                        tx.send(control_response(&req.frame_id, result))
                            .await
                            .map_err(|e| Status::internal(e.to_string()))?;
                    }
                    Some(proto::exec_stream::Msg::ProcessSignal(req)) => {
                        let signal = match clouisle_proto::ProcessSignal::try_from(req.signal as u8)
                        {
                            Ok(signal) => signal,
                            Err(_) => {
                                let _ = tx
                                    .send(Err(Status::invalid_argument("unsupported signal")))
                                    .await;
                                continue;
                            }
                        };
                        let result = agent
                            .process_control(
                                &sandbox_id,
                                &req.frame_id,
                                ProcessControlOp::Signal(signal),
                            )
                            .await;
                        tx.send(control_response(&req.frame_id, result))
                            .await
                            .map_err(|e| Status::internal(e.to_string()))?;
                    }
                    Some(proto::exec_stream::Msg::ProcessResize(req)) => {
                        let result = agent
                            .process_control(
                                &sandbox_id,
                                &req.frame_id,
                                ProcessControlOp::Resize {
                                    cols: req.cols as u16,
                                    rows: req.rows as u16,
                                },
                            )
                            .await;
                        tx.send(control_response(&req.frame_id, result))
                            .await
                            .map_err(|e| Status::internal(e.to_string()))?;
                    }
                    Some(_) | None => continue,
                }
            }

            if !started_any {
                let req = match legacy_exec {
                    Some(r) => r,
                    None => return Err(Status::invalid_argument("no exec request in stream")),
                };
                let task_sandbox_id = req.sandbox_id.clone();
                let agent = self.agent.clone();
                let forward_tx = tx.clone();
                let forward_sandbox = sandbox_id.clone();
                tokio::spawn(async move {
                    let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(32);
                    let pump = tokio::spawn(async move {
                        agent
                            .exec_command_stream(
                                &task_sandbox_id,
                                req.argv,
                                req.env,
                                if req.cwd.is_empty() {
                                    None
                                } else {
                                    Some(req.cwd)
                                },
                                req.timeout_ms,
                                event_tx,
                            )
                            .await
                    });
                    while let Some(event) = event_rx.recv().await {
                        let msg = event.map(|event| match event {
                            NodeExecEvent::Stdout(data) => ExecStream {
                                msg: Some(proto::exec_stream::Msg::ExecStdout(proto::ExecOutput {
                                    sandbox_id: forward_sandbox.clone(),
                                    data: data.to_vec(),
                                    frame_id: String::new(),
                                })),
                            },
                            NodeExecEvent::Stderr(data) => ExecStream {
                                msg: Some(proto::exec_stream::Msg::ExecStderr(proto::ExecOutput {
                                    sandbox_id: forward_sandbox.clone(),
                                    data: data.to_vec(),
                                    frame_id: String::new(),
                                })),
                            },
                            NodeExecEvent::Exit(exit_code) => ExecStream {
                                msg: Some(proto::exec_stream::Msg::ExecExit(proto::ExecExit {
                                    sandbox_id: forward_sandbox.clone(),
                                    exit_code,
                                    frame_id: String::new(),
                                })),
                            },
                        });
                        if forward_tx.send(msg).await.is_err() {
                            break;
                        }
                    }
                    if let Ok(Err(error)) = pump.await {
                        let _ = forward_tx.send(Err(to_status(error))).await;
                    }
                });
            }
            let stream = tokio_stream::wrappers::ReceiverStream::new(rx);
            Ok(Response::new(Box::pin(stream)))
        }
    }

    async fn file_op(
        &self,
        request: Request<FileRequest>,
    ) -> Result<Response<FileResponse>, Status> {
        let request = request.into_inner();
        let sandbox_id = request.sandbox_id;
        let frame = match request.op {
            Some(proto::file_request::Op::Write(write)) => clouisle_proto::Frame::WriteFile {
                path: write.path,
                mode: write.mode,
                content: bytes::Bytes::from(write.content),
            },
            Some(proto::file_request::Op::Read(read)) => clouisle_proto::Frame::ReadFile {
                path: read.path,
                offset: 0,
                length: u64::MAX,
            },
            Some(proto::file_request::Op::List(list)) => {
                clouisle_proto::Frame::ListDir { path: list.path }
            }
            None => return Err(Status::invalid_argument("file operation is required")),
        };
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (sandbox_id, frame);
            return Err(Status::unimplemented("guest file operations require Linux"));
        }
        #[cfg(target_os = "linux")]
        {
            let response = self
                .agent
                .file_op(&sandbox_id, frame)
                .await
                .map_err(to_status)?;
            let result = match response {
                clouisle_proto::Frame::WriteFileResult { .. } => {
                    proto::file_response::Result::WriteOk(true)
                }
                clouisle_proto::Frame::ReadFileResult { content, .. } => {
                    proto::file_response::Result::Content(content.to_vec())
                }
                clouisle_proto::Frame::ListDirResult { entries } => {
                    proto::file_response::Result::Entries(proto::FileEntries {
                        entries: entries
                            .into_iter()
                            .map(|entry| proto::FileEntry {
                                name: entry.name,
                                size: entry.size,
                                mode: entry.mode,
                                mtime: entry.mtime,
                                is_dir: entry.is_dir,
                            })
                            .collect(),
                    })
                }
                _ => return Err(Status::internal("unexpected guest file response")),
            };
            Ok(Response::new(FileResponse {
                result: Some(result),
            }))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use clouisle_vmm::{
        SnapshotKind, SnapshotPaths, StopMode, VmHandle, VmStats, Vmm, VmmCapabilities,
    };
    use proto::node_service_client::NodeServiceClient;
    use tokio_stream::wrappers::ReceiverStream;

    #[derive(Clone)]
    struct TestVmm;

    #[async_trait]
    impl Vmm for TestVmm {
        async fn create(
            &self,
            _: &str,
            _: &clouisle_core::SandboxSpec,
        ) -> Result<VmHandle, clouisle_core::ClouisleError> {
            Ok(VmHandle {
                id: "vm-1".into(),
                backend: "test".into(),
                owner_id: None,
                pid: None,
                api_socket: None,
                vsock_socket: None,
                vsock_cid: None,
                subnet: None,
            })
        }
        async fn image_cache_hit(
            &self,
            _: &clouisle_core::SandboxSpec,
        ) -> Result<bool, clouisle_core::ClouisleError> {
            Ok(true)
        }
        async fn prefetch_image(
            &self,
            _: &clouisle_core::SandboxSpec,
        ) -> Result<(), clouisle_core::ClouisleError> {
            Ok(())
        }
        async fn probe(&self, _: &VmHandle) -> Result<bool, clouisle_core::ClouisleError> {
            Ok(true)
        }
        async fn start(&self, _: &VmHandle) -> Result<(), clouisle_core::ClouisleError> {
            Ok(())
        }
        async fn pause(&self, _: &VmHandle) -> Result<(), clouisle_core::ClouisleError> {
            Ok(())
        }
        async fn resume(&self, _: &VmHandle) -> Result<(), clouisle_core::ClouisleError> {
            Ok(())
        }
        async fn snapshot(
            &self,
            _: &VmHandle,
            _: SnapshotKind,
            _: &SnapshotPaths,
        ) -> Result<(), clouisle_core::ClouisleError> {
            Ok(())
        }
        async fn restore(
            &self,
            _: &str,
            _: &clouisle_core::SandboxSpec,
            _: &SnapshotPaths,
        ) -> Result<VmHandle, clouisle_core::ClouisleError> {
            Ok(VmHandle {
                id: "vm-1".into(),
                backend: "test".into(),
                owner_id: None,
                pid: None,
                api_socket: None,
                vsock_socket: None,
                vsock_cid: None,
                subnet: None,
            })
        }
        async fn stop(
            &self,
            _: &VmHandle,
            _: StopMode,
        ) -> Result<(), clouisle_core::ClouisleError> {
            Ok(())
        }
        async fn stats(&self, _: &VmHandle) -> Result<VmStats, clouisle_core::ClouisleError> {
            Ok(VmStats::default())
        }
        fn capabilities(&self) -> VmmCapabilities {
            VmmCapabilities {
                snapshot: false,
                vsock: true,
                balloon: false,
            }
        }
    }

    fn test_service() -> NodeServiceImpl {
        use std::collections::HashMap;
        let config = crate::agent::NodeAgentConfig {
            node_id: "node-1".into(),
            hostname: "node-1".into(),
            total_vcpu: 8,
            total_memory_mb: 16384,
            total_disk_mb: 102400,
            kvm_available: true,
            kernel_version: "6.1".into(),
            firecracker_version: "1.4".into(),
            labels: HashMap::new(),
            manage_network: false,
            heartbeat_secs: 3,
        };
        let agent = crate::agent::NodeAgent::new(config, Arc::new(TestVmm));
        NodeServiceImpl::new(agent, Arc::new(clouisle_store::InMemoryStore::new()))
    }

    async fn spawn_service() -> String {
        let service = test_service();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(NodeServiceServer::new(service))
                .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
                .await
        });
        format!("http://{addr}")
    }

    /// gRPC 双向流对交互式消息的分发：ProcessStart 对未知沙盒返回错误，
    /// 验证 oneof 识别、分发与错误传播路径。
    #[tokio::test]
    async fn exec_stream_dispatches_process_start_error() {
        let endpoint = spawn_service().await;
        let mut client = NodeServiceClient::connect(endpoint).await.unwrap();
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        tx.send(proto::ExecStream {
            msg: Some(proto::exec_stream::Msg::ProcessStart(proto::ProcessStart {
                sandbox_id: "missing-sandbox".into(),
                frame_id: "frame-1".into(),
                argv: vec!["echo".into(), "hi".into()],
                env: Default::default(),
                cwd: String::new(),
                timeout_ms: 5000,
                stdin: false,
                pty: None,
            })),
        })
        .await
        .unwrap();
        drop(tx);
        let response = client
            .exec(tonic::Request::new(ReceiverStream::new(rx)))
            .await
            .unwrap();
        let mut stream = response.into_inner();
        let status = match stream.message().await {
            Ok(Some(msg)) => panic!("expected NotFound error, got message {msg:?}"),
            Ok(None) => panic!("stream ended without error"),
            Err(status) => status,
        };
        assert_eq!(status.code(), tonic::Code::NotFound);
    }

    /// 控制消息（stdin）对未知进程返回错误帧。
    #[tokio::test]
    async fn exec_stream_dispatches_process_control_error() {
        let endpoint = spawn_service().await;
        let mut client = NodeServiceClient::connect(endpoint).await.unwrap();
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        tx.send(proto::ExecStream {
            msg: Some(proto::exec_stream::Msg::ProcessInput(proto::ProcessInput {
                frame_id: "frame-1".into(),
                data: b"hello".to_vec(),
            })),
        })
        .await
        .unwrap();
        drop(tx);
        // 孤立控制帧（无先行的 ProcessStart 会话）在握手阶段被拒绝。
        let error = client
            .exec(tonic::Request::new(ReceiverStream::new(rx)))
            .await
            .unwrap_err();
        assert_eq!(error.code(), tonic::Code::InvalidArgument);
    }
}
