use std::env;
use serde::Deserialize;
use tracing::{info, warn};

#[derive(Deserialize)]
pub struct Config {
    pub targets: Vec<String>,
    pub compositor_service: String,
    pub compositor_shutdown: String,

    /// Window (seconds) over which compositor restarts are counted toward the
    /// start limit. Together with `start_limit_burst` this defines flap
    /// detection: more than `burst` crashes within this window drives the
    /// compositor unit into the failed state, which fires its OnFailure=
    /// logout handler. Written into the runtime drop-in's StartLimitIntervalSec.
    #[serde(default = "default_start_limit_interval_sec")]
    pub start_limit_interval_sec: u32,

    /// Number of compositor restarts permitted within `start_limit_interval_sec`
    /// before the unit is considered to be crash-looping. Written into the
    /// runtime drop-in's StartLimitBurst.
    #[serde(default = "default_start_limit_burst")]
    pub start_limit_burst: u32,
}

fn default_start_limit_interval_sec() -> u32 {
    30
}

fn default_start_limit_burst() -> u32 {
    5
}

pub fn export_env_vars(config: &Config) -> Vec<String> {
    let mut assignments = Vec::new();

    for var_name in &config.targets {
        if let Ok(value) = env::var(var_name) {
            info!("exporting environment variable {var_name}={value}");
            assignments.push(format!("{}={}", var_name, value));
        } else {
            warn!("environment variable {var_name} not found, skipping");
        }
    }
    assignments
}
