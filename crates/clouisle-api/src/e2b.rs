use std::collections::HashMap;

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use clouisle_core::{
    NetworkConfig, Resources, RestartPolicy, Sandbox, SandboxSpec, SandboxStatus, VolumeMountSpec,
};

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum E2bInitCommand {
    Argv(Vec<String>),
    Shell(String),
    Process {
        cmd: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default, alias = "env")]
        envs: HashMap<String, String>,
        cwd: Option<String>,
    },
}

impl E2bInitCommand {
    fn into_parts(self) -> (Vec<String>, HashMap<String, String>, Option<String>) {
        match self {
            Self::Argv(argv) => (argv, HashMap::new(), None),
            Self::Shell(command) => (
                vec!["/bin/sh".into(), "-lc".into(), command],
                HashMap::new(),
                None,
            ),
            Self::Process {
                cmd,
                args,
                envs,
                cwd,
            } => {
                let mut argv = Vec::with_capacity(args.len() + 1);
                argv.push(cmd);
                argv.extend(args);
                (argv, envs, cwd)
            }
        }
    }
}

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
    #[serde(rename = "allowInternetAccess", alias = "allow_internet_access")]
    pub allow_internet_access: Option<bool>,
    pub network: Option<E2bNetwork>,
    pub metadata: Option<HashMap<String, String>>,
    #[serde(rename = "envVars")]
    pub env_vars: Option<HashMap<String, String>>,
    #[serde(rename = "initCommand", alias = "command")]
    pub init_command: Option<E2bInitCommand>,
    #[serde(rename = "initTimeoutMs", default)]
    pub init_timeout_ms: Option<u64>,
    #[serde(rename = "snapshotID")]
    pub snapshot_id: Option<String>,
    #[serde(rename = "volumeMounts", default)]
    pub volume_mounts: Vec<E2bVolumeMount>,
    #[serde(default)]
    pub secure: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct E2bVolumeMount {
    pub name: String,
    pub path: String,
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

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct E2bNetwork {
    #[serde(rename = "allowOut", default)]
    pub allow_out: Vec<String>,
    #[serde(rename = "denyOut", default)]
    pub deny_out: Vec<String>,
    #[serde(rename = "allowPublicTraffic", default)]
    pub allow_public_traffic: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct E2bLifecycle {
    #[serde(rename = "autoResume")]
    pub auto_resume: bool,
    #[serde(rename = "onTimeout")]
    pub on_timeout: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct E2bConnectRequest {
    pub timeout: Option<u64>,
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
    #[serde(rename = "allowInternetAccess")]
    pub allow_internet_access: Option<bool>,
    pub network: E2bNetwork,
    pub lifecycle: E2bLifecycle,
    #[serde(rename = "volumeMounts")]
    pub volume_mounts: Vec<E2bVolumeMount>,
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
    let (init_command, init_env, init_cwd) = request
        .init_command
        .map(E2bInitCommand::into_parts)
        .unwrap_or_default();
    let init_timeout_ms = request.init_timeout_ms.unwrap_or(30_000);
    Ok(SandboxSpec {
        image: clouisle_core::ImageRef::new(request.template_id),
        resources: Resources::default(),
        network: NetworkConfig {
            enabled,
            allow_egress: network.allow_out,
            deny_egress: network.deny_out,
        },
        env: request.env_vars.unwrap_or_default(),
        metadata: request.metadata.unwrap_or_default(),
        volume_mounts: request
            .volume_mounts
            .into_iter()
            .map(|mount| VolumeMountSpec {
                name: mount.name,
                target: mount.path,
            })
            .collect(),
        init_command,
        init_env,
        init_cwd,
        init_timeout_ms,
        restart_policy: RestartPolicy::OnFailure,
        auto_pause,
        auto_pause_memory: request.auto_pause_memory,
        auto_resume,
        ttl_secs: Some(request.timeout.unwrap_or(15)),
        tenant_id: Some(tenant_id),
        ..SandboxSpec::default()
    })
}

pub fn from_sandbox(sandbox: &Sandbox) -> E2bSandbox {
    let state = match sandbox.status {
        SandboxStatus::Pending | SandboxStatus::Starting => "starting",
        SandboxStatus::Running => "running",
        SandboxStatus::Paused => "paused",
        SandboxStatus::Stopping => "stopping",
        SandboxStatus::Stopped => "killed",
        SandboxStatus::Error => "error",
    };
    // 官方 SDK 的 SandboxDetail 将 startedAt/endAt 视为必填；running 前
    // ready_at 尚缺，回退 created_at 与 created_at+ttl 保证恒有值。
    let started_at = sandbox.ready_at.unwrap_or(sandbox.created_at);
    let end_at = sandbox.expires_at.or_else(|| {
        sandbox
            .spec
            .ttl_secs
            .map(|ttl| started_at + Duration::seconds(ttl as i64))
    });
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
        allow_internet_access: Some(sandbox.spec.network.enabled),
        network: E2bNetwork {
            allow_out: sandbox.spec.network.allow_egress.clone(),
            deny_out: sandbox.spec.network.deny_egress.clone(),
            allow_public_traffic: true,
        },
        lifecycle: E2bLifecycle {
            auto_resume: sandbox.spec.auto_resume,
            on_timeout: if sandbox.spec.auto_pause {
                "pause".into()
            } else {
                "kill".into()
            },
        },
        volume_mounts: sandbox
            .spec
            .volume_mounts
            .iter()
            .map(|mount| E2bVolumeMount {
                name: mount.name.clone(),
                path: mount.target.clone(),
            })
            .collect(),
        started_at: Some(started_at),
        end_at,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_e2b_pause_lifecycle_fields() {
        let request = serde_json::from_value::<E2bCreateRequest>(serde_json::json!({
            "templateID": "alpine:latest",
            "autoPause": true,
            "autoPauseMemory": false,
            "autoResume": true
        }))
        .unwrap();
        let spec = to_spec(request, "tenant-a".into()).unwrap();
        assert!(spec.auto_pause);
        assert!(!spec.auto_pause_memory);
        assert!(spec.auto_resume);
    }

    #[test]
    fn maps_volume_mounts() {
        let request = serde_json::from_value::<E2bCreateRequest>(serde_json::json!({
            "templateID": "alpine:latest",
            "volumeMounts": [{"name": "data", "path": "/mnt/data"}]
        }))
        .unwrap();
        let spec = to_spec(request, "tenant-a".into()).unwrap();
        assert_eq!(spec.volume_mounts[0].name, "data");
        assert_eq!(spec.volume_mounts[0].target, "/mnt/data");
    }

    /// 确定性伪随机 JSON 生成器（LCG）：验证任意结构输入不会 panic。
    #[test]
    fn random_json_never_panics_on_create_parse() {
        struct Lcg(u64);
        impl Lcg {
            fn next(&mut self) -> u64 {
                self.0 = self
                    .0
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                self.0
            }
            fn value(&mut self, depth: u32) -> serde_json::Value {
                use serde_json::Value;
                match self.next() % if depth > 3 { 4 } else { 7 } {
                    0 => Value::Null,
                    1 => Value::Bool(self.next().is_multiple_of(2)),
                    2 => Value::from(self.next() as i64 % 1000),
                    3 => Value::from(self.next() as f64 / 1000.0),
                    4 => {
                        let len = (self.next() % 5) as usize;
                        Value::Array((0..len).map(|_| self.value(depth + 1)).collect())
                    }
                    5 => {
                        let len = (self.next() % 5) as usize;
                        let mut map = serde_json::Map::new();
                        for _ in 0..len {
                            let key = format!("k{}", self.next() % 20);
                            map.insert(key, self.value(depth + 1));
                        }
                        Value::Object(map)
                    }
                    _ => Value::String(format!("s{}", self.next() % 100)),
                }
            }
        }
        let mut rng = Lcg(0x5EED_CAFE);
        for _ in 0..200 {
            let input = rng.value(0);
            let _ = serde_json::from_value::<E2bCreateRequest>(input);
        }
    }

    #[test]
    fn maps_structured_initialization_command() {
        let request = serde_json::from_value::<E2bCreateRequest>(serde_json::json!({
            "templateID": "alpine:latest",
            "initCommand": {
                "cmd": "sh",
                "args": ["-c", "echo ready"],
                "envs": {"INIT_ONLY": "yes"},
                "cwd": "/work"
            }
        }))
        .unwrap();
        let spec = to_spec(request, "tenant-a".into()).unwrap();
        assert_eq!(spec.init_command, ["sh", "-c", "echo ready"]);
        assert_eq!(spec.init_env.get("INIT_ONLY"), Some(&"yes".to_string()));
        assert_eq!(spec.init_cwd.as_deref(), Some("/work"));
    }

    #[test]
    fn maps_metadata_into_sandbox_spec() {
        let request = serde_json::from_value::<E2bCreateRequest>(serde_json::json!({
            "templateID": "alpine:latest",
            "metadata": {"role": "worker"}
        }))
        .unwrap();
        let spec = to_spec(request, "tenant-a".into()).unwrap();
        assert_eq!(spec.metadata.get("role"), Some(&"worker".to_string()));
    }

    #[test]
    fn maps_e2b_network_allow_and_deny_rules() {
        let request = serde_json::from_value::<E2bCreateRequest>(serde_json::json!({
            "templateID": "alpine:latest",
            "network": {
                "allowOut": ["example.com"],
                "denyOut": ["192.0.2.0/24"]
            }
        }))
        .unwrap();
        let spec = to_spec(request, "tenant-a".into()).unwrap();
        assert_eq!(spec.network.allow_egress, ["example.com"]);
        assert_eq!(spec.network.deny_egress, ["192.0.2.0/24"]);
    }

    #[test]
    fn exposes_pending_and_error_states() {
        let mut sandbox = Sandbox::new("sandbox-1".into(), SandboxSpec::default());
        sandbox.status = SandboxStatus::Starting;
        assert_eq!(from_sandbox(&sandbox).state, "starting");
        sandbox.status = SandboxStatus::Error;
        assert_eq!(from_sandbox(&sandbox).state, "error");
    }
}
