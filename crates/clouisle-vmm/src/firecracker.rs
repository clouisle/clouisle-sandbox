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
use http_body_util::{BodyExt, Full};
use hyper::{Request, body::Incoming};
use hyper_util::client::legacy::Client;
use hyperlocal::{UnixClientExt, UnixConnector, Uri as UnixUri};
use serde::Serialize;
use serde_json::Value;
use tokio::sync::Mutex;
use tracing::{info, warn};

use clouisle_core::{ClouisleError, ErrorKind, Result, SandboxSpec};

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
#[derive(Debug, Clone)]
pub struct FirecrackerVmm {
    config: FirecrackerConfig,
    vms: Arc<Mutex<HashMap<String, FcProcess>>>,
}

impl FirecrackerVmm {
    pub fn new(config: FirecrackerConfig) -> Self {
        Self {
            config,
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
    fn rootfs_path(&self, spec: &SandboxSpec) -> PathBuf {
        let key = spec
            .image
            .digest
            .as_deref()
            .unwrap_or(&spec.image.reference);
        // 替换 / 和 : 为 _ 以免路径冲突
        let safe = key.replace('/', "_").replace(':', "_");
        self.config.images_dir.join(format!("{safe}.ext4"))
    }

    /// 内核命令行参数，从 spec 的 env 中提取 `boot_args` 或使用默认值。
    /// 默认值包含 rootfs 挂载 + guest 静态 IP（与 clouisle-net netns 网段一致）。
    fn boot_args(&self, sandbox_id: &str, spec: &SandboxSpec) -> String {
        let base = spec.env.get("boot_args").cloned().unwrap_or_else(|| {
            "console=ttyS0 reboot=k panic=1 pci=off root=/dev/vda rw".to_string()
        });
        // 追加 guest IP 配置（10.{a}.{b}.2/30，网关 10.{a}.{b}.1）
        let (a, b) = Self::sandbox_subnet(sandbox_id);
        format!(
            "{base} ip=10.{a}.{b}.2::10.{a}.{b}.1:255.255.255.252::eth0:off \
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
        self.check_environment()?;

        let id = sandbox_id.to_string();
        let sock_path = self.config.api_sock_dir.join(format!("{id}.sock"));
        std::fs::create_dir_all(&self.config.api_sock_dir)
            .map_err(|e| ClouisleError::io(e.to_string()))?;

        // 启动 firecracker 进程（在沙盒 netns 内运行，新进程组便于 kill）
        let ns_name = format!("clo-{}", short_name(&id, ""));
        let mut cmd = tokio::process::Command::new("ip");
        cmd.arg("netns").arg("exec").arg(&ns_name);
        cmd.arg(&self.config.firecracker_bin);
        cmd.arg("--api-sock").arg(&sock_path);
        if !self.config.enable_seccomp {
            cmd.arg("--no-seccomp");
        }
        cmd.stdin(std::process::Stdio::null());
        // 创建新进程组，使 firecracker 及其子进程在同一组
        cmd.process_group(0);

        let child = cmd.spawn().map_err(|e| {
            ClouisleError::new(
                ErrorKind::Vmm,
                format!("spawn firecracker in ns {ns_name}: {e}"),
            )
        })?;

        let pid = child.id().map(|p| p as u64);
        let cid = self.next_cid();
        let vsock_path = format!("/tmp/clouisle-{id}.vsock");
        // TAP 设备在 netns 内，Firecracker 直连 tap0
        let host_dev = "tap0";

        let handle = VmHandle {
            id: id.clone(),
            backend: "firecracker".into(),
            pid,
            api_socket: Some(sock_path.to_string_lossy().into_owned()),
            vsock_socket: Some(vsock_path.clone()),
            vsock_cid: Some(cid),
        };

        let mut vms = self.vms.lock().await;
        vms.insert(
            id.clone(),
            FcProcess {
                handle: handle.clone(),
                child: Some(child),
            },
        );
        drop(vms);

        // 等待 API socket 就绪
        self.wait_for_socket(&sock_path).await?;

        // 1. 配置机器规格
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

        // 2. 配置启动源
        #[derive(Serialize)]
        struct BootSource<'a> {
            kernel_image_path: &'a str,
            boot_args: &'a str,
        }
        let kernel_path = self.config.kernel_path.to_string_lossy().into_owned();
        let boot_args = self.boot_args(&id, spec);
        self.fc_put(
            &handle,
            "/boot-source",
            &BootSource {
                kernel_image_path: &kernel_path,
                boot_args: &boot_args,
            },
        )
        .await?;

        // 3. 配置根文件系统
        #[derive(Serialize)]
        struct DriveAdd<'a> {
            drive_id: &'a str,
            path_on_host: &'a str,
            is_root_device: bool,
            is_read_only: bool,
        }
        let rootfs = self.rootfs_path(spec).to_string_lossy().into_owned();
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

        // 4. 配置 vsock
        #[derive(Serialize)]
        struct VsockConfig<'a> {
            guest_cid: u64,
            uds_path: &'a str,
        }
        self.fc_put(
            &handle,
            "/vsock",
            &VsockConfig {
                guest_cid: cid,
                uds_path: &vsock_path,
            },
        )
        .await?;

        // 5. 配置网络接口（如果启用）
        if spec.network.enabled {
            #[derive(Serialize)]
            struct NetIface<'a> {
                iface_id: &'a str,
                host_dev_name: &'a str,
            }
            // 使用与 netns rs 一致的短名作为宿主机 veth 设备名
            self.fc_put(
                &handle,
                "/network-interfaces/eth0",
                &NetIface {
                    iface_id: "eth0",
                    host_dev_name: &host_dev,
                },
            )
            .await?;
        }

