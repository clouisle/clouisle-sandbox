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

/// 配置 guest 网络（eth0 静态 IP + 默认网关）。
///
/// 从内核 cmdline 读取 `clouisle.guest_ip` / `clouisle.gateway`，
/// 用 ifconfig/route 配置 eth0。不依赖发行版网络管理（Ubuntu 的
/// systemd-networkd 不认内核 `ip=` 参数）。
pub fn configure_network() -> Result<(), String> {
    let params = parse_cmdline(&read_cmdline());
    let guest_ip = params
        .get("guest_ip")
        .ok_or_else(|| "clouisle.guest_ip not set in cmdline".to_string())?;
    let gateway = params
        .get("gateway")
        .ok_or_else(|| "clouisle.gateway not set in cmdline".to_string())?;

    // 根文件系统可能预置了宿主 DNS；替换为本沙盒网关上的白名单代理。
    let _ = std::fs::remove_file("/etc/resolv.conf");
    if let Err(e) = std::fs::write("/etc/resolv.conf", format!("nameserver {gateway}\n")) {
        tracing::warn!(error = %e, "failed to configure guest resolv.conf");
    }

    // 优先 ifconfig（net-tools），缺失则用 ip 命令
    let ifcfg_ok = std::process::Command::new("ifconfig")
        .args(["eth0", guest_ip, "netmask", "255.255.255.252", "up"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    if ifcfg_ok {
        // ifconfig up 隐含 link up
    } else {
        // ip 命令：先 link up，再配地址
        let up_ok = std::process::Command::new("ip")
            .args(["link", "set", "eth0", "up"])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        let _ = std::process::Command::new("ip")
            .args(["addr", "add", &format!("{guest_ip}/30"), "dev", "eth0"])
            .status();
        if !up_ok {
            return Err(format!("failed to bring eth0 up (guest_ip={guest_ip})"));
        }
    }

    let _ = std::process::Command::new("ip")
        .args(["route", "replace", "default", "via", gateway, "dev", "eth0"])
        .status();
    if let Ok(out) = std::process::Command::new("ip")
        .args(["addr", "show"])
        .output()
    {
        tracing::info!(addrs = %String::from_utf8_lossy(&out.stdout), "guest addr state");
    }

    tracing::info!(guest_ip = %guest_ip, gateway = %gateway, "guest network configured");
    Ok(())
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
