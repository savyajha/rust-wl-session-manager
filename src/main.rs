mod scanner;
mod systemd_utils;
use std::fs;
use systemd_utils::*;
use zbus::Connection;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config_content = fs::read_to_string("config.toml")
        .expect("Could not read config.toml");
    let config: scanner::Config = toml::from_str(&config_content)
        .expect("Invalid toml format.");

    println!("--- Starting session ---");
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
    manager.unset_environment(&env_list).await?;
    println!("--- Session stopped ---");
    Ok(())
}
