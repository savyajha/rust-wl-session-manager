mod scanner;
mod systemd_utils;
mod cli;

use std::collections::HashSet;
use std::fs;
use std::io::IsTerminal;
use std::process;
use systemd_utils::{SystemdManagerProxy, UnitProxy, LogindManagerProxy, UnitExt};
use zbus::Connection;
use clap::Parser;
use futures_util::StreamExt;
use tokio::signal::unix::{signal, SignalKind};
use tracing::{info, warn};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

/// The freedesktop session target session-manager watches. The compositor's
/// packaged unit binds itself to it (niri: `BindsTo=`/`Before=`), and every
/// session client is `PartOf=` it, so its lifetime is the lifetime of one
/// compositor instance. Its ActiveState going inactive is the single
/// definition of "session over".
const SESSION_TARGET: &str = "graphical-session.target";

/// D-Bus error systemd returns when asked about a unit that is not loaded.
const NO_SUCH_UNIT: &str = "org.freedesktop.systemd1.NoSuchUnit";

enum ShutdownReason {
    /// The session target went inactive. This is the single definition of
    /// "session over". The compositor has no Restart= policy, so any exit —
    /// a clean quit, a crash, or a logout that stops the target — ends the
    /// session: once the compositor is gone nothing needs the target, it stops
    /// (StopWhenUnneeded=), and its PartOf= clients stop with it. Every
    /// Wayland client dies with the compositor anyway, so there is nothing
    /// worth keeping alive across a compositor restart.
    SessionEnded,
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
/// This no longer stops the session target: under the current design the
/// session target is either already stopped (it is what woke us) or is stopped
/// by `compositor_shutdown` (which `Conflicts=` it), and that stop is what
/// drives the ordered teardown of the compositor and its clients.
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

async fn shutdown(
    reason: ShutdownReason,
    manager: &SystemdManagerProxy<'_>,
    conn: &Connection,
    inhibitor: zbus::zvariant::OwnedFd,
    config: &scanner::Config,
    baseline: &HashSet<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    match reason {
        ShutdownReason::SessionEnded => {
            // graphical-session.target went inactive — its stop is what woke us,
            // and the session-scoped clients have already been stopped with it.
            // Run the compositor shutdown unit anyway: it is idempotent, and it
            // also stops graphical-session-pre.target, which nothing else takes
            // down after a compositor crash. Then clean up the env we pushed.
            info!("session ended; running teardown");
            manager.start_unit(&config.compositor_shutdown, "replace").await?;
            unset_session_environment(manager, baseline).await;
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
            info!("SIGTERM received; stopping session and running teardown");

            // End the session by starting the shutdown unit, which Conflicts=
            // graphical-session.target — so starting it stops the target, and
            // that stop drives the ordered teardown (clients before the
            // compositor). We then wait for the target to actually reach
            // inactive so env cleanup does not race the teardown.
            //
            // Subscribe before issuing the stop: zbus PropertyStream is backed
            // by a buffered broadcast channel, so any transition that occurs
            // after subscription but before the first .next() is queued and
            // will not be lost. The post-subscribe active_state() read handles
            // the one race buffering cannot cover — a transition that completed
            // before the subscription was established.
            let unit_path = manager.get_unit(SESSION_TARGET).await?;
            let target = UnitProxy::builder(conn)
                .path(unit_path)?
                .build()
                .await?;
            let mut exit_stream = target.receive_active_state_changed().await;
            let current_state = target.active_state().await?;

            manager.start_unit(&config.compositor_shutdown, "replace").await?;

            if current_state != "inactive" && current_state != "failed" {
                while let Some(change) = exit_stream.next().await {
                    if let Ok(state) = change.get().await
                        && (state == "inactive" || state == "failed")
                    {
                        break;
                    }
                }
            }

            unset_session_environment(manager, baseline).await;
        }
    }

    // inhibitor is dropped here; this is the explicit signal to logind that
    // teardown is complete and the shutdown may proceed.
    drop(inhibitor);
    info!("session stopped, inhibitor released");
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    init_logging();

    let args = cli::Cli::parse();
    let config_content = fs::read_to_string(&args.config)?;
    let config: scanner::Config = toml::from_str(&config_content)?;

    info!(config = ?args.config, "starting session");

    // Two separate buses: systemd user manager lives on the session bus,
    // logind lives on the system bus.
    let session_conn = Connection::session().await?;
    let system_conn = Connection::system().await?;
    let manager = SystemdManagerProxy::new(&session_conn).await?;
    let logind = LogindManagerProxy::new(&system_conn).await?;

    // systemd only emits unit signals — including the standard
    // PropertiesChanged that backs every receive_active_state_changed()
    // stream below — to clients that have called Subscribe(). Without this,
    // the compositor-exit detection would silently never fire on systemd
    // versions that gate PropertiesChanged behind a subscriber.
    manager.subscribe().await?;

    // Establish the shutdown signal stream BEFORE acquiring the inhibitor so
    // there is no window in which PrepareForShutdown could fire unobserved.
    // Sleep is intentionally not inhibited: suspend/resume must be transparent
    // — exiting on PrepareForSleep would cause greetd to respawn the greeter.
    let mut shutdown_stream = logind.receive_prepare_for_shutdown().await?;

    let inhibitor = logind
        .inhibit("shutdown", "session-manager", "Graceful graphical session teardown", "delay")
        .await?;
    info!("inhibitor lock acquired");

    // Snapshot the user manager environment BEFORE we add anything. Cleanup
    // at teardown unsets only the vars that appeared after this point — what
    // session-manager pushed plus what the compositor imported — and leaves
    // pre-existing user-manager vars (PATH, DBUS_SESSION_BUS_ADDRESS,
    // XDG_RUNTIME_DIR, etc.) untouched.
    let baseline = env_var_names(&manager).await;

    let env_list = scanner::export_env_vars(&config);
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

    // Start the compositor. Its packaged unit is BindsTo=/Before=
    // graphical-session.target, so starting it pulls the session target up.
    // Selecting the compositor stays a pure config concern; its packaged unit
    // is used as-is, with no Restart= policy added.
    info!(unit = %config.compositor_service, "starting compositor");
    manager.start_unit(&config.compositor_service, "replace").await?;

    // Watch graphical-session.target, not the compositor: every way a session
    // ends (compositor quit or crash, logout, niri-shutdown.target) passes
    // through the target going inactive. A manual `systemctl --user restart`
    // of the compositor keeps the target active (the restart job keeps it
    // needed), so that alone does not end the session.
    let unit_path = manager.get_unit(SESSION_TARGET).await?;
    let session_target = UnitProxy::builder(&session_conn)
        .path(unit_path)?
        .build()
        .await?;

    // Gate on the session target actually coming up before arming the exit
    // watch. The target is inactive until the compositor finishes starting; a
    // bare exit watch would read that startup-inactive as "session over" and
    // tear down immediately. wait_for_unit_active returns as soon as it goes
    // active (event-driven, no polling).
    session_target.wait_for_unit_active().await?;

    let mut sigterm = signal(SignalKind::terminate())?;

    // Pin the session-exit future so it is polled across loop iterations
    // without re-creating the internal property-change stream subscription.
    let session_exit = session_target.wait_for_unit_exit();
    tokio::pin!(session_exit);

    info!("session started");

    // Loop handles PrepareForShutdown(false) (cancelled shutdown) without
    // dropping the session-exit future's stream subscription.
    let reason = loop {
        tokio::select! {
            result = &mut session_exit => {
                result?;
                break ShutdownReason::SessionEnded;
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

    shutdown(reason, &manager, &session_conn, inhibitor, &config, &baseline).await
}
