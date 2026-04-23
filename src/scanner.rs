use std::env;
use serde::Deserialize;
use tracing::{info, warn};

#[derive(Deserialize)]
pub struct Config {
    pub targets: Vec<String>,
    pub compositor_service: String,
    pub compositor_shutdown: String,
}

pub fn export_env_vars(config: &Config) -> Vec<String> {
    let mut assignments = Vec::new();

    for var_name in &config.targets {
        if let Ok(value) = env::var(var_name) {
            info!(var = %var_name, "exporting environment variable");
            assignments.push(format!("{}={}", var_name, value));
        } else {
            warn!(var = %var_name, "variable not found, skipping");
        }
    }
    assignments
}