        info!(
            id = %id,
            pid = ?pid,
            cid = cid,
            "firecracker VM configured"
        );

        Ok(handle)
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
        struct Action<'a> {
            action_type: &'a str,
        }
        self.fc_post(
            h,
            "/actions",
            &Action {
                action_type: "Pause",
            },
        )
        .await?;
        info!(id = %h.id, "firecracker VM paused");
        Ok(())
    }

    async fn resume(&self, h: &VmHandle) -> Result<()> {
        #[derive(Serialize)]
        struct Action<'a> {
            action_type: &'a str,
        }
        self.fc_post(
            h,
            "/actions",
            &Action {
                action_type: "Resume",
            },
        )
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
        self.fc_post(
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

    async fn restore(&self, _spec: &SandboxSpec, _from: &SnapshotPaths) -> Result<VmHandle> {
        Err(ClouisleError::invalid_state(
            "restore not fully implemented",
        ))
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

        // 总是 kill 进程组（确保 firecracker 及其子进程被清理）
        let mut vms = self.vms.lock().await;
        if let Some(mut proc) = vms.remove(&h.id) {
            if let Some(pid) = proc.handle.pid {
                let pgid = nix::unistd::Pid::from_raw(pid as i32);
                let _ = nix::sys::signal::killpg(pgid, nix::sys::signal::Signal::SIGKILL);
            }
            if let Some(mut child) = proc.child.take() {
                let _ = child.wait().await;
            }
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
        let mut cfg = FirecrackerConfig::default();
        cfg.firecracker_bin = PathBuf::from("/nonexistent/firecracker");
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
        assert!(path.to_string_lossy().contains("alpine:latest"));
        assert!(path.extension().map(|e| e == "ext4").unwrap_or(false));
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
        let args = vmm.boot_args(&spec);
        assert!(args.contains("console=ttyS0"));
    }

    #[test]
    fn boot_args_from_env() {
        let vmm = FirecrackerVmm::new(FirecrackerConfig::default());
        let mut spec = SandboxSpec::default();
        spec.env.insert("boot_args".into(), "custom=1".into());
        let args = vmm.boot_args(&spec);
        assert_eq!(args, "custom=1");
    }
}
