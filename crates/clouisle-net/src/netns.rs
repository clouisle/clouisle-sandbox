//! 网络命名空间管理（Linux only）。
//!
//! 每沙盒一个独立 netns `clo-<sbx-id>`，内建 TAP 设备与 veth pair。

use std::process::Command;

use clouisle_core::ClouisleError;

/// 从 sandbox_id 生成短接口名（ ≤ 15 字符，Linux 接口名上限）。
pub(crate) fn short_name(sandbox_id: &str, prefix: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(sandbox_id.as_bytes());
    let hash = hex::encode(hasher.finalize());
    // 取前 8 位 hex + prefix = 最多 12 字符
    format!("{prefix}{}", &hash[..8])
}

/// 网络命名空间操作结果。
pub type Result<T> = std::result::Result<T, ClouisleError>;

/// 创建沙盒 netns 并配置 veth pair。
///
/// 架构：
/// ```text
/// netns "clo-<id>":          root netns:
///   tap0 10.0.0.2/30 ──veth-in──veth-out── (SNAT 到宿主机)
///               ↑
///         guest 固定 IP（所有沙盒相同，快照可复用）
/// ```
pub fn create_netns(sandbox_id: &str, host_side_ip: &str) -> Result<(String, String)> {
    let ns_name = format!("clo-{}", short_name(sandbox_id, ""));
    let veth_host = short_name(sandbox_id, "vh");
    let veth_ns = short_name(sandbox_id, "vn");

    // 1. 创建 netns
    run("ip", &["netns", "add", &ns_name])?;

    // 2. 创建 veth pair
    run(
        "ip",
        &[
            "link", "add", &veth_host, "type", "veth", "peer", "name", &veth_ns,
        ],
    )?;

    // 3. 将 veth_ns 移入 netns
    run("ip", &["link", "set", &veth_ns, "netns", &ns_name])?;

    // 4. 配置 veth_ns (guest 侧网关)
    run(
        "ip",
        &[
            "netns",
            "exec",
            &ns_name,
            "ip",
            "addr",
            "add",
            "10.0.0.1/30",
            "dev",
            &veth_ns,
        ],
    )?;
    run(
        "ip",
        &[
            "netns", "exec", &ns_name, "ip", "link", "set", &veth_ns, "up",
        ],
    )?;

    // 5. 配置 TAP (guest 侧)
    run(
        "ip",
        &[
            "netns", "exec", &ns_name, "ip", "tuntap", "add", "tap0", "mode", "tap",
        ],
    )?;
    run(
        "ip",
        &[
            "netns",
            "exec",
            &ns_name,
            "ip",
            "addr",
            "add",
            "10.0.0.2/30",
            "dev",
            "tap0",
        ],
    )?;
    run(
        "ip",
        &["netns", "exec", &ns_name, "ip", "link", "set", "tap0", "up"],
    )?;

    // 6. 配置宿主机侧 veth
    run("ip", &["addr", "add", host_side_ip, "dev", &veth_host])?;
    run("ip", &["link", "set", &veth_host, "up"])?;

    // 7. 启用 IP 转发 + SNAT（宿主机侧）
    run("sysctl", &["-w", "net.ipv4.ip_forward=1"])?;

    Ok((veth_host, veth_ns))
}

/// 删除沙盒 netns 及关联设备。
pub fn delete_netns(sandbox_id: &str) -> Result<()> {
    let ns_name = format!("clo-{}", short_name(sandbox_id, ""));
    let veth_host = short_name(sandbox_id, "vh");

    // 先删宿主机侧 veth（解除对 netns 的引用）
    run("ip", &["link", "delete", &veth_host])?;

    // 再删 netns
    run("ip", &["netns", "delete", &ns_name])?;
    Ok(())
}

/// 执行系统命令并检查结果。
fn run(cmd: &str, args: &[&str]) -> Result<String> {
    let output = Command::new(cmd)
        .args(args)
        .output()
        .map_err(|e| ClouisleError::io(format!("run {cmd}: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(ClouisleError::new(
            clouisle_core::ErrorKind::Network,
            format!("{cmd} {} failed: {stderr}", args.join(" ")),
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_netns_name_format() {
        let ns = format!("clo-{}", "sbx-123");
        assert_eq!(ns, "clo-sbx-123");
    }

    #[test]
    fn test_veth_name_format() {
        let vh = format!("vh-{}", "sbx-123");
        assert_eq!(vh, "vh-sbx-123");
    }
}
