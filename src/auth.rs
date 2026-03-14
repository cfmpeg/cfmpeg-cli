use crate::config::Config;
use crate::error::{CfmpegError, Result};
use std::io::{self, Write};

pub async fn login(config: &mut Config) -> Result<()> {
    let api_key_url = config.dashboard_api_keys_url();

    println!("Open this page to create an API key:");
    println!("  {api_key_url}");
    println!();

    if let Err(error) = open::that(&api_key_url) {
        eprintln!("warning: failed to open browser automatically: {error}");
    }

    print!("Paste API key: ");
    io::stdout().flush()?;

    let mut api_key = String::new();
    io::stdin().read_line(&mut api_key)?;

    let api_key = api_key.trim().to_string();
    if api_key.is_empty() {
        return Err(CfmpegError::Config("no API key provided".to_string()));
    }

    config.api_key = Some(api_key);
    config.save()?;

    println!();
    println!("Saved API key to {}", Config::config_path()?.display());

    Ok(())
}

pub fn status(config: &Config) {
    match config.api_key() {
        Some(api_key) => {
            let source = if config.api_key_from_env() {
                "environment"
            } else {
                "config"
            };

            println!("Authenticated via {source}: {}", mask_secret(&api_key));
        }
        None => println!("Not authenticated."),
    }

    println!("API base: {}", config.api_base());
}

pub fn logout(config: &mut Config) -> Result<()> {
    config.api_key = None;
    config.save()?;

    println!(
        "Removed saved API key from {}",
        Config::config_path()?.display()
    );

    if config.api_key_from_env() {
        println!("CFMPEG_API_KEY is still set in the environment.");
    }

    Ok(())
}

fn mask_secret(secret: &str) -> String {
    if secret.len() <= 8 {
        return "*".repeat(secret.len());
    }

    format!("{}...{}", &secret[..4], &secret[secret.len() - 4..])
}
