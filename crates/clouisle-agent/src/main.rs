//! clouisle-agent 二进制入口。
//!
//! 用法:
//!   clouisle-agent serve     # 启动 vsock agent，监听端口 5201

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(|s| s.as_str()) {
        Some("serve") => {
            tracing_subscriber::fmt()
                .with_max_level(tracing::Level::INFO)
                .init();
            if let Err(e) = clouisle_agent::serve::run_serve().await {
                tracing::error!(error = %e, "agent serve failed");
            }
        }
        _ => {
            eprintln!("usage: clouisle-agent serve");
            std::process::exit(1);
        }
    }
}
