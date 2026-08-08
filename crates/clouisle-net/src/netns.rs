//! 沙盒网络管理（Linux only）。
//!
//! 每沙盒一个 host 侧 TAP 设备 `fc-<short-id>`，Firecracker 直连。
//! nftables 规则在宿主侧直接控制 tap 设备流量。

use std::process::Command;

use clouisle_core::ClouisleError;

/// 从 sandbox_id 生成短接口名（ ≤ 15 字符，Linux 接口名上限）。
pub(crate) fn short_name(sandbox_id: &str, prefix: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(sandbox_id.as_bytes());
    let hash = hex::encode(hasher.finalize());
    format!("{prefix}{}", &hash[..8])
}

/// 操作结果。
pub type Result<T> = std::result::Result<T, ClouisleError>;

/// 创建沙盒的 TAP 设备，供 Firecracker 直连。
///
/// 架构：
/// ```text
/// guest eth0 ── fc-<hash> (TAP) ── nftables ── SNAT ── 宿主机
/// ```
/// 返回 TAP 设备名（如 `fc-a1b2c3d4`）。
pub fn create_tap(sandbox_id: &str) -> Result<String> {
    let tap_name = short_name(sandbox_id, "fc");

    // 1. 创建 TAP 设备
    run("ip", &["tuntap", "add", &tap_name, "mode", "tap"])?;

    // 2. 启用
    run("ip", &["link", "set", &tap_name, "up"])?;

    // 3. 启用 IP 转发
    run("sysctl", &["-w", "net.ipv4.ip_forward=1"])?;

    Ok(tap_name)
}

/// 删除沙盒的 TAP 设备。
pub fn delete_tap(sandbox_id: &str) -> Result<()> {
    let tap_name = short_name(sandbox_id, "fc");
    let _ = run("ip", &["link", "delete", &tap_name]);
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
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_name_length() {
        let name = short_name("019fe000-0000-0000-0000-000000000000", "fc");
        assert!(name.len() <= 15, "name '{name}' too long");
        assert!(name.starts_with("fc"));
    }
}