//! clouislectl CLI 定义。

use clap::Parser;

#[derive(Parser, Debug)]
#[command(name = "clouislectl", about = "Clouisle Sandbox CLI")]
pub enum Cli {
    /// 创建沙盒
    Create {
        #[arg(long)]
        image: String,
        #[arg(long, default_value = "1")]
        vcpu: u16,
        #[arg(long, default_value = "256")]
        memory_mb: u32,
        #[arg(long)]
        api: Option<String>,
    },
    /// 列出沙盒
    List {
        #[arg(long)]
        status: Option<String>,
        #[arg(long)]
        api: Option<String>,
    },
    /// 删除沙盒
    Delete {
        id: String,
        #[arg(long)]
        api: Option<String>,
    },
    /// 执行命令
    Exec {
        id: String,
        command: Vec<String>,
        #[arg(long)]
        api: Option<String>,
    },
    /// 健康检查
    Health {
        #[arg(long)]
        api: Option<String>,
    },
}

impl Cli {
    pub fn api_url(&self) -> String {
        let default = "http://127.0.0.1:8080".to_string();
        match self {
            Cli::Create { api, .. } => api.clone().unwrap_or_else(|| default.clone()),
            Cli::List { api, .. } => api.clone().unwrap_or_else(|| default.clone()),
            Cli::Delete { api, .. } => api.clone().unwrap_or_else(|| default.clone()),
            Cli::Exec { api, .. } => api.clone().unwrap_or_else(|| default.clone()),
            Cli::Health { api, .. } => api.clone().unwrap_or_else(|| default.clone()),
        }
    }
}