use std::collections::BTreeMap;
use std::env::{self, VarError};

use futures_lite::StreamExt;
use tracing::{info, warn};
use zbus::proxy;
use zbus::zvariant::OwnedObjectPath;

/// Job mode that replaces any conflicting queued job.
pub const MODE_REPLACE: &str = "replace";

/// D-Bus error systemd returns when asked about a unit that is not loaded.
pub const NO_SUCH_UNIT: &str = "org.freedesktop.systemd1.NoSuchUnit";

/// A snapshot of the user manager's environment, by name.
pub type Env = BTreeMap<String, String>;

#[proxy(
    interface = "org.freedesktop.systemd1.Manager",
    default_service = "org.freedesktop.systemd1",
    default_path = "/org/freedesktop/systemd1"
)]
pub trait SystemdManager {
    fn set_environment(&self, assignments: &[String]) -> zbus::Result<()>;

    /// Unset `names`, then apply `assignments`, as one atomic change.
    fn unset_and_set_environment(
        &self,
        names: &[String],
        assignments: &[String],
    ) -> zbus::Result<()>;

    fn subscribe(&self) -> zbus::Result<()>;

    fn start_unit(&self, name: &str, mode: &str) -> zbus::Result<OwnedObjectPath>;

    fn load_unit(&self, name: &str) -> zbus::Result<OwnedObjectPath>;

    fn reset_failed_unit(&self, name: &str) -> zbus::Result<()>;

    /// A job finished; `result` is "done" on success, otherwise "failed", "canceled", etc.
    #[zbus(signal)]
    fn job_removed(
        &self,
        id: u32,
        job: OwnedObjectPath,
        unit: String,
        result: String,
    ) -> zbus::Result<()>;

    /// Uncached, so every read returns the manager's current environment.
    #[zbus(property(emits_changed_signal = "false"))]
    fn environment(&self) -> zbus::Result<Vec<String>>;
}

impl SystemdManagerProxy<'_> {
    /// A proxy for `unit`, loading the unit if needed. Its property cache starts fresh.
    pub async fn unit_proxy(&self, unit: &str) -> zbus::Result<UnitProxy<'static>> {
        let path = self.load_unit(unit).await?;
        UnitProxy::new(self.inner().connection(), path).await
    }

    /// The manager's current environment.
    pub async fn env_snapshot(&self) -> zbus::Result<Env> {
        let list = self.environment().await?;
        Ok(list
            .into_iter()
            .filter_map(|kv| {
                kv.split_once('=')
                    .map(|(k, v)| (k.to_owned(), v.to_owned()))
            })
            .collect())
    }

    /// Copy the named variables from our own environment into the manager.
    pub async fn push_env(&self, names: &[String]) -> zbus::Result<()> {
        let assignments = env_assignments(names, |name| env::var(name));
        self.set_environment(&assignments).await
    }

    /// Return the manager's environment to `baseline`.
    pub async fn restore_env(&self, baseline: &Env) -> zbus::Result<()> {
        let (unset, restore) = env_restore_plan(baseline, &self.env_snapshot().await?);
        if !unset.is_empty() || !restore.is_empty() {
            let (unset_names, restored_names) = (unset.join(" "), restore.join(" "));
            info!("restoring environment: unset [{unset_names}], restored [{restored_names}]");
        }
        let assignments: Vec<String> = restore
            .iter()
            .map(|name| format!("{name}={}", baseline[name]))
            .collect();
        self.unset_and_set_environment(&unset, &assignments).await
    }
}

/// `NAME=value` for each of `names` that `lookup` finds. Logs names only, never values.
fn env_assignments(
    names: &[String],
    lookup: impl Fn(&str) -> Result<String, VarError>,
) -> Vec<String> {
    let mut assignments = Vec::new();
    for name in names {
        match lookup(name) {
            Ok(value) => {
                info!("exporting environment variable {name}");
                assignments.push(format!("{name}={value}"));
            }
            Err(VarError::NotPresent) => warn!("{name} is not set, skipping"),
            Err(VarError::NotUnicode(_)) => warn!("{name} is not valid UTF-8, skipping"),
        }
    }
    assignments
}

/// Names to unset and names to restore so that `current` matches `baseline`.
fn env_restore_plan(baseline: &Env, current: &Env) -> (Vec<String>, Vec<String>) {
    let unset = current
        .keys()
        .filter(|name| !baseline.contains_key(*name))
        .cloned()
        .collect();
    let restore = baseline
        .iter()
        .filter(|(name, value)| current.get(*name) != Some(*value))
        .map(|(name, _)| name.clone())
        .collect();
    (unset, restore)
}

#[proxy(
    interface = "org.freedesktop.systemd1.Unit",
    default_service = "org.freedesktop.systemd1"
)]
pub trait Unit {
    #[zbus(property)]
    fn active_state(&self) -> zbus::Result<String>;

    /// The unit's queued job as (id, path); `(0, "/")` when none is queued.
    #[zbus(property)]
    fn job(&self) -> zbus::Result<(u32, OwnedObjectPath)>;
}

impl UnitProxy<'_> {
    /// Wait until the unit is inactive or failed with no job queued to bring it back.
    pub async fn wait_until_stopped(&self) -> zbus::Result<()> {
        // Subscribe to both before reading, so a change in between is not lost.
        let mut states = self.receive_active_state_changed().await;
        let mut jobs = self.receive_job_changed().await;
        loop {
            let (job_id, _) = self.job().await?;
            if job_id == 0 && matches!(self.active_state().await?.as_str(), "inactive" | "failed") {
                return Ok(());
            }
            tokio::select! {
                Some(_) = states.next() => {}
                Some(_) = jobs.next() => {}
                else => return Err(zbus::Error::Failure("unit property stream ended".to_owned())),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, &str)]) -> Env {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn restore_plan_unsets_added_and_restores_changed() {
        let baseline = map(&[("PATH", "/a"), ("KEEP", "x"), ("GONE", "y")]);
        let current = map(&[("PATH", "/b"), ("KEEP", "x"), ("NEW", "z")]);
        let (unset, restore) = env_restore_plan(&baseline, &current);
        assert_eq!(unset, vec!["NEW"]);
        assert_eq!(restore, vec!["GONE", "PATH"]);
    }

    #[test]
    fn assignments_skip_unset_and_non_utf8_vars() {
        let names = ["SET", "MISSING", "BINARY"].map(str::to_owned);
        let assignments = env_assignments(&names, |name| match name {
            "SET" => Ok("a=b".to_owned()),
            "MISSING" => Err(VarError::NotPresent),
            _ => Err(VarError::NotUnicode("\u{fffd}".into())),
        });
        assert_eq!(assignments, vec!["SET=a=b"]);
    }
}
