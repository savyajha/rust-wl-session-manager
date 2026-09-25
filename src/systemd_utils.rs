use zbus::proxy;
use futures_util::StreamExt;

/// Job mode that replaces any conflicting queued job.
pub const MODE_REPLACE: &str = "replace";

/// logind inhibitor lock type for system shutdown and reboot.
pub const INHIBIT_SHUTDOWN: &str = "shutdown";

/// logind inhibitor mode that delays the operation instead of blocking it.
pub const INHIBIT_DELAY: &str = "delay";

#[proxy(
    interface = "org.freedesktop.systemd1.Manager",
    default_service = "org.freedesktop.systemd1",
    default_path = "/org/freedesktop/systemd1",
)]
pub trait SystemdManager {
    #[zbus(name = "SetEnvironment")]
    fn set_environment(&self, env_list: &[String]) -> zbus::Result<()>;

    #[zbus(name = "UnsetEnvironment")]
    fn unset_environment(&self, env_list: &[String]) -> zbus::Result<()>;

    fn subscribe(&self) -> zbus::Result<()>;

    fn start_unit(&self, name: &str, mode: &str) -> zbus::Result<zbus::zvariant::OwnedObjectPath>;

    fn load_unit(&self, name: &str) -> zbus::Result<zbus::zvariant::OwnedObjectPath>;

    #[zbus(name = "ResetFailedUnit")]
    fn reset_failed_unit(&self, name: &str) -> zbus::Result<()>;

    /// A job finished; `result` is "done" on success, otherwise "failed", "canceled", etc.
    #[zbus(signal)]
    fn job_removed(
        &self,
        id: u32,
        job: zbus::zvariant::OwnedObjectPath,
        unit: String,
        result: String,
    ) -> zbus::Result<()>;

    /// Uncached, so every read returns the manager's current environment.
    #[zbus(property(emits_changed_signal = "false"), name = "Environment")]
    fn environment(&self) -> zbus::Result<Vec<String>>;
}

#[proxy(
    interface = "org.freedesktop.systemd1.Unit",
    default_service = "org.freedesktop.systemd1",
)]
pub trait Unit {
    #[zbus(property)]
    fn active_state(&self) -> zbus::Result<String>;

    /// The unit's queued job as (id, path); `(0, "/")` when none is queued.
    #[zbus(property)]
    fn job(&self) -> zbus::Result<(u32, zbus::zvariant::OwnedObjectPath)>;
}

/// True if a unit's `ActiveState` means it has stopped, cleanly or not.
fn stopped(state: &str) -> bool {
    matches!(state, "inactive" | "failed")
}

impl UnitProxy<'_> {
    /// Wait until the unit is inactive or failed with no job queued to bring it back.
    pub async fn wait_until_stopped(&self) -> zbus::Result<()> {
        // Subscribe to both before reading, so a change in between is not lost.
        let mut states = self.receive_active_state_changed().await;
        let mut jobs = self.receive_job_changed().await;
        loop {
            let (job_id, _) = self.job().await?;
            if job_id == 0 && stopped(&self.active_state().await?) {
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

#[proxy(
    interface = "org.freedesktop.login1.Manager",
    default_service = "org.freedesktop.login1",
    default_path = "/org/freedesktop/login1",
)]
pub trait LogindManager {
    fn inhibit(
        &self,
        what: &str,
        who: &str,
        why: &str,
        mode: &str,
    ) -> zbus::Result<zbus::zvariant::OwnedFd>;

    #[zbus(signal)]
    fn prepare_for_shutdown(&self, active: bool) -> zbus::Result<()>;
}
