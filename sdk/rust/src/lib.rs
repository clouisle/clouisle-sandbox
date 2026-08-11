//! Clouisle Sandbox Rust SDK — fully typed API client.

use std::collections::HashMap;

use reqwest::{Client as HttpClient, Url};
use serde::{Deserialize, Serialize};
use thiserror::Error;

// ──────────────────────────────────────────────
//  Domain Types
// ──────────────────────────────────────────────

/// Image reference for a sandbox.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageRef {
    pub reference: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
}

/// Resources allocated to a sandbox.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct Resources {
    #[serde(default = "default_vcpu")]
    pub vcpu: u16,
    #[serde(default = "default_memory_mb")]
    pub memory_mb: u32,
    #[serde(default = "default_disk_mb")]
    pub disk_mb: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bandwidth_mbps: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub iops: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pids_max: Option<u32>,
}

const fn default_vcpu() -> u16 {
    1
}
const fn default_memory_mb() -> u32 {
    256
}
const fn default_disk_mb() -> u32 {
    512
}

impl Default for Resources {
    fn default() -> Self {
        Self {
            vcpu: 1,
            memory_mb: 256,
            disk_mb: 512,
            bandwidth_mbps: None,
            iops: None,
            pids_max: Some(512),
        }
    }
}

/// Network configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct NetworkConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub allow_egress: Vec<String>,
    #[serde(default)]
    pub deny_egress: Vec<String>,
}

const fn default_true() -> bool {
    true
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            allow_egress: Vec::new(),
            deny_egress: Vec::new(),
        }
    }
}

/// A host path mounted into the sandbox.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MountSpec {
    pub source: String,
    pub target: String,
    pub readonly: bool,
}

/// A secret materialized at `/run/secrets/<name>`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecretSpec {
    pub name: String,
    pub value: String,
}

/// Restart behavior after an unexpected sandbox failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RestartPolicy {
    #[default]
    Never,
    OnFailure,
    Always,
}

/// Spec for creating a sandbox.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct SandboxSpec {
    pub image: ImageRef,
    #[serde(default)]
    pub resources: Resources,
    #[serde(default)]
    pub network: NetworkConfig,
    #[serde(default)]
    pub mounts: Vec<MountSpec>,
    #[serde(default)]
    pub secrets: Vec<SecretSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_secs: Option<u64>,
    #[serde(default = "default_start_timeout")]
    pub start_timeout_secs: u64,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default)]
    pub node_selector: HashMap<String, String>,
    #[serde(default)]
    pub restart_policy: RestartPolicy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tenant_id: Option<String>,
}

const fn default_start_timeout() -> u64 {
    10
}

impl Default for SandboxSpec {
    fn default() -> Self {
        Self {
            image: ImageRef {
                reference: "alpine:latest".into(),
                digest: None,
            },
            resources: Resources::default(),
            network: NetworkConfig::default(),
            mounts: Vec::new(),
            secrets: Vec::new(),
            ttl_secs: None,
            start_timeout_secs: 10,
            env: HashMap::new(),
            node_selector: HashMap::new(),
            restart_policy: RestartPolicy::Never,
            tenant_id: None,
        }
    }
}

/// VMM metadata.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct VmmMeta {
    pub backend: String,
    #[serde(default)]
    pub pid: Option<u64>,
    #[serde(default)]
    pub api_socket: Option<String>,
    #[serde(default)]
    pub vsock_socket: Option<String>,
    #[serde(default)]
    pub vmm_id: Option<String>,
    #[serde(default)]
    pub extra: HashMap<String, String>,
}

/// Sandbox status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxStatus {
    Pending,
    Starting,
    Running,
    Stopping,
    Stopped,
    Error,
}

/// A sandbox instance.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct Sandbox {
    pub id: String,
    pub spec: SandboxSpec,
    pub status: SandboxStatus,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default)]
    pub ready_at: Option<String>,
    #[serde(default)]
    pub vmm_meta: VmmMeta,
    #[serde(default)]
    pub node_id: Option<String>,
}

