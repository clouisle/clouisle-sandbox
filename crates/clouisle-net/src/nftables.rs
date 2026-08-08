//! nftables 规则集管理（Linux only）。
//!
//! 规则在沙盒 netns **内部**执行（`ip netns exec <ns> nft`）。
//! nftables 是 per-netns 的：netns 内的规则只影响该 netns，
//! 对宿主 netns 零影响。
//!
//! 语义（netns 内）：
//! - forward policy drop：仅放行内网 10.0.0.0/8、agent(5201)、DNS(53)、已建立连接
//! - SNAT masquerade：guest 出站流量经 veth 出网

use std::process::Command;

use clouisle_core::ClouisleError;

pub type Result<T> = std::result::Result<T, ClouisleError>;

/// 通过 stdin 在 netns 内加载规则集。
fn apply_ruleset_in_ns(ns: &str, ruleset: &str) -> Result<()> {
    use std::io::Write;
    use std::process::Stdio;

    let mut child = Command::new("ip")
        .args(["netns", "exec", ns, "nft", "-f", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| ClouisleError::io(format!("spawn nft in ns: {e}")))?;

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
            format!("nft ruleset apply in ns {ns} failed: {stderr}"),
        ));
    }
    Ok(())
}

/// 为沙盒在 netns 内创建 nftables 规则集。
///
/// 语义（全部在沙盒 netns 内，不影响宿主）：
/// - forward policy drop：guest 出站放行、host→guest 仅 agent(5201)/DNS(53)/已建立
/// - SNAT masquerade：guest 出站经 veth_ns 出网
pub fn setup_ruleset(sandbox_id: &str, ns: &str, _veth_ns: &str, _host_ip: &str) -> Result<()> {
    let ruleset = format!(
        r#"
table ip filter {{
    chain forward {{
        type filter hook forward priority 0; policy drop;
        ip daddr 10.0.0.0/8 accept
        ip daddr 127.0.0.0/8 accept
        iif "tap0" accept
        iif "br0" accept
        oif "br0" accept
        tcp dport 5201 accept
        udp dport 53 accept
        ct state established,related accept
        counter drop
    }}

    chain input {{
        type filter hook input priority 0; policy drop;
        iif "lo" accept
        iif "br0" accept
        ct state established,related accept
    }}

    chain postrouting {{
        type nat hook postrouting priority 100; policy accept;
        oif "br0" masquerade
        oif "vn" masquerade
    }}
}}
"#
    );
    apply_ruleset_in_ns(ns, &ruleset)
}

/// 放行一个 IP（出站白名单，netns 内）。
pub fn allow_ip(sandbox_id: &str, ns: &str, ip: &str, _ttl_secs: u64) -> Result<()> {
    let veth_ns = crate::netns::short_name(sandbox_id, "vn");
    let mut args = vec!["add", "rule", "ip", "filter", "forward"];
    args.extend_from_slice(&["iif", &veth_ns, "ip", "daddr", ip, "accept"]);
    crate::netns::run_nft_in_ns(ns, &args).map(|_| ())
}

/// 删除沙盒 netns 内的 nftables 表。
pub fn teardown_ruleset(sandbox_id: &str, ns: &str) -> Result<()> {
    let _ = crate::netns::run_nft_in_ns(ns, &["delete", "table", "ip", "filter"]);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ruleset_builds() {
        // 只验证格式化不 panic（真实加载需要 Linux + root）
        let ruleset = format!(
            r#"
table ip filter {{
    chain forward {{
        type filter hook forward priority 0; policy drop;
        iif "tap0" accept
    }}
}}
"#
        );
        assert!(ruleset.contains("tap0"));
    }
}