//! 全局应用状态。

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, atomic::AtomicBool};

use crate::agent::AgentConnector;
use crate::auth::Authenticator;
use clouisle_core::{ImageRef, Resources, RestartPolicy, Sandbox, SandboxStatus};
use clouisle_scheduler::ResourcePool;
use clouisle_store::Store;
use clouisle_vmm::Vmm;
use serde::Serialize;

#[cfg(target_os = "linux")]
use clouisle_net::FirewallManager;

#[derive(Debug, Clone, Serialize)]
pub struct ImagePrefetchJob {
    pub job_id: String,
    pub image: ImageRef,
    #[serde(skip)]
    pub tenant_id: String,
    pub status: String,
    pub error: Option<String>,
}

#[derive(Clone, Default)]
pub struct ImageJobRegistry {
    jobs: Arc<tokio::sync::RwLock<HashMap<String, ImagePrefetchJob>>>,
}

impl ImageJobRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn insert(&self, job: ImagePrefetchJob) {
        self.jobs.write().await.insert(job.job_id.clone(), job);
    }

    pub async fn update(&self, id: &str, status: &str, error: Option<String>) {
        if let Some(job) = self.jobs.write().await.get_mut(id) {
            job.status = status.to_string();
            job.error = error;
        }
    }

    pub async fn get(&self, id: &str) -> Option<ImagePrefetchJob> {
        self.jobs.read().await.get(id).cloned()
    }
}

/// 应用状态（所有 handler 共享）。
#[derive(Clone)]
pub struct AppState {
    pub store: Arc<dyn Store>,
    /// Durable E2B team/control-plane records, separate from runtime rows.
    pub e2b: Arc<crate::e2b_cloud::E2bControlPlane>,
    pub vmm: Arc<dyn Vmm>,
    pub pool: Arc<ResourcePool>,
    /// Pre-started VM slots keyed by sandbox ID until the sandbox is deleted.
    pub warm_pool: Arc<clouisle_pool::Pool>,
    pub warm_slots: Arc<tokio::sync::Mutex<HashMap<String, clouisle_pool::PoolSlot>>>,
    /// Reservations held for each live local sandbox; dropping on delete releases capacity.
    pub reservations: Arc<tokio::sync::Mutex<HashMap<String, clouisle_scheduler::Reservation>>>,
    /// True only when this API directly owns the local VMM resource pool.
    pub manage_resources: bool,
    pub agent: Arc<dyn AgentConnector>,
    pub auth: Arc<Authenticator>,
    #[cfg(target_os = "linux")]
    pub firewall: Arc<FirewallManager>,
    /// Production owns netns/TAP lifecycle; HTTP test fixtures disable it.
    /// Asynchronous image pulls share one observable in-memory job registry.
    pub image_jobs: Arc<ImageJobRegistry>,
    /// Raw envd access tokens are kept only in memory and are never persisted with sandbox rows.
    pub e2b_tokens: Arc<tokio::sync::Mutex<HashMap<String, (String, String)>>>,
    pub processes: Arc<ProcessRegistry>,
    /// 预热快照池（模板 → 快照 + 固定子网），create 快路径优先使用。
    pub snapshots: Arc<tokio::sync::Mutex<Vec<WarmSnapshot>>>,
    /// 快照预热子网分配器（顺序递增，跨沙盒身份复用）。
    #[cfg(target_os = "linux")]
    pub subnet_alloc: clouisle_net::netns::SubnetAllocator,
    /// IDs currently being provisioned or recovered; prevents duplicate jobs.
    pub provisioning: Arc<tokio::sync::Mutex<HashSet<String>>>,
    /// Set before graceful shutdown so readiness probes drain the instance.
    pub draining: Arc<AtomicBool>,
    #[cfg(target_os = "linux")]
    pub manage_network: bool,
    /// 服务版本
    pub version: &'static str,
}