/// Command execution request.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct ExecRequest {
    pub argv: Vec<String>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub env: HashMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
    #[serde(default)]
    pub stream: bool,
}

#[allow(dead_code)]
const fn default_timeout_ms() -> u64 {
    30000
}

impl ExecRequest {
    pub fn new(argv: Vec<String>) -> Self {
        Self {
            argv,
            env: HashMap::new(),
            cwd: None,
            timeout_ms: 30000,
            stream: false,
        }
    }
}

/// Result of a command execution.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ExecResult {
    pub exec_id: String,
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub duration_ms: u64,
    #[serde(default)]
    pub timed_out: bool,
    #[serde(default)]
    pub stdout_truncated: bool,
    #[serde(default)]
    pub stderr_truncated: bool,
}

/// An ordered server-sent execution event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecStreamEvent {
    Stdout(String),
    Stderr(String),
    Exit(i32),
    Error(String),
}

/// Execution record (persisted).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ExecutionRecord {
    pub id: String,
    pub sandbox_id: String,
    pub exit_code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub started_at: String,
    pub finished_at: String,
    #[serde(default)]
    pub timed_out: bool,
    #[serde(default)]
    pub stdout_truncated: bool,
    #[serde(default)]
    pub stderr_truncated: bool,
    #[serde(default)]
    pub node_id: Option<String>,
}

/// Directory entry.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct DirEntry {
    pub name: String,
    pub size: u64,
    pub mode: u32,
    pub mtime: i64,
    pub is_dir: bool,
}

/// Directory listing response.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ListFilesResponse {
    pub items: Vec<DirEntry>,
}

/// Sandbox list response.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct SandboxListResponse {
    pub items: Vec<Sandbox>,
    pub total: usize,
}

/// Health check response.
#[derive(Debug, Clone, Deserialize)]
pub struct HealthResponse {
    pub status: String,
    pub store: String,
    pub version: String,
}

/// Liveness/readiness response.
#[derive(Debug, Clone, Deserialize)]
pub struct StatusResponse {
    pub status: String,
}

/// Successful file upload response.
#[derive(Debug, Clone, Deserialize)]
pub struct UploadResponse {
    pub ok: bool,
}

// ──────────────────────────────────────────────
//  SDK Error
// ──────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum SdkError {
    #[error("HTTP {status}: {body}")]
    Http { status: u16, body: String },
    #[error("request failed: {0}")]
    Request(#[from] reqwest::Error),
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("API error: {code} {message}")]
    Api { code: String, message: String },
}

pub type Result<T> = std::result::Result<T, SdkError>;

// ──────────────────────────────────────────────
//  Client
// ──────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct Client {
    http: HttpClient,
    base_url: Url,
    api_key: String,
}

impl Client {
    /// Create a new client.
    pub fn new(base_url: &str, api_key: &str) -> Self {
        let normalized_url = base_url.trim_end_matches('/');
        let base_url = Url::parse(normalized_url).unwrap_or_else(|_| {
            Url::parse(&format!("http://{normalized_url}")).expect("invalid base_url")
        });
        Self {
            http: HttpClient::new(),
            base_url,
            api_key: api_key.to_string(),
        }
    }

    // ──────────────────────────────────────────
    //  Sandbox Lifecycle
    // ──────────────────────────────────────────

    /// Create a sandbox.
    pub async fn create_sandbox(&self, spec: &SandboxSpec) -> Result<Sandbox> {
        self.post("/api/v1/sandboxes", spec).await
    }

    /// Get a sandbox by ID.
    pub async fn get_sandbox(&self, id: &str) -> Result<Sandbox> {
        self.get(&format!("/api/v1/sandboxes/{id}")).await
    }

