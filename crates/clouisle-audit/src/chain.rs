//! 审计日志哈希链（SR-05 / ADR-003）。

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEvent {
    pub seq: u64,
    pub timestamp: DateTime<Utc>,
    pub node_id: String,
    pub sandbox_id: Option<String>,
    pub source: String, // "host" | "guest"
    pub event_type: String,
    pub payload: serde_json::Value,
    pub trust_level: String, // "trusted" | "advisory"
}

#[derive(Debug, Clone)]
pub struct HashChain {
    events: Vec<ChainEntry>,
}

#[derive(Debug, Clone)]
pub struct ChainEntry {
    pub event: AuditEvent,
    pub hash: String,
    pub prev_hash: String,
}

impl Default for HashChain {
    fn default() -> Self {
        Self::new()
    }
}

impl HashChain {
    pub fn new() -> Self {
        Self { events: Vec::new() }
    }

    pub fn append(&mut self, event: AuditEvent) -> String {
        let prev_hash = self
            .events
            .last()
            .map(|e| e.hash.clone())
            .unwrap_or_default();
        let hash = compute_hash(&event, &prev_hash);
        self.events.push(ChainEntry {
            event,
            hash: hash.clone(),
            prev_hash,
        });
        hash
    }

    pub fn entries(&self) -> &[ChainEntry] {
        &self.events
    }

    pub fn verify(&self) -> Result<(), Vec<ChainEntry>> {
        let mut broken = Vec::new();
        for (i, entry) in self.events.iter().enumerate() {
            let expected_prev = if i == 0 {
                String::new()
            } else {
                self.events[i - 1].hash.clone()
            };
            if entry.prev_hash != expected_prev {
                broken.push(entry.clone());
            }
            let expected_hash = compute_hash(&entry.event, &entry.prev_hash);
            if entry.hash != expected_hash {
                broken.push(entry.clone());
            }
        }
        if broken.is_empty() {
            Ok(())
        } else {
            Err(broken)
        }
    }
}

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
    use serde_json::json;

    #[test]
    fn chain_construction_and_verification() {
        let mut chain = HashChain::new();
        for i in 0..10 {
            chain.append(AuditEvent {
                seq: i,
                timestamp: Utc::now(),
                node_id: "node-1".into(),
                sandbox_id: Some(format!("sbx-{i}")),
                source: "host".into(),
                event_type: "sandbox_create".into(),
                payload: json!({"spec": "test"}),
                trust_level: "trusted".into(),
            });
        }
        assert!(chain.verify().is_ok());
    }

    #[test]
    fn tampering_detected() {
        let mut chain = HashChain::new();
        for i in 0..5 {
            chain.append(AuditEvent {
                seq: i,
                timestamp: Utc::now(),
                node_id: "node-1".into(),
                sandbox_id: Some(format!("sbx-{i}")),
                source: "host".into(),
                event_type: "sandbox_create".into(),
                payload: json!({"spec": "test"}),
                trust_level: "trusted".into(),
            });
        }
        // 篡改第 3 条事件
        let mut entries = chain.events.clone();
        entries[2].event.payload = json!({"evil": "data"});
        let tampered = HashChain { events: entries };
        let result = tampered.verify();
        assert!(result.is_err());
    }

    #[test]
    fn empty_chain_verifies() {
        let chain = HashChain::new();
        assert!(chain.verify().is_ok());
    }

    #[test]
    fn prev_hash_pointers_correct() {
        let mut chain = HashChain::new();
        for i in 0..3 {
            chain.append(AuditEvent {
                seq: i,
                timestamp: Utc::now(),
                node_id: "n1".into(),
                sandbox_id: None,
                source: "host".into(),
                event_type: "test".into(),
                payload: json!({}),
                trust_level: "trusted".into(),
            });
        }
        let entries = chain.entries();
        assert_eq!(entries[0].prev_hash, "");
        assert_eq!(entries[1].prev_hash, entries[0].hash);
        assert_eq!(entries[2].prev_hash, entries[1].hash);
    }
}
