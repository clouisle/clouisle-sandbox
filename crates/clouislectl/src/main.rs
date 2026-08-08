//! clouislectl 可执行文件。

use clap::Parser;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = clouislectl::Cli::parse();
    let base = cli.api_url();

    match cli {
        clouislectl::Cli::Create { image, vcpu, memory_mb, .. } => {
            let body = serde_json::json!({
                "image": { "reference": image },
                "resources": { "vcpu": vcpu, "memory_mb": memory_mb, "disk_mb": 512 }
            });
            let resp = reqwest::Client::new()
                .post(format!("{base}/api/v1/sandboxes"))
                .json(&body)
                .send()
                .await?;
            let status = resp.status();
            let text = resp.text().await?;
            println!("{status}\n{text}");
        }
        clouislectl::Cli::List { status, .. } => {
            let mut url = format!("{base}/api/v1/sandboxes");
            if let Some(s) = status {
                url.push_str(&format!("?status={s}"));
            }
            let resp = reqwest::get(&url).await?;
            println!("{}", resp.text().await?);
        }
        clouislectl::Cli::Delete { id, .. } => {
            let resp = reqwest::Client::new()
                .delete(format!("{base}/api/v1/sandboxes/{id}"))
                .send()
                .await?;
            println!("{}", resp.status());
        }
        clouislectl::Cli::Exec { id, command, .. } => {
            let body = serde_json::json!({
                "argv": command,
                "timeout_ms": 30000,
                "stream": false,
            });
            let resp = reqwest::Client::new()
                .post(format!("{base}/api/v1/sandboxes/{id}/exec"))
                .json(&body)
                .send()
                .await?;
            println!("{}", resp.text().await?);
        }
        clouislectl::Cli::Health { .. } => {
            let resp = reqwest::get(format!("{base}/health")).await?;
            println!("{}", resp.text().await?);
        }
    }
    Ok(())
}