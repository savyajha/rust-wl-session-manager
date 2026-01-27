mod scanner;
use std::fs;

fn main() {
    let config_content = fs::read_to_string("config.toml")
        .expect("Could not read config.toml");
    let config: scanner::Config = toml::from_str(&config_content)
        .expect("Invalid toml format.");
    println!("--- Exporting Environment to niri.env ---");
    if let Err(e) = scanner::create_env_file(config, "niri.env") {
        eprintln!("Failed to write file: {}", e);
    } else {
        println!("--- Export Complete ---");
    }
}
