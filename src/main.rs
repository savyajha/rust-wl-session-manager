mod config;
mod logind;
mod systemd;

use std::env;
use std::ffi::OsString;
use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, bail};
use futures_lite::StreamExt;
use tokio::signal::unix::{Signal, SignalKind, signal};
use tracing::{error, info, warn};
use tracing_subscriber::{filter::LevelFilter, layer::SubscriberExt, util::SubscriberInitExt};
use zbus::Connection;
use zbus::zvariant::{OwnedFd, OwnedObjectPath};

use config::Config;
use logind::{INHIBIT_DELAY, INHIBIT_SHUTDOWN, LogindManagerProxy, PrepareForShutdownStream};
use systemd::{Env, JobRemovedStream, MODE_REPLACE, NO_SUCH_UNIT, SystemdManagerProxy};

/// The freedesktop session target, which rust-wl-session-manager stops explicitly (see DESIGN.md).
const SESSION_TARGET: &str = "graphical-session.target";

/// Why the session run ended.
enum ShutdownReason {
    /// "Session over": the compositor is inactive or failed with no job queued to bring it back.
    CompositorExited,
    /// logind sent `PrepareForShutdown(true)`.
    SystemInitiated,
    /// SIGTERM, e.g. from `systemctl --user stop` of its unit.
    Terminated,
}

/// The path from exactly `--config <path>`, or `None` for any other arguments.
fn parse_args(mut args: impl Iterator<Item = OsString>) -> Option<PathBuf> {
    match (args.next(), args.next(), args.next()) {
        (Some(flag), Some(path), None) if flag == "--config" => Some(path.into()),
        _ => None,
    }
}

/// Log to journald, or to stderr if journald is unavailable.
fn init_logging() {
    let journald = tracing_journald::layer()
        .inspect_err(|e| eprintln!("journald unavailable ({e}); falling back to stderr"))
        .ok();
    let stderr = journald.is_none().then(|| {
        tracing_subscriber::fmt::layer()
            .with_ansi(io::stderr().is_terminal())
            .with_writer(io::stderr)
    });
    tracing_subscriber::registry()
        .with(LevelFilter::INFO)
        .with(journald)
        .with(stderr)
        .init();
}

/// Wait for `job` to finish and return its result.
async fn wait_for_job(mut jobs: JobRemovedStream, job: &OwnedObjectPath) -> anyhow::Result<String> {
    while let Some(signal) = jobs.next().await {
        match signal.args() {
            Ok(args) if args.job == *job => return Ok(args.result),
            Ok(_) => {}
            Err(e) => warn!("failed to decode JobRemoved: {e}"),
        }
    }
    bail!("JobRemoved stream ended")
}

/// Start the compositor and wait until it is ready.
async fn start_compositor(manager: &SystemdManagerProxy<'_>, unit: &str) -> anyhow::Result<()> {
    info!("starting compositor {unit}");
    // Subscribe to JobRemoved before StartUnit, so our job cannot finish unseen.
    let jobs = manager.receive_job_removed().await?;
    let job = manager
        .start_unit(unit, MODE_REPLACE)
        .await
        .with_context(|| format!("starting {unit}"))?;
    match wait_for_job(jobs, &job).await?.as_str() {
        "done" => Ok(()),
        result => bail!("{unit} start job {result}"),
    }
}

/// What the run and teardown need once the session environment has been pushed.
struct Session {
    config: Config,
    manager: SystemdManagerProxy<'static>,
    baseline: Env,
    /// The logind delay lock; dropping it releases it.
    inhibitor: OwnedFd,
}

impl Session {
    /// End the graphical session, restore the environment, and release the inhibitor.
    /// Logs each failure and returns whether every step succeeded.
    async fn teardown(self) -> bool {
        info!("running teardown");
        let stopped = self.stop_session().await;
        if let Err(e) = &stopped {
            error!("failed to end the session: {e:#}");
        }
        let restored = self.manager.restore_env(&self.baseline).await;
        if let Err(e) = &restored {
            error!("failed to restore the user manager environment: {e}");
        }
        drop(self.inhibitor);
        info!("session ended, inhibitor released");
        stopped.is_ok() && restored.is_ok()
    }

    /// Start `compositor_shutdown`, then wait for the session target and the compositor to stop.
    async fn stop_session(&self) -> anyhow::Result<()> {
        let shutdown = &self.config.compositor_shutdown;
        self.manager
            .start_unit(shutdown, MODE_REPLACE)
            .await
            .with_context(|| format!("starting {shutdown}"))?;
        for unit in [SESSION_TARGET, self.config.compositor_service.as_str()] {
            let proxy = self
                .manager
                .unit_proxy(unit)
                .await
                .with_context(|| format!("loading {unit}"))?;
            proxy
                .wait_until_stopped()
                .await
                .with_context(|| format!("waiting for {unit} to stop"))?;
        }
        Ok(())
    }
}

