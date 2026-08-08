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
    CreateSandboxRequest, DeleteResult, ExecRequest, ExecStream, ExecOutput, ExecExit,
    HeartbeatCommand, HeartbeatReport, NodeId, NodeInfo, SandboxHandle, SandboxId,
};

/// 将 ClouisleError 转为 tonic Status。
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
        while let Some(report) = stream.message().await.map_err(|e| Status::internal(e.to_string()))? {
            reports.push(report);
            break; // 简化：只处理第一条，生产用双向流持续
        }

        if let Some(r) = reports.first() {
            tracing::debug!(node_id = %r.node_id, vcpu = r.allocated_vcpu, "heartbeat received");
        }

        // 返回空命令集（无排空指令）
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        let _ = tx.try_send(Ok(HeartbeatCommand {
            cmd: None,
        }));
        Ok(Response::new(tokio_stream::wrappers::ReceiverStream::new(rx)))
    }

    /// 创建沙盒。
    async fn create_sandbox(
        &self,
        request: Request<CreateSandboxRequest>,
    ) -> Result<Response<SandboxHandle>, Status> {
        let req = request.into_inner();
        let spec: SandboxSpec = serde_json::from_str(&req.spec_json)
            .map_err(|e| Status::invalid_argument(format!("bad spec_json: {e}")))?;

        let sandbox = self.agent.create_sandbox(spec, self.store.as_ref())
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
        match self.agent.delete_sandbox(&req.sandbox_id, self.store.as_ref()).await {
            Ok(_) => Ok(Response::new(DeleteResult { ok: true, error: String::new() })),
            Err(e) => Ok(Response::new(DeleteResult { ok: false, error: e.message })),
        }
    }

    /// 执行命令（双向流，简化：请求→响应）。
    type ExecStream = tokio_stream::wrappers::ReceiverStream<Result<ExecStream, Status>>;

    async fn exec(
        &self,
        request: Request<Streaming<ExecStream>>,
    ) -> Result<Response<Self::ExecStream>, Status> {
        let mut stream = request.into_inner();
        let mut exec_req: Option<ExecRequest> = None;

        while let Some(msg) = stream.message().await.map_err(|e| Status::internal(e.to_string()))? {
            if let Some(ExecStream { msg: Some(proto::exec_stream::Msg::ExecReq(req)) }) = Some(msg) {
                exec_req = Some(req);
                break;
            }
        }

        let req = match exec_req {
            Some(r) => r,
            None => return Err(Status::invalid_argument("no exec request in stream")),
        };

        // 通过 agent 执行
        let result = match self.agent.exec_command(
            &req.sandbox_id,
            req.argv,
            req.env,
            if req.cwd.is_empty() { None } else { Some(req.cwd) },
            req.timeout_ms,
        ).await {
            Ok(r) => r,
            Err(e) => {
                let (tx, rx) = tokio::sync::mpsc::channel(8);
                let _ = tx.try_send(Err(to_status(e)));
                return Ok(Response::new(tokio_stream::wrappers::ReceiverStream::new(rx)));
            }
        };

        let (tx, rx) = tokio::sync::mpsc::channel(8);
        if !result.stdout.is_empty() {
            let _ = tx.try_send(Ok(ExecStream {
                msg: Some(proto::exec_stream::Msg::ExecStdout(ExecOutput {
                    sandbox_id: req.sandbox_id.clone(),
                    data: result.stdout.to_vec(),
                })),
            }));
        }
        if !result.stderr.is_empty() {
            let _ = tx.try_send(Ok(ExecStream {
                msg: Some(proto::exec_stream::Msg::ExecStderr(ExecOutput {
                    sandbox_id: req.sandbox_id.clone(),
                    data: result.stderr.to_vec(),
                })),
            }));
        }
        let _ = tx.try_send(Ok(ExecStream {
            msg: Some(proto::exec_stream::Msg::ExecExit(ExecExit {
                sandbox_id: req.sandbox_id,
                exit_code: result.exit_code,
            })),
        }));

        Ok(Response::new(tokio_stream::wrappers::ReceiverStream::new(rx)))
    }
}