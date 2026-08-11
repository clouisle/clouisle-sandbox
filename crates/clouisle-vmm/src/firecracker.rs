//! FirecrackerVmm: Linux + KVM 生产后端（ADR-004 方案 B）。
//!
//! 通过外部 `firecracker` 进程 + Unix socket HTTP API 集成。
//! 仅 Linux 编译（`#[cfg(target_os = "linux")]`）。
//!
//! 使用 hyper 的 client + hyperlocal 的 UnixConnector 进行 HTTP-over-UDS 通信
//! （reqwest 0.12 不支持 Unix domain socket）。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use http_body_util::Full;
use hyper::{Request, body::Incoming};
use hyper_util::client::legacy::Client;
use hyperlocal::{UnixClientExt, UnixConnector, Uri as UnixUri};
use serde::Serialize;
use serde_json::Value;
use tokio::sync::Mutex;
use tracing::{info, warn};

use clouisle_core::{ClouisleError, ErrorKind, Result, SandboxSpec};
use clouisle_images::ImageSpec;
use clouisle_images::builder::ImageManager;

use crate::{SnapshotKind, SnapshotPaths, StopMode, VmHandle, VmStats, Vmm, VmmCapabilities};

/// 默认 guest CID 起始值（CID 0/1/2 为保留值）。
const MIN_CID: u64 = 3;

/// 用于出站白名单短名（与 clouisle-net/src/netns.rs 一致）。
fn short_name(sandbox_id: &str, prefix: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(sandbox_id.as_bytes());
    let hash = hex::encode(hasher.finalize());
    format!("{prefix}{}", &hash[..8])
}

/// FirecrackerVmm 配置。
#[derive(Debug, Clone)]
pub struct FirecrackerConfig {
    /// firecracker 二进制路径
    pub firecracker_bin: PathBuf,
    /// jailer 二进制路径（可选）
    pub jailer_bin: Option<PathBuf>,
    /// 内核 vmlinux 路径
    pub kernel_path: PathBuf,
    /// API socket 基础目录
    pub api_sock_dir: PathBuf,
    /// 是否使用 jailer（推荐 true，生产）
    pub use_jailer: bool,
    /// 是否启用 seccomp
    pub enable_seccomp: bool,
    /// 已构建的 rootfs 镜像缓存目录
    pub images_dir: PathBuf,
    /// 每沙盒独立 rootfs 副本目录（冷创建时复制，避免共享镜像写互相污染）
    pub rootfs_work_dir: PathBuf,
}

impl Default for FirecrackerConfig {
    fn default() -> Self {
        Self {
            firecracker_bin: PathBuf::from("/usr/local/bin/firecracker"),
            jailer_bin: Some(PathBuf::from("/usr/local/bin/jailer")),
            kernel_path: PathBuf::from("/opt/clouisle/vmlinux"),
            api_sock_dir: PathBuf::from("/tmp/clouisle-fc"),
            use_jailer: true,
            enable_seccomp: true,
            images_dir: PathBuf::from("/tmp/clouisle-cache"),
            rootfs_work_dir: PathBuf::from("/tmp/clouisle-cache/.rootfs"),
        }
    }
}

/// 运行中的 Firecracker 进程。
#[derive(Debug)]
struct FcProcess {
    handle: VmHandle,
    child: Option<tokio::process::Child>,
}

/// Hyper client 类型别名：Unix domain socket 连接器 + JSON body。
type FcClient = Client<UnixConnector, Full<Bytes>>;

/// FirecrackerVmm 后端。
#[derive(Clone)]
pub struct FirecrackerVmm {
    config: FirecrackerConfig,
    image_manager: ImageManager,
    vms: Arc<Mutex<HashMap<String, FcProcess>>>,
}

/// 在宿主 cgroup v2 `io` 控制器上限制 FC 进程对 rootfs 所在设备的
/// IOPS（guest 的 disk IO 经 FC 后端读/写 rootfs 文件，host 侧限制生效）。
/// `None`/0 不限制。清理由 [`remove_io_limit`] 负责。
fn apply_io_limit(
    sandbox_id: &str,
    iops: Option<u32>,
    fc_pid: u64,
    rootfs: &std::path::Path,
) -> Result<()> {
    use std::fs;
    use std::os::unix::fs::MetadataExt;
    let Some(iops) = iops.filter(|i| *i > 0) else {
        return Ok(());
    };
    let base = "/sys/fs/cgroup/clouisle-io";
    let _ = fs::create_dir_all(base);
    let _ = fs::write(format!("{base}/cgroup.subtree_control"), "+io");
    let dir = format!("{base}/{sandbox_id}");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).map_err(|e| ClouisleError::io(format!("create io cgroup: {e}")))?;
    let dev = fs::metadata(rootfs)
        .map_err(|e| ClouisleError::io(format!("stat rootfs {rootfs:?}: {e}")))?
        .dev();
    let major = ((dev >> 8) & 0xfff) | ((dev >> 32) & !0xfff);
    let minor = (dev & 0xff) | ((dev >> 12) & !0xff);
    fs::write(
        format!("{dir}/io.max"),
        format!("{major}:{minor} riops={iops} wiops={iops}"),
    )
    .map_err(|e| ClouisleError::io(format!("write io.max: {e}")))?;
    fs::write(format!("{dir}/cgroup.procs"), fc_pid.to_string())
        .map_err(|e| ClouisleError::io(format!("move fc into io cgroup: {e}")))?;
    tracing::info!(sandbox_id, iops, "applied host io.max limit");
    Ok(())
}

