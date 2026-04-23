mod scanner;
mod systemd_utils;
mod cli;

use std::collections::HashSet;
use std::fs;
use std::process;
use systemd_utils::*;
use zbus::Connection;
use clap::Parser;
use futures_util::StreamExt;
use tokio::signal::unix::{signal, SignalKind};
use tracing::{info, warn};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

enum ShutdownReason {
    /// The compositor unit went inactive on its own (user quit niri).
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
        Err(_) => tracing_subscriber::registry()
            .with(filter)
            .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
            .try_init()
            .map(|_| "stderr"),
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

async fn shutdown(
    reason: ShutdownReason,
    manager: &SystemdManagerProxy<'_>,
    conn: &Connection,
    inhibitor: zbus::zvariant::OwnedFd,
    config: &scanner::Config,
    baseline: &HashSet<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    match reason {
        ShutdownReason::CompositorExited => {
            info!("compositor exited; running teardown");
            manager.start_unit(&config.compositor_shutdown, "replace").await?;
            let to_unset = vars_added_since(manager, baseline).await;
            let (r1, r2) = tokio::join!(
                manager.stop_unit("graphical-session.target", "replace"),
                manager.unset_environment(&to_unset),
            );
            if let Err(e) = r1 { warn!(error = %e, "stop_unit graphical-session.target failed"); }
            if let Err(e) = r2 { warn!(error = %e, "unset_environment failed"); }
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
            info!("SIGTERM received; stopping compositor and running teardown");

            // Subscribe before stop_unit: zbus PropertyStream is backed by a
            // buffered broadcast channel, so any transition that occurs after
            // subscription but before the first .next() is queued and will not
            // be lost. The post-subscribe active_state() read handles the one
            // race buffering cannot cover — a transition that completed before
            // the subscription was established.
            let unit_path = manager.get_unit(&config.compositor_service).await?;
            let leader = SessionLeaderProxy::builder(conn)
                .path(unit_path)?
                .build()
                .await?;
            let mut exit_stream = leader.receive_active_state_changed().await;
            let current_state = leader.active_state().await?;

            manager.stop_unit(&config.compositor_service, "replace").await?;

            if current_state != "inactive" && current_state != "failed" {
                while let Some(change) = exit_stream.next().await {
                    if let Ok(state) = change.get().await {
                        if state == "inactive" || state == "failed" {
                            break;
                        }
                    }
                }
            }

            manager.start_unit(&config.compositor_shutdown, "replace").await?;
            let to_unset = vars_added_since(manager, baseline).await;
            let (r1, r2) = tokio::join!(
                manager.stop_unit("graphical-session.target", "replace"),
                manager.unset_environment(&to_unset),
            );
            if let Err(e) = r1 { warn!(error = %e, "stop_unit graphical-session.target failed"); }
            if let Err(e) = r2 { warn!(error = %e, "unset_environment failed"); }
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
    let config: scanner::Config = toml::from_str(&config_content)
        .expect("Invalid TOML config");

    info!(config = ?args.config, "starting session");

    // Two separate buses: systemd user manager lives on the session bus,
    // logind lives on the system bus.
    let session_conn = Connection::session().await?;
    let system_conn = Connection::system().await?;
    let manager = SystemdManagerProxy::new(&session_conn).await?;
    let logind = LogindManagerProxy::new(&system_conn).await?;

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
    // mode "replace" does not reset a unit in the failed state on its own.
    if let Err(e) = manager.reset_failed_unit(&config.compositor_service).await {
        warn!(error = %e, unit = %config.compositor_service, "reset_failed_unit failed (ignoring)");
    }

    info!(unit = %config.compositor_service, "starting compositor");
    manager.start_unit(&config.compositor_service, "replace").await?;

    let unit_path = manager.get_unit(&config.compositor_service).await?;
    let leader = SessionLeaderProxy::builder(&session_conn)
        .path(unit_path)?
        .build()
        .await?;

    let mut sigterm = signal(SignalKind::terminate())?;

    // Pin the compositor-exit future so it is polled across loop iterations
    // without re-creating the internal property-change stream subscription.
    let compositor_exit = leader.wait_for_unit_exit();
    tokio::pin!(compositor_exit);

    info!("session started");

    // Loop handles PrepareForShutdown(false) (cancelled shutdown) without
    // dropping the compositor-exit future's stream subscription.
    let reason = loop {
        tokio::select! {
            result = &mut compositor_exit => {
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

    shutdown(reason, &manager, &session_conn, inhibitor, &config, &baseline).await
}
