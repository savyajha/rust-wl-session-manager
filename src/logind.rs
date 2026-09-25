use zbus::proxy;
use zbus::zvariant::OwnedFd;

/// Inhibitor lock type for system shutdown and reboot.
pub const INHIBIT_SHUTDOWN: &str = "shutdown";

/// Inhibitor mode that delays the operation instead of blocking it.
pub const INHIBIT_DELAY: &str = "delay";

#[proxy(
    interface = "org.freedesktop.login1.Manager",
    default_service = "org.freedesktop.login1",
    default_path = "/org/freedesktop/login1"
)]
pub trait LogindManager {
    /// Take an inhibitor lock; it is held until the returned fd is closed.
    fn inhibit(&self, what: &str, who: &str, why: &str, mode: &str) -> zbus::Result<OwnedFd>;

    /// Emitted with `true` before shutdown or reboot, and with `false` if it is cancelled.
    #[zbus(signal)]
    fn prepare_for_shutdown(&self, active: bool) -> zbus::Result<()>;
}
