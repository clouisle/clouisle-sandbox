//! clouislectl 可执行文件。

use clap::Parser;

fn auth_header(key: &Option<String>) -> Option<(&'static str, String)> {
    key.as_ref()
        .map(|k| ("Authorization", format!("Bearer {k}")))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = clouislectl::Cli::parse();
    let base = cli.api_url();
    let key = cli.api_key();

    match cli {
        clouislectl::Cli::Create {
            image,
            vcpu,
            memory_mb,
            ..
        } => {
            let body = serde_json::json!({
                "image": { "reference": image },
                "resources": { "vcpu": vcpu, "memory_mb": memory_mb, "disk_mb": 512 }
            });
            let mut req = reqwest::Client::new()
                .post(format!("{base}/api/v1/sandboxes"))
                .json(&body);
            if let Some((name, value)) = auth_header(&key) {
                req = req.header(name, value);
            }
            let resp = req.send().await?;
            let text = resp.text().await?;
            println!("{text}");
        }
        clouislectl::Cli::List { status, .. } => {
            let mut url = format!("{base}/api/v1/sandboxes");
            if let Some(s) = status {
                url.push_str(&format!("?status={s}"));
            }
            let mut req = reqwest::Client::new().get(&url);
            if let Some((name, value)) = auth_header(&key) {
                req = req.header(name, value);
            }
            let resp = req.send().await?;
            println!("{}", resp.text().await?);
        }
        clouislectl::Cli::Delete { id, .. } => {
            let mut req = reqwest::Client::new().delete(format!("{base}/api/v1/sandboxes/{id}"));
            if let Some((name, value)) = auth_header(&key) {
                req = req.header(name, value);
            }
            let resp = req.send().await?;
            println!("{}", resp.status());
        }
        clouislectl::Cli::Exec { id, command, .. } => {
            let body = serde_json::json!({
                "argv": command,
                "timeout_ms": 30000,
                "stream": false,
            });
            let mut req = reqwest::Client::new()
                .post(format!("{base}/api/v1/sandboxes/{id}/exec"))
                .json(&body);
            if let Some((name, value)) = auth_header(&key) {
                req = req.header(name, value);
            }
            let resp = req.send().await?;
            println!("{}", resp.text().await?);
        }
        clouislectl::Cli::Health { .. } => {
            let resp = reqwest::get(format!("{base}/health")).await?;
            println!("{}", resp.text().await?);
        }
    }
    Ok(())
}
