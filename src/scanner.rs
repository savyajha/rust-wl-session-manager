use std::env;
use serde::Deserialize;
use tracing::{info, warn};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub targets: Vec<String>,
    pub compositor_service: String,
    pub compositor_shutdown: String,
}

pub fn export_env_vars(targets: &[String]) -> Vec<String> {
    let mut assignments = Vec::new();

    for var_name in targets {
        match env::var(var_name) {
            Ok(value) => {
                info!("exporting environment variable {var_name}");
                assignments.push(format!("{var_name}={value}"));
            }
            Err(env::VarError::NotPresent) => warn!("{var_name} is not set, skipping"),
            Err(env::VarError::NotUnicode(_)) => warn!("{var_name} is not valid UTF-8, skipping"),
        }
    }
    assignments
}
