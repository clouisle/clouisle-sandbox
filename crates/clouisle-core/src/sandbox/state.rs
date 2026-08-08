//! 沙盒状态机（UNIT-001 ~ UNIT-003）。

use serde::{Deserialize, Serialize};

use crate::error::{ClouisleError, Result};

/// 沙盒状态。设计文档中的状态图：
///
/// ```text
/// Pending → Starting → Running → Stopping → Stopped → (delete)
///              │          │
///              ▼          ▼
///            Error      Error
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxStatus {
    /// 已创建但尚未启动（镜像未就绪等）
    Pending,
    /// 启动中（VMM 进程拉起、agent hello 等待中）
    Starting,
    /// 运行中（agent 已就绪，可接受 exec）
    Running,
    /// 停止中（VMM 进程退出等待中）
    Stopping,
    /// 已停止（VMM 进程已退出）
    Stopped,
    /// 错误状态（VMM 失败、心跳超时、崩溃）
    Error,
}

/// 触发状态转换的事件。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxEvent {
    Start,
    AgentHello,
    Stop,
    VmmExited,
    Failed,
    Restart,
}

impl SandboxStatus {
    /// 尝试执行状态转换。非法转换返回 `InvalidTransition`。
    pub fn transition(&self, event: SandboxEvent) -> Result<SandboxStatus> {
        use SandboxEvent::*;
        use SandboxStatus::*;

        let next = match (*self, event) {
            (Pending, Start) => Starting,
            (Pending, Failed) => Error,
            (Starting, AgentHello) => Running,
            (Starting, Failed) => Error,
            (Starting, Stop) => Stopping,
            (Running, Stop) => Stopping,
            (Running, Failed) => Error,
            (Running, Restart) => Starting,
            (Stopping, VmmExited) => Stopped,
            (Stopping, Failed) => Error,
            (Stopped, Start) => Starting,
            (Stopped, Restart) => Starting,
            (Error, Start) => Starting,
            (Error, Restart) => Starting,
            // 合法但幂等的转换：某些事件可重复发生
            (Starting, Start) => Starting,
            (Stopping, Stop) => Stopping,
            (Running, AgentHello) => Running,
            // 非法转换
            (from, ev) => {
                return Err(ClouisleError::invalid_state(format!(
                    "invalid transition: {from:?} --{ev:?}--> ?"
                )));
            }
        };
        Ok(next)
    }

    /// 是否是可执行命令的状态。
    pub fn is_executable(&self) -> bool {
        *self == SandboxStatus::Running
    }

    /// 是否是活动状态（需要 reconciler 关注的）。
    pub fn is_active(&self) -> bool {
        matches!(
            self,
            SandboxStatus::Starting | SandboxStatus::Running | SandboxStatus::Stopping
        )
    }

    /// 可接受删除的状态。
    pub fn can_delete(&self) -> bool {
        matches!(self, SandboxStatus::Stopped | SandboxStatus::Error)
    }

    /// 终止态。
    pub fn is_terminal(&self) -> bool {
        matches!(self, SandboxStatus::Stopped | SandboxStatus::Error)
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            SandboxStatus::Pending => "pending",
            SandboxStatus::Starting => "starting",
            SandboxStatus::Running => "running",
            SandboxStatus::Stopping => "stopping",
            SandboxStatus::Stopped => "stopped",
            SandboxStatus::Error => "error",
        }
    }
}

impl std::fmt::Display for SandboxStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legal_transition_running_to_stopping() {
        let s = SandboxStatus::Running;
        assert_eq!(
            s.transition(SandboxEvent::Stop).unwrap(),
            SandboxStatus::Stopping
        );
    }

    #[test]
    fn illegal_transition_running_to_starting() {
        let s = SandboxStatus::Running;
        let err = s.transition(SandboxEvent::Start).unwrap_err();
        assert_eq!(err.kind, crate::error::ErrorKind::InvalidState);
    }

    #[test]
    fn full_lifecycle() {
        let mut s = SandboxStatus::Pending;
        s = s.transition(SandboxEvent::Start).unwrap();
        assert_eq!(s, SandboxStatus::Starting);
        s = s.transition(SandboxEvent::AgentHello).unwrap();
        assert_eq!(s, SandboxStatus::Running);
        s = s.transition(SandboxEvent::Stop).unwrap();
        assert_eq!(s, SandboxStatus::Stopping);
        s = s.transition(SandboxEvent::VmmExited).unwrap();
        assert_eq!(s, SandboxStatus::Stopped);
    }

    #[test]
    fn full_transition_matrix() {
        use SandboxEvent::*;
        use SandboxStatus::*;

        let all_statuses = [Pending, Starting, Running, Stopping, Stopped, Error];
        let all_events = [Start, AgentHello, Stop, VmmExited, Failed, Restart];

        // 期望合法表（人工核对）
        let legal: &[(SandboxStatus, SandboxEvent, SandboxStatus)] = &[
            (Pending, Start, Starting),
            (Pending, Failed, Error),
            (Starting, AgentHello, Running),
            (Starting, Failed, Error),
            (Starting, Stop, Stopping),
            (Starting, Start, Starting),
            (Running, Stop, Stopping),
            (Running, Failed, Error),
            (Running, Restart, Starting),
            (Running, AgentHello, Running),
            (Stopping, VmmExited, Stopped),
            (Stopping, Failed, Error),
            (Stopping, Stop, Stopping),
            (Stopped, Start, Starting),
            (Stopped, Restart, Starting),
            (Error, Start, Starting),
            (Error, Restart, Starting),
        ];

        for from in all_statuses {
            for ev in all_events {
                let result = from.transition(ev);
                match legal.iter().find(|(f, e, _)| *f == from && *e == ev) {
                    Some((_, _, to)) => {
                        assert_eq!(
                            result.unwrap(),
                            *to,
                            "{from:?} --{ev:?}--> should be {to:?}"
                        );
                    }
                    None => {
                        assert!(
                            result.is_err(),
                            "{from:?} --{ev:?}--> should be illegal but succeeded"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn status_str() {
        assert_eq!(SandboxStatus::Running.as_str(), "running");
        assert_eq!(SandboxStatus::Pending.as_str(), "pending");
    }

    #[test]
    fn executable_states() {
        assert!(SandboxStatus::Running.is_executable());
        assert!(!SandboxStatus::Stopped.is_executable());
        assert!(!SandboxStatus::Starting.is_executable());
    }

    #[test]
    fn active_states() {
        assert!(SandboxStatus::Starting.is_active());
        assert!(SandboxStatus::Running.is_active());
        assert!(SandboxStatus::Stopping.is_active());
        assert!(!SandboxStatus::Pending.is_active());
        assert!(!SandboxStatus::Stopped.is_active());
        assert!(!SandboxStatus::Error.is_active());
    }

    #[test]
    fn terminal_states() {
        assert!(SandboxStatus::Stopped.is_terminal());
        assert!(SandboxStatus::Error.is_terminal());
        assert!(!SandboxStatus::Running.is_terminal());
    }
}
