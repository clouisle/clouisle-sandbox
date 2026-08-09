//! clouisle-apiserver 可执行程序。

use std::sync::Arc;

use clap::Parser;

use clouisle_api::{AppState, agent, build_router};
use clouisle_scheduler::ResourcePool;
use clouisle_store::{PostgresStore, SqliteStore, Store};

#[derive(Parser, Debug)]
#[command(name = "clouisle-apiserver", about = "Clouisle Sandbox control plane")]
struct Cli {
    /// 监听地址
    #[arg(long, default_value = "0.0.0.0:8080")]
    addr: String,
    /// 存储连接（SQLite 路径 或 postgres:// 连接串）
    #[arg(long, default_value = "clouisle.db")]
    db: String,
    /// Optional clouisled endpoint. When set, VMM and guest execution are remote.
    #[arg(long)]
    node_endpoint: Option<String>,
    /// Select remote nodes from the durable heartbeat registry.
    #[arg(long, conflicts_with = "node_endpoint")]
    cluster_scheduling: bool,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    init_logging();

    // 自动选择 Store：postgres:// → PostgresStore，其余 → SQLite
    let store: Arc<dyn Store> =
        if cli.db.starts_with("postgres://") || cli.db.starts_with("postgresql://") {
            tracing::info!(db = %cli.db, "connecting to PostgreSQL (HA mode)");
            let pg = PostgresStore::connect(&cli.db).await?;
            Arc::new(pg)
        } else {
            tracing::info!(db = %cli.db, "opening SQLite (single-node mode)");
            let sq = SqliteStore::open(&cli.db)?;
            Arc::new(sq)
        };

    // 资源池
    let capacity = AppState::host_capacity();
    let pool = Arc::new(ResourcePool::new(capacity, 200));
    // Only a local Firecracker control plane owns permits. Remote nodes own
    // their NodeAgent pools and must not be admitted against API host limits.
    let reservations = Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));
    let manage_resources = cli.node_endpoint.is_none() && !cli.cluster_scheduling;
    let mut restored = 0usize;
    if manage_resources {
        for sandbox in store
            .list_sandboxes(None)
            .await?
            .into_iter()
            .filter(|sandbox| sandbox.status.is_active())
        {
            match pool.admit(&sandbox.spec).await {
                Ok(reservation) => {
                    reservations.lock().await.insert(sandbox.id, reservation);
                    restored += 1;
                }
                Err(error) => {
                    tracing::error!(sandbox_id = %sandbox.id, %error, "active sandbox exceeds local pool capacity")
                }
            }
        }
    }
    tracing::info!(restored, "reconciled resource pool from store");

    // VMM 后端（Linux + KVM 为唯一后端）
    #[cfg_attr(
        not(target_os = "linux"),
        allow(unreachable_code, unused_variables, clippy::diverging_sub_expression)
    )]
    {
        #[cfg(target_os = "linux")]
        let vmm: Arc<dyn clouisle_vmm::Vmm> = if cli.cluster_scheduling {
            Arc::new(clouisle_api::node_client::ScheduledNodeVmm::new(
                store.clone(),
            ))
        } else {
            match cli.node_endpoint.as_deref() {
                Some(endpoint) => Arc::new(clouisle_api::node_client::GrpcNodeVmm::new(endpoint)),
                None => Arc::new(clouisle_vmm::FirecrackerVmm::new(
                    clouisle_vmm::FirecrackerConfig::default(),
                )),
            }
        };
        #[cfg(not(target_os = "linux"))]
        let vmm: Arc<dyn clouisle_vmm::Vmm> = if cli.cluster_scheduling {
            Arc::new(clouisle_api::node_client::ScheduledNodeVmm::new(
                store.clone(),
            ))
        } else {
            match cli.node_endpoint.as_deref() {
                Some(endpoint) => Arc::new(clouisle_api::node_client::GrpcNodeVmm::new(endpoint)),
                None => return Err("clouisle-apiserver requires Linux + KVM, --node-endpoint, or --cluster-scheduling".into()),
            }
        };

        #[cfg(target_os = "linux")]
        let agent_conn: Arc<dyn agent::AgentConnector> = if cli.cluster_scheduling {
            Arc::new(clouisle_api::node_client::GrpcAgentConnector::new(""))
        } else {
            match cli.node_endpoint.as_deref() {
                Some(endpoint) => {
                    Arc::new(clouisle_api::node_client::GrpcAgentConnector::new(endpoint))
                }
                None => Arc::new(agent::VsockAgentConnector::default()),
            }
        };
        #[cfg(not(target_os = "linux"))]
        let agent_conn: Arc<dyn agent::AgentConnector> = if cli.cluster_scheduling {
            Arc::new(clouisle_api::node_client::GrpcAgentConnector::new(""))
        } else {
            match cli.node_endpoint.as_deref() {
                Some(endpoint) => Arc::new(clouisle_api::node_client::GrpcAgentConnector::new(endpoint)),
                None => return Err("clouisle-apiserver requires Linux + KVM, --node-endpoint, or --cluster-scheduling".into()),
            }
        };

        let auth = load_authenticator().await?;
        let state = AppState {
            store,
            vmm,
            pool,
            image_jobs: Arc::new(clouisle_api::ImageJobRegistry::new()),
            reservations,
            manage_resources,
            agent: agent_conn,
            auth: Arc::new(auth),
            #[cfg(target_os = "linux")]
            firewall: Arc::new(clouisle_net::FirewallManager::new()),
            #[cfg(target_os = "linux")]
            manage_network: cli.node_endpoint.is_none() && !cli.cluster_scheduling,
            version: env!("CARGO_PKG_VERSION"),
        };

        clouisle_api::metrics::init();
        tokio::spawn(expiry_reaper(state.clone()));

        let router = build_router(state);
        let listener = tokio::net::TcpListener::bind(&cli.addr).await?;
        tracing::info!(addr = %cli.addr, "apiserver listening");

        // 优雅关闭（AR-04）：SIGTERM/SIGINT → 等 30s 内进行中请求完成
        axum::serve(listener, router)
            .with_graceful_shutdown(shutdown_signal())
            .await?;
        tracing::info!("apiserver shut down gracefully");
    }

    Ok(())
}

