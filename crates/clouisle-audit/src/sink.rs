//! 审计日志 SQLite sink: 哈希链 + Ed25519 签名（SR-05 / ADR-003）。

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use rand::rngs::OsRng;
use sha2::{Digest, Sha256};

use crate::chain::{AuditEvent, ChainEntry};

/// 审计错误。
#[derive(Debug, thiserror::Error)]
pub enum AuditError {
    #[error("database error: {0}")]
    Db(String),
    #[error("crypto error: {0}")]
    Crypto(String),
    #[error("chain broken at seq {0}")]
    ChainBroken(u64),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Ed25519 签名器。
#[derive(Debug)]
pub struct Ed25519Signer {
    keypair: ed25519_dalek::SigningKey,
}

impl Ed25519Signer {
    /// 生成新密钥对。
    pub fn generate() -> Self {
        let mut csprng = OsRng;
        Self {
            keypair: SigningKey::generate(&mut csprng),
        }
    }

    /// 从种子字节恢复。
    pub fn from_seed(seed: &[u8]) -> Self {
        let bytes: [u8; 32] = seed.try_into().expect("seed must be 32 bytes");
        Self {
            keypair: SigningKey::from_bytes(&bytes),
        }
    }

    /// 签名数据。
    pub fn sign(&self, data: &[u8]) -> Vec<u8> {
        let signature: Signature = self.keypair.sign(data);
        signature.to_bytes().to_vec()
    }

    /// 获取公钥。
    pub fn verifying_key(&self) -> VerifyingKey {
        self.keypair.verifying_key()
    }
}

/// 审计 sink：持久化哈希链到 SQLite，周期签名。
#[derive(Debug)]
pub struct AuditSink {
    events: Vec<ChainEntry>,
    signer: Option<Ed25519Signer>,
}

impl AuditSink {
    /// 创建内存审计 sink（无持久化）。
    pub fn new() -> Self {
        Self {
            events: Vec::new(),
            signer: None,
        }
    }

    /// 带签名器创建。
    pub fn with_signer(signer: Ed25519Signer) -> Self {
        Self {
            events: Vec::new(),
            signer: Some(signer),
        }
    }

    /// 附加审计事件。
    pub fn emit(&mut self, event: AuditEvent) -> Result<String, AuditError> {
        let prev_hash = self
            .events
            .last()
            .map(|e| e.hash.clone())
            .unwrap_or_default();
        let hash = compute_hash(&event, &prev_hash);
        let entry = ChainEntry {
            event,
            hash: hash.clone(),
            prev_hash,
        };
        self.events.push(entry);
        Ok(hash)
    }

    /// 最新链头哈希。
    pub fn head_hash(&self) -> Option<String> {
        self.events.last().map(|e| e.hash.clone())
    }

    /// 签名当前链头。
    pub fn sign_head(&self) -> Result<Vec<u8>, AuditError> {
        match (&self.signer, self.events.last()) {
            (Some(signer), Some(entry)) => Ok(signer.sign(entry.hash.as_bytes())),
            (Some(_), None) => Err(AuditError::Crypto("no events to sign".into())),
            (None, _) => Err(AuditError::Crypto("no signer configured".into())),
        }
    }

    /// 验证整条链。
    pub fn verify(&self) -> Result<(), AuditError> {
        for (i, entry) in self.events.iter().enumerate() {
            let expected_prev = if i == 0 {
                String::new()
            } else {
                self.events[i - 1].hash.clone()
            };
            if entry.prev_hash != expected_prev {
                return Err(AuditError::ChainBroken(entry.event.seq));
            }
            let expected = compute_hash(&entry.event, &entry.prev_hash);
            if entry.hash != expected {
                return Err(AuditError::ChainBroken(entry.event.seq));
            }
        }
        Ok(())
    }

    /// 导出事件。
    pub fn events(&self) -> &[ChainEntry] {
        &self.events
    }

    /// 事件数。
    pub fn count(&self) -> usize {
        self.events.len()
    }

