mod scanner;
mod systemd_utils;
mod cli;

use std::collections::BTreeMap;
use std::fs;
use std::io::IsTerminal;
use std::process;
use systemd_utils::{
    JobRemovedStream, LogindManagerProxy, PrepareForShutdownStream,
    SystemdManagerProxy, UnitProxy, INHIBIT_DELAY, INHIBIT_SHUTDOWN, MODE_REPLACE,
};
use zbus::Connection;
use zbus::zvariant::{OwnedFd, OwnedObjectPath};
use clap::Parser;
use futures_util::StreamExt;
use tokio::signal::unix::{signal, Signal, SignalKind};
use tracing::{error, info, warn};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

/// The freedesktop session target. The compositor's packaged unit binds itself
/// to it (niri: `BindsTo=`/`Before=`) and every session client is `PartOf=` it,
/// so stopping it tears the whole session down in order. session-manager does
/// not rely on it stopping by itself when the compositor dies: units such as
/// xdg-desktop-portal are `Requisite=` it and outlive the compositor, which
/// keeps it "needed" and defeats its StopWhenUnneeded=. Ending the session
/// therefore always stops it explicitly, via `compositor_shutdown`.
const SESSION_TARGET: &str = "graphical-session.target";

/// D-Bus error systemd returns when asked about a unit that is not loaded.
const NO_SUCH_UNIT: &str = "org.freedesktop.systemd1.NoSuchUnit";

enum ShutdownReason {
    /// The compositor stopped for good: inactive or failed, with no start job
    /// queued. This is the single definition of "session over". The compositor
    /// has no Restart= policy, so any exit — a clean quit, a crash, or a logout
    /// that stops the session target (which stops the compositor via BindsTo=)
    /// — ends the session. Every Wayland client dies with the compositor
    /// anyway, so there is nothing worth keeping alive across it.
    CompositorExited,
    /// logind PrepareForShutdown(true) fired. The system is tearing everything
    /// down; release the inhibitor cleanly.
    SystemInitiated,
    /// Received SIGTERM (e.g. `systemctl --user stop session-manager`).
    Terminated,
}

fn init_logging() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info"));

    let result = match tracing_journald::layer() {
        Ok(layer) => tracing_subscriber::registry()
            .with(filter)
            .with(layer)
            .try_init()
            .map(|_| "journald"),
        Err(e) => {
            eprintln!("journald unavailable ({e}); falling back to stderr");
            tracing_subscriber::registry()
                .with(filter)
                .with(
                    tracing_subscriber::fmt::layer()
                        .with_ansi(std::io::stderr().is_terminal())
                        .with_writer(std::io::stderr),
                )
                .try_init()
                .map(|_| "stderr")
        }
    };

    match result {
        Ok(backend) => info!(backend, "logging initialized"),
        Err(e) => {
            eprintln!("fatal: failed to initialize logging: {e}");
            process::exit(1);
        }
    }
}

/// Errors that end the session run.
type Error = Box<dyn std::error::Error>;

/// A snapshot of the user manager's environment, by name.
type Env = BTreeMap<String, String>;

/// Parse systemd's `NAME=value` environment list into a map.
fn env_map(list: Vec<String>) -> Env {
    list.into_iter()
        .filter_map(|kv| kv.split_once('=').map(|(k, v)| (k.to_owned(), v.to_owned())))
        .collect()
}

/// Names to unset and names to restore so that `current` matches `baseline`.
/// After an unclean exit, leftovers in the user manager join the next baseline
/// and are restored on logout.
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