/// 预热的 FC 快照：与固定子网一对一绑定，可被一个 sandbox 占用。
#[derive(Debug, Clone)]
pub struct WarmSnapshot {
    pub pool_key: String,
    pub paths: clouisle_vmm::SnapshotPaths,
    pub subnet: (u16, u16),
    pub owner: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ProcessEventRecord {
    pub payload: bytes::Bytes,
    pub terminal: bool,
}

#[derive(Debug)]
pub struct ProcessSession {
    pub sandbox_id: String,
    pub pid: u32,
    pub config: serde_json::Value,
    pub tag: Option<String>,
    /// Guest 侧帧 id，用于 stdin/signal/resize 控制寻址。
    pub guest_id: String,
    /// 是否 PTY 模式（输出事件编码为 `pty` 字段）。
    pub pty: bool,
    events: tokio::sync::Mutex<Vec<ProcessEventRecord>>,
    updates: tokio::sync::broadcast::Sender<ProcessEventRecord>,
}

impl ProcessSession {
    pub async fn snapshot_and_subscribe(
        &self,
    ) -> (
        Vec<ProcessEventRecord>,
        tokio::sync::broadcast::Receiver<ProcessEventRecord>,
    ) {
        let events = self.events.lock().await.clone();
        (events, self.updates.subscribe())
    }

    pub async fn publish(&self, event: ProcessEventRecord) {
        self.events.lock().await.push(event.clone());
        let _ = self.updates.send(event);
    }
}

#[derive(Clone, Default)]
pub struct ProcessRegistry {
    sessions: Arc<tokio::sync::RwLock<HashMap<String, Arc<ProcessSession>>>>,
}

impl ProcessRegistry {
    pub async fn create(
        &self,
        sandbox_id: &str,
        pid: u32,
        config: serde_json::Value,
        tag: Option<String>,
        guest_id: String,
        pty: bool,
    ) -> Arc<ProcessSession> {
        let (updates, _) = tokio::sync::broadcast::channel(64);
        let session = Arc::new(ProcessSession {
            sandbox_id: sandbox_id.to_string(),
            pid,
            config,
            tag: tag.clone(),
            guest_id,
            pty,
            events: tokio::sync::Mutex::new(Vec::new()),
            updates,
        });
        self.sessions
            .write()
            .await
            .insert(format!("{sandbox_id}:{pid}"), session.clone());
        if let Some(tag) = tag {
            self.sessions
                .write()
                .await
                .insert(format!("{sandbox_id}:tag:{tag}"), session.clone());
        }
        session
    }

    pub async fn get(&self, sandbox_id: &str, pid: u32) -> Option<Arc<ProcessSession>> {
        self.sessions
            .read()
            .await
            .get(&format!("{sandbox_id}:{pid}"))
            .cloned()
    }

    pub async fn get_by_tag(&self, sandbox_id: &str, tag: &str) -> Option<Arc<ProcessSession>> {
        self.sessions
            .read()
            .await
            .get(&format!("{sandbox_id}:tag:{tag}"))
            .cloned()
    }

    pub async fn list(&self, sandbox_id: &str) -> Vec<Arc<ProcessSession>> {
        self.sessions
            .read()
            .await
            .values()
            .filter(|session| session.sandbox_id == sandbox_id)
            .cloned()
            .collect()
    }

    pub async fn remove_sandbox(&self, sandbox_id: &str) {
        self.sessions
            .write()
            .await
            .retain(|_, session| session.sandbox_id != sandbox_id);
    }
}

impl AppState {
    /// 读取宿主机资源上限（macOS 上读取本机近似值；Linux 上从 /proc 读）。
    pub fn host_capacity() -> Resources {
        #[cfg(target_os = "linux")]
        {
            let vcpu = std::fs::read_to_string("/proc/cpuinfo")
                .map(|s| s.lines().filter(|l| l.starts_with("processor")).count() as u16)
                .unwrap_or(4)
                .max(1);
            let mem_kb = std::fs::read_to_string("/proc/meminfo")
                .ok()
                .and_then(|s| {
                    s.lines().find(|l| l.starts_with("MemTotal")).and_then(|l| {
                        l.split_whitespace()
                            .nth(1)
                            .and_then(|v| v.parse::<u64>().ok())
                    })
                })
                .unwrap_or(8 * 1024 * 1024);
            Resources {
                vcpu,
                memory_mb: (mem_kb / 1024) as u32,
                disk_mb: 100 * 1024,
                ..Resources::default()
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            // macOS 开发环境：模拟 8 vCPU / 16 GiB / 100 GiB
            Resources {
                vcpu: 8,
                memory_mb: 16 * 1024,
                disk_mb: 100 * 1024,
                ..Resources::default()
            }
        }
    }

    pub async fn ensure_e2b_access_token(
        &self,
        sandbox_id: &str,
        tenant_id: &str,
    ) -> clouisle_core::Result<String> {
        let mut tokens = self.e2b_tokens.lock().await;
        if let Some((_, token)) = tokens.get(sandbox_id) {
            return Ok(token.clone());
        }
        let record = self
            .e2b
            .create_access_token(tenant_id, &format!("sandbox-{sandbox_id}"))
            .await
            .map_err(|error| clouisle_core::ClouisleError::internal(error.to_string()))?;
        let token_id = record
            .get("id")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| clouisle_core::ClouisleError::internal("access token id missing"))?;
        let token = record
            .get("token")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| clouisle_core::ClouisleError::internal("access token value missing"))?;
        tokens.insert(
            sandbox_id.to_string(),
            (token_id.to_string(), token.to_string()),
        );
        Ok(token.to_string())
    }

