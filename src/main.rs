mod scanner;
mod systemd_utils;
mod cli;

use std::collections::HashSet;
use std::fs;
use std::io::IsTerminal;
use std::process;
use systemd_utils::{
    LogindManagerProxy, PrepareForShutdownStream, SystemdManagerProxy, UnitExt, UnitProxy,
};
use zbus::Connection;
use clap::Parser;
use futures_util::StreamExt;
use tokio::signal::unix::{signal, SignalKind};
use tracing::{info, warn};
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

async fn env_var_names(manager: &SystemdManagerProxy<'_>) -> HashSet<String> {
    match manager.environment().await {
        Ok(env) => env
            .into_iter()
            .filter_map(|kv| kv.split_once('=').map(|(k, _)| k.to_string()))
            .collect(),
        Err(e) => {
            warn!(error = %e, "failed to read user manager environment");
            HashSet::new()
        }
    }
}

async fn vars_added_since(
    manager: &SystemdManagerProxy<'_>,
    baseline: &HashSet<String>,
) -> Vec<String> {
    let current = env_var_names(manager).await;
    current.difference(baseline).cloned().collect()
}

/// Unset every environment variable that appeared since `baseline` was
/// snapshotted. Each variable name is interpolated into the log message (not
/// attached as a structured field) so it is visible in journald's default
/// output, mirroring the per-variable logging done at export.
///
/// Called only after the session target has stopped, so nothing still
/// starting up in the session can race the unset.
async fn unset_session_environment(
    manager: &SystemdManagerProxy<'_>,
    baseline: &HashSet<String>,
) {
    let to_unset = vars_added_since(manager, baseline).await;
    for var in &to_unset {
        info!("unsetting environment variable {var}");
    }
    if let Err(e) = manager.unset_environment(&to_unset).await {
        warn!(error = %e, "unset_environment failed");
    }
}

/// True if a start (or restart) job is queued for `unit`. Asked via ListJobs
/// rather than the unit's cached `Job` property, so the answer is current.
async fn start_pending(manager: &SystemdManagerProxy<'_>, unit: &str) -> zbus::Result<bool> {
    let jobs = manager.list_jobs().await?;
    Ok(jobs
        .iter()
        .any(|(_, name, kind, ..)| name == unit && (kind == "start" || kind == "restart")))
}

/// Resolve once the compositor has stopped for good: inactive or failed with
/// no start job queued. A manual `systemctl --user restart` passes through
/// inactive with its start job still pending — that is not the end of the
/// session, so wait for it to come back up and keep watching.
async fn wait_for_compositor_gone(
    manager: &SystemdManagerProxy<'_>,
    compositor: &UnitProxy<'_>,
    name: &str,
) -> zbus::Result<()> {
    loop {
        compositor.wait_for_unit_exit().await?;
        if !start_pending(manager, name).await? {
            return Ok(());
        }
        compositor.wait_for_unit_active().await?;
    }
}

/// End the session: start `compositor_shutdown`, which `Conflicts=` the
/// session target (and graphical-session-pre.target), so the target stops and
/// its `PartOf=` clients stop with it — clients before the compositor, per
/// their `After=`. Wait for the target to actually reach inactive so the env
/// cleanup does not race the teardown, then unset what we pushed.
///
/// wait_for_unit_exit subscribes and then reads the current state, so it is
/// fine to call after the stop has been issued: a transition that completed in
/// between is caught by the read.
async fn end_session(
    manager: &SystemdManagerProxy<'_>,
    session_target: &UnitProxy<'_>,
    config: &scanner::Config,
    baseline: &HashSet<String>,
) -> zbus::Result<()> {
    manager.start_unit(&config.compositor_shutdown, "replace").await?;
    session_target.wait_for_unit_exit().await?;
    unset_session_environment(manager, baseline).await;
    Ok(())
}

async fn shutdown(
    reason: ShutdownReason,
    manager: &SystemdManagerProxy<'_>,
    session_target: &UnitProxy<'_>,
    inhibitor: zbus::zvariant::OwnedFd,
    config: &scanner::Config,
    baseline: &HashSet<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    match reason {
        ShutdownReason::CompositorExited => {
            info!("compositor exited; ending session");
            end_session(manager, session_target, config, baseline).await?;
        }

        ShutdownReason::SystemInitiated => {
            // The system is shutting down. systemd will stop all session units
            // and the user manager; unsetting env vars is redundant. Our only
            // job is to release the inhibitor so logind can proceed.
            //
            // NOTE: InhibitDelayMaxSec defaults to 5 s (set in logind.conf,
            // not adjustable from this process). This path does zero D-Bus
            // calls so it is always well within that budget.
            info!("system shutdown signalled; releasing inhibitor");
        }

        ShutdownReason::Terminated => {
            info!("SIGTERM received; ending session");
            end_session(manager, session_target, config, baseline).await?;
        }
    }

    // inhibitor is dropped here; this is the explicit signal to logind that
    // teardown is complete and the shutdown may proceed.
    drop(inhibitor);
    info!("session stopped, inhibitor released");
    Ok(())
}

