//! agent 错误类型。

use clouisle_proto::CodecError;

pub type AgentResult<T> = std::result::Result<T, AgentError>;

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("protocol error: {0}")]
    Protocol(#[from] CodecError),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("command failed: {0}")]
    Command(String),
    #[error("invalid request: {0}")]
    InvalidRequest(String),
}