/// Session-bus half of startup: subscribe to systemd, snapshot the environment,
/// and clear failed state left by a previous session.
async fn prepare_user_manager(
    compositor: &str,
) -> anyhow::Result<(SystemdManagerProxy<'static>, Env)> {
    let conn = Connection::session()
        .await
        .context("connecting to the session bus")?;
    let manager = SystemdManagerProxy::new(&conn).await?;
    // systemd only emits unit and job signals to clients that have called Subscribe().
    manager
        .subscribe()
        .await
        .context("subscribing to systemd signals")?;
    let baseline = manager
        .env_snapshot()
        .await
        .context("reading the user manager environment")?;
    for unit in [compositor, SESSION_TARGET] {
        match manager.reset_failed_unit(unit).await {
            Ok(()) => {}
            Err(zbus::Error::MethodError(name, _, _)) if name == NO_SUCH_UNIT => {}
            Err(e) => warn!("failed to reset {unit}: {e}"),
        }
    }
    Ok((manager, baseline))
}

/// System-bus half of startup: subscribe to `PrepareForShutdown` and take the shutdown inhibitor.
async fn prepare_logind() -> anyhow::Result<(PrepareForShutdownStream, OwnedFd)> {
    let conn = Connection::system()
        .await
        .context("connecting to the system bus")?;
    let logind = LogindManagerProxy::new(&conn).await?;
    // Set up the signal stream before taking the inhibitor, so no signal is missed.
    let shutdown_stream = logind.receive_prepare_for_shutdown().await?;
    // Sleep is deliberately not inhibited: suspend and resume leave the session alone.
    let inhibitor = logind
        .inhibit(
            INHIBIT_SHUTDOWN,
            "rust-wl-session-manager",
            "Graceful graphical session teardown",
            INHIBIT_DELAY,
        )
        .await
        .context("taking the logind inhibitor")?;
    info!("inhibitor lock acquired");
    Ok((shutdown_stream, inhibitor))
}

/// Listen for SIGTERM, load the config, prepare both buses, and push the session environment.
async fn start(config_path: &Path) -> anyhow::Result<(Session, PrepareForShutdownStream, Signal)> {
    // Registered before anything else, so a stop during startup still runs teardown.
    let sigterm = signal(SignalKind::terminate()).context("listening for SIGTERM")?;
    let config = Config::load(config_path)?;
    info!("starting session with config {}", config_path.display());

    let ((manager, baseline), (shutdown_stream, inhibitor)) = tokio::try_join!(
        prepare_user_manager(&config.compositor_service),
        prepare_logind()
    )?;
    manager
        .push_env(&config.env_vars)
        .await
        .context("pushing the session environment")?;

    let session = Session {
        config,
        manager,
        baseline,
        inhibitor,
    };
    Ok((session, shutdown_stream, sigterm))
}

/// Run the compositor and watch the session until something ends it.
async fn run(
    session: &Session,
    mut shutdown_stream: PrepareForShutdownStream,
    mut sigterm: Signal,
) -> anyhow::Result<ShutdownReason> {
    let manager = &session.manager;
    let compositor = session.config.compositor_service.as_str();
    let watch = async {
        start_compositor(manager, compositor).await?;
        info!("session started");
        let proxy = manager
            .unit_proxy(compositor)
            .await
            .with_context(|| format!("loading {compositor}"))?;
        proxy
            .wait_until_stopped()
            .await
            .with_context(|| format!("watching {compositor}"))
    };
    tokio::pin!(watch);

    loop {
        tokio::select! {
            // PrepareForShutdown is polled first, so a shutdown always wins.
            biased;
            sig = shutdown_stream.next() => {
                let Some(sig) = sig else {
                    bail!("PrepareForShutdown stream ended");
                };
                match sig.args() {
                    Ok(args) if args.active => return Ok(ShutdownReason::SystemInitiated),
                    Ok(_) => info!("system shutdown cancelled, continuing"),
                    Err(e) => warn!("failed to decode PrepareForShutdown: {e}"),
                }
            }
            result = &mut watch => {
                result?;
                return Ok(ShutdownReason::CompositorExited);
            }
            _ = sigterm.recv() => return Ok(ShutdownReason::Terminated),
        }
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let Some(config_path) = parse_args(env::args_os().skip(1)) else {
        eprintln!("usage: rust-wl-session-manager --config <path>");
        return ExitCode::from(2);
    };
    init_logging();

    let Ok((session, shutdown_stream, sigterm)) =
        start(&config_path).await.inspect_err(|e| error!("{e:#}"))
    else {
        return ExitCode::FAILURE;
    };

    let result = run(&session, shutdown_stream, sigterm).await;
    match &result {
        Ok(ShutdownReason::SystemInitiated) => {
            // No D-Bus calls on this path: logind's inhibit delay budget is only 5 s by default.
            info!("system shutdown signalled; releasing inhibitor");
            return ExitCode::SUCCESS;
        }
        Ok(ShutdownReason::CompositorExited) => info!("compositor exited, ending session"),
        Ok(ShutdownReason::Terminated) => info!("SIGTERM received, ending session"),
        Err(e) => error!("session failed: {e:#}"),
    }
    if session.teardown().await && result.is_ok() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_config_with_a_path_parses() {
        let parse = |args: &[&str]| parse_args(args.iter().map(OsString::from));
        assert_eq!(parse(&["--config", "a"]), Some(PathBuf::from("a")));
        assert_eq!(parse(&[]), None);
        assert_eq!(parse(&["--config"]), None);
        assert_eq!(parse(&["--bogus", "x"]), None);
        assert_eq!(parse(&["--config", "a", "b"]), None);
    }
}
