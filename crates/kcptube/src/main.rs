// SPDX-License-Identifier: GPL-3.0-only

//! kcptube（Rust 实现）入口。
//!
//! 命令行行为与 C++ 版保持一致：
//!
//! ```text
//! Usage: kcptube config1.conf
//!        kcptube config1.conf config2.conf...
//! Connectivity Testing:
//!        kcptube --try config1.conf
//!        kcptube --try config1.conf config2.conf...
//!        kcptube config1.conf --try
//!        kcptube config1.conf config2.conf... --try
//! ```

use clap::Parser;
use kcptube_client::Client;
use kcptube_config::{RunningMode, UserSettings};
use kcptube_protocol::APP_NAME;
use kcptube_relay::Relay;
use kcptube_server::Server;
use kcptube_tester::Tester;
use std::path::PathBuf;

/// 程序版本号的唯一来源：根 `Cargo.toml` 的 `[workspace.package] version`
/// （`crates/kcptube/Cargo.toml` 经 `version.workspace = true` 继承）。
///
/// 启动横幅、usage 与 `--version` 全部使用它，程序只对外自报一个版本号。
/// 注意：与 C++ 版横幅的 `20260329`（协议兼容目标版本）不再逐字一致，
/// 需要识别兼容目标时以 `docs/dev/migration-plan.md` 的锁定版本为准。
const APP_DISPLAY_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Parser, Debug)]
#[command(
    name = "kcptube",
    version = APP_DISPLAY_VERSION,
    about = "KCP Tube rewritten in Rust (wire-compatible with the C++ version)"
)]
struct Cli {
    /// Configuration file(s)
    #[arg(value_name = "CONFIG")]
    configs: Vec<PathBuf>,

    /// Test connectivity to the remote peer
    #[arg(long = "try")]
    try_mode: bool,

    /// Only check the configuration file(s)
    #[arg(long = "check-config")]
    check_config: bool,
}

/// 版本横幅：与 `--version` 使用同一个版本号（`APP_DISPLAY_VERSION`），
/// 保证启动、usage 与参数查询三处输出一致。
fn print_version_banner() {
    println!("{APP_NAME} version {APP_DISPLAY_VERSION}");
}

fn print_usage() {
    print_version_banner();
    println!("Usage: {APP_NAME} config1.conf");
    println!("       {APP_NAME} config1.conf config2.conf...");
    println!("Connectivity Testing:");
    println!("       {APP_NAME} --try config1.conf");
    println!("       {APP_NAME} --try config1.conf config2.conf...");
    println!("       {APP_NAME} config1.conf --try");
    println!("       {APP_NAME} config1.conf config2.conf... --try");
}

fn init_logging() {
    use tracing::level_filters::LevelFilter;
    use tracing_subscriber::Layer;
    use tracing_subscriber::filter::Targets;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    // 用 `Targets` 解析 `RUST_LOG`（`info`、`kcptube_transport=debug` 等），
    // 只依赖级别匹配，不引入 `env-filter` 特性及其 regex 依赖。
    let targets = std::env::var("RUST_LOG")
        .ok()
        .and_then(|raw| raw.parse::<Targets>().ok())
        .unwrap_or_default()
        .with_default(LevelFilter::INFO);

    let _ = tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_target(false)
                .with_filter(targets),
        )
        .try_init();
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_logging();

    let cli = Cli::parse();
    if cli.configs.is_empty() {
        print_usage();
        return Ok(());
    }

    print_version_banner();

    let mut profiles: Vec<UserSettings> = Vec::new();
    let mut error_found = false;

    for path in &cli.configs {
        match UserSettings::parse_file(path.clone()) {
            Ok(settings) => profiles.push(settings),
            Err(errors) => {
                eprintln!("Error(s) found in setting file {}", path.display());
                for error in errors {
                    eprintln!("\t{error}");
                }
                eprintln!();
                error_found = true;
            }
        }
    }

    println!(
        "Error Found in Configuration File(s): {}",
        if error_found { "Yes" } else { "No" }
    );

    if error_found || cli.check_config {
        return Ok(());
    }

    if cli.try_mode {
        for settings in profiles {
            match settings.mode {
                RunningMode::Client | RunningMode::Relay => {
                    if let Err(error) = Tester::new(settings).run().await {
                        eprintln!("{error}");
                    }
                }
                // 与 C++ 一致：server 等模式不参与连通性测试
                _ => {}
            }
        }
        return Ok(());
    }

    let mut started: usize = 0;
    for settings in profiles {
        let result = match settings.mode {
            RunningMode::Client => Client::new(settings.clone())
                .run()
                .await
                .map_err(anyhow::Error::from),
            RunningMode::Server => Server::new(settings.clone())
                .run()
                .await
                .map_err(anyhow::Error::from),
            RunningMode::Relay => Relay::new(settings.clone())
                .run()
                .await
                .map_err(anyhow::Error::from),
            RunningMode::RelayIngress | RunningMode::RelayEgress => {
                eprintln!(
                    "配置文件 {} 的 relay 分段不能单独运行",
                    settings.config_filename.display()
                );
                Ok(())
            }
            RunningMode::Unknown => {
                eprintln!(
                    "配置文件 {} 未指定有效的 mode",
                    settings.config_filename.display()
                );
                Ok(())
            }
        };

        match result {
            Ok(()) => started += 1,
            Err(error) => eprintln!("{error}"),
        }
    }

    if started == 0 {
        eprintln!("没有成功启动任何模式");
    }

    Ok(())
}
