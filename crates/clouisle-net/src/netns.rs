//! 沙盒网络命名空间管理（Linux only）。
//!
//! 每沙盒一个独立 netns `clo-<hash>`：
//! - veth pair：宿主侧 `vh-<hash>` ↔ netns 侧 `vn-<hash>`
//! - 宿主侧 `vh-<hash>` 与 netns 网桥使用同一网关地址 `.1/30`
//! - TAP 设备 `tap0` 在 netns 内，Firecracker 进程也在 netns 内运行，
//!   guest eth0 直连 tap0
//! - nftables 规则在 netns 内执行（per-netns，对宿主零影响）
//!
//! IP 规划（每沙盒独立网段，多沙盒不冲突）：
//!   宿主 veth / netns br0: 10.{a}.{b}.1/30
//!   guest (tap0):          10.{a}.{b}.2/30
//!   宿主路由:              10.{a}.{b}.0/30 dev vh-<hash>

use std::process::Command;

use clouisle_core::ClouisleError;

/// 从 sandbox_id 生成短接口名（≤ 15 字符，Linux 接口名上限）。
pub(crate) fn short_name(sandbox_id: &str, prefix: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(sandbox_id.as_bytes());
    let hash = hex::encode(hasher.finalize());
    format!("{prefix}{}", &hash[..8])
}

/// netns 名（`clo-<hash>`）。
pub(crate) fn ns_name(sandbox_id: &str) -> String {
    format!("clo-{}", short_name(sandbox_id, ""))
}

/// 从 sandbox_id 派生独立网段 10.{a}.{b}.0/30。
fn sandbox_subnet(sandbox_id: &str) -> (u16, u16) {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(sandbox_id.as_bytes());
    let digest = hasher.finalize();
    // a/b ∈ [10, 209]，避开常见内网段
    let a = 10 + (digest[0] as u16 % 200);
    let b = 10 + (digest[1] as u16 % 200);
    (a, b)
}

/// 沙盒网段信息。
#[derive(Debug, Clone)]
pub struct NetInfo {
    pub ns_name: String,
    pub veth_host: String,
    pub veth_ns: String,
    pub subnet: String,   // 10.{a}.{b}.0/30
    pub gateway: String,  // 10.{a}.{b}.1
    pub guest_ip: String, // 10.{a}.{b}.2
}
/// 操作结果。
pub type Result<T> = std::result::Result<T, ClouisleError>;

/// 创建沙盒 netns 拓扑。
///
/// 1. `ip netns add clo-<hash>`
/// 2. 创建 veth pair `vh-<hash>` + `vn-<hash>`
/// 3. `vn-<hash>` 移入 netns，加入网桥 `br0`
/// 4. `br0` 配网关 IP
/// 5. netns 内开启 IP 转发
/// 6. 宿主侧 `vh-<hash>` up + 添加指向沙盒网段的路由
///
/// 注意：**不预建 tap0**。tap0 由 Firecracker 在 VMM.create 时创建
/// （host_dev_name="tap0"），随后由 [`attach_tap`] 加入 br0。
pub fn create_netns(sandbox_id: &str) -> Result<NetInfo> {
    let (a, b) = sandbox_subnet(sandbox_id);
    let info = NetInfo {
        ns_name: ns_name(sandbox_id),
        veth_host: short_name(sandbox_id, "vh"),
        veth_ns: short_name(sandbox_id, "vn"),
        subnet: format!("10.{a}.{b}.0/30"),
        gateway: format!("10.{a}.{b}.1"),
        guest_ip: format!("10.{a}.{b}.2"),
    };

    // 1. 创建 netns
    run("ip", &["netns", "add", &info.ns_name])?;

    // 2. 创建 veth pair
    run(
        "ip",
        &[
            "link",
            "add",
            &info.veth_host,
            "type",
            "veth",
            "peer",
            "name",
            &info.veth_ns,
        ],
    )?;

    // 3. veth_ns 移入 netns，创建网桥并桥接
    run(
        "ip",
        &["link", "set", &info.veth_ns, "netns", &info.ns_name],
    )?;
    run_in_ns(&info.ns_name, &["link", "add", "br0", "type", "bridge"])?;
    run_in_ns(
        &info.ns_name,
        &["link", "set", &info.veth_ns, "master", "br0"],
    )?;
    run_in_ns(&info.ns_name, &["link", "set", &info.veth_ns, "up"])?;

    // 4. 不预建 tap0：由 Firecracker 在 VMM.create 时创建（host_dev_name="tap0"），
    //    VMM.start 后由 attach_tap 轮询其出现并加入 br0。

    // 5. br0 配网关 IP
    run_in_ns(
        &info.ns_name,
        &["addr", "add", &format!("{}/30", info.gateway), "dev", "br0"],
    )?;
    run_in_ns(&info.ns_name, &["link", "set", "br0", "up"])?;

    // 6. netns 内开启 IP 转发
    run_sysctl_in_ns(&info.ns_name, &["-w", "net.ipv4.ip_forward=1"])?;

    // 7. 宿主侧 veth 配置网关 IP、up + 添加指向沙盒网段的路由。
    //    没有地址时仅添加 link-scope 路由会导致宿主 ARP 使用 0.0.0.0，
    //    guest 不会回 ARP，host→guest 连接始终卡在 INCOMPLETE。
    run(
        "ip",
        &[
            "addr",
            "add",
            &format!("{}/30", info.gateway),
            "dev",
            &info.veth_host,
        ],
    )?;
    run("ip", &["link", "set", &info.veth_host, "up"])?;
    run(
        "ip",
        &["route", "replace", &info.subnet, "dev", &info.veth_host],
    )?;

    Ok(info)
}