    pub async fn remove_e2b_access_token(&self, sandbox_id: &str, tenant_id: &str) {
        if let Some((token_id, _)) = self.e2b_tokens.lock().await.remove(sandbox_id)
            && let Err(error) = self.e2b.delete_access_token(tenant_id, &token_id).await
        {
            tracing::debug!(sandbox_id, %error, "E2B access token cleanup skipped");
        }
    }

    /// 预热一个模板快照（create 快路径用）。已存在空闲快照则跳过。
    pub async fn warm_snapshot(
        &self,
        spec: &clouisle_core::SandboxSpec,
    ) -> clouisle_core::Result<()> {
        use clouisle_vmm::{SnapshotKind, SnapshotPaths, StopMode};
        let key = spec.pool_key();
        {
            let pool = self.snapshots.lock().await;
            if pool
                .iter()
                .any(|snap| snap.pool_key == key && snap.owner.is_none())
            {
                return Ok(());
            }
        }
        let subnet = self.subnet_alloc.allocate();
        let temp_id = uuid::Uuid::now_v7().to_string();
        #[cfg(target_os = "linux")]
        if self.manage_network {
            self.firewall
                .create_network_in_subnet(&temp_id, Some(subnet))
                .await?;
        }
        let handle = self.vmm.create_in_subnet(&temp_id, spec, subnet).await?;
        self.vmm.start(&handle).await?;
        #[cfg(target_os = "linux")]
        if self.manage_network {
            // FC 启动后在 netns 内创建 tap0；此刻再桥接并拉起。
            #[cfg(target_os = "linux")]
            clouisle_net::netns::attach_tap(&temp_id)?;
        }
        let hello = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            self.agent.connect_and_hello(&handle, &temp_id),
        )
        .await
        .map_err(|_| clouisle_core::ClouisleError::timeout("snapshot warm hello timed out"))??;
        hello.ping().await?;
        drop(hello);
        // 最小复现证明：guest 完全稳定后再快照可稳定恢复（无网络静默时
        // 早期快照会恢复出崩溃的内核栈）。agent 刚就绪时 guest 内核仍在
        // 收敛（定时器/工作队列），再等待数秒让系统进入安静期。
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        self.vmm.pause(&handle).await?;
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let snap_dir = std::path::PathBuf::from("/data/e2b/snapshots").join(&temp_id);
        tokio::fs::create_dir_all(&snap_dir)
            .await
            .map_err(|e| clouisle_core::ClouisleError::io(format!("create snapshot dir: {e}")))?;
        let paths = SnapshotPaths {
            state_path: snap_dir.join("state.bin").to_string_lossy().into_owned(),
            mem_path: snap_dir.join("mem.bin").to_string_lossy().into_owned(),
        };
        let snap_result = self.vmm.snapshot(&handle, SnapshotKind::Full, &paths).await;
        let _ = self.vmm.stop(&handle, StopMode::Force).await;
        #[cfg(target_os = "linux")]
        if self.manage_network {
            let _ = self
                .firewall
                .teardown_sandbox_network(&temp_id, handle.subnet)
                .await;
        }
        snap_result?;
        self.snapshots.lock().await.push(WarmSnapshot {
            pool_key: key.clone(),
            paths,
            subnet,
            owner: None,
        });
        tracing::info!(pool_key = %key, subnet = ?subnet, "snapshot warmed");
        Ok(())
    }

    /// 认领一个空闲快照（create 快路径）；返回快照路径与继承子网。
    pub async fn claim_snapshot(
        &self,
        pool_key: &str,
        sandbox_id: &str,
    ) -> Option<(clouisle_vmm::SnapshotPaths, (u16, u16))> {
        let mut pool = self.snapshots.lock().await;
        let pos = pool
            .iter()
            .position(|snap| snap.pool_key == pool_key && snap.owner.is_none());
        tracing::debug!(
            pool_key,
            sandbox_id,
            pool_len = pool.len(),
            found = pos.is_some(),
            "snapshot claim"
        );
        let pos = pos?;
        let snap = &mut pool[pos];
        snap.owner = Some(sandbox_id.to_string());
        Some((snap.paths.clone(), snap.subnet))
    }

    /// 释放 sandbox 占用的快照（回池）。
    pub async fn release_snapshot(&self, sandbox_id: &str) {
        let mut pool = self.snapshots.lock().await;
        for snap in pool.iter_mut() {
            if snap.owner.as_deref() == Some(sandbox_id) {
                snap.owner = None;
            }
        }
    }
}

