//! OCI 镜像拉取与 ext4 构建（FR-06）。

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use oci_distribution::Reference;
use oci_distribution::client::{Client, ClientConfig};
use oci_distribution::manifest::{OciDescriptor, OciImageManifest};
use oci_distribution::secrets::RegistryAuth;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tokio::sync::RwLock;
use tracing::{debug, info};

/// 镜像引用。
#[derive(Debug, Clone, Hash, Eq, PartialEq)]
pub struct ImageSpec {
    pub reference: String,
    pub digest: Option<String>,
}

/// 镜像管道错误。
#[derive(Debug, thiserror::Error)]
pub enum ImageError {
    #[error("invalid image reference '{0}': {1}")]
    InvalidReference(String, String),
    #[error("registry operation failed: {0}")]
    Registry(String),
    #[error("layer extraction failed: {0}")]
    Extract(String),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("ext4 build failed: {0}")]
    FsBuild(String),
    #[error("agent binary not configured: {0}")]
    AgentBinary(String),
    #[error("unsupported platform: {0}")]
    UnsupportedPlatform(String),
}

/// OCI 镜像管理器：拉取镜像、构建 ext4 根文件系统、按 digest 缓存。
#[derive(Clone)]
pub struct ImageManager {
    cache: Arc<RwLock<HashMap<String, String>>>,
    cache_dir: PathBuf,
    agent_binary: Option<PathBuf>,
    /// Lazily computed, streaming SHA-256 of the injected agent binary.
    agent_fingerprint: Arc<OnceLock<Result<String, String>>>,
    client: Client,
    auth: RegistryAuth,
}

impl Default for ImageManager {
    fn default() -> Self {
        Self::new()
    }
}

impl ImageManager {
    pub fn new() -> Self {
        Self {
            cache: Arc::new(RwLock::new(HashMap::new())),
            cache_dir: PathBuf::from("/tmp/clouisle-cache"),
            agent_binary: None,
            agent_fingerprint: Arc::new(OnceLock::new()),
            client: Client::new(ClientConfig::default()),
            auth: RegistryAuth::Anonymous,
        }
    }

