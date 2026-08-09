//! clouisle-agent 二进制入口。
//!
//! Used as either guest PID 1 (no argument) or as `clouisle-agent serve`.

fn serves_guest_agent(command: Option<&str>) -> bool {
    matches!(command, None | Some("serve"))
}

#[tokio::main]
async fn main() {
    let command = std::env::args().nth(1);
    if !serves_guest_agent(command.as_deref()) {
        eprintln!("usage: clouisle-agent [serve]");
        std::process::exit(1);
    }

    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();
    if let Err(error) = clouisle_agent::serve::run_serve().await {
        tracing::error!(%error, "agent serve failed");
    }
}

#[cfg(test)]
mod tests {
    use super::serves_guest_agent;

    #[test]
    fn pid_one_defaults_to_serve_mode() {
        assert!(serves_guest_agent(None));
        assert!(serves_guest_agent(Some("serve")));
        assert!(!serves_guest_agent(Some("unexpected")));
    }
}
