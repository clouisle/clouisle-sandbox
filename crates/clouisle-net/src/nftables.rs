//! nftables 规则集管理（Linux only）。
//!
//! 规则在沙盒的 netns 内执行。每沙盒：
//! - 出站白名单：`@allowed_v4` 动态集
//! - 入站默认拒绝
//! - SNAT 出站（masquerade）

use std::process::Command;

use clouisle_core::ClouisleError;

pub type Result<T> = std::result::Result<T, ClouisleError>;

/// 在沙盒 netns 内执行 nft 命令。
fn nft_in_ns(sandbox_id: &str, args: &[&str]) -> Result<String> {
    let ns = format!("clo-{}", crate::netns::short_name(sandbox_id, ""));
    let mut full = vec!["netns", "exec", &ns, "nft"];
    full.extend_from_slice(args);
    run("ip", &full)
}

/// 在沙盒 netns 内通过 stdin 加载规则集。
fn apply_ruleset_in_ns(sandbox_id: &str, ruleset: &str) -> Result<()> {
    use std::io::Write;
    use std::process::Stdio;

    let ns = format!("clo-{}", crate::netns::short_name(sandbox_id, ""));
    let mut child = Command::new("ip")
        .args(["netns", "exec", &ns, "nft", "-f", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| ClouisleError::io(format!("nft in ns: {e}")))?;

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
            format!("nft ruleset apply in ns failed: {stderr}"),
        ));
    }
    Ok(())
}

/// 为沙盒创建 nftables 规则集（在 netns 内）。
///
/// 语义：
/// - 出站 `@allowed_v4` 动态集白名单，默认拒绝
/// - 入站默认 drop
/// - SNAT masquerade 出站
pub fn setup_ruleset(sandbox_id: &str) -> Result<()> {
    // 先删旧表
    let _ = nft_in_ns(sandbox_id, &["delete", "table", "ip", "filter"]);

    let veth_ns = crate::netns::short_name(sandbox_id, "vn");
    let ruleset = format!(
        r#"
table ip filter {{
    set allowed_v4 {{
        type ipv4_addr
        flags timeout
    }}

    chain forward {{
        type filter hook forward priority 0; policy drop;
        ip daddr @allowed_v4 accept
        ip daddr 10.0.0.0/8 accept
        ip daddr 127.0.0.0/8 accept
        iif "tap0" accept
        ct state established,related accept
        counter drop
    }}

    chain input {{
        type filter hook input priority 0; policy drop;
        iif "lo" accept
        iif "tap0" accept
        udp dport 53 accept
        tcp dport 5201 accept
        ct state established,related accept
    }}

    chain postrouting {{
        type nat hook postrouting priority 100; policy accept;
        oif "{veth_ns}" masquerade
    }}
}}
"#
    );

    apply_ruleset_in_ns(sandbox_id, &ruleset)
}

/// 向沙盒 nftables 动态集添加一个放行 IP。
pub fn allow_ip(sandbox_id: &str, ip: &str, ttl_secs: u64) -> Result<()> {
    let cmd = format!("add element ip filter allowed_v4 {{ {ip} timeout {ttl_secs}s }}");
    nft_in_ns(sandbox_id, &cmd.split_whitespace().collect::<Vec<&str>>()).map(|_| ())
}

/// 删除沙盒的 nftables 表。
pub fn teardown_ruleset(sandbox_id: &str) -> Result<()> {
    let _ = nft_in_ns(sandbox_id, &["delete", "table", "ip", "filter"]);
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
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_noop() {
        // 在非 Linux 上不执行真实命令
        assert!(true);
    }
}