    /// 覆盖 ext4 缓存目录。
    pub fn with_cache_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.cache_dir = dir.into();
        self
    }

    /// 指定要注入的 clouisle-agent 二进制路径。
    pub fn with_agent_binary(mut self, path: impl Into<PathBuf>) -> Self {
        self.agent_binary = Some(path.into());
        self.agent_fingerprint = Arc::new(OnceLock::new());
        self
    }

    /// 为私有 registry 配置 Basic 认证。
    pub fn with_registry_auth(
        mut self,
        username: impl Into<String>,
        password: impl Into<String>,
    ) -> Self {
        self.auth = RegistryAuth::Basic(username.into(), password.into());
        self
    }

    /// Check the two-level rootfs cache when the image digest is already known.
    /// The key includes the injected-agent digest, so a guest-agent upgrade
    /// never reuses a rootfs containing a stale executable.
    pub async fn cache_hit(&self, spec: &ImageSpec) -> bool {
        let Some(image_digest) = &spec.digest else {
            return false;
        };
        let Ok(key) = self.cache_key_for_image(image_digest) else {
            return false;
        };
        self.cached(&key).await.is_some()
    }

    /// Pull an OCI image and build an ext4 rootfs, returning its `.ext4` path.
    ///
    /// The cache key combines the platform image-manifest digest with the
    /// injected guest-agent digest. A pinned image digest can therefore check
    /// the cache without a registry request, while still invalidating on an
    /// agent upgrade.
    pub async fn pull_and_build(&self, spec: &ImageSpec) -> Result<String, ImageError> {
        if let Some(image_digest) = &spec.digest {
            let key = self.cache_key_for_image(image_digest)?;
            if let Some(path) = self.cached(&key).await {
                info!(key = %key, path = %path, "pinned digest cache hit");
                return Ok(path);
            }
        }

        let reference = Reference::try_from(spec.reference.clone())
            .map_err(|e| ImageError::InvalidReference(spec.reference.clone(), e.to_string()))?;

        debug!(reference = %spec.reference, "pulling platform image manifest");
        let (manifest, digest) = self
            .client
            .pull_image_manifest(&reference, &self.auth)
            .await
            .map_err(|error| {
                ImageError::Registry(format!("pull platform image manifest: {error}"))
            })?;

        let image_digest = spec.digest.as_deref().unwrap_or(&digest);
        let key = self.cache_key_for_image(image_digest)?;

        // Second cache check: the unpinned digest is only available after the
        // platform image manifest has been selected.
        if let Some(path) = self.cached(&key).await {
            return Ok(path);
        }

        info!(key = %key, reference = %spec.reference, "building ext4 rootfs");
        self.build(&reference, manifest, &key).await
    }

    /// 检查内存 + 磁盘两级缓存。
    async fn cached(&self, key: &str) -> Option<String> {
        {
            let cache = self.cache.read().await;
            if let Some(p) = cache.get(key)
                && Path::new(p).exists()
            {
                return Some(p.clone());
            }
        }
        let path = cache_path(&self.cache_dir, key);
        if path.is_file() {
            let mut cache = self.cache.write().await;
            let s = path.to_string_lossy().into_owned();
            cache.insert(key.to_string(), s.clone());
            Some(s)
        } else {
            None
        }
    }

    fn cache_key_for_image(&self, image_digest: &str) -> Result<String, ImageError> {
        let Some(agent_binary) = &self.agent_binary else {
            return Ok(image_digest.to_string());
        };
        let fingerprint = self
            .agent_fingerprint
            .get_or_init(|| fingerprint_file(agent_binary).map_err(|error| error.to_string()));
        let fingerprint = fingerprint
            .as_ref()
            .map_err(|error| ImageError::AgentBinary(error.clone()))?;
        Ok(format!("{image_digest}-agent-{fingerprint}"))
    }

    /// Build an ext4 rootfs from an already platform-resolved image manifest.
    async fn build(
        &self,
        reference: &Reference,
        image_manifest: OciImageManifest,
        key: &str,
    ) -> Result<String, ImageError> {
        // 解压工作目录（TempDir 在 build 结束时自动清理）。
        let work = tempfile::tempdir()?;
        let rootfs = work.path().join("rootfs");
        tokio::fs::create_dir_all(&rootfs).await?;

        for (i, layer) in image_manifest.layers.iter().enumerate() {
            debug!(
                layer = i,
                digest = %layer.digest,
                media_type = %layer.media_type,
                "extracting layer"
            );
            self.extract_layer(reference, layer, work.path(), &rootfs)
                .await?;
        }

        // 注入 agent 二进制（若配置）。
        if let Some(agent_path) = &self.agent_binary {
            self.inject_agent(agent_path, &rootfs).await?;
        }

        // 生成 ext4。
        tokio::fs::create_dir_all(&self.cache_dir).await?;
        let ext4_path = cache_path(&self.cache_dir, key);
        let ext4_str = ext4_path.to_string_lossy().into_owned();
        self.build_ext4(&rootfs, &ext4_path)?;

        // 记入缓存并返回。
        let mut cache = self.cache.write().await;
        cache.insert(key.to_string(), ext4_str.clone());
        info!(path = %ext4_str, "ext4 rootfs built and cached");
        Ok(ext4_str)
    }

    /// 下载单个 layer blob 并解压到 rootfs。
    async fn extract_layer(
        &self,
        reference: &Reference,
        layer: &OciDescriptor,
        work_dir: &Path,
        rootfs: &Path,
    ) -> Result<(), ImageError> {
        let layer_path = work_dir.join(format!("layer-{}", layer.digest.replace(':', "_")));

        // 流式下载 blob 到临时文件。
        let mut file = tokio::fs::File::create(&layer_path).await?;
        self.client
            .pull_blob(reference, layer, &mut file)
            .await
            .map_err(|e| ImageError::Registry(format!("pull blob {}: {e}", layer.digest)))?;
        file.flush().await?;
        drop(file);

        // 解压 + 展开 tar（CPU/IO 密集，放阻塞线程池）。
        let layer_path = layer_path.to_path_buf();
        let dst = rootfs.to_path_buf();
        let media_type = layer.media_type.clone();
        let digest = layer.digest.clone();
        tokio::task::spawn_blocking(move || {
            extract_tar_archive(&layer_path, &dst, &media_type)
                .map_err(|e| ImageError::Extract(format!("extract layer {digest}: {e}")))
        })
        .await
        .map_err(|e| ImageError::Extract(format!("join error: {e}")))??;
        Ok(())
    }

    /// 将 agent 二进制复制到 rootfs 的 /usr/local/bin/clouisle-agent。
    async fn inject_agent(&self, agent_path: &Path, rootfs: &Path) -> Result<(), ImageError> {
        if !agent_path.is_file() {
            return Err(ImageError::AgentBinary(format!(
                "agent binary not found at {}",
                agent_path.display()
            )));
        }
        let dest_dir = rootfs.join("usr/local/bin");
        let dest = dest_dir.join("clouisle-agent");
        tokio::fs::create_dir_all(&dest_dir).await?;
        tokio::fs::copy(agent_path, &dest).await?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = tokio::fs::metadata(&dest).await?.permissions();
            perms.set_mode(0o755);
            tokio::fs::set_permissions(&dest, perms).await?;
        }
        debug!("injected agent binary");
        Ok(())
    }

    /// 用 fallocate + mkfs.ext4 + mount + cp + umount 构建 ext4 镜像。
    #[cfg(target_os = "linux")]
    fn build_ext4(&self, rootfs: &Path, out_path: &Path) -> Result<(), ImageError> {
        let data_bytes = dir_size(rootfs)?;
        // ext4 大小 = 数据 × 1.3 余量 + 64 MB 元数据（与 volumes.rs 估算一致）。
        let ext4_size = (data_bytes as f64 * 1.3) as u64 + (64 * 1024 * 1024);
        debug!(
            rootfs = %rootfs.display(),
            data_bytes,
            ext4_bytes = ext4_size,
            "creating ext4 image"
        );

        // 清理可能残留的同名文件。
        if out_path.exists() {
            std::fs::remove_file(out_path)?;
        }

        // 1. fallocate 预分配稀疏镜像文件。
        run_cmd(
            "fallocate",
            &["-l", &ext4_size.to_string(), &out_path.to_string_lossy()],
        )?;

        // 2. mkfs.ext4 格式化。
        run_cmd("mkfs.ext4", &["-q", &out_path.to_string_lossy()])?;

        // 3. 挂载到临时目录。
        let mount_point = tempfile::tempdir()?;
        let mount_str = mount_point.path().to_string_lossy().into_owned();
        run_cmd(
            "mount",
            &["-o", "loop", &out_path.to_string_lossy(), &mount_str],
        )?;

        // 4. 把 rootfs 内容复制进挂载点（cp -a 保留权限/时间戳）。
        let copy_result = {
            let src = format!("{}/.", rootfs.display());
            run_cmd("cp", &["-a", &src, &mount_str])
        };

        // 5. 无论复制成败都卸载，避免残留挂载。
        let umount_result = run_cmd("umount", &[&mount_str]);
        copy_result?;
        umount_result?;
        Ok(())
    }

    #[cfg(not(target_os = "linux"))]
    fn build_ext4(&self, _rootfs: &Path, _out_path: &Path) -> Result<(), ImageError> {
        Err(ImageError::UnsupportedPlatform(
            "ext4 building requires Linux (fallocate/mkfs.ext4/mount)".into(),
        ))
    }
}

