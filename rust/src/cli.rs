use std::path::PathBuf;

use clap::{Parser, Subcommand};

/// Native SponsorBlock companion for YouTube TV.
#[derive(Debug, Parser)]
#[command(name = "isponsorblocktvrust", version)]
pub struct Cli {
    #[arg(short = 'd', long, global = true)]
    pub data: Option<PathBuf>,
    #[arg(long, global = true)]
    pub debug: bool,
    #[arg(long, global = true)]
    pub http_tracing: bool,
    #[arg(long = "setup", hide = true, global = true)]
    pub legacy_setup: bool,
    #[arg(long = "setup-cli", hide = true, global = true)]
    pub legacy_setup_cli: bool,
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Clone, Copy, Debug, Subcommand, PartialEq, Eq)]
pub enum Command {
    /// Start the native device-control service.
    Start,
    /// Configure devices and preferences in a full-screen terminal interface.
    Setup,
    /// Configure devices and preferences using terminal prompts.
    SetupCli,
}

impl Cli {
    pub fn selected_command(&self) -> Command {
        self.command.unwrap_or({
            if self.legacy_setup {
                Command::Setup
            } else if self.legacy_setup_cli {
                Command::SetupCli
            } else {
                Command::Start
            }
        })
    }
}
