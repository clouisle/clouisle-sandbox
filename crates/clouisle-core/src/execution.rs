//! 命令执行模型（FR-02）。

use std::collections::HashMap;

use bytes::Bytes;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// 执行规格。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionSpec {
    pub argv: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// 工作目录，None = guest 默认 cwd
    #[serde(default)]
    pub cwd: Option<String>,
    /// 执行超时（毫秒）
    pub timeout_ms: u64,
}

impl ExecutionSpec {
    pub fn validate(&self) -> Result<(), crate::error::ClouisleError> {
        if self.argv.is_empty() {
            return Err(crate::error::ClouisleError::validation("argv must not be empty"));
        }
        if self.timeout_ms == 0 {
            return Err(crate::error::ClouisleError::validation(
                "timeout_ms must be >= 1",
            ));
        }
        Ok(())
    }
}

/// 执行结果（一次性模式）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionResult {
    pub exit_code: i32,
    #[serde(default)]
    pub stdout: Bytes,
    #[serde(default)]
    pub stderr: Bytes,
    pub duration_ms: u64,
}

/// 持久化的执行记录（store 中保存）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionRecord {
    pub id: String,
    pub sandbox_id: String,
    pub spec: ExecutionSpec,
    pub exit_code: i32,
    #[serde(default)]
    pub stdout: Bytes,
    #[serde(default)]
    pub stderr: Bytes,
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
    /// 是否超时
    #[serde(default)]
    pub timed_out: bool,
    /// stdout 是否截断
    #[serde(default)]
    pub stdout_truncated: bool,
    /// stderr 是否截断
    #[serde(default)]
    pub stderr_truncated: bool,
    /// 执行节点 ID
    #[serde(default)]
    pub node_id: Option<String>,
}

/// stdout/stderr 截断上限（字节）。
pub const MAX_OUTPUT_BYTES: usize = 1024 * 1024; // 1 MiB

/// 截断到上限，返回截断标识。
pub fn truncate_output(data: &[u8]) -> (Bytes, bool) {
    if data.len() > MAX_OUTPUT_BYTES {
        (Bytes::copy_from_slice(&data[..MAX_OUTPUT_BYTES]), true)
    } else {
        (Bytes::copy_from_slice(data), false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn execution_spec_empty_argv_rejected() {
        let spec = ExecutionSpec {
            argv: vec![],
            env: HashMap::new(),
            cwd: None,
            timeout_ms: 1000,
        };
        assert!(spec.validate().is_err());
    }

    #[test]
    fn execution_spec_zero_timeout_rejected() {
        let spec = ExecutionSpec {
            argv: vec!["echo".into()],
            env: HashMap::new(),
            cwd: None,
            timeout_ms: 0,
        };
        assert!(spec.validate().is_err());
    }

    #[test]
    fn truncate_small_no_effect() {
        let data = b"hello";
        let (out, truncated) = truncate_output(data);
        assert_eq!(out.as_ref(), b"hello");
        assert!(!truncated);
    }

    #[test]
    fn truncate_large() {
        let data = vec![b'a'; MAX_OUTPUT_BYTES + 100];
        let (out, truncated) = truncate_output(&data);
        assert!(truncated);
        assert_eq!(out.len(), MAX_OUTPUT_BYTES);
    }
}