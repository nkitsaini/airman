use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

#[derive(Debug, Clone, Parser)]
#[command(
    name = "airman",
    version,
    about = "Fast, reliable Wi-Fi and Bluetooth control"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,

    /// Network backend to use. Auto refuses ambiguous ownership.
    #[arg(long, value_enum)]
    pub backend: Option<BackendChoice>,

    /// Wi-Fi interface to select initially.
    #[arg(long)]
    pub wifi_interface: Option<String>,

    /// Bluetooth adapter to select initially (for example hci0).
    #[arg(long)]
    pub bluetooth_adapter: Option<String>,

    /// Do not request a fresh Wi-Fi scan at startup.
    #[arg(long)]
    pub no_auto_scan: bool,

    /// Do not keep a BlueZ discovery session while airman is open.
    #[arg(long)]
    pub no_auto_discover: bool,

    /// Tracing filter, such as info or airman=debug.
    #[arg(long)]
    pub log_level: Option<String>,

    /// Override the per-session log file.
    #[arg(long)]
    pub log_file: Option<PathBuf>,
}

#[derive(Debug, Clone, Subcommand)]
pub enum Command {
    /// Report daemon ownership, versions, and detected capabilities.
    Diagnose {
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BackendChoice {
    #[default]
    Auto,
    NetworkManager,
    Iwd,
    WpaNetworkd,
    ConnMan,
}