/// 计算磁盘缓存中的 ext4 文件路径（对 key 做文件名安全化）。
fn cache_path(cache_dir: &Path, key: &str) -> PathBuf {
    let safe = key.replace([':', '/', '\\'], "_");
    cache_dir.join(format!("{safe}.ext4"))
}

/// 递归计算目录/文件大小。
/// Recursively calculate a rootfs size without following layer symlinks.
/// OCI images commonly contain symlinked directories; following them can form
/// loops such as `/var/run -> /run`.
#[allow(dead_code)]
fn dir_size(path: &Path) -> Result<u64, std::io::Error> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.file_type().is_dir() {
        return Ok(metadata.len());
    }

    let mut total = 0u64;
    for entry in std::fs::read_dir(path)? {
        total += dir_size(&entry?.path())?;
    }
    Ok(total)
}

/// 执行系统命令并检查退出码。
#[cfg(target_os = "linux")]
fn run_cmd(program: &str, args: &[&str]) -> Result<(), ImageError> {
    let output = std::process::Command::new(program)
        .args(args)
        .output()
        .map_err(|e| ImageError::FsBuild(format!("failed to run '{program}': {e}")))?;
    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(ImageError::FsBuild(format!(
            "`{program} {}` failed (exit code {:?}): {}",
            args.join(" "),
            output.status.code(),
            stderr.trim(),
        )))
    }
}

