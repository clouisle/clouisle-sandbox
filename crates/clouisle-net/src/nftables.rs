//! nftables 规则集管理（Linux only）。
//!
//! 规则直接作用于 host 侧 TAP 设备。每沙盒：
//! - 入站默认 drop（仅允许 DNS + agent + 已建立连接）
//! - SNAT 出站（masquerade）

use std::process::Command;

use clouisle_core::ClouisleError;

pub type Result<T> = std::result::Result<T, ClouisleError>;

/// TAP 设备名（与 netns.rs 一致）。
fn tap_name(sandbox_id: &str) -> String {
    crate::netns::short_name(sandbox_id, "fc")
}

/// 通过 stdin 加载 nftables 规则集。
fn apply_ruleset(ruleset: &str) -> Result<()> {
    use std::io::Write;
    use std::process::Stdio;

    let mut child = Command::new("nft")
        .args(["-f", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| ClouisleError::io(format!("spawn nft: {e}")))?;

    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(ruleset.as_bytes())
        .map_err(|e| ClouisleError::io(format!("write ruleset: {e}")))?;
    drop(child.stdin.take());

    let output = child
        .wait_with_output()
        .map_err(|e| ClouisleError::io(format!("nft wait: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(ClouisleError::new(
            clouisle_core::ErrorKind::Network,
            format!("nft ruleset apply failed: {stderr}"),
        ));
    }
    Ok(())
}

/// 为沙盒创建 nftables 规则集。
///
/// 语义（全部 policy accept，只限定 TAP 接口流量，绝不影响宿主自身）：
/// - guest 出站（来自 TAP）：放行
/// - host → guest（去往 TAP）：仅放行 agent(5201)、DNS(53)、已建立连接，其余 drop
/// - SNAT：仅对 TAP 源流量 masquerade
pub fn setup_ruleset(sandbox_id: &str, tap: &str, _host_ip: &str) -> Result<()> {
    let table = format!("clo_{}", crate::netns::short_name(sandbox_id, ""));
    let ruleset = format!(
        r#"
table ip {table} {{
    chain forward {{
        type filter hook forward priority 0; policy accept;
        # guest 出站放行
        iifname "{tap}" accept
        # host → guest：仅 agent / DNS / 已建立
        oifname "{tap}" tcp dport 5201 accept
        oifname "{tap}" udp dport 53 accept
        oifname "{tap}" ct state established,related accept
        oifname "{tap}" drop
    }}

    chain postrouting {{
        type nat hook postrouting priority 100; policy accept;
        # 仅 TAP 出站流量 SNAT（绝不碰宿主其它接口）
        iifname "{tap}" oifname != "{tap}" masquerade
    }}
}}
"#
    );
    apply_ruleset(&ruleset)
}

/// 向 nftables 放行一个 IP（出站白名单）。
pub fn allow_ip(sandbox_id: &str, ip: &str, _ttl_secs: u64) -> Result<()> {
    let tap = tap_name(sandbox_id);
    run(
        "nft",
        &[
            "add",
            "rule",
            &format!("ip clo_{}", crate::netns::short_name(sandbox_id, "")),
            "forward",
            "iifname",
            &tap,
            "ip",
            "daddr",
            ip,
            "accept",
        ],
    )
    .map(|_| ())
}

/// 删除沙盒的 nftables 表。
pub fn teardown_ruleset(sandbox_id: &str) -> Result<()> {
    let table = format!("clo_{}", crate::netns::short_name(sandbox_id, ""));
    let _ = run("nft", &["delete", "table", "ip", &table]);
    Ok(())
}

/// 执行系统命令。
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
    fn tap_name_short() {
        let n = tap_name("019fe000-0000-0000-0000-000000000000");
        assert!(n.len() <= 15);
    }
}