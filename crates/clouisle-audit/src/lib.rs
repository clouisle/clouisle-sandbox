//! clouisle-audit: 审计日志哈希链与签名（SR-05 / ADR-003）。

pub mod chain;
pub mod sink;

pub use chain::{AuditEvent, ChainEntry, HashChain};
pub use sink::{AuditError, AuditSink, Ed25519Signer};