    /// List sandboxes.
    pub async fn list_sandboxes(
        &self,
        status: Option<SandboxStatus>,
        limit: Option<usize>,
        offset: Option<usize>,
    ) -> Result<SandboxListResponse> {
        let mut q: Vec<(String, String)> = Vec::new();
        if let Some(ref s) = status {
            q.push(("status".into(), format!("{:?}", s).to_lowercase()));
        }
        if let Some(l) = limit {
            q.push(("limit".into(), l.to_string()));
        }
        if let Some(o) = offset {
            q.push(("offset".into(), o.to_string()));
        }
        self.get_with_query("/api/v1/sandboxes", q).await
    }

    /// Delete a sandbox.
    pub async fn delete_sandbox(&self, id: &str) -> Result<()> {
        self.delete(&format!("/api/v1/sandboxes/{id}")).await
    }

    // ──────────────────────────────────────────
    //  Command Execution
    // ──────────────────────────────────────────

    /// Execute a command synchronously.
    pub async fn exec(&self, sandbox_id: &str, req: &ExecRequest) -> Result<ExecResult> {
        self.post(&format!("/api/v1/sandboxes/{sandbox_id}/exec"), req)
            .await
    }

    /// Convenience: exec with argv + timeout.
    pub async fn exec_cmd(
        &self,
        sandbox_id: &str,
        argv: Vec<String>,
        timeout_ms: u64,
    ) -> Result<ExecResult> {
        self.exec(
            sandbox_id,
            &ExecRequest {
                argv,
                timeout_ms,
                ..ExecRequest::new(vec![])
            },
        )
        .await
    }

    /// Execute and collect ordered server-sent output events.
    pub async fn stream_exec(
        &self,
        sandbox_id: &str,
        request: &ExecRequest,
    ) -> Result<Vec<ExecStreamEvent>> {
        let response = self
            .http
            .post(self.url(&format!("/api/v1/sandboxes/{sandbox_id}/exec/stream")))
            .headers(self.headers())
            .json(request)
            .send()
            .await?;
        let status = response.status();
        let body = response.text().await?;
        if !status.is_success() {
            return Err(SdkError::Http {
                status: status.as_u16(),
                body,
            });
        }
        let mut current_event = None;
        let mut events = Vec::new();
        for line in body.lines() {
            if let Some(event) = line.strip_prefix("event: ") {
                current_event = Some(event);
            } else if let Some(data) = line.strip_prefix("data: ") {
                let event = match current_event.take() {
                    Some("stdout") => ExecStreamEvent::Stdout(data.to_string()),
                    Some("stderr") => ExecStreamEvent::Stderr(data.to_string()),
                    Some("exit") => {
                        ExecStreamEvent::Exit(data.parse().map_err(|error| SdkError::Api {
                            code: "SSE_PARSE".to_string(),
                            message: format!("invalid exit event: {error}"),
                        })?)
                    }
                    Some("error") => ExecStreamEvent::Error(data.to_string()),
                    Some(_) | None => continue,
                };
                events.push(event);
            }
        }
        Ok(events)
    }

    /// Get a single execution record.
    pub async fn get_execution(&self, sandbox_id: &str, exec_id: &str) -> Result<ExecutionRecord> {
        self.get(&format!("/api/v1/sandboxes/{sandbox_id}/exec/{exec_id}"))
            .await
    }

    /// List execution records.
    pub async fn list_executions(
        &self,
        sandbox_id: &str,
        limit: Option<usize>,
    ) -> Result<Vec<ExecutionRecord>> {
        let mut q = Vec::new();
        if let Some(l) = limit {
            q.push(("limit".into(), l.to_string()));
        }
        self.get_with_query(&format!("/api/v1/sandboxes/{sandbox_id}/exec"), q)
            .await
    }

    // ──────────────────────────────────────────
    //  File Transfer
    // ──────────────────────────────────────────

    /// Upload a file.
    pub async fn upload_file(
        &self,
        sandbox_id: &str,
        path: &str,
        data: &[u8],
    ) -> Result<UploadResponse> {
        self.post_raw(
            &format!("/api/v1/sandboxes/{sandbox_id}/files/upload"),
            path,
            data,
        )
        .await
    }

