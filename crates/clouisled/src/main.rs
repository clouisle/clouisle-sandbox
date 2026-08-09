//! clouisled node daemon.

#[cfg(target_os = "linux")]
mod linux {
    use std::collections::HashMap;
    use std::sync::Arc;

    use clap::Parser;
    use clouisle_store::SqliteStore;
    use clouisle_vmm::{FirecrackerConfig, FirecrackerVmm};
    use clouisled::server::NodeServiceImpl;
    use clouisled::{NodeAgent, NodeAgentConfig};

    #[derive(Debug, Parser)]
    #[command(name = "clouisled", about = "Clouisle Firecracker node daemon")]
    struct Cli {
        /// gRPC listen address.
        #[arg(long, default_value = "0.0.0.0:9090")]
        addr: String,
        /// Local metadata database path.
        #[arg(long, default_value = "/data/clouisled.db")]
        db: String,
        /// Stable node identifier.
        #[arg(long, default_value = "")]
        node_id: String,
        /// Hostname reported to the control plane.
        #[arg(long, default_value = "")]
        hostname: String,
        /// Guest kernel image.
        #[arg(long, default_value = "/opt/clouisle/vmlinux")]
        kernel: String,
        /// Rootfs cache directory.
        #[arg(long, default_value = "/opt/clouisle/images")]
        images_dir: String,
        /// Firecracker API socket directory.
        #[arg(long, default_value = "/run/clouisle/firecracker")]
        api_socket_dir: String,
        /// Control-plane HTTP base URL for durable node registration and heartbeats.
        #[arg(long)]
        control_plane: Option<String>,
        /// Full-scope control-plane API key used for node registration.
        #[arg(long, requires = "control_plane")]
        control_plane_api_key: Option<String>,
        /// Public gRPC endpoint advertised to the control plane.
        #[arg(long)]
        advertised_endpoint: Option<String>,
    }

    #[tokio::main]
    pub async fn run() -> Result<(), Box<dyn std::error::Error>> {
        let cli = Cli::parse();
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| "info".into()),
            )
            .init();

        let node_id = if cli.node_id.is_empty() {
            std::env::var("NODE_NAME")
                .ok()
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| hostname().unwrap_or_else(|| "clouisled".to_string()))
        } else {
            cli.node_id
        };
        let hostname = if cli.hostname.is_empty() {
            hostname().unwrap_or_else(|| node_id.clone())
        } else {
            cli.hostname
        };

        let config = NodeAgentConfig {
            node_id,
            hostname,
            total_vcpu: std::thread::available_parallelism()
                .map(|count| count.get() as u16)
                .unwrap_or(1),
            total_memory_mb: host_memory_mb(),
            total_disk_mb: 100 * 1024,
            kvm_available: std::path::Path::new("/dev/kvm").exists(),
            kernel_version: kernel_version(),
            firecracker_version: firecracker_version(),
            manage_network: true,
            labels: HashMap::new(),
            heartbeat_secs: 3,
        };

        let store = Arc::new(SqliteStore::open(&cli.db)?);
        let firecracker = FirecrackerConfig {
            kernel_path: cli.kernel.into(),
            images_dir: cli.images_dir.into(),
            api_sock_dir: cli.api_socket_dir.into(),
            ..FirecrackerConfig::default()
        };
        let vmm = Arc::new(FirecrackerVmm::new(firecracker));
        vmm.check_environment()?;
        let agent = NodeAgent::new(config, vmm);
        if let Some(control_plane) = cli.control_plane {
            let api_key = cli
                .control_plane_api_key
                .ok_or("--control-plane-api-key is required")?;
            let endpoint = cli
                .advertised_endpoint
                .ok_or("--advertised-endpoint is required")?;
            tokio::spawn(heartbeat_loop(
                control_plane,
                api_key,
                endpoint,
                agent.clone(),
            ));
        }
        tracing::info!(node_id = %agent.config.node_id, addr = %cli.addr, "clouisled starting");
        NodeServiceImpl::new(agent, store).serve(&cli.addr).await
    }

    fn hostname() -> Option<String> {
        std::fs::read_to_string("/etc/hostname")
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    }

    fn host_memory_mb() -> u64 {
        std::fs::read_to_string("/proc/meminfo")
            .ok()
            .and_then(|content| {
                content.lines().find_map(|line| {
                    line.strip_prefix("MemTotal:")
                        .and_then(|value| value.split_whitespace().next())
                        .and_then(|value| value.parse::<u64>().ok())
                })
            })
            .map(|kilobytes| kilobytes / 1024)
            .unwrap_or(1024)
    }

    fn kernel_version() -> String {
        std::process::Command::new("uname")
            .arg("-r")
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "unknown".to_string())
    }

    fn firecracker_version() -> String {
        std::process::Command::new("firecracker")
            .arg("--version")
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "unknown".to_string())
    }

    async fn heartbeat_loop(
        control_plane: String,
        api_key: String,
        endpoint: String,
        agent: NodeAgent,
    ) {
        let client = reqwest::Client::new();
        let url = format!("{}/api/v1/nodes", control_plane.trim_end_matches('/'));
        loop {
            let heartbeat = agent.heartbeat().await;
            let node = clouisle_core::RegisteredNode {
                info: agent.config.node_info(),
                endpoint: endpoint.clone(),
                status: clouisle_core::NodeStatus::Ready,
                last_heartbeat_ms: chrono::Utc::now().timestamp_millis(),
                allocated_vcpu: heartbeat.allocated_vcpu,
                allocated_memory_mb: heartbeat.allocated_memory_mb,
                running_sandboxes: heartbeat.running_sandboxes.len(),
            };
            if let Err(error) = client
                .post(&url)
                .bearer_auth(&api_key)
                .json(&node)
                .send()
                .await
                .and_then(reqwest::Response::error_for_status)
            {
                tracing::warn!(%error, "control-plane node heartbeat failed");
            }
            tokio::time::sleep(std::time::Duration::from_secs(agent.config.heartbeat_secs)).await;
        }
    }
}

#[cfg(target_os = "linux")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    linux::run()
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("clouisled requires Linux + KVM");
    std::process::exit(1);
}
