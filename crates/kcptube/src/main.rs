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
use kcptube_protocol::{APP_NAME, APP_VERSION};
use kcptube_relay::Relay;
use kcptube_server::Server;
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(
    name = "kcptube",
    version = APP_VERSION,
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

fn print_usage() {
    println!("{APP_NAME} version {APP_VERSION}");
    println!("Usage: {APP_NAME} config1.conf");
    println!("       {APP_NAME} config1.conf config2.conf...");
    println!("Connectivity Testing:");
    println!("       {APP_NAME} --try config1.conf");
    println!("       {APP_NAME} --try config1.conf config2.conf...");
    println!("       {APP_NAME} config1.conf --try");
    println!("       {APP_NAME} config1.conf config2.conf... --try");
}

fn init_logging() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
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

    println!("{APP_NAME} version {APP_VERSION}");

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
        tracing::warn!("连通性测试（--try）尚未实现");
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
            RunningMode::Relay => Relay::new(settings.clone(), settings.clone())
                .run()
                .await
                .map_err(anyhow::Error::from),
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
        eprintln!("没有成功启动任何模式（当前为骨架版本，功能尚未实现）");
    }

    Ok(())
}