    /// Download a file as raw bytes.
    pub async fn download_file(&self, sandbox_id: &str, path: &str) -> Result<Vec<u8>> {
        let resp = self
            .http
            .get(self.url(&format!("/api/v1/sandboxes/{sandbox_id}/files/download")))
            .headers(self.headers())
            .query(&[("path", path)])
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            return Err(SdkError::Http {
                status: status.as_u16(),
                body: resp.text().await?,
            });
        }
        Ok(resp.bytes().await?.to_vec())
    }

    /// List files in a directory.
    pub async fn list_files(&self, sandbox_id: &str, path: &str) -> Result<ListFilesResponse> {
        self.get_with_query(
            &format!("/api/v1/sandboxes/{sandbox_id}/files/ls"),
            vec![("path".into(), path.into())],
        )
        .await
    }

    // ──────────────────────────────────────────
    //  Observability
    // ──────────────────────────────────────────

    pub async fn health(&self) -> Result<HealthResponse> {
        self.get_no_auth("/health").await
    }
    pub async fn liveness(&self) -> Result<StatusResponse> {
        self.get("/health/live").await
    }
    pub async fn readiness(&self) -> Result<StatusResponse> {
        self.get("/health/ready").await
    }
    pub async fn metrics(&self) -> Result<String> {
        Ok(self
            .http
            .get(self.url("/metrics"))
            .send()
            .await?
            .text()
            .await?)
    }

    // ──────────────────────────────────────────
    //  Internal HTTP
    // ──────────────────────────────────────────

    fn url(&self, path: &str) -> String {
        self.base_url
            .join(path)
            .expect("invalid API path")
            .to_string()
    }

    fn headers(&self) -> reqwest::header::HeaderMap {
        let mut h = reqwest::header::HeaderMap::new();
        if !self.api_key.is_empty() {
            h.insert(
                "Authorization",
                format!("Bearer {}", self.api_key).parse().unwrap(),
            );
        }
        h
    }

    async fn get<T: for<'de> Deserialize<'de>>(&self, path: &str) -> Result<T> {
        self.check_response(
            self.http
                .get(self.url(path))
                .headers(self.headers())
                .send()
                .await?,
        )
        .await
    }
    async fn get_no_auth<T: for<'de> Deserialize<'de>>(&self, path: &str) -> Result<T> {
        self.check_response(self.http.get(self.url(path)).send().await?)
            .await
    }
    async fn get_with_query<T: for<'de> Deserialize<'de>>(
        &self,
        path: &str,
        q: Vec<(String, String)>,
    ) -> Result<T> {
        self.check_response(
            self.http
                .get(self.url(path))
                .headers(self.headers())
                .query(&q)
                .send()
                .await?,
        )
        .await
    }
    async fn post<T: for<'de> Deserialize<'de>, B: Serialize>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T> {
        self.check_response(
            self.http
                .post(self.url(path))
                .headers(self.headers())
                .json(body)
                .send()
                .await?,
        )
        .await
    }
    async fn post_raw<T: for<'de> Deserialize<'de>>(
        &self,
        path: &str,
        file_path: &str,
        data: &[u8],
    ) -> Result<T> {
        self.check_response(
            self.http
                .post(self.url(path))
                .headers(self.headers())
                .query(&[("path", file_path)])
                .body(data.to_vec())
                .send()
                .await?,
        )
        .await
    }
    async fn delete(&self, path: &str) -> Result<()> {
        let resp = self
            .http
            .delete(self.url(path))
            .headers(self.headers())
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            return Err(SdkError::Http {
                status: status.as_u16(),
                body: resp.text().await?,
            });
        }
        Ok(())
    }
    async fn check_response<T: for<'de> Deserialize<'de>>(
        &self,
        resp: reqwest::Response,
    ) -> Result<T> {
        let status = resp.status();
        let body = resp.text().await?;
        if !status.is_success() {
            return Err(SdkError::Http {
                status: status.as_u16(),
                body,
            });
        }
        Ok(serde_json::from_str(&body)?)
    }
}
