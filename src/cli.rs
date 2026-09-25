use std::path::PathBuf;

use clap::Parser;

/// A systemd-based session manager.
#[derive(Parser)]
#[command(version)]
pub struct Cli {
    /// Path to the TOML config file.
    #[arg(short, long, value_hint = clap::ValueHint::FilePath)]
    pub config: PathBuf,
}