/// Reconcile persisted sandbox records with their local runtime probes.
/// Starting records without a runtime are resumed asynchronously; failed
/// records honor their bounded restart policy.
pub async fn reconcile_sandboxes(state: &AppState) {
    let sandboxes = match state.store.list_sandboxes(None).await {
        Ok(sandboxes) => sandboxes,
        Err(error) => {
            tracing::error!(%error, "sandbox reconciliation scan failed");
            return;
        }
    };
    let ready_node_ids = match state
        .store
        .list_ready_nodes(chrono::Utc::now().timestamp_millis() - 15_000)
        .await
    {
        Ok(nodes) => Some(
            nodes
                .into_iter()
                .map(|node| node.info.node_id)
                .collect::<HashSet<_>>(),
        ),
        Err(error) => {
            tracing::warn!(%error, "node lease scan failed during sandbox reconciliation");
            None
        }
    };

    for sandbox in sandboxes {
        if state.manage_resources
            && sandbox.node_id.is_none()
            && sandbox.status.is_active()
            && !state.reservations.lock().await.contains_key(&sandbox.id)
        {
            match state.pool.admit(&sandbox.spec).await {
                Ok(reservation) => {
                    state
                        .reservations
                        .lock()
                        .await
                        .insert(sandbox.id.clone(), reservation);
                }
                Err(error) => {
                    let _ = state
                        .store
                        .update_sandbox_status_message(
                            &sandbox.id,
                            &SandboxStatus::Error,
                            Some(&format!("resource reservation restore failed: {error}")),
                        )
                        .await;
                    tracing::warn!(sandbox_id = %sandbox.id, %error, "persisted sandbox reservation could not be restored");
                    continue;
                }
            }
        }
        if sandbox.status.is_active()
            && sandbox.node_id.as_deref().is_some_and(|node_id| {
                ready_node_ids
                    .as_ref()
                    .is_some_and(|ready| !ready.contains(node_id))
            })
        {
            let _ = state
                .store
                .update_sandbox_status_message(
                    &sandbox.id,
                    &SandboxStatus::Error,
                    Some("node heartbeat lease expired"),
                )
                .await;
            state.reservations.lock().await.remove(&sandbox.id);
            continue;
        }
        if sandbox.status == SandboxStatus::Starting && sandbox.vmm_meta.vmm_id.is_none() {
            schedule_provision(state, sandbox).await;
            continue;
        }
        if sandbox.status.is_active() && sandbox.vmm_meta.vmm_id.is_some() {
            let subnet = sandbox.vmm_meta.inherited_subnet();
            let handle = clouisle_vmm::VmHandle {
                id: sandbox
                    .vmm_meta
                    .vmm_id
                    .clone()
                    .unwrap_or_else(|| sandbox.id.clone()),
                backend: sandbox.vmm_meta.backend.clone(),
                owner_id: sandbox.vmm_meta.owner_id.clone(),
                pid: sandbox.vmm_meta.pid,
                api_socket: sandbox.vmm_meta.api_socket.clone(),
                vsock_socket: sandbox.vmm_meta.vsock_socket.clone(),
                vsock_cid: sandbox.vmm_meta.vsock_cid,
                subnet,
            };
            if !state.vmm.probe(&handle).await.unwrap_or(false) {
                let message = "persisted sandbox runtime is not reachable";
                let _ = state
                    .store
                    .update_sandbox_status_message(
                        &sandbox.id,
                        &SandboxStatus::Error,
                        Some(message),
                    )
                    .await;
                state.reservations.lock().await.remove(&sandbox.id);
                tracing::warn!(sandbox_id = %sandbox.id, "sandbox marked unhealthy during reconciliation");
            } else if matches!(
                sandbox.status,
                SandboxStatus::Starting | SandboxStatus::Running
            ) {
                let guest_probe = tokio::time::timeout(std::time::Duration::from_secs(5), async {
                    let connection = state.agent.connect_and_hello(&handle, &sandbox.id).await?;
                    connection.ping().await
                })
                .await;
                match guest_probe {
                    Ok(Ok(())) => {
                        if sandbox.status == SandboxStatus::Starting {
                            let _ = state
                                .store
                                .update_sandbox_status_message(
                                    &sandbox.id,
                                    &SandboxStatus::Running,
                                    None,
                                )
                                .await;
                        }
                    }
                    Ok(Err(error)) => {
                        if sandbox.status == SandboxStatus::Running {
                            let message =
                                format!("guest probe failed during reconciliation: {error}");
                            let _ = state
                                .store
                                .update_sandbox_status_message(
                                    &sandbox.id,
                                    &SandboxStatus::Error,
                                    Some(&message),
                                )
                                .await;
                            state.reservations.lock().await.remove(&sandbox.id);
                        }
                        // Starting：provision 仍在进行（agent 可能尚未就绪），
                        // 让 provision 自身的超时逻辑处理，避免抢跑误杀。
                    }
                    Err(_) => {
                        if sandbox.status == SandboxStatus::Running {
                            let message = "guest probe timed out during reconciliation";
                            let _ = state
                                .store
                                .update_sandbox_status_message(
                                    &sandbox.id,
                                    &SandboxStatus::Error,
                                    Some(message),
                                )
                                .await;
                            state.reservations.lock().await.remove(&sandbox.id);
                        }
                    }
                }
            }
            continue;
        }
        if sandbox.status == SandboxStatus::Error
            && !matches!(sandbox.spec.restart_policy, RestartPolicy::Never)
        {
            let attempts = sandbox
                .vmm_meta
                .extra
                .get("recovery_attempts")
                .and_then(|value| value.parse::<u8>().ok())
                .unwrap_or(0);
            if attempts < 3 {
                let mut meta = sandbox.vmm_meta.clone();
                meta.extra
                    .insert("recovery_attempts".into(), (attempts + 1).to_string());
                if state
                    .store
                    .update_sandbox_vmm_meta(&sandbox.id, &meta)
                    .await
                    .is_ok()
                    && state
                        .store
                        .update_sandbox_status_message(&sandbox.id, &SandboxStatus::Starting, None)
                        .await
                        .is_ok()
                {
                    let recovered = state
                        .store
                        .get_sandbox(&sandbox.id)
                        .await
                        .unwrap_or(sandbox);
                    schedule_provision(state, recovered).await;
                }
            }
        }
    }
}

async fn schedule_provision(state: &AppState, sandbox: Sandbox) {
    if state.provisioning.lock().await.contains(&sandbox.id) {
        return;
    }
    let reservation = if state.manage_resources
        && !state.reservations.lock().await.contains_key(&sandbox.id)
    {
        match state.pool.admit(&sandbox.spec).await {
            Ok(reservation) => Some(reservation),
            Err(error) => {
                tracing::warn!(sandbox_id = %sandbox.id, %error, "cannot reserve resources for reconciliation");
                return;
            }
        }
    } else {
        None
    };
    let job_id = sandbox.id.clone();
    let task_state = state.clone();
    tokio::spawn(async move {
        if let Err(error) =
            crate::handlers::sandbox::run_provision(task_state, sandbox, reservation).await
        {
            tracing::error!(sandbox_id = %job_id, %error, "reconciliation provisioning failed");
        }
    });
}
