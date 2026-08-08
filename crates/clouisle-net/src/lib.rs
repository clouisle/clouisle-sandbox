//! clouisle-net: per-sandbox 网络隔离。
//!
//! 每沙盒独立 netns + nftables 规则集 + DNS 白名单代理。
//! Linux only（netns / nftables / TAP）。

#[cfg(target_os = "linux")]
pub mod dns_proxy;
#[cfg(target_os = "linux")]
pub mod firewall;
#[cfg(target_os = "linux")]
pub mod netns;
#[cfg(target_os = "linux")]
pub mod nftables;

#[cfg(target_os = "linux")]
pub use dns_proxy::{DnsProxy, DnsRule};
#[cfg(target_os = "linux")]
pub use firewall::FirewallManager;