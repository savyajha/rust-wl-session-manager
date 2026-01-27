use std::env;
use std::fs::File;
use std::io::{Write, BufWriter};
use serde::Deserialize;

#[derive(Deserialize)]
pub struct Config {
    pub targets: Vec<String>,
}

pub fn print_vars(config: Config) {
    for var_name in config.targets {
        match env::var(&var_name) {
            Ok(value) => println!("{}={}", var_name, value),
            Err(_) => println!("{}: Variable not found.", var_name),
        }
    }
}

pub fn create_env_file(config: Config, filename: &str) -> std::io::Result<()> {
    let file = File::create(filename)?;
    let mut writer = BufWriter::new(file);

    for var_name in config.targets {
        if let Ok(value) = env::var(&var_name) {
                println!("Exporting {}={}", var_name, value);
                writeln!(writer, "{}={}", var_name, value)?;
        } else {
            println!("{}: Variable not found, Skipping...", var_name);
        }
    }
    writer.flush()?;
    Ok(())
}