fn fingerprint_file(path: &Path) -> Result<String, std::io::Error> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 32 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// 按 media type 解压并展开 tar 归档到 dst。
fn extract_tar_archive(src: &Path, dst: &Path, media_type: &str) -> Result<(), String> {
    let file = std::fs::File::open(src).map_err(|e| e.to_string())?;

    let reader: Box<dyn std::io::Read> = if media_type.contains("zstd") {
        Box::new(zstd::stream::read::Decoder::new(file).map_err(|e| e.to_string())?)
    } else if media_type.contains("gzip") {
        Box::new(flate2::read::GzDecoder::new(file))
    } else {
        // 未压缩 tar。
        Box::new(file)
    };

    let mut archive = tar::Archive::new(reader);
    archive.set_preserve_permissions(true);
    archive.set_preserve_mtime(true);
    archive.unpack(dst).map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_mgr() -> ImageManager {
        let dir = tempfile::tempdir().unwrap();
        ImageManager::new().with_cache_dir(dir.path())
    }

    #[tokio::test]
    async fn cache_hit_requires_digest() {
        let mgr = temp_mgr();
        // 无 digest 时无法离线判定，返回 false。
        let spec = ImageSpec {
            reference: "alpine:latest".into(),
            digest: None,
        };
        assert!(!mgr.cache_hit(&spec).await);

        // 有 digest 且未缓存 → false。
        let spec2 = ImageSpec {
            reference: "alpine:latest".into(),
            digest: Some("sha256:abc".into()),
        };
        assert!(!mgr.cache_hit(&spec2).await);
    }

    #[tokio::test]
    async fn digest_cache_key_hits_across_references() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = ImageManager::new().with_cache_dir(dir.path());
        let spec = ImageSpec {
            reference: "python:3.11".into(),
            digest: Some("sha256:abc".into()),
        };
        // 手动写入缓存（构造 `digest_cache_key` 语义：同 digest 不同 reference 命中）。
        let path = dir.path().join("sha256_abc.ext4");
        std::fs::write(&path, b"").unwrap();
        {
            let mut c = mgr.cache.write().await;
            c.insert("sha256:abc".into(), path.to_string_lossy().into_owned());
        }
        assert!(mgr.cache_hit(&spec).await);
        let spec2 = ImageSpec {
            reference: "python:3.11-slim".into(),
            digest: Some("sha256:abc".into()),
        };
        assert!(mgr.cache_hit(&spec2).await);
    }

    #[tokio::test]
    async fn disk_cache_survives_restart() {
        let dir = tempfile::tempdir().unwrap();
        let key = "sha256:disk1";
        let path = dir.path().join("sha256_disk1.ext4");
        std::fs::write(&path, b"").unwrap();

        // 新实例（空内存缓存）应命中磁盘缓存。
        let mgr = ImageManager::new().with_cache_dir(dir.path());
        let spec = ImageSpec {
            reference: "bogus:tag".into(),
            digest: Some(key.into()),
        };
        assert!(mgr.cache_hit(&spec).await);
    }

    #[test]
    fn cache_path_sanitizes_key() {
        let p = cache_path(Path::new("/tmp/c"), "sha256:ab/cd");
        assert_eq!(p, PathBuf::from("/tmp/c/sha256_ab_cd.ext4"));
    }

    #[test]
    fn dir_size_counts_recursively() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a"), b"12345").unwrap();
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/b"), b"123").unwrap();
        assert_eq!(dir_size(dir.path()).unwrap(), 8);
    }

    #[test]
    fn agent_fingerprint_invalidates_rootfs_cache_key() {
        let dir = tempfile::tempdir().unwrap();
        let agent = dir.path().join("clouisle-agent");
        std::fs::write(&agent, b"agent-v1").unwrap();
        let first = ImageManager::new()
            .with_agent_binary(&agent)
            .cache_key_for_image("sha256:image")
            .unwrap();

        std::fs::write(&agent, b"agent-v2").unwrap();
        let second = ImageManager::new()
            .with_agent_binary(&agent)
            .cache_key_for_image("sha256:image")
            .unwrap();

        assert_ne!(first, second);
    }

    #[test]
    fn extract_plain_tar() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        // 构造一个未压缩 tar。
        let tar_path = src.path().join("layer.tar");
        {
            let file = std::fs::File::create(&tar_path).unwrap();
            let mut builder = tar::Builder::new(file);
            let mut header = tar::Header::new_gnu();
            header.set_size(4);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(&mut header, "hello.txt", &b"data"[..])
                .unwrap();
            builder.finish().unwrap();
        }
        extract_tar_archive(
            &tar_path,
            dst.path(),
            "application/vnd.oci.image.layer.v1.tar",
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(dst.path().join("hello.txt")).unwrap(),
            "data"
        );
    }

    /// Multi-arch OCI index resolution plus ext4 construction. This exercises
    /// `pull_and_build` end to end, so using `pull_manifest` instead of
    /// `pull_image_manifest` regresses this test.
    #[tokio::test]
    #[ignore = "requires registry access and Linux root to build ext4"]
    async fn multi_arch_index_resolves_and_builds_rootfs() {
        let cache_dir = tempfile::tempdir().unwrap();
        // 注入一个真实可执行文件作为 agent 二进制（这里用 /bin/true 占位验证管道）。

        let mgr = ImageManager::new()
            .with_cache_dir(cache_dir.path())
            .with_agent_binary("/bin/sh");
        let spec = ImageSpec {
            reference: "alpine:latest".into(),
            digest: None,
        };
        let path = mgr.pull_and_build(&spec).await.unwrap();
        assert!(path.ends_with(".ext4"));
        assert!(Path::new(&path).is_file(), "ext4 file exists: {path}");

        // 第二次调用应命中缓存（无 pinned digest 时也会重新拉 manifest 但跳过构建）。
        let path2 = mgr.pull_and_build(&spec).await.unwrap();
        assert_eq!(path, path2);
    }
    #[cfg(unix)]
    #[test]
    fn dir_size_does_not_follow_directory_symlinks() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("payload"), b"abc").unwrap();
        symlink(".", root.path().join("loop")).unwrap();

        // The symlink itself is counted; its directory target is not recursed.
        assert_eq!(dir_size(root.path()).unwrap(), 4);
    }
}