/// 删除沙盒的 io 限制 cgroup（stop 清理）。
fn remove_io_limit(sandbox_id: &str) {
    let dir = format!("/sys/fs/cgroup/clouisle-io/{sandbox_id}");
    let _ = std::fs::remove_dir_all(&dir);
}

impl FirecrackerVmm {
    async fn create_inner(
        &self,
        sandbox_id: &str,
        spec: &SandboxSpec,
        subnet: Option<(u16, u16)>,
    ) -> Result<VmHandle> {
        self.check_environment()?;

        let id = sandbox_id.to_string();
        let (handle, sock_path, child) = self.spawn_firecracker(&id, subnet)?;
        // TAP device is created by FirewallManager before VMM creation.
        let host_dev = "tap0";

        let mut vms = self.vms.lock().await;
        vms.insert(
            id.clone(),
            FcProcess {
                handle: handle.clone(),
                child: Some(child),
            },
        );
        drop(vms);

        let configured: Result<()> = async {
            // 等待 API socket 就绪
            self.wait_for_socket(&sock_path).await?;

            #[derive(Serialize)]
            struct MachineConfig {
                vcpu_count: u16,
                mem_size_mib: u32,
                smt: bool,
                track_dirty_pages: bool,
            }
            self.fc_put(
                &handle,
                "/machine-config",
                &MachineConfig {
                    vcpu_count: spec.resources.vcpu,
                    mem_size_mib: spec.resources.memory_mb,
                    smt: false,
                    track_dirty_pages: false,
                },
            )
            .await?;

            #[derive(Serialize)]
            struct BootSource<'a> {
                kernel_image_path: &'a str,
                boot_args: &'a str,
            }
            let kernel_path = self.config.kernel_path.to_string_lossy().into_owned();
            let boot_args = match subnet {
                Some((a, b)) => self.boot_args_for_subnet(spec, a, b),
                None => self.boot_args(&id, spec),
            };
            self.fc_put(
                &handle,
                "/boot-source",
                &BootSource {
                    kernel_image_path: &kernel_path,
                    boot_args: &boot_args,
                },
            )
            .await?;

            #[derive(Serialize)]
            struct DriveAdd<'a> {
                drive_id: &'a str,
                path_on_host: &'a str,
                is_root_device: bool,
                is_read_only: bool,
            }
            let rootfs = self
                .prepare_rootfs(spec, &id)
                .await?
                .to_string_lossy()
                .into_owned();
            info!(rootfs = %rootfs, "configuring rootfs drive");
            self.fc_put(
                &handle,
                "/drives/rootfs",
                &DriveAdd {
                    drive_id: "rootfs",
                    path_on_host: &rootfs,
                    is_root_device: true,
                    is_read_only: false,
                },
            )
            .await?;
            if let Some(fc_pid) = handle.pid {
                // io.max 需要宿主 cgroup io 控制器暴露对应块设备；受限环境
                // （容器/无 nvme 注册）写入失败时记录并继续，不阻断沙盒。
                if let Err(error) = apply_io_limit(
                    &id,
                    spec.resources.iops,
                    fc_pid,
                    std::path::Path::new(&rootfs),
                ) {
                    tracing::warn!(sandbox_id = %id, %error, "apply host io limit failed");
                }
            }