async fn expiry_reaper(state: AppState) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
    loop {
        interval.tick().await;
        let now = chrono::Utc::now();
        let sandboxes = match state.store.list_sandboxes(None).await {
            Ok(sandboxes) => sandboxes,
            Err(error) => {
                tracing::error!(%error, "sandbox expiry scan failed");
                continue;
            }
        };
        for sandbox in sandboxes.into_iter().filter(|sandbox| {
            sandbox
                .expires_at
                .is_some_and(|expires_at| expires_at <= now)
        }) {
            let handle = clouisle_vmm::VmHandle {
                id: sandbox
                    .vmm_meta
                    .vmm_id
                    .clone()
                    .unwrap_or_else(|| sandbox.id.clone()),
                backend: sandbox.vmm_meta.backend.clone(),
                pid: sandbox.vmm_meta.pid,
                api_socket: sandbox.vmm_meta.api_socket.clone(),
                vsock_socket: sandbox.vmm_meta.vsock_socket.clone(),
                vsock_cid: sandbox.vmm_meta.vsock_cid,
            };
            if sandbox.vmm_meta.vmm_id.is_some()
                && let Err(error) = state.vmm.stop(&handle, clouisle_vmm::StopMode::Force).await
            {
                tracing::warn!(sandbox_id = %sandbox.id, %error, "failed to stop expired sandbox");
                continue;
            }
            if let Err(error) = state.store.delete_sandbox(&sandbox.id).await {
                tracing::warn!(sandbox_id = %sandbox.id, %error, "failed to delete expired sandbox metadata");
                continue;
            }
            state.reservations.lock().await.remove(&sandbox.id);
            #[cfg(target_os = "linux")]
            if state.manage_network
                && let Err(error) = state.firewall.teardown_sandbox_network(&sandbox.id).await
            {
                tracing::warn!(sandbox_id = %sandbox.id, %error, "failed to clean expired sandbox network");
            }
            tracing::info!(sandbox_id = %sandbox.id, "expired sandbox deleted");
        }
    }
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => tracing::info!("received SIGINT"),
        _ = terminate => tracing::info!("received SIGTERM"),
    }
}

fn init_logging() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
}

async fn load_authenticator()
-> Result<clouisle_api::auth::Authenticator, Box<dyn std::error::Error>> {
    let encoded = std::env::var("CLOUISLE_API_KEYS").map_err(
        |_| "CLOUISLE_API_KEYS is required; use the test router for anonymous development",
    )?;
    let auth = clouisle_api::auth::Authenticator::new_production();
    for entry in encoded.split(',').filter(|entry| !entry.is_empty()) {
        let mut fields = entry.splitn(3, ':');
        let key = fields.next().unwrap_or_default();
        let tenant = fields.next().unwrap_or_default();
        let scope = fields.next().unwrap_or_default();
        if key.is_empty() || tenant.is_empty() || !matches!(scope, "read" | "full" | "admin") {
            return Err("invalid CLOUISLE_API_KEYS entry; expected key:tenant:read|full".into());
        }
        auth.register(key, tenant, clouisle_api::auth::Scope::from_string(scope))
            .await;
    }
    if auth.is_empty().await {
        return Err("CLOUISLE_API_KEYS must contain at least one key".into());
    }
    Ok(auth)
}
