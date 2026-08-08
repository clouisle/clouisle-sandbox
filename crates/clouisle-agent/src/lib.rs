//! clouisle-agent: guest 内二进制（--init PID 1 模式 + --serve vsock 模式）。

pub mod errors;
pub mod init;
pub mod serve;

pub use errors::{AgentError, AgentResult};
pub use serve::run_serve;