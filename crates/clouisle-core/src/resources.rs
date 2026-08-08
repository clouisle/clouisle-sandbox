//! 资源描述与校验（FR-04 基础）。

use serde::{Deserialize, Serialize};

/// 沙盒资源需求。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Resources {
    /// vCPU 数量，≥ 1
    pub vcpu: u16,
    /// 内存（MiB），≥ 64
    pub memory_mb: u32,
    /// 磁盘 scratch 大小（MiB），≥ 64
    pub disk_mb: u32,
    /// 出站带宽上限（Mbps），None = 不限制
    pub bandwidth_mbps: Option<u32>,
    /// 磁盘 IOPS 上限，None = 不限制
    pub iops: Option<u32>,
    /// 进程数上限（cgroup pids.max），None = 默认 512
    pub pids_max: Option<u32>,
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

/// 校验错误，携带具体字段名。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidationError {
    pub field: String,
    pub message: String,
}

impl ValidationError {
    pub(crate) fn new(field: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            field: field.into(),
            message: message.into(),
        }
    }
}

impl Resources {
    /// 校验资源配置的合法性。
    ///
    /// 规则：
    /// - `vcpu` ≥ 1 且 ≤ 4（单沙盒上限，Phase 1 定）
    /// - `memory_mb` ≥ 64 且 ≤ 8192（单沙盒上限 8 GB）
    /// - `disk_mb` ≥ 64
    /// - `bandwidth_mbps` ≥ 1（若 Some）
    pub fn validate(&self) -> Result<(), Vec<ValidationError>> {
        let mut errors = Vec::new();

        if self.vcpu == 0 {
            errors.push(ValidationError::new("vcpu", "vcpu must be >= 1"));
        } else if self.vcpu > 4 {
            errors.push(ValidationError::new(
                "vcpu",
                format!("vcpu must be <= 4, got {}", self.vcpu),
            ));
        }

        if self.memory_mb < 64 {
            errors.push(ValidationError::new(
                "memory_mb",
                format!("memory_mb must be >= 64, got {}", self.memory_mb),
            ));
        } else if self.memory_mb > 8192 {
            errors.push(ValidationError::new(
                "memory_mb",
                format!("memory_mb must be <= 8192, got {}", self.memory_mb),
            ));
        }

        if self.disk_mb < 64 {
            errors.push(ValidationError::new(
                "disk_mb",
                format!("disk_mb must be >= 64, got {}", self.disk_mb),
            ));
        }

        if let Some(bw) = self.bandwidth_mbps {
            if bw == 0 {
                errors.push(ValidationError::new(
                    "bandwidth_mbps",
                    "bandwidth_mbps must be >= 1 if set",
                ));
            }
        }

        if let Some(iops) = self.iops {
            if iops == 0 {
                errors.push(ValidationError::new("iops", "iops must be >= 1 if set"));
            }
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }

    /// 计算两个资源需求的并集（用于合并累加）。
    pub fn checked_add(&self, other: &Resources) -> Option<Resources> {
        Some(Resources {
            vcpu: self.vcpu.checked_add(other.vcpu)?,
            memory_mb: self.memory_mb.checked_add(other.memory_mb)?,
            disk_mb: self.disk_mb.checked_add(other.disk_mb)?,
            bandwidth_mbps: match (self.bandwidth_mbps, other.bandwidth_mbps) {
                (Some(a), Some(b)) => Some(a.max(b)),
                (a, b) => a.or(b),
            },
            iops: match (self.iops, other.iops) {
                (Some(a), Some(b)) => Some(a.max(b)),
                (a, b) => a.or(b),
            },
            pids_max: self.pids_max.or(other.pids_max),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resources_default_valid() {
        let r = Resources::default();
        assert!(r.validate().is_ok());
    }

    #[test]
    fn resources_vcpu_zero_rejected() {
        let r = Resources {
            vcpu: 0,
            ..Resources::default()
        };
        let errs = r.validate().unwrap_err();
        assert!(errs.iter().any(|e| e.field == "vcpu"));
    }

    #[test]
    fn resources_memory_too_small_rejected() {
        let r = Resources {
            memory_mb: 32,
            ..Resources::default()
        };
        let errs = r.validate().unwrap_err();
        assert!(errs.iter().any(|e| e.field == "memory_mb"));
    }

    #[test]
    fn resources_vcpu_over_cap_rejected() {
        let r = Resources {
            vcpu: 8,
            ..Resources::default()
        };
        let errs = r.validate().unwrap_err();
        assert!(errs.iter().any(|e| e.field == "vcpu"));
    }

    #[test]
    fn resources_multiple_errors_reported() {
        let r = Resources {
            vcpu: 0,
            memory_mb: 16,
            disk_mb: 8,
            ..Resources::default()
        };
        let errs = r.validate().unwrap_err();
        assert_eq!(errs.len(), 3);
    }

    #[test]
    fn resources_zero_bandwidth_rejected() {
        let r = Resources {
            bandwidth_mbps: Some(0),
            ..Resources::default()
        };
        let errs = r.validate().unwrap_err();
        assert!(errs.iter().any(|e| e.field == "bandwidth_mbps"));
    }

    #[test]
    fn resources_checked_add() {
        let a = Resources {
            vcpu: 1,
            memory_mb: 256,
            disk_mb: 512,
            bandwidth_mbps: Some(10),
            ..Resources::default()
        };
        let b = Resources {
            vcpu: 2,
            memory_mb: 512,
            disk_mb: 1024,
            bandwidth_mbps: Some(5),
            ..Resources::default()
        };
        let sum = a.checked_add(&b).unwrap();
        assert_eq!(sum.vcpu, 3);
        assert_eq!(sum.memory_mb, 768);
        assert_eq!(sum.disk_mb, 1536);
        assert_eq!(sum.bandwidth_mbps, Some(10)); // max
    }
}