mod scanner;
mod systemd_utils;
mod cli;

use std::fs;
use std::sync::LazyLock;
use std::collections::HashSet;
use systemd_utils::*;
use zbus::Connection;
use clap::Parser;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = cli::Cli::parse();
    let config_content = fs::read_to_string(&args.config)?;
    let config: scanner::Config = toml::from_str(&config_content)
        .expect("Invalid toml format.");

    static CLEANUP_VARS: LazyLock<Vec<&'static str>> = LazyLock::new(|| {
        vec![
            "DISPLAY",
            "WAYLAND_DISPLAY",
            "XDG_CURRENT_DESKTOP",
            "NOTIFY_SOCKET",
        ]
    });

    let mut all_targets: HashSet<String> = config.targets.iter().cloned().collect();
    for &var in CLEANUP_VARS.iter() {
        all_targets.insert(var.to_string());
    }

    println!("--- Starting session with config: {:?} ---", args.config);
    let env_list = scanner::export_env_vars(&config);
    let conn = Connection::session().await?;

    let manager = SystemdManagerProxy::new(&conn).await?;

    manager.set_environment(&env_list).await?;
    println!("Starting Compositor");
    manager.start_unit(&config.compositor_service, "replace").await?;
    println!("--- Session started ---");
    let unit_path = manager.get_unit(&config.compositor_service).await?;
    let leader = SessionLeaderProxy::builder(&conn)
        .path(unit_path)?
        .build()
        .await?;

    leader.wait_for_unit_exit().await?;
    println!("--- Stopping session ---");
    manager.stop_unit("graphical-session.target", "replace").await?;
    manager.start_unit(&config.compositor_shutdown, "replace").await?;
    println!("Unsetting environment");
    let targets_vec: Vec<String> = all_targets.into_iter().collect();
    manager.unset_environment(&targets_vec).await?;
    println!("--- Session stopped ---");
    Ok(())
}