/// Session-bus half of startup: everything the systemd user manager needs
/// before the compositor can start. Returns the connection (kept for building
/// unit proxies later), the manager proxy, and the environment baseline.
async fn prepare_user_manager(
    config: &scanner::Config,
) -> zbus::Result<(Connection, SystemdManagerProxy<'static>, HashSet<String>)> {
    let conn = Connection::session().await?;
    let manager = SystemdManagerProxy::new(&conn).await?;

    // systemd only emits unit signals — including the standard
    // PropertiesChanged that backs every receive_active_state_changed()
    // stream — to clients that have called Subscribe(). Without this, the
    // session-exit detection would silently never fire on systemd versions
    // that gate PropertiesChanged behind a subscriber.
    manager.subscribe().await?;

    // Snapshot the user manager environment BEFORE we add anything. Cleanup
    // at teardown unsets only the vars that appeared after this point — what
    // session-manager pushed plus what the compositor imported — and leaves
    // pre-existing user-manager vars (PATH, DBUS_SESSION_BUS_ADDRESS,
    // XDG_RUNTIME_DIR, etc.) untouched.
    let baseline = env_var_names(&manager).await;

    let env_list = scanner::export_env_vars(config);
    manager.set_environment(&env_list).await?;

    // Clear any lingering failed state from a prior session; start_unit with
    // mode "replace" does not reset a unit in the failed state on its own. A
    // compositor crash in the previous session leaves it failed.
    //
    // On a fresh user manager neither unit is loaded yet, and systemd answers
    // NoSuchUnit — expected, so it is ignored silently. The error is put in
    // the message text (not a structured field) so journalctl shows it.
    for unit in [config.compositor_service.as_str(), SESSION_TARGET] {
        match manager.reset_failed_unit(unit).await {
            Ok(()) => {}
            Err(zbus::Error::MethodError(name, _, _)) if name.as_str() == NO_SUCH_UNIT => {}
            Err(e) => warn!("reset_failed_unit({unit}) failed (ignoring): {e}"),
        }
    }

    Ok((conn, manager, baseline))
}

/// System-bus half of startup: subscribe to logind's PrepareForShutdown and
/// take the shutdown delay inhibitor. Returns the signal stream (which keeps
/// its own handle on the system bus connection) and the inhibitor fd, whose
/// drop releases the lock.
async fn prepare_logind() -> zbus::Result<(PrepareForShutdownStream, zbus::zvariant::OwnedFd)> {
    let conn = Connection::system().await?;
    let logind = LogindManagerProxy::new(&conn).await?;

    // Establish the shutdown signal stream BEFORE acquiring the inhibitor so
    // there is no window in which PrepareForShutdown could fire unobserved.
    // Sleep is intentionally not inhibited: suspend/resume must be transparent
    // — exiting on PrepareForSleep would cause greetd to respawn the greeter.
    let shutdown_stream = logind.receive_prepare_for_shutdown().await?;

    let inhibitor = logind
        .inhibit("shutdown", "session-manager", "Graceful graphical session teardown", "delay")
        .await?;
    info!("inhibitor lock acquired");

    Ok((shutdown_stream, inhibitor))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    init_logging();

    let args = cli::Cli::parse();
    let config_content = fs::read_to_string(&args.config)?;
    let config: scanner::Config = toml::from_str(&config_content)?;

    info!(config = ?args.config, "starting session");

    // The two buses are independent: the systemd user manager lives on the
    // session bus, logind on the system bus, and neither side's setup needs
    // anything from the other. Run them concurrently; both must be done
    // before the compositor starts.
    let ((session_conn, manager, baseline), (mut shutdown_stream, inhibitor)) =
        tokio::try_join!(prepare_user_manager(&config), prepare_logind())?;

    // Start the compositor. Its packaged unit is BindsTo=/Before=
    // graphical-session.target, so starting it pulls the session target up.
    // Selecting the compositor stays a pure config concern; its packaged unit
    // is used as-is, with no Restart= policy added.
    info!(unit = %config.compositor_service, "starting compositor");
    manager.start_unit(&config.compositor_service, "replace").await?;

    // Watch the compositor itself: every way a session ends (quit, crash, a
    // logout that stops the session target) ends with the compositor stopped.
    // Both units are loaded now that the start job has been queued. The
    // session target proxy is kept for end_session.
    let compositor_path = manager.get_unit(&config.compositor_service).await?;
    let compositor = UnitProxy::builder(&session_conn)
        .path(compositor_path)?
        .build()
        .await?;
    let target_path = manager.get_unit(SESSION_TARGET).await?;
    let session_target = UnitProxy::builder(&session_conn)
        .path(target_path)?
        .build()
        .await?;

    // Wait for the compositor to finish starting (it is Type=notify, so
    // "active" means ready) before logging the session as started.
    // wait_for_unit_active returns as soon as it goes active (event-driven, no
    // polling).
    compositor.wait_for_unit_active().await?;

    let mut sigterm = signal(SignalKind::terminate())?;

    // Pin the exit future so it is polled across loop iterations without
    // re-creating the internal property-change stream subscription.
    let compositor_gone =
        wait_for_compositor_gone(&manager, &compositor, &config.compositor_service);
    tokio::pin!(compositor_gone);

    info!("session started");

    // Loop handles PrepareForShutdown(false) (cancelled shutdown) without
    // dropping the exit future's stream subscription.
    let reason = loop {
        tokio::select! {
            result = &mut compositor_gone => {
                result?;
                break ShutdownReason::CompositorExited;
            }
            Some(sig) = shutdown_stream.next() => {
                if let Ok(args) = sig.args() {
                    if args.active {
                        break ShutdownReason::SystemInitiated;
                    }
                    info!("system shutdown cancelled, continuing");
                }
            }
            _ = sigterm.recv() => break ShutdownReason::Terminated,
        }
    };

    shutdown(reason, &manager, &session_target, inhibitor, &config, &baseline).await
}
