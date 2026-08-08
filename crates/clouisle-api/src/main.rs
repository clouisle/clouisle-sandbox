//! clouisle-apiserver 可执行程序。

use std::sync::Arc;

use clap::Parser;

use clouisle_api::{AppState, agent, build_router};
use clouisle_core::SandboxSpec;
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
    // 重启恢复：读取 store 中所有 active 沙盒资源
    let active = store.list_sandboxes(None).await?;
    let active_specs: Vec<SandboxSpec> = active
        .iter()
        .filter(|sb| sb.status.is_active())
        .map(|sb| sb.spec.clone())
        .collect();
    pool.restore(&active_specs).await;
    tracing::info!(
        restored = active_specs.len(),
        "reconciled resource pool from store"
    );

    // VMM 后端（Linux + KVM 为唯一后端）
    #[cfg_attr(
        not(target_os = "linux"),
        allow(unreachable_code, unused_variables, clippy::diverging_sub_expression)
    )]
    {
        #[cfg(target_os = "linux")]
        let vmm: Arc<dyn clouisle_vmm::Vmm> = {
            Arc::new(clouisle_vmm::FirecrackerVmm::new(
                clouisle_vmm::FirecrackerConfig::default(),
            ))
        };
        #[cfg(not(target_os = "linux"))]
        let vmm: Arc<dyn clouisle_vmm::Vmm> = {
            panic!("clouisle-apiserver requires Linux + KVM");
        };

        #[cfg(target_os = "linux")]
        let agent_conn: Arc<dyn agent::AgentConnector> = Arc::new(agent::VsockAgentConnector::default());
        #[cfg(not(target_os = "linux"))]
        let agent_conn: Arc<dyn agent::AgentConnector> = {
            panic!("clouisle-apiserver requires Linux + KVM");
        };

        let state = AppState {
            store,
            vmm,
            pool,
            agent: agent_conn,
            auth: Arc::new(clouisle_api::auth::Authenticator::new()),
            #[cfg(target_os = "linux")]
            firewall: Arc::new(clouisle_net::FirewallManager::new()),
            version: env!("CARGO_PKG_VERSION"),
        };

        // metrics
        clouisle_api::metrics::init();

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
