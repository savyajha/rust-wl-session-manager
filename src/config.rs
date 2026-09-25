use std::fs;
use std::path::Path;

use anyhow::Context;
use serde::Deserialize;

/// The session-manager config file.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Names of the variables to push into the systemd user manager.
    #[serde(alias = "targets")]
    pub env_vars: Vec<String>,
    /// The compositor's user unit, e.g. `niri.service`.
    pub compositor_service: String,
    /// The unit whose start ends the session, e.g. `niri-shutdown.target`.
    pub compositor_shutdown: String,
}

impl Config {
    /// Read and parse the TOML config at `path`.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("parsing config {}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const UNITS: &str = "compositor_service = \"niri.service\"\n\
                         compositor_shutdown = \"niri-shutdown.target\"\n";

    #[test]
    fn env_vars_and_targets_alias_both_parse() {
        for key in ["env_vars", "targets"] {
            let config: Config = toml::from_str(&format!("{key} = [\"PATH\"]\n{UNITS}")).unwrap();
            assert_eq!(config.env_vars, ["PATH"], "key {key}");
            assert_eq!(config.compositor_service, "niri.service");
            assert_eq!(config.compositor_shutdown, "niri-shutdown.target");
        }
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let text = format!("env_vars = []\ncompositor = \"niri.service\"\n{UNITS}");
        let err = toml::from_str::<Config>(&text).unwrap_err();
        assert!(
            err.to_string().contains("unknown field `compositor`"),
            "{err}"
        );
    }
}
