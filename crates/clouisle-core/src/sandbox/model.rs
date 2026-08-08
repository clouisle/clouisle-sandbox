//! `Sandbox` 聚合模型：id + spec + status + 时间戳 + VMM 元数据。

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::Result;
use crate::sandbox::SandboxSpec;
use crate::sandbox::state::{SandboxEvent, SandboxStatus};

/// VMM 运行时元数据（回填自 vmm.create）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct VmmMeta {
    /// 后端类型（mock / firecracker / docker）
    pub backend: String,
    /// 进程 PID（若适用）
    pub pid: Option<u64>,
    /// API socket 路径（Firecracker UDS）
    pub api_socket: Option<String>,
    /// vsock UDS 路径
    pub vsock_socket: Option<String>,
    /// VMM 分配的 ID
    pub vmm_id: Option<String>,
    /// 额外 JSON 元数据
    #[serde(default)]
    pub extra: std::collections::HashMap<String, String>,
}

/// 沙盒聚合。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sandbox {
    pub id: String,
    pub spec: SandboxSpec,
    pub status: SandboxStatus,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// 进入 Running 的时间（用于 SLO 测量）。
    pub ready_at: Option<DateTime<Utc>>,
    /// 到期时间（ttl）。
    pub expires_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub vmm_meta: VmmMeta,
    /// 正常停止原因 / 错误信息。
    #[serde(default)]
    pub terminal_message: Option<String>,
    /// 归属节点 ID（Phase 3）。
    #[serde(default)]
    pub node_id: Option<String>,
}

impl Sandbox {
    /// 构造一个处于 `Pending` 的新沙盒。
    pub fn new(id: String, spec: SandboxSpec) -> Self {
        let now = Utc::now();
        let expires_at = spec
            .ttl_secs
            .map(|ttl| now + chrono::Duration::seconds(ttl as i64));
        Self {
            id,
            spec,
            status: SandboxStatus::Pending,
            created_at: now,
            updated_at: now,
            ready_at: None,
            expires_at,
            vmm_meta: VmmMeta::default(),
            terminal_message: None,
            node_id: None,
        }
    }

    /// 幂等应用一次状态转换。
    pub fn transition(&mut self, event: SandboxEvent) -> Result<()> {
        let next = self.status.transition(event)?;
        self.status = next;
        self.updated_at = Utc::now();
        if next == SandboxStatus::Running {
            self.ready_at = Some(self.updated_at);
        }
        Ok(())
    }

    /// 是否可执行命令。
    pub fn is_executable(&self) -> bool {
        self.status.is_executable()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::SandboxStatus;

    fn spec() -> SandboxSpec {
        SandboxSpec {
            image: crate::sandbox::ImageRef::new("alpine:latest"),
            ..SandboxSpec::default()
        }
    }

    #[test]
    fn sandbox_new_pending() {
        let s = Sandbox::new("sbx-1".into(), spec());
        assert_eq!(s.status, SandboxStatus::Pending);
    }

    #[test]
    fn sandbox_full_cycle_reachable() {
        let mut s = Sandbox::new("sbx-1".into(), spec());
        s.transition(SandboxEvent::Start).unwrap();
        assert_eq!(s.status, SandboxStatus::Starting);
        s.transition(SandboxEvent::AgentHello).unwrap();
        assert_eq!(s.status, SandboxStatus::Running);
        assert!(s.ready_at.is_some());
        s.transition(SandboxEvent::Stop).unwrap();
        assert_eq!(s.status, SandboxStatus::Stopping);
        s.transition(SandboxEvent::VmmExited).unwrap();
        assert_eq!(s.status, SandboxStatus::Stopped);
    }

    #[test]
    fn sandbox_illegal_transition() {
        let mut s = Sandbox::new("sbx-1".into(), spec());
        // Running 后不能直接 VmmExited
        s.transition(SandboxEvent::Start).unwrap();
        s.transition(SandboxEvent::AgentHello).unwrap();
        assert!(s.transition(SandboxEvent::VmmExited).is_err());
    }

    #[test]
    fn ttl_sets_expires() {
        let mut sp = spec();
        sp.ttl_secs = Some(60);
        let s = Sandbox::new("sbx-1".into(), sp);
        assert!(s.expires_at.is_some());
    }

    #[test]
    fn sandbox_executable_check() {
        let mut s = Sandbox::new("sbx-1".into(), spec());
        s.transition(SandboxEvent::Start).unwrap();
        s.transition(SandboxEvent::AgentHello).unwrap();
        assert!(s.is_executable());
    }
}