/// 将 Firecracker 创建的 tap0 加入网桥（VMM.create 之后调用）。
/// Firecracker 建 tap0 有延迟，轮询等待其出现。
pub fn attach_tap(sandbox_id: &str) -> Result<()> {
    let ns = ns_name(sandbox_id);
    // 最多等 15s，每 500ms 检查一次
    for _ in 0..30 {
        match run_in_ns(&ns, &["link", "show", "tap0"]) {
            Ok(_) => {
                run_in_ns(&ns, &["link", "set", "tap0", "master", "br0"])?;
                run_in_ns(&ns, &["link", "set", "tap0", "up"])?;
                return Ok(());
            }
            Err(_) => {
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
        }
    }
    Err(ClouisleError::io(format!(
        "tap0 did not appear in netns {ns} after Firecracker start"
    )))
}

/// 删除沙盒 netns（自动清理 veth/tap/路由不自动删，需手动删路由）。
pub fn delete_netns(sandbox_id: &str) -> Result<()> {
    let info = NetInfo {
        ns_name: ns_name(sandbox_id),
        veth_host: short_name(sandbox_id, "vh"),
        veth_ns: short_name(sandbox_id, "vn"),
        subnet: String::new(),
        gateway: String::new(),
        guest_ip: String::new(),
    };
    // 删宿主路由
    let _ = run(
        "ip",
        &[
            "route",
            "del",
            &sandbox_subnet_str(sandbox_id),
            "dev",
            &info.veth_host,
        ],
    );
    // 删 netns（自动移除内部设备）
    let _ = run("ip", &["netns", "del", &info.ns_name]);
    Ok(())
}

/// 在沙盒 netns 内执行命令（无 `ip netns exec` 前缀，调用方传完整命令）。
pub(crate) fn run_in_ns(ns: &str, args: &[&str]) -> Result<String> {
    let mut full = vec!["netns", "exec", ns];
    full.push("ip");
    full.extend_from_slice(args);
    run("ip", &full)
}

/// 在沙盒 netns 内执行 sysctl 命令。
pub(crate) fn run_sysctl_in_ns(ns: &str, args: &[&str]) -> Result<String> {
    let mut full = vec!["netns", "exec", ns, "sysctl"];
    full.extend_from_slice(args);
    run("ip", &full)
}

/// 在沙盒 netns 内执行 nft 命令。
pub(crate) fn run_nft_in_ns(ns: &str, args: &[&str]) -> Result<String> {
    let mut full = vec!["netns", "exec", ns, "nft"];
    full.extend_from_slice(args);
    run("ip", &full)
}

/// 沙盒 guest IP（10.{a}.{b}.2）。
pub fn guest_ip(sandbox_id: &str) -> String {
    let (a, b) = sandbox_subnet(sandbox_id);
    format!("10.{a}.{b}.2")
}

/// 沙盒网关 IP（10.{a}.{b}.1）。
pub fn gateway_ip(sandbox_id: &str) -> String {
    let (a, b) = sandbox_subnet(sandbox_id);
    format!("10.{a}.{b}.1")
}

/// 沙盒网段（10.{a}.{b}.0/30）。
pub fn subnet(sandbox_id: &str) -> String {
    let (a, b) = sandbox_subnet(sandbox_id);
    format!("10.{a}.{b}.0/30")
}

fn sandbox_subnet_str(sandbox_id: &str) -> String {
    subnet(sandbox_id)
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
        assert!(name.len() <= 15);
    }

    #[test]
    fn subnet_derived() {
        let (a, b) = sandbox_subnet("019fe000-0000-0000-0000-000000000000");
        assert!((10..210).contains(&a));
        assert!((10..210).contains(&b));
    }
}
