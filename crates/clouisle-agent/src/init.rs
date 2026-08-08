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
