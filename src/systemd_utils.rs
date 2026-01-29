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
}

#[proxy(
    interface = "org.freedesktop.systemd1.Unit",
    default_service = "org.freedesktop.systemd1",
)]
pub trait SessionLeader {
    #[zbus(property)]
    fn active_state(&self) -> zbus::Result<String>;
}

pub trait SessionLeaderExt {
    async fn wait_for_unit_exit(&self) -> zbus::Result<()>;
}

impl<'a> SessionLeaderExt for SessionLeaderProxy<'a> {
    async fn wait_for_unit_exit(&self) -> zbus::Result<()> {
        let mut stream = self.receive_active_state_changed().await;

        let current = self.active_state().await?;
        if current == "inactive" || current == "failed" {
            return Ok(());
        }

        while let Some(change) = stream.next().await {
            if let Ok(state) = change.get().await {
                if state == "inactive" || state == "failed" {
                    break;
                }
            }
        }
        Ok(())
    }
}
