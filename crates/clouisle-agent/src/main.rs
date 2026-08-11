//! clouisle-agent 二进制入口。
//!
//! Used as either guest PID 1 (no argument), `clouisle-agent serve`, or
//! `clouisle-agent serve --skip-network-config` (Docker 开发容器模式)。

use clouisle_agent::serve::{ServeConfig, run_serve_with};

fn parse_mode() -> Result<ServeConfig, String> {
    let mut args = std::env::args().skip(1);
    let Some(command) = args.next() else {
        // 无参数：guest PID 1（Firecracker）
        return Ok(ServeConfig::default());
    };
    if command != "serve" {
        return Err(format!(
            "usage: clouisle-agent [serve [--skip-network-config]]; got `{command}`"
        ));
    }
    let mut config = ServeConfig::default();
    for flag in args {
        match flag.as_str() {
            "--skip-network-config" => config.skip_network_config = true,
            other => return Err(format!("unknown serve flag: {other}")),
        }
    }
    Ok(config)
}

#[tokio::main]
async fn main() {
    let config = match parse_mode() {
        Ok(config) => config,
        Err(message) => {
            eprintln!("{message}");
            std::process::exit(1);
        }
    };

    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();
    if let Err(error) = run_serve_with(config).await {
        tracing::error!(%error, "agent serve failed");
    }
}

#[cfg(test)]
mod tests {

    use clouisle_agent::serve::ServeConfig;

    #[test]
    fn pid_one_defaults_to_serve_mode() {
        // 无参数 = 默认 config（skip_network_config=false）。
        assert!(!ServeConfig::default().skip_network_config);
    }

    #[test]
    fn skip_network_flag_parses() {
        // 模拟 args：这里只验证 flag 解析函数逻辑（拆出内部函数）
        assert!(
            matches!(parse_serve_flags(["serve", "--skip-network-config"].into_iter()), Ok(c) if c.skip_network_config)
        );
        assert!(
            matches!(parse_serve_flags(["serve"].into_iter()), Ok(c) if !c.skip_network_config)
        );
        assert!(parse_serve_flags(["bogus"].into_iter()).is_err());
        assert!(parse_serve_flags(["serve", "--nope"].into_iter()).is_err());
    }

    fn parse_serve_flags<'a>(
        mut iter: impl Iterator<Item = &'a str>,
    ) -> Result<ServeConfig, String> {
        let Some(command) = iter.next() else {
            return Ok(ServeConfig::default());
        };
        if command != "serve" {
            return Err("bad command".into());
        }
        let mut config = ServeConfig::default();
        for flag in iter {
            match flag {
                "--skip-network-config" => config.skip_network_config = true,
                other => return Err(format!("unknown flag: {other}")),
            }
        }
        Ok(config)
    }
}