/// Return the user manager's environment to `baseline`.
async fn restore_environment(
    manager: &SystemdManagerProxy<'_>,
    baseline: &Env,
) -> zbus::Result<()> {
    let current = env_map(manager.environment().await?);
    let (unset, restore) = env_restore_plan(baseline, &current);

    if !unset.is_empty() {
        info!("unsetting environment variables: {}", unset.join(" "));
        manager.unset_environment(&unset).await?;
    }
    if !restore.is_empty() {
        info!("restoring environment variables: {}", restore.join(" "));
        let assignments: Vec<String> = restore
            .iter()
            .map(|name| format!("{name}={}", baseline[name]))
            .collect();
        manager.set_environment(&assignments).await?;
    }
    Ok(())
}

/// Wait for `job` to finish and return its result.
async fn wait_for_job(mut jobs: JobRemovedStream, job: &OwnedObjectPath) -> zbus::Result<String> {
    while let Some(signal) = jobs.next().await {
        match signal.args() {
            Ok(args) if args.job == *job => return Ok(args.result),
            Ok(_) => {}
            Err(e) => warn!("failed to decode JobRemoved: {e}"),
        }
    }
    Err(zbus::Error::Failure("JobRemoved stream ended".to_owned()))
}

/// Start the compositor and wait until it is ready.
async fn start_compositor(manager: &SystemdManagerProxy<'_>, unit: &str) -> Result<(), Error> {
    info!("starting compositor {unit}");
    // Subscribe to JobRemoved before StartUnit, so our job cannot finish unseen.
    let jobs = manager.receive_job_removed().await?;
    let job = manager.start_unit(unit, MODE_REPLACE).await?;
    match wait_for_job(jobs, &job).await?.as_str() {
        "done" => Ok(()),
        result => Err(format!("failed to start {unit}: job {result}").into()),
    }
}

/// A proxy for `unit`, loading the unit if needed. Its property cache starts fresh.
async fn unit_proxy(
    manager: &SystemdManagerProxy<'_>,
    unit: &str,
) -> zbus::Result<UnitProxy<'static>> {
    let path = manager.load_unit(unit).await?;
    UnitProxy::new(manager.inner().connection(), path).await
}

/// What teardown needs once the session environment has been pushed.
struct Session {
    manager: SystemdManagerProxy<'static>,
    baseline: Env,
    /// The logind delay lock; dropping it releases it.
    inhibitor: OwnedFd,
    compositor_shutdown: String,
}

impl Session {
    /// End the graphical session, restore the environment, and release the inhibitor.
    async fn teardown(self) -> zbus::Result<()> {
        info!("running teardown");
        let stopped = self.stop_session_target().await;
        if let Err(e) = &stopped {
            warn!("failed to stop {SESSION_TARGET}: {e}");
        }
        let restored = restore_environment(&self.manager, &self.baseline).await;
        if let Err(e) = &restored {
            warn!("failed to restore the user manager environment: {e}");
        }
        drop(self.inhibitor);
        info!("session ended, inhibitor released");
        stopped.and(restored)
    }

    /// Start `compositor_shutdown` and wait for the session target to stop.
    async fn stop_session_target(&self) -> zbus::Result<()> {
        self.manager.start_unit(&self.compositor_shutdown, MODE_REPLACE).await?;
        unit_proxy(&self.manager, SESSION_TARGET).await?.wait_until_stopped().await
    }
}

/// Session-bus half of startup: subscribe to systemd, snapshot the environment,
/// and clear failed state left by a previous session.
async fn prepare_user_manager(
    compositor: &str,
) -> zbus::Result<(SystemdManagerProxy<'static>, Env)> {
    let conn = Connection::session().await?;
    let manager = SystemdManagerProxy::new(&conn).await?;

    // systemd only emits unit and job signals to clients that have called Subscribe().
    manager.subscribe().await?;

    let baseline = env_map(manager.environment().await?);

    for unit in [compositor, SESSION_TARGET] {
        match manager.reset_failed_unit(unit).await {
            Ok(()) => {}
            Err(zbus::Error::MethodError(name, _, _)) if name.as_str() == NO_SUCH_UNIT => {}
            Err(e) => warn!("failed to reset {unit}: {e}"),
        }
    }

    Ok((manager, baseline))
}

