use std::env;
use serde::Deserialize;

#[derive(Deserialize)]
pub struct Config {
    pub targets: Vec<String>,
    pub compositor_service: String,
    pub compositor_shutdown: String,
}

pub fn export_env_vars(config: &Config) -> Vec<String> {
    let mut assignments = Vec::new();

    for var_name in &config.targets {
        if let Ok(value) = env::var(&var_name) {
                println!("Exporting {}={}", var_name, value);
                assignments.push(format!("{}={}", var_name, value));
        } else {
            println!("{}: Variable not found, Skipping...", var_name);
        }
    }
    assignments
}
