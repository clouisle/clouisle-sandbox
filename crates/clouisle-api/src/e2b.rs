use std::collections::HashMap;

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use clouisle_core::{NetworkConfig, Resources, Sandbox, SandboxStatus, SandboxSpec};

#[derive(Debug, Clone, Deserialize)]
pub struct E2bCreateRequest {
    #[serde(rename = "templateID")]
    pub template_id: String,
    pub timeout: Option<u64>,
    #[serde(rename = "autoPause", default)]
    pub auto_pause: bool,
    #[serde(rename = "autoPauseMemory", default = "default_true")]
    pub auto_pause_memory: bool,
    #[serde(rename = "autoResume")]
    pub auto_resume: Option<E2bAutoResume>,
    #[serde(rename = "allow_internet_access")]
    pub allow_internet_access: Option<bool>,
    pub network: Option<E2bNetwork>,
    pub metadata: Option<HashMap<String, String>>,
    #[serde(rename = "envVars")]
    pub env_vars: Option<HashMap<String, String>>,
    #[serde(default)]
    pub secure: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum E2bAutoResume {
    Bool(bool),
    Config { enabled: bool },
}

impl E2bAutoResume {
    pub fn enabled(&self) -> bool {
        match self {
            Self::Bool(enabled) => *enabled,
            Self::Config { enabled } => *enabled,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct E2bNetwork {
    #[serde(rename = "allowOut", default)]
    pub allow_out: Vec<String>,
    #[serde(rename = "allowPublicTraffic", default)]
    pub allow_public_traffic: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct E2bConnectRequest {
    pub timeout: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct E2bResumeRequest {
    pub timeout: Option<u64>,
    #[serde(rename = "autoPause")]
    pub auto_pause: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct E2bPauseRequest {
    #[serde(default = "default_true")]
    pub memory: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct E2bTimeoutRequest {
    pub timeout: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct E2bSandbox {
    #[serde(rename = "templateID")]
    pub template_id: String,
    #[serde(rename = "sandboxID")]
    pub sandbox_id: String,
    #[serde(rename = "clientID")]
    pub client_id: String,
    #[serde(rename = "envdVersion")]
    pub envd_version: String,
    #[serde(rename = "envdAccessToken")]
    pub envd_access_token: Option<String>,
    #[serde(rename = "trafficAccessToken")]
    pub traffic_access_token: Option<String>,
    pub domain: Option<String>,
    #[serde(rename = "startedAt", skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(rename = "endAt", skip_serializing_if = "Option::is_none")]
    pub end_at: Option<DateTime<Utc>>,
    pub metadata: HashMap<String, String>,
    pub state: String,
    #[serde(rename = "cpuCount")]
    pub cpu_count: u16,
    #[serde(rename = "memoryMB")]
    pub memory_mb: u32,
    #[serde(rename = "diskSizeMB")]
    pub disk_size_mb: u32,
}

pub fn to_spec(request: E2bCreateRequest, tenant_id: String) -> Result<SandboxSpec, String> {
    if request.template_id.trim().is_empty() {
        return Err("templateID is required".into());
    }
    let auto_resume = request
        .auto_resume
        .as_ref()
        .is_some_and(E2bAutoResume::enabled);
    let auto_pause = request.auto_pause || auto_resume;
    if auto_resume && !auto_pause {
        return Err("autoResume requires autoPause".into());
    }
    let network = request.network.unwrap_or_default();
    let enabled = request.allow_internet_access.unwrap_or(true);
    Ok(SandboxSpec {
        image: clouisle_core::ImageRef::new(request.template_id),
        resources: Resources::default(),
        network: NetworkConfig {
            enabled,
            allow_egress: network.allow_out,
        },
        ttl_secs: Some(request.timeout.unwrap_or(15)),
        env: request.env_vars.unwrap_or_default(),
        metadata: request.metadata.unwrap_or_default(),
        auto_pause,
        auto_resume,
        tenant_id: Some(tenant_id),
        ..SandboxSpec::default()
    })
}

pub fn from_sandbox(sandbox: &Sandbox) -> E2bSandbox {
    let state = match sandbox.status {
        SandboxStatus::Paused => "paused",
        _ => "running",
    };
    E2bSandbox {
        template_id: sandbox.spec.image.reference.clone(),
        sandbox_id: sandbox.id.clone(),
        client_id: sandbox
            .spec
            .tenant_id
            .clone()
            .unwrap_or_else(|| "clouisle".into()),
        envd_version: env!("CARGO_PKG_VERSION").into(),
        envd_access_token: None,
        traffic_access_token: None,
        domain: None,
        started_at: sandbox.ready_at,
        end_at: sandbox.expires_at,
        metadata: sandbox.spec.metadata.clone(),
        state: state.into(),
        cpu_count: sandbox.spec.resources.vcpu,
        memory_mb: sandbox.spec.resources.memory_mb,
        disk_size_mb: sandbox.spec.resources.disk_mb,
    }
}

pub fn expiry_from_now(timeout: u64) -> DateTime<Utc> {
    Utc::now() + Duration::seconds(timeout as i64)
}

fn default_true() -> bool {
    true
}