/// System-bus half of startup: subscribe to logind's PrepareForShutdown and
/// take the shutdown delay inhibitor.
async fn prepare_logind() -> zbus::Result<(PrepareForShutdownStream, OwnedFd)> {
    let conn = Connection::system().await?;
    let logind = LogindManagerProxy::new(&conn).await?;

    // Set up the signal stream before taking the inhibitor, so no signal is missed.
    let shutdown_stream = logind.receive_prepare_for_shutdown().await?;

    // Sleep is deliberately not inhibited: suspend and resume leave the session alone.
    let inhibitor = logind
        .inhibit(
            INHIBIT_SHUTDOWN,
            "session-manager",
            "Graceful graphical session teardown",
            INHIBIT_DELAY,
        )
        .await?;
    info!("inhibitor lock acquired");

    Ok((shutdown_stream, inhibitor))
}

/// Run the compositor and watch the session until something ends it.
async fn run(
    manager: &SystemdManagerProxy<'_>,
    compositor: &str,
    mut shutdown_stream: PrepareForShutdownStream,
    mut sigterm: Signal,
) -> Result<ShutdownReason, Error> {
    let session = async {
        start_compositor(manager, compositor).await?;
        info!("session started");
        unit_proxy(manager, compositor).await?.wait_until_stopped().await?;
        Ok::<_, Error>(())
    };
    tokio::pin!(session);

    loop {
        tokio::select! {
            biased;
            sig = shutdown_stream.next() => {
                let Some(sig) = sig else {
                    return Err("PrepareForShutdown stream ended".into());
                };
                match sig.args() {
                    Ok(args) if args.active => return Ok(ShutdownReason::SystemInitiated),
                    Ok(_) => info!("system shutdown cancelled, continuing"),
                    Err(e) => warn!("failed to decode PrepareForShutdown: {e}"),
                }
            }
            result = &mut session => {
                result?;
                return Ok(ShutdownReason::CompositorExited);
            }
            _ = sigterm.recv() => return Ok(ShutdownReason::Terminated),
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    let sigterm = signal(SignalKind::terminate())?;

    init_logging();

    let cli = cli::Cli::parse();
    let config_content = fs::read_to_string(&cli.config)?;
    let scanner::Config { targets, compositor_service, compositor_shutdown } =
        toml::from_str(&config_content)?;

    info!("starting session with config {}", cli.config.display());

    let ((manager, baseline), (shutdown_stream, inhibitor)) =
        tokio::try_join!(prepare_user_manager(&compositor_service), prepare_logind())?;

    manager.set_environment(&scanner::export_env_vars(&targets)).await?;

    let session = Session {
        manager,
        baseline,
        inhibitor,
        compositor_shutdown,
    };

    let result = run(&session.manager, &compositor_service, shutdown_stream, sigterm).await;
    match &result {
        Ok(ShutdownReason::CompositorExited) => info!("compositor exited; ending session"),
        Ok(ShutdownReason::Terminated) => info!("SIGTERM received; ending session"),
        Ok(ShutdownReason::SystemInitiated) => {
            // No D-Bus calls on this path: logind's inhibit delay budget is only 5 s by default.
            info!("system shutdown signalled; releasing inhibitor");
            return Ok(());
        }
        Err(e) => error!("session failed: {e}"),
    }
    let teardown = session.teardown().await;
    result?;
    teardown?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, &str)]) -> Env {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn restore_plan_unsets_added_and_restores_changed() {
        let baseline = map(&[("PATH", "/a"), ("KEEP", "x"), ("GONE", "y")]);
        let current = map(&[("PATH", "/b"), ("KEEP", "x"), ("NEW", "z")]);
        let (unset, restore) = env_restore_plan(&baseline, &current);
        assert_eq!(unset, vec!["NEW"]);
        assert_eq!(restore, vec!["GONE", "PATH"]);
    }
}
