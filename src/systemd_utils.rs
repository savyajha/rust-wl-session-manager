use zbus::proxy;
use futures_util::StreamExt;

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

    fn stop_unit(&self, name: &str, mode: &str) -> zbus::Result<zbus::zvariant::OwnedObjectPath>;

    #[zbus(name = "GetUnit")]
    fn get_unit(&self, name: &str) -> zbus::Result<zbus::zvariant::OwnedObjectPath>;

    #[zbus(name = "ResetFailedUnit")]
    fn reset_failed_unit(&self, name: &str) -> zbus::Result<()>;

    /// Every queued job: (id, unit name, job type, job state, job path, unit path).
    #[zbus(name = "ListJobs")]
    #[allow(clippy::type_complexity)]
    fn list_jobs(
        &self,
    ) -> zbus::Result<
        Vec<(
            u32,
            String,
            String,
            String,
            zbus::zvariant::OwnedObjectPath,
            zbus::zvariant::OwnedObjectPath,
        )>,
    >;

    #[zbus(property, name = "Environment")]
    fn environment(&self) -> zbus::Result<Vec<String>>;
}

#[proxy(
    interface = "org.freedesktop.systemd1.Unit",
    default_service = "org.freedesktop.systemd1",
)]
pub trait Unit {
    #[zbus(property)]
    fn active_state(&self) -> zbus::Result<String>;
}

pub trait UnitExt {
    async fn wait_for_unit_exit(&self) -> zbus::Result<()>;
    async fn wait_for_unit_active(&self) -> zbus::Result<()>;
}

impl<'a> UnitExt for UnitProxy<'a> {
    async fn wait_for_unit_exit(&self) -> zbus::Result<()> {
        // Subscribe first: PropertyStream buffers events from this point on.
        // The active_state() read below catches any transition that completed
        // before the subscription was established.
        let mut stream = self.receive_active_state_changed().await;

        let current = self.active_state().await?;
        if current == "inactive" || current == "failed" {
            return Ok(());
        }

        while let Some(change) = stream.next().await {
            if let Ok(state) = change.get().await
                && (state == "inactive" || state == "failed")
            {
                break;
            }
        }
        Ok(())
    }

    /// Block until the unit reaches the `active` state. The mirror image of
    /// `wait_for_unit_exit`, with the same subscribe-before-read ordering so no
    /// transition is lost in the gap: subscribe first (the stream then buffers
    /// every change), then read the current state to catch the case where the
    /// unit was already active before we subscribed.
    ///
    /// Used to gate the session-exit watch on graphical-session.target until it
    /// has actually come up — otherwise the `inactive` it holds during startup
    /// would be misread as "session over".
    async fn wait_for_unit_active(&self) -> zbus::Result<()> {
        let mut stream = self.receive_active_state_changed().await;

        if self.active_state().await? == "active" {
            return Ok(());
        }

        while let Some(change) = stream.next().await {
            if let Ok(state) = change.get().await
                && state == "active"
            {
                break;
            }
        }
        Ok(())
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
