//! Guest PID 1 初始化（Stage 0.5）。
//!
//! Phase 0 简化：真正的 mount/pivot_root 在 Linux 上做；
//! 此处提供可测试的引导参数解析与 `fail_at` 故障注入逻辑。

/// 从内核 cmdline 解析 `clouisle.` 前缀参数。
/// 例：`clouisle.vsock_port=52 clouisle.fail_at=overlay`
pub fn parse_cmdline(cmdline: &str) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    for token in cmdline.split_whitespace() {
        if let Some(rest) = token.strip_prefix("clouisle.") {
            if let Some((k, v)) = rest.split_once('=') {
                out.insert(k.to_string(), v.to_string());
            } else {
                out.insert(rest.to_string(), "true".to_string());
            }
        }
    }
    out
}

/// 引导流程入口（返回 true = 应继续 serve）。
/// `fail_at` 用于负向测试：在指定阶段模拟失败。
pub fn init_boot(cmdline: &str) -> Result<(), String> {
    let params = parse_cmdline(cmdline);

    if let Some(fail) = params.get("fail_at")
        && fail == "overlay"
    {
        return Err("clouisle-init: overlay mount failed (injected)".into());
    }

    let log_level = params
        .get("log_level")
        .cloned()
        .unwrap_or_else(|| "info".into());
    let _ = log_level;
    Ok(())
}

/// 读取当前内核 cmdline（/proc/cmdline）。
pub fn read_cmdline() -> String {
    std::fs::read_to_string("/proc/cmdline").unwrap_or_default()
}

/// Configure the guest's static TCP management network without relying on
/// distro utilities such as `ip` or `ifconfig`, which OCI application images
/// commonly omit.
#[cfg(target_os = "linux")]
pub async fn configure_network() -> Result<(), String> {
    use std::net::{IpAddr, Ipv4Addr};

    ensure_dev_mounted()?;
    ensure_proc_mounted()?;
    ensure_sys_mounted()?;
    let params = parse_cmdline(&read_cmdline());
    let guest_ip = params
        .get("guest_ip")
        .ok_or_else(|| "clouisle.guest_ip not set in cmdline".to_string())?
        .parse::<Ipv4Addr>()
        .map_err(|error| format!("invalid clouisle.guest_ip: {error}"))?;
    let gateway = params
        .get("gateway")
        .ok_or_else(|| "clouisle.gateway not set in cmdline".to_string())?
        .parse::<Ipv4Addr>()
        .map_err(|error| format!("invalid clouisle.gateway: {error}"))?;

    let _ = std::fs::remove_file("/etc/resolv.conf");
    if let Err(error) = std::fs::write("/etc/resolv.conf", format!("nameserver {gateway}\n")) {
        tracing::warn!(%error, "failed to configure guest resolv.conf");
    }

    let (connection, handle, _) =
        rtnetlink::new_connection().map_err(|error| format!("open netlink: {error}"))?;
    tokio::spawn(connection);

    use futures::TryStreamExt;
    let link = handle
        .link()
        .get()
        .match_name("eth0".into())
        .execute()
        .try_next()
        .await
        .map_err(|error| format!("find eth0: {error}"))?
        .ok_or_else(|| "eth0 not found".to_string())?;
    let index = link.header.index;
    handle
        .link()
        .set(index)
        .up()
        .execute()
        .await
        .map_err(|error| format!("bring eth0 up: {error}"))?;
    handle
        .address()
        .add(index, IpAddr::V4(guest_ip), 30)
        .replace()
        .execute()
        .await
        .map_err(|error| format!("assign guest address: {error}"))?;
    handle
        .route()
        .add()
        .v4()
        .output_interface(index)
        .gateway(gateway)
        .replace()
        .execute()
        .await
        .map_err(|error| format!("install guest default route: {error}"))?;

    tracing::info!(%guest_ip, %gateway, "guest network configured");
    Ok(())
}

#[cfg(target_os = "linux")]
fn ensure_proc_mounted() -> Result<(), String> {
    use nix::errno::Errno;
    use nix::mount::{MsFlags, mount};

    std::fs::create_dir_all("/proc").map_err(|error| format!("create /proc: {error}"))?;
    match mount(
        Some("proc"),
        "/proc",
        Some("proc"),
        MsFlags::empty(),
        None::<&str>,
    ) {
        Ok(()) | Err(Errno::EBUSY) => Ok(()),
        Err(error) => Err(format!("mount /proc: {error}")),
    }
}

/// 挂载 sysfs，供 cgroup v2（/sys/fs/cgroup）等子系统使用。
#[cfg(target_os = "linux")]
fn ensure_sys_mounted() -> Result<(), String> {
    use nix::errno::Errno;
    use nix::mount::{MsFlags, mount};

    std::fs::create_dir_all("/sys").map_err(|error| format!("create /sys: {error}"))?;
    match mount(
        Some("sysfs"),
        "/sys",
        Some("sysfs"),
        MsFlags::empty(),
        None::<&str>,
    ) {
        Ok(()) | Err(Errno::EBUSY) => Ok(()),
        Err(error) => Err(format!("mount sysfs: {error}")),
    }
}

/// 挂载 devtmpfs 与 devpts，保证 `/dev/ptmx` 可用（PTY 分配依赖它）。
#[cfg(target_os = "linux")]
fn ensure_dev_mounted() -> Result<(), String> {
    use nix::errno::Errno;
    use nix::mount::{MsFlags, mount};

    std::fs::create_dir_all("/dev/pts").map_err(|error| format!("create /dev/pts: {error}"))?;
    match mount(
        Some("devtmpfs"),
        "/dev",
        Some("devtmpfs"),
        MsFlags::empty(),
        None::<&str>,
    ) {
        Ok(()) | Err(Errno::EBUSY) => {}
        Err(error) => return Err(format!("mount devtmpfs: {error}")),
    }
    match mount(
        Some("devpts"),
        "/dev/pts",
        Some("devpts"),
        MsFlags::empty(),
        None::<&str>,
    ) {
        Ok(()) | Err(Errno::EBUSY) => {}
        Err(error) => return Err(format!("mount devpts: {error}")),
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub async fn configure_network() -> Result<(), String> {
    Err("guest network configuration requires Linux".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_empty() {
        assert!(parse_cmdline("root=/dev/vda console=ttyS0").is_empty());
    }

    #[test]
    fn parse_clouisle_params() {
        let p = parse_cmdline("root=/dev/vda clouisle.vsock_port=52 clouisle.log_level=debug");
        assert_eq!(p.get("vsock_port").unwrap(), "52");
        assert_eq!(p.get("log_level").unwrap(), "debug");
    }

    #[test]
    fn parse_flag_without_value() {
        let p = parse_cmdline("clouisle.readonly");
        assert_eq!(p.get("readonly").unwrap(), "true");
    }

    #[test]
    fn init_succeeds_without_fail() {
        assert!(init_boot("clouisle.vsock_port=52").is_ok());
    }

    #[test]
    fn init_fails_at_overlay() {
        assert!(init_boot("clouisle.fail_at=overlay").is_err());
    }
}
