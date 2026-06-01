use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use tracing_subscriber::EnvFilter;

use asahi_brightness::{config, ipc, reactor};

#[derive(Parser)]
#[command(name = "asahi-brightness", version, about)]
struct Cli {
    #[arg(long, global = true)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the daemon (default if no subcommand is given).
    Run,
    /// Print current daemon status as JSON.
    Status,
    /// Pause auto-adjustment for N seconds (0 = until resume).
    Pause {
        #[arg(default_value_t = 0)]
        seconds: u64,
    },
    /// Resume auto-adjustment.
    Resume,
    /// Bias the display curve by ±N percent until next significant lux change.
    Nudge {
        #[arg(allow_hyphen_values = true)]
        delta: i32,
    },
    /// Print the resolved configuration.
    DumpConfig,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .init();

    let cli = Cli::parse();
    let cfg = config::load(cli.config.as_deref()).context("loading config")?;

    match cli.cmd.unwrap_or(Cmd::Run) {
        Cmd::Run => reactor::run(cfg),
        Cmd::DumpConfig => {
            println!("{}", toml::to_string_pretty(&cfg)?);
            Ok(())
        }
        Cmd::Status => ipc::client::status(),
        Cmd::Pause { seconds } => ipc::client::pause(seconds),
        Cmd::Resume => ipc::client::resume(),
        Cmd::Nudge { delta } => ipc::client::nudge(delta),
    }
}
