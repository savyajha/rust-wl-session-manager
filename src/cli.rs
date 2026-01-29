use clap::Parser;
use std::path::PathBuf;

#[derive(Parser)]
#[command(author, version, about = "A systemd-based session manager")]
pub struct Cli {
    #[arg(short, long, default_value = "config.toml", value_hint = clap::ValueHint::FilePath)]
    pub config: PathBuf,
}
