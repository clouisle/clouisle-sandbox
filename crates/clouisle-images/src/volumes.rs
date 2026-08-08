//! 持久化存储挂载（FR-12）。
//!
//! 三种语义（ADR-003）：
//! 1. `readonly: true` → 宿主机目录打成只读 ext4 镜像，多 VM 可共享
//! 2. `readonly: false` → 创建时通过 agent 推入 guest，销毁时回写宿主机
//! 3. 超过 100 MB 的可写 mount 拒绝（建议用只读或对象存储）

use std::path::{Path, PathBuf};

use clouisle_core::MountSpec;

/// 可写 mount 大小上限（100 MB）。
pub const MAX_WRITABLE_MOUNT_BYTES: u64 = 100 * 1024 * 1024;

/// 卷管理错误。
#[derive(Debug, thiserror::Error)]
pub enum VolumeError {
    #[error("source not found: {0}")]
    SourceNotFound(String),
    #[error("writable mount exceeds 100MB limit: {0} bytes")]
    TooLarge(u64),
    #[error("invalid target path: {0}")]
    InvalidTarget(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// 卷配置。
#[derive(Debug, Clone)]
pub struct Volume {
    pub source: PathBuf,
    pub target: String,
    pub readonly: bool,
    pub size_bytes: u64,
}

impl Volume {
    /// 从 MountSpec 构造，校验存在性与大小。
    pub fn from_spec(spec: &MountSpec) -> Result<Self, VolumeError> {
        let source = Path::new(&spec.source);
        if !source.exists() {
            return Err(VolumeError::SourceNotFound(spec.source.clone()));
        }
        if !spec.target.starts_with('/') {
            return Err(VolumeError::InvalidTarget(spec.target.clone()));
        }
        let size_bytes = dir_size(source)?;
        if !spec.readonly && size_bytes > MAX_WRITABLE_MOUNT_BYTES {
            return Err(VolumeError::TooLarge(size_bytes));
        }
        Ok(Self {
            source: source.to_path_buf(),
            target: spec.target.clone(),
            readonly: spec.readonly,
            size_bytes,
        })
    }

    /// 只读卷：估算 ext4 镜像所需大小。
    pub fn ext4_size_bytes(&self) -> u64 {
        // 目录大小 × 1.3 余量 + 64 MB 元数据
        (self.size_bytes as f64 * 1.3) as u64 + (64 * 1024 * 1024)
    }
}

/// 递归计算目录/文件大小。
fn dir_size(path: &Path) -> Result<u64, std::io::Error> {
    if path.is_file() {
        return Ok(path.metadata()?.len());
    }
    let mut total = 0u64;
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let p = entry.path();
        if p.is_dir() {
            total += dir_size(&p)?;
        } else {
            total += entry.metadata()?.len();
        }
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn missing_source_rejected() {
        let spec = MountSpec {
            source: "/nonexistent/path".into(),
            target: "/work".into(),
            readonly: true,
        };
        assert!(matches!(
            Volume::from_spec(&spec),
            Err(VolumeError::SourceNotFound(_))
        ));
    }

    #[test]
    fn relative_target_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let spec = MountSpec {
            source: dir.path().to_string_lossy().into_owned(),
            target: "relative/path".into(),
            readonly: true,
        };
        assert!(matches!(
            Volume::from_spec(&spec),
            Err(VolumeError::InvalidTarget(_))
        ));
    }

    #[test]
    fn writable_too_large_rejected() {
        let dir = tempfile::tempdir().unwrap();
        // 写入超过 100MB 的文件
        let big = dir.path().join("big.bin");
        let f = fs::File::create(&big).unwrap();
        f.set_len(MAX_WRITABLE_MOUNT_BYTES + 1).unwrap();
        drop(f);
        let spec = MountSpec {
            source: dir.path().to_string_lossy().into_owned(),
            target: "/work".into(),
            readonly: false,
        };
        assert!(matches!(
            Volume::from_spec(&spec),
            Err(VolumeError::TooLarge(_))
        ));
    }

    #[test]
    fn readonly_allows_large() {
        let dir = tempfile::tempdir().unwrap();
        let big = dir.path().join("big.bin");
        let f = fs::File::create(&big).unwrap();
        f.set_len(MAX_WRITABLE_MOUNT_BYTES + 1).unwrap();
        drop(f);
        let spec = MountSpec {
            source: dir.path().to_string_lossy().into_owned(),
            target: "/work".into(),
            readonly: true,
        };
        let vol = Volume::from_spec(&spec).unwrap();
        assert!(vol.readonly);
    }

    #[test]
    fn ext4_size_has_headroom() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), vec![0u8; 1024]).unwrap();
        let spec = MountSpec {
            source: dir.path().to_string_lossy().into_owned(),
            target: "/work".into(),
            readonly: true,
        };
        let vol = Volume::from_spec(&spec).unwrap();
        assert!(vol.ext4_size_bytes() > vol.size_bytes);
    }
}