            // 快照预热（显式子网）不带 vsock：FC 恢复时无法重配 vsock 设备，
            // 快照内固化路径会导致多 clone 绑定冲突；agent 走 TCP 不依赖 vsock。
            if subnet.is_none() {
                #[derive(Serialize)]
                struct VsockConfig<'a> {
                    guest_cid: u64,
                    uds_path: &'a str,
                }
                let cid = handle
                    .vsock_cid
                    .ok_or_else(|| ClouisleError::invalid_state("missing guest CID"))?;
                let vsock_path = handle
                    .vsock_socket
                    .as_deref()
                    .ok_or_else(|| ClouisleError::invalid_state("missing vsock socket path"))?;
                self.fc_put(
                    &handle,
                    "/vsock",
                    &VsockConfig {
                        guest_cid: cid,
                        uds_path: vsock_path,
                    },
                )
                .await?;
            }

            // 管理面 TCP agent 依赖 eth0；network.enabled=false 仍需该接口，
            // 其出站能力由 nftables 以空 allowlist 拒绝。
            #[derive(Serialize)]
            struct NetIface<'a> {
                iface_id: &'a str,
                host_dev_name: &'a str,
            }
            self.fc_put(
                &handle,
                "/network-interfaces/eth0",
                &NetIface {
                    iface_id: "eth0",
                    host_dev_name: host_dev,
                },
            )
            .await?;
            Ok(())
        }
        .await;

        if let Err(error) = configured {
            let _ = self.stop(&handle, StopMode::Force).await;
            return Err(error);
        }

        info!(id = %id, pid = ?handle.pid, cid = ?handle.vsock_cid, "firecracker VM configured");
        Ok(handle)
    }

    pub fn new(config: FirecrackerConfig) -> Self {
        let image_manager = ImageManager::new()
            .with_cache_dir(config.images_dir.clone())
            .with_agent_binary("/usr/local/bin/clouisle-agent");
        Self {
            config,
            image_manager,
            vms: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// 检查 KVM 与二进制可用性。
    pub fn check_environment(&self) -> Result<()> {
        if !self.config.firecracker_bin.exists() {
            return Err(ClouisleError::new(
                ErrorKind::Vmm,
                format!(
                    "firecracker not found at {}",
                    self.config.firecracker_bin.display()
                ),
            ));
        }
        if self.config.use_jailer
            && let Some(j) = &self.config.jailer_bin
            && !j.exists()
        {
            return Err(ClouisleError::new(
                ErrorKind::Vmm,
                format!("jailer not found at {}", j.display()),
            ));
        }
        if !std::path::Path::new("/dev/kvm").exists() {
            return Err(ClouisleError::new(
                ErrorKind::Vmm,
                "no /dev/kvm; please join the kvm group or run on a KVM-capable host",
            ));
        }
        Ok(())
    }

    // ── HTTP-over-UDS 辅助方法 ──────────────────────────────────────────

    /// 构建一个指向 `handle.api_socket` 的 `UnixUri`（hyperlocal URI）。
    fn api_uri(&self, handle: &VmHandle, path: &str) -> Result<UnixUri> {
        let sock = handle
            .api_socket
            .as_ref()
            .ok_or_else(|| ClouisleError::invalid_state("VmHandle missing api_socket"))?;
        Ok(UnixUri::new(sock, path))
    }

    /// 返回一个 hyper + UnixConnector 的 HTTP 客户端。
    fn fc_client(&self) -> FcClient {
        Client::unix()
    }

    /// 等待 API socket 文件就绪（指数退避，最多约 10s）。
    async fn wait_for_socket(&self, sock_path: &Path) -> Result<()> {
        let mut delay = Duration::from_millis(50);
        for _ in 0..14 {
            if sock_path.exists() {
                return Ok(());
            }
            tokio::time::sleep(delay).await;
            delay = (delay * 2).min(Duration::from_secs(2));
        }
        Err(ClouisleError::new(
            ErrorKind::Vmm,
            format!(
                "firecracker API socket not ready at {}",
                sock_path.display()
            ),
        ))
    }

    /// 构建 rootfs 路径：`{images_dir}/{image_reference_sanitized}.ext4`
    #[cfg(test)]
    fn rootfs_path(&self, spec: &SandboxSpec) -> PathBuf {
        let key = spec
            .image
            .digest
            .as_deref()
            .unwrap_or(&spec.image.reference);
        // 替换 / 和 : 为 _ 以免路径冲突
        let safe = key.replace(['/', ':'], "_");
        self.config.images_dir.join(format!("{safe}.ext4"))
    }

    /// 解析共享镜像缓存并复制为每沙盒独立副本（冷创建路径）。
    ///
    /// 所有沙盒直接共享同一个 ext4 镜像文件时，一个沙盒的写入（大输出、安装
    /// 依赖、文件操作）会持久化到共享镜像并填满磁盘，导致其他沙盒
    /// `write_file` ENOSPC——多租户数据面不隔离。这里为每个沙盒复制一份，
    /// 写入互不影响；副本随 `stop` 清理。快照 restore 路径不经过本函数
    /// （FC 快照固化 drive 路径，属 FC clone 已知限制，另行记录）。
    async fn prepare_rootfs(&self, spec: &SandboxSpec, sandbox_id: &str) -> Result<PathBuf> {
        let cached = self.resolve_rootfs(spec).await?;
        let work = self.config.rootfs_work_dir.clone();
        tokio::fs::create_dir_all(&work).await.map_err(|error| {
            ClouisleError::io(format!(
                "create rootfs work dir {}: {error}",
                work.display()
            ))
        })?;
        let target = work.join(format!("{sandbox_id}.ext4"));
        tokio::fs::copy(&cached, &target).await.map_err(|error| {
            ClouisleError::io(format!(
                "copy rootfs {} -> {}: {error}",
                cached.display(),
                target.display()
            ))
        })?;
        Ok(target)
    }

    /// Resolve an image reference to a local ext4 rootfs. Existing managed
    /// cache entries remain usable for offline operation; cache misses are
    /// built from OCI and inject the guest agent before Firecracker starts.
    async fn resolve_rootfs(&self, spec: &SandboxSpec) -> Result<PathBuf> {
        let image = ImageSpec {
            reference: spec.image.reference.clone(),
            digest: spec.image.digest.clone(),
        };
        self.image_manager
            .pull_and_build(&image)
            .await
            .map(PathBuf::from)
            .map_err(|error| {
                ClouisleError::new(
                    ErrorKind::Vmm,
                    format!("resolve OCI rootfs {}: {error}", spec.image.reference),
                )
            })
    }

    /// 内核命令行参数，从 spec 的 env 中提取 `boot_args` 或使用默认值。
    /// 默认值包含 rootfs 挂载 + guest 静态 IP（与 clouisle-net netns 网段一致）。
    fn boot_args(&self, sandbox_id: &str, spec: &SandboxSpec) -> String {
        let (a, b) = Self::sandbox_subnet(sandbox_id);
        self.boot_args_for_subnet(spec, a, b)
    }

    /// 用显式子网构造内核 cmdline（快照预热路径）。
    fn boot_args_for_subnet(&self, spec: &SandboxSpec, a: u16, b: u16) -> String {
        let base = spec.env.get("boot_args").cloned().unwrap_or_else(|| {
            "console=ttyS0 reboot=k panic=1 pci=off root=/dev/vda rw".to_string()
        });
        // OCI application images usually lack a distro init. The injected,
        // statically linked guest agent is therefore the portable PID 1.
        let init = if base.split_whitespace().any(|arg| arg.starts_with("init=")) {
            ""
        } else {
            " init=/usr/local/bin/clouisle-agent"
        };
        // Append guest static IP configuration (10.{a}.{b}.2/30, gateway .1).
        format!(
            "{base}{init} ip=10.{a}.{b}.2::10.{a}.{b}.1:255.255.255.252::eth0:off \
             clouisle.guest_ip=10.{a}.{b}.2 clouisle.gateway=10.{a}.{b}.1"
        )
    }

    /// 从 sandbox_id 派生独立网段（与 clouisle-net/src/netns.rs 算法一致）。
    fn sandbox_subnet(sandbox_id: &str) -> (u16, u16) {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(sandbox_id.as_bytes());
        let digest = hasher.finalize();
        let a = 10 + (digest[0] as u16 % 200);
        let b = 10 + (digest[1] as u16 % 200);
        (a, b)
    }

    /// 生成一个可用的 guest CID（≥ 3，当前时间低位 + 随机）。
    fn next_cid(&self) -> u64 {
        use std::time::{SystemTime, UNIX_EPOCH};
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        MIN_CID + (ts & 0xFFFF) // 0–65535 偏移，碰撞概率极低
    }

    // ── 底层 HTTP 方法 ──────────────────────────────────────────────────

    /// PUT JSON 请求到 Firecracker API。
    async fn fc_put<T: Serialize>(&self, handle: &VmHandle, path: &str, body: &T) -> Result<()> {
        let client = self.fc_client();
        let uri = self.api_uri(handle, path)?;
        let json = serde_json::to_vec(body)
            .map_err(|e| ClouisleError::new(ErrorKind::Vmm, format!("serialize: {e}")))?;
        let req = Request::put(hyper::Uri::from(uri))
            .header("Content-Type", "application/json")
            .body(Full::new(Bytes::from(json)))
            .map_err(|e| ClouisleError::new(ErrorKind::Vmm, format!("build request: {e}")))?;
        let resp = client.request(req).await.map_err(|e| {
            ClouisleError::new(ErrorKind::Vmm, format!("firecracker PUT {path}: {e}"))
        })?;
        let status = resp.status();
        if !status.is_success() {
            let body = read_body(resp).await;
            return Err(ClouisleError::new(
                ErrorKind::Vmm,
                format!("firecracker PUT {path} => {status}: {body}"),
            ));
        }
        Ok(())
    }

    /// POST JSON 请求到 Firecracker API。
    async fn fc_post<T: Serialize>(&self, handle: &VmHandle, path: &str, body: &T) -> Result<()> {
        let client = self.fc_client();
        let uri = self.api_uri(handle, path)?;
        let json = serde_json::to_vec(body)
            .map_err(|e| ClouisleError::new(ErrorKind::Vmm, format!("serialize: {e}")))?;
        let req = Request::post(hyper::Uri::from(uri))
            .header("Content-Type", "application/json")
            .body(Full::new(Bytes::from(json)))
            .map_err(|e| ClouisleError::new(ErrorKind::Vmm, format!("build request: {e}")))?;
        let resp = tokio::time::timeout(Duration::from_secs(30), client.request(req))
            .await
            .map_err(|_| {
                ClouisleError::new(ErrorKind::Vmm, format!("firecracker POST {path} timed out"))
            })?
            .map_err(|e| {
                ClouisleError::new(ErrorKind::Vmm, format!("firecracker POST {path}: {e}"))
            })?;
        let status = resp.status();
        if !status.is_success() {
            let body = read_body(resp).await;
            return Err(ClouisleError::new(
                ErrorKind::Vmm,
                format!("firecracker POST {path} => {status}: {body}"),
            ));
        }
        Ok(())
    }

    /// PATCH JSON 请求到 Firecracker API。
    async fn fc_patch<T: Serialize>(&self, handle: &VmHandle, path: &str, body: &T) -> Result<()> {
        let client = self.fc_client();
        let uri = self.api_uri(handle, path)?;
        let json = serde_json::to_vec(body)
            .map_err(|e| ClouisleError::new(ErrorKind::Vmm, format!("serialize: {e}")))?;
        let req = Request::patch(hyper::Uri::from(uri))
            .header("Content-Type", "application/json")
            .body(Full::new(Bytes::from(json)))
            .map_err(|e| ClouisleError::new(ErrorKind::Vmm, format!("build request: {e}")))?;
        let resp = tokio::time::timeout(Duration::from_secs(30), client.request(req))
            .await
            .map_err(|_| {
                ClouisleError::new(
                    ErrorKind::Vmm,
                    format!("firecracker PATCH {path} timed out"),
                )
            })?
            .map_err(|e| {
                ClouisleError::new(ErrorKind::Vmm, format!("firecracker PATCH {path}: {e}"))
            })?;
        let status = resp.status();
        if !status.is_success() {
            let body = read_body(resp).await;
            return Err(ClouisleError::new(
                ErrorKind::Vmm,
                format!("firecracker PATCH {path} => {status}: {body}"),
            ));
        }
        Ok(())
    }

    /// GET 请求并返回 JSON 值。
    async fn fc_get(&self, handle: &VmHandle, path: &str) -> Result<Value> {
        let client = self.fc_client();
        let uri = self.api_uri(handle, path)?;
        let req = Request::get(hyper::Uri::from(uri))
            .body(Full::new(Bytes::new()))
            .map_err(|e| ClouisleError::new(ErrorKind::Vmm, format!("build request: {e}")))?;
        let resp = client.request(req).await.map_err(|e| {
            ClouisleError::new(ErrorKind::Vmm, format!("firecracker GET {path}: {e}"))
        })?;
        let status = resp.status();
        if !status.is_success() {
            let body = read_body(resp).await;
            return Err(ClouisleError::new(
                ErrorKind::Vmm,
                format!("firecracker GET {path} => {status}: {body}"),
            ));
        }
        let body = read_body(resp).await;
        serde_json::from_str(&body)
            .map_err(|e| ClouisleError::new(ErrorKind::Vmm, format!("parse JSON: {e}")))
    }

    /// Spawn Firecracker inside the sandbox network namespace and retain its
    /// process-group handle for deterministic teardown.
    fn spawn_firecracker(
        &self,
        sandbox_id: &str,
        subnet: Option<(u16, u16)>,
    ) -> Result<(VmHandle, PathBuf, tokio::process::Child)> {
        // 残留的 vsock socket 会让 FC 绑定失败（Address in use）。
        let _ = std::fs::remove_file(format!("/tmp/clouisle-{sandbox_id}.vsock"));
        let id = sandbox_id.to_string();
        let sock_path = self.config.api_sock_dir.join(format!("{id}.sock"));
        std::fs::create_dir_all(&self.config.api_sock_dir)
            .map_err(|e| ClouisleError::io(e.to_string()))?;
        if sock_path.exists() {
            std::fs::remove_file(&sock_path).map_err(|e| {
                ClouisleError::io(format!(
                    "remove stale API socket {}: {e}",
                    sock_path.display()
                ))
            })?;
        }

        let ns_name = format!("clo-{}", short_name(&id, ""));
        let mut cmd = tokio::process::Command::new("ip");
        cmd.arg("netns").arg("exec").arg(&ns_name);
        cmd.arg(&self.config.firecracker_bin);
        cmd.arg("--api-sock").arg(&sock_path);
        if !self.config.enable_seccomp {
            cmd.arg("--no-seccomp");
        }
        cmd.stdin(std::process::Stdio::null());
        cmd.process_group(0);
        let child = cmd.spawn().map_err(|e| {
            ClouisleError::new(
                ErrorKind::Vmm,
                format!("spawn firecracker in ns {ns_name}: {e}"),
            )
        })?;
        let handle = VmHandle {
            id,
            backend: "firecracker".into(),
            owner_id: None,
            pid: child.id().map(|pid| pid as u64),
            api_socket: Some(sock_path.to_string_lossy().into_owned()),
            vsock_socket: Some(format!("/tmp/clouisle-{sandbox_id}.vsock")),
            vsock_cid: Some(self.next_cid()),
            subnet,
        };
        Ok((handle, sock_path, child))
    }
}

/// 读取 hyper 响应体到字符串。
async fn read_body(resp: hyper::Response<Incoming>) -> String {
    use http_body_util::BodyExt;
    match resp.into_body().collect().await {
        Ok(collected) => {
            let bytes = collected.to_bytes();
            String::from_utf8_lossy(&bytes).to_string()
        }
        Err(e) => format!("<read error: {e}>"),
    }
}

#[async_trait]
impl Vmm for FirecrackerVmm {
    async fn create(&self, sandbox_id: &str, spec: &SandboxSpec) -> Result<VmHandle> {
        self.create_inner(sandbox_id, spec, None).await
    }

    async fn create_in_subnet(
        &self,
        sandbox_id: &str,
        spec: &SandboxSpec,
        subnet: (u16, u16),
    ) -> Result<VmHandle> {
        self.create_inner(sandbox_id, spec, Some(subnet)).await
    }

    async fn image_cache_hit(&self, spec: &SandboxSpec) -> Result<bool> {
        let image = ImageSpec {
            reference: spec.image.reference.clone(),
            digest: spec.image.digest.clone(),
        };
        Ok(self.image_manager.cache_hit(&image).await)
    }

    async fn prefetch_image(&self, spec: &SandboxSpec) -> Result<()> {
        let image = ImageSpec {
            reference: spec.image.reference.clone(),
            digest: spec.image.digest.clone(),
        };
        self.image_manager
            .pull_and_build(&image)
            .await
            .map(|_| ())
            .map_err(|error| {
                ClouisleError::new(
                    ErrorKind::Vmm,
                    format!("prefetch OCI rootfs {}: {error}", spec.image.reference),
                )
            })
    }

    async fn probe(&self, handle: &VmHandle) -> Result<bool> {
        if let Some(pid) = handle.pid {
            use nix::sys::signal::kill;
            use nix::unistd::Pid;
            if kill(Pid::from_raw(pid as i32), None).is_err() {
                return Ok(false);
            }
        }
        let Some(socket) = handle.api_socket.as_deref() else {
            return Ok(false);
        };
        if !Path::new(socket).exists() {
            return Ok(false);
        }
        Ok(self.fc_get(handle, "/vm/config").await.is_ok())
    }

    async fn discover(&self) -> Result<Vec<VmHandle>> {
        let socket_dir = self.config.api_sock_dir.clone();
        let candidates = tokio::task::spawn_blocking(move || {
            let mut handles = Vec::new();
            let Ok(entries) = std::fs::read_dir(&socket_dir) else {
                return handles;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|value| value.to_str()) != Some("sock") {
                    continue;
                }
                let Some(id) = path.file_stem().and_then(|value| value.to_str()) else {
                    continue;
                };
                let pid = std::fs::read_dir("/proc").ok().and_then(|processes| {
                    processes.flatten().find_map(|process| {
                        let name = process.file_name().to_string_lossy().parse::<u64>().ok()?;
                        let cmdline = std::fs::read(process.path().join("cmdline")).ok()?;
                        let args = cmdline
                            .split(|byte| *byte == 0)
                            .filter(|arg| !arg.is_empty())
                            .collect::<Vec<_>>();
                        args.windows(2)
                            .any(|pair| {
                                pair[0] == b"--api-sock"
                                    && pair[1] == path.as_os_str().as_encoded_bytes()
                            })
                            .then_some(name)
                    })
                });
                handles.push(VmHandle {
                    id: id.to_string(),
                    backend: "firecracker".into(),
                    owner_id: None,
                    pid,
                    api_socket: Some(path.to_string_lossy().into_owned()),
                    vsock_socket: Some(format!("/tmp/clouisle-{id}.vsock")),
                    vsock_cid: None,
                    subnet: None,
                });
            }
            handles
        })
        .await
        .map_err(|error| ClouisleError::io(format!("discover Firecracker runtimes: {error}")))?;
        let mut live = Vec::new();
        for handle in candidates {
            if self.probe(&handle).await.unwrap_or(false) {
                live.push(handle);
            }
        }
        Ok(live)
    }

    async fn start(&self, h: &VmHandle) -> Result<()> {
        #[derive(Serialize)]
        struct Action<'a> {
            action_type: &'a str,
        }
        self.fc_put(
            h,
            "/actions",
            &Action {
                action_type: "InstanceStart",
            },
        )
        .await?;
        info!(id = %h.id, "firecracker VM started");
        Ok(())
    }

    async fn pause(&self, h: &VmHandle) -> Result<()> {
        #[derive(Serialize)]
        struct VmStatePatch<'a> {
            state: &'a str,
        }
        self.fc_patch(h, "/vm", &VmStatePatch { state: "Paused" })
            .await?;
        info!(id = %h.id, "firecracker VM paused");
        Ok(())
    }
    async fn resume(&self, h: &VmHandle) -> Result<()> {
        #[derive(Serialize)]
        struct VmStatePatch<'a> {
            state: &'a str,
        }
        self.fc_patch(h, "/vm", &VmStatePatch { state: "Resumed" })
            .await?;
        info!(id = %h.id, "firecracker VM resumed");
        Ok(())
    }

    async fn snapshot(&self, h: &VmHandle, kind: SnapshotKind, out: &SnapshotPaths) -> Result<()> {
        let snapshot_type = match kind {
            SnapshotKind::Full => "Full",
            SnapshotKind::Diff => "Diff",
        };
        #[derive(Serialize)]
        struct SnapshotReq<'a> {
            snapshot_type: &'a str,
            mem_file_path: &'a str,
            snapshot_path: &'a str,
        }
        self.fc_put(
            h,
            "/snapshot/create",
            &SnapshotReq {
                snapshot_type,
                mem_file_path: &out.mem_path,
                snapshot_path: &out.state_path,
            },
        )
        .await?;
        info!(id = %h.id, "firecracker snapshot created");
        Ok(())
    }
    async fn restore(
        &self,
        sandbox_id: &str,
        _spec: &SandboxSpec,
        from: &SnapshotPaths,
    ) -> Result<VmHandle> {
        for path in [&from.state_path, &from.mem_path] {
            if !Path::new(path).is_file() {
                return Err(ClouisleError::new(
                    ErrorKind::NotFound,
                    format!("snapshot file not found: {path}"),
                ));
            }
        }
        self.check_environment()?;

        let (handle, socket, child) = self.spawn_firecracker(sandbox_id, None)?;
        self.vms.lock().await.insert(
            handle.id.clone(),
            FcProcess {
                handle: handle.clone(),
                child: Some(child),
            },
        );

        #[derive(Serialize)]
        struct SnapshotLoad<'a> {
            mem_file_path: &'a str,
            snapshot_path: &'a str,
            enable_diff_snapshots: bool,
            resume_vm: bool,
        }
        let loaded = async {
            self.wait_for_socket(&socket).await?;
            // FC v1.10 不允许在 load 前配置任何设备（vsock/network 均触发
            // boot_path）；恢复时 FC 用快照内设备配置——network 依赖当前
            // netns 中的同名 tap0（官方 clone 方案），预热快照不含 vsock。
            self.fc_put(
                &handle,
                "/snapshot/load",
                &SnapshotLoad {
                    mem_file_path: &from.mem_path,
                    snapshot_path: &from.state_path,
                    enable_diff_snapshots: false,
                    resume_vm: true,
                },
            )
            .await
        }
        .await;
        if let Err(error) = loaded {
            let _ = self.stop(&handle, StopMode::Force).await;
            return Err(error);
        }
        info!(id = %handle.id, "firecracker snapshot restored");
        Ok(handle)
    }

    async fn stop(&self, h: &VmHandle, mode: StopMode) -> Result<()> {
        match mode {
            StopMode::Graceful => {
                // 先尝试 ACPI 关机
                #[derive(Serialize)]
                struct Action<'a> {
                    action_type: &'a str,
                }
                let result = self
                    .fc_post(
                        h,
                        "/actions",
                        &Action {
                            action_type: "SendCtrlAltDel",
                        },
                    )
                    .await;
                if let Err(e) = result {
                    warn!(
                        id = %h.id,
                        error = %e,
                        "SendCtrlAltDel failed, falling back to force stop"
                    );
                } else {
                    info!(id = %h.id, "SendCtrlAltDel sent");
                    // 给 Guest 一个短暂的时间关闭
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
            }
            StopMode::Force => {
                // 不尝试优雅关闭，直接杀进程组
            }
        }

        // The current process owns a VM if it created it. After an API
        // restart, durable VMM metadata still carries the Firecracker process
        // group leader, so stop that group directly instead of leaking it.
        let mut vms = self.vms.lock().await;
        let mut process_group = None;
        let mut child = None;
        if let Some(mut proc) = vms.remove(&h.id) {
            process_group = proc.handle.pid;
            child = proc.child.take();
        }
        drop(vms);
        if let Some(pid) = process_group.or(h.pid) {
            let pgid = nix::unistd::Pid::from_raw(pid as i32);
            if let Err(error) = nix::sys::signal::killpg(pgid, nix::sys::signal::Signal::SIGKILL) {
                warn!(id = %h.id, pid, %error, "failed to kill Firecracker process group");
            }
        }
        if let Some(mut child) = child {
            // `ip netns exec` may be a distinct supervisor. Kill its direct
            // child as well so a failed group signal cannot orphan the VM.
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
        for socket in [h.api_socket.as_deref(), h.vsock_socket.as_deref()]
            .into_iter()
            .flatten()
        {
            if let Err(error) = std::fs::remove_file(socket)
                && error.kind() != std::io::ErrorKind::NotFound
            {
                warn!(id = %h.id, socket, %error, "failed to remove VM socket");
            }
        }
        // 清理每沙盒独立 rootfs 副本与 io 限制 cgroup。
        remove_io_limit(&h.id);
        let rootfs = self.config.rootfs_work_dir.join(format!("{}.ext4", h.id));
        if let Err(error) = std::fs::remove_file(&rootfs)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            warn!(id = %h.id, %error, "failed to remove per-sandbox rootfs");
        }
        info!(id = %h.id, "firecracker VM stopped");
        Ok(())
    }

    async fn stats(&self, h: &VmHandle) -> Result<VmStats> {
        let config = self.fc_get(h, "/vm/config").await?;
        let mem_used_mb = config.get("mem_size_mib").and_then(|v| v.as_u64());

        Ok(VmStats {
            boot_time_us: None,
            vcpu_usage: None,
            mem_used_mb,
            rx_bytes: None,
            tx_bytes: None,
        })
    }

    fn supports_detached_warm_pool(&self) -> bool {
        // Firecracker is launched inside an ID-derived netns and its guest IP
        // is part of the boot command line, so a pre-created VM cannot safely
        // be reassigned to another sandbox identity.
        false
    }

    fn capabilities(&self) -> VmmCapabilities {
        VmmCapabilities {
            snapshot: true,
            vsock: true,
            balloon: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_env_missing_firecracker() {
        // 指向不存在的二进制路径，验证 check_environment 报错
        let cfg = FirecrackerConfig {
            firecracker_bin: PathBuf::from("/nonexistent/firecracker"),
            ..FirecrackerConfig::default()
        };
        let vmm = FirecrackerVmm::new(cfg);
        let err = vmm.check_environment().unwrap_err();
        assert_eq!(err.kind, ErrorKind::Vmm);
    }

    #[test]
    fn capabilities() {
        let vmm = FirecrackerVmm::new(FirecrackerConfig::default());
        assert!(vmm.capabilities().snapshot);
        assert!(vmm.capabilities().vsock);
    }

    #[test]
    fn short_name_length() {
        let name = short_name("test-sandbox-id", "vh");
        assert!(name.len() <= 15, "short_name too long: {name}");
        assert!(name.starts_with("vh"));
    }

    #[test]
    fn rootfs_path_default() {
        let cfg = FirecrackerConfig::default();
        let vmm = FirecrackerVmm::new(cfg);
        let mut spec = SandboxSpec::default();
        spec.image.reference = "docker.io/library/alpine:latest".into();
        let path = vmm.rootfs_path(&spec);
        assert!(path.to_string_lossy().contains("alpine_latest"));
        assert_eq!(
            path.extension().and_then(|extension| extension.to_str()),
            Some("ext4")
        );
    }
    #[test]
    fn next_cid_in_range() {
        let vmm = FirecrackerVmm::new(FirecrackerConfig::default());
        let cid = vmm.next_cid();
        assert!(cid >= 3, "CID {cid} too low");
    }

    #[test]
    fn boot_args_default() {
        let vmm = FirecrackerVmm::new(FirecrackerConfig::default());
        let spec = SandboxSpec::default();
        let args = vmm.boot_args("sandbox-1", &spec);
        assert!(args.contains("console=ttyS0"));
        assert!(args.contains("init=/usr/local/bin/clouisle-agent"));
        assert!(args.contains("clouisle.guest_ip="));
    }

    #[test]
    fn boot_args_from_env() {
        let vmm = FirecrackerVmm::new(FirecrackerConfig::default());
        let mut spec = SandboxSpec::default();
        spec.env.insert("boot_args".into(), "custom=1".into());
        let args = vmm.boot_args("sandbox-1", &spec);
        assert!(args.starts_with("custom=1 "));
    }

    #[test]
    fn boot_args_preserve_explicit_init() {
        let vmm = FirecrackerVmm::new(FirecrackerConfig::default());
        let mut spec = SandboxSpec::default();
        spec.env
            .insert("boot_args".into(), "init=/custom-init custom=1".into());
        let args = vmm.boot_args("sandbox-1", &spec);
        assert!(args.contains("init=/custom-init"));
        assert!(!args.contains("init=/usr/local/bin/clouisle-agent"));
    }

    #[tokio::test]
    async fn restore_rejects_missing_snapshot_before_spawning() {
        let vmm = FirecrackerVmm::new(FirecrackerConfig::default());
        let missing = SnapshotPaths {
            state_path: "/missing/state.snap".into(),
            mem_path: "/missing/memory.snap".into(),
        };
        let err = vmm
            .restore("sandbox-1", &SandboxSpec::default(), &missing)
            .await
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::NotFound);
    }

    #[tokio::test]
    async fn stop_removes_socket_artifacts_without_live_process() {
        let api_socket =
            std::env::temp_dir().join(format!("clouisle-test-{}.sock", uuid::Uuid::now_v7()));
        let vsock_socket =
            std::env::temp_dir().join(format!("clouisle-test-{}.vsock", uuid::Uuid::now_v7()));
        std::fs::write(&api_socket, "test").unwrap();
        std::fs::write(&vsock_socket, "test").unwrap();
        let vmm = FirecrackerVmm::new(FirecrackerConfig::default());
        vmm.stop(
            &VmHandle {
                id: "cleanup-test".into(),
                backend: "firecracker".into(),
                owner_id: None,
                pid: None,
                api_socket: Some(api_socket.to_string_lossy().into_owned()),
                vsock_socket: Some(vsock_socket.to_string_lossy().into_owned()),
                vsock_cid: None,
                subnet: None,
            },
            StopMode::Force,
        )
        .await
        .unwrap();
        assert!(!api_socket.exists());
        assert!(!vsock_socket.exists());
    }
}