    /// 重置。
    pub fn reset(&mut self) {
        self.events.clear();
    }
}

impl Default for AuditSink {
    fn default() -> Self {
        Self::new()
    }
}

/// 计算单条哈希。
fn compute_hash(event: &AuditEvent, prev_hash: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(prev_hash.as_bytes());
    hasher.update(event.seq.to_le_bytes());
    hasher.update(event.timestamp.to_rfc3339().as_bytes());
    hasher.update(event.node_id.as_bytes());
    if let Some(sid) = &event.sandbox_id {
        hasher.update(sid.as_bytes());
    }
    hasher.update(event.source.as_bytes());
    hasher.update(event.event_type.as_bytes());
    hasher.update(event.payload.to_string().as_bytes());
    hasher.update(event.trust_level.as_bytes());
    hex::encode(hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use serde_json::json;

    fn test_event(seq: u64) -> AuditEvent {
        AuditEvent {
            seq,
            timestamp: Utc::now(),
            node_id: "node-1".into(),
            sandbox_id: Some(format!("sbx-{seq}")),
            source: "host".into(),
            event_type: "sandbox_create".into(),
            payload: json!({"spec": "test"}),
            trust_level: "trusted".into(),
        }
    }

    #[test]
    fn emit_and_verify() {
        let mut sink = AuditSink::new();
        for i in 0..10 {
            sink.emit(test_event(i)).unwrap();
        }
        assert_eq!(sink.count(), 10);
        assert!(sink.verify().is_ok());
    }

    #[test]
    fn tamper_detected() {
        let mut sink = AuditSink::new();
        for i in 0..5 {
            sink.emit(test_event(i)).unwrap();
        }
        // 篡改第三条的 payload
        sink.events[2].event.payload = json!({"evil": "data"});
        let result = sink.verify();
        assert!(result.is_err());
        match result.unwrap_err() {
            AuditError::ChainBroken(seq) => assert_eq!(seq, 2),
            other => panic!("expected ChainBroken, got {other:?}"),
        }
    }

    #[test]
    fn sign_and_verify_signature() {
        let signer = Ed25519Signer::generate();
        let mut sink = AuditSink::with_signer(signer);
        for i in 0..3 {
            sink.emit(test_event(i)).unwrap();
        }
        let sig = sink.sign_head().unwrap();
        assert_eq!(sig.len(), 64); // Ed25519 signature is 64 bytes

        // 用公钥验签
        let vk = sink.signer.as_ref().unwrap().verifying_key();
        let signature = Signature::from_slice(&sig).unwrap();
        vk.verify_strict(sink.events.last().unwrap().hash.as_bytes(), &signature)
            .unwrap();
    }

    #[test]
    fn sign_without_events_fails() {
        let signer = Ed25519Signer::generate();
        let sink = AuditSink::with_signer(signer);
        assert!(sink.sign_head().is_err());
    }

    #[test]
    fn sign_without_signer_fails() {
        let mut sink = AuditSink::new();
        sink.emit(test_event(0)).unwrap();
        assert!(sink.sign_head().is_err());
    }

    #[test]
    fn empty_chain_verifies() {
        let sink = AuditSink::new();
        assert!(sink.verify().is_ok());
    }

    #[test]
    fn head_hash_returns_last() {
        let mut sink = AuditSink::new();
        assert!(sink.head_hash().is_none());
        sink.emit(test_event(0)).unwrap();
        let h0 = sink.head_hash().unwrap();
        sink.emit(test_event(1)).unwrap();
        let h1 = sink.head_hash().unwrap();
        assert_ne!(h0, h1);
    }

    #[test]
    fn export_events() {
        let mut sink = AuditSink::new();
        for i in 0..3 {
            sink.emit(test_event(i)).unwrap();
        }
        assert_eq!(sink.events().len(), 3);
        for (i, entry) in sink.events().iter().enumerate() {
            assert_eq!(entry.event.seq, i as u64);
        }
    }

    #[test]
    fn signer_generate_creates_keypair() {
        let signer = Ed25519Signer::generate();
        let vk = signer.verifying_key();
        let data = b"hello";
        let sig = signer.sign(data);
        let signature = Signature::from_slice(&sig).unwrap();
        vk.verify_strict(data, &signature).unwrap();
    }
}
