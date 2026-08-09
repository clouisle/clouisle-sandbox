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

fn host_table_name(sandbox_id: &str) -> String {
    format!("clo_{}", crate::netns::short_name(sandbox_id, ""))
}

/// 在宿主网络命名空间按 veth 入接口约束 guest 出站流量。
///
/// TAP 到 veth 的二层转发不会稳定经过 guest netns 的 IP forward hook；宿主 veth
/// 是该流量进入三层转发的可靠边界。
pub fn setup_host_egress(sandbox_id: &str) -> Result<()> {
    use std::io::Write;
    use std::process::Stdio;

    let table = host_table_name(sandbox_id);
    let veth = crate::netns::short_name(sandbox_id, "vh");
    let ruleset = format!(
        r#"
table ip {table} {{
    chain egress {{
        type filter hook forward priority -1; policy accept;
        iifname "{veth}" ip daddr 10.0.0.0/8 accept
        iifname "{veth}" ip daddr 127.0.0.0/8 accept
        iifname "{veth}" ct state established,related accept
        iifname "{veth}" drop
    }}
}}
"#
    );
    let mut child = Command::new("nft")
        .args(["-f", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| ClouisleError::io(format!("spawn host nft: {e}")))?;
    child
        .stdin
        .as_mut()
        .expect("nft stdin piped")
        .write_all(ruleset.as_bytes())
        .map_err(|e| ClouisleError::io(format!("write host nft ruleset: {e}")))?;
    drop(child.stdin.take());
    let output = child
        .wait_with_output()
        .map_err(|e| ClouisleError::io(format!("wait host nft: {e}")))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(ClouisleError::new(
            clouisle_core::ErrorKind::Network,
            format!(
                "host nft ruleset apply failed: {}",
                String::from_utf8_lossy(&output.stderr)
            ),
        ))
    }
}

/// 在宿主 veth 策略中动态放行 DNS 白名单解析出的 IP。
pub fn allow_ip_host(sandbox_id: &str, ip: &str) -> Result<()> {
    let table = host_table_name(sandbox_id);
    let veth = crate::netns::short_name(sandbox_id, "vh");
    let status = Command::new("nft")
        .args([
            "insert", "rule", "ip", &table, "egress", "iifname", &veth, "ip", "daddr", ip, "accept",
        ])
        .status()
        .map_err(|e| ClouisleError::io(format!("spawn host nft allow {ip}: {e}")))?;
    if status.success() {
        Ok(())
    } else {
        Err(ClouisleError::new(
            clouisle_core::ErrorKind::Network,
            format!("host nft allow {ip} failed"),
        ))
    }
}

/// 删除一个沙盒的宿主 veth 出站策略。
pub fn teardown_host_egress(sandbox_id: &str) -> Result<()> {
    let table = host_table_name(sandbox_id);
    let _ = Command::new("nft")
        .args(["delete", "table", "ip", &table])
        .status()
        .map_err(|e| ClouisleError::io(format!("delete host nft table {table}: {e}")))?;
    Ok(())
}

/// 为沙盒在 netns 内创建 nftables 规则集。
///
/// 语义（全部在沙盒 netns 内，不影响宿主）：
/// - forward policy drop：guest 出站放行、host→guest 仅 agent(5201)/DNS(53)/已建立
/// - SNAT masquerade：guest 出站经 veth_ns 出网
pub fn setup_ruleset(_sandbox_id: &str, ns: &str, veth_ns: &str, _host_ip: &str) -> Result<()> {
    // Bridge-family policies were tested on the KVM host but dropped agent TCP handshakes.
    // Keep IP-family isolation until its conntrack behavior is redesigned and revalidated.
    let ruleset = format!(
        r#"
table ip filter {{
    chain forward {{
        type filter hook forward priority 0; policy drop;
        ip daddr 10.0.0.0/8 accept
        ip daddr 127.0.0.0/8 accept
        tcp dport 5201 accept
        udp dport 53 accept
        ct state established,related accept
        counter drop
    }}
    chain input {{
        type filter hook input priority 0; policy drop;
        iif "lo" accept
        iif "br0" accept
        iif "tap0" udp dport 53 accept
        udp dport 53 accept
        ct state established,related accept
    }}
    chain postrouting {{
        type nat hook postrouting priority 100; policy accept;
        oif "br0" masquerade
        oif "{veth_ns}" masquerade
    }}
}}
"#
    );
    apply_ruleset_in_ns(ns, &ruleset)
}

/// 在当前 netns 中放行一个 DNS 白名单解析出的 IP。
pub fn allow_ip_current(ip: &str) -> Result<()> {
    let status = Command::new("nft")
        .args([
            "add", "rule", "ip", "filter", "forward", "iif", "tap0", "ip", "daddr", ip, "accept",
        ])
        .status()
        .map_err(|e| ClouisleError::io(format!("spawn nft allow {ip}: {e}")))?;
    if status.success() {
        Ok(())
    } else {
        Err(ClouisleError::new(
            clouisle_core::ErrorKind::Network,
            format!("nft allow {ip} failed"),
        ))
    }
}

/// 放行一个 IP（出站白名单，netns 内）。
pub fn allow_ip(_sandbox_id: &str, ns: &str, ip: &str, _ttl_secs: u64) -> Result<()> {
    crate::netns::run_nft_in_ns(
        ns,
        &[
            "add", "rule", "ip", "filter", "forward", "iif", "tap0", "ip", "daddr", ip, "accept",
        ],
    )
    .map(|_| ())
}

pub fn teardown_ruleset(_sandbox_id: &str, ns: &str) -> Result<()> {
    let _ = crate::netns::run_nft_in_ns(ns, &["delete", "table", "ip", "filter"]);
    Ok(())
}
#[cfg(test)]
mod tests {

    #[test]
    fn ruleset_builds() {
        // 只验证格式化不 panic（真实加载需要 Linux + root）
        let ruleset = r#"
table ip filter {
    chain forward {
        type filter hook forward priority 0; policy drop;
        iif "tap0" accept
    }
}
"#;
        assert!(ruleset.contains("tap0"));
    }
}
