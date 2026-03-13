mod api;
mod auth;
mod cli;
mod config;
mod download;
mod error;
mod fallback;
mod job;
mod parser;
mod upload;

use crate::api::{ApiClient, CreateJobRequest, JobInput};
use crate::cli::{AuthAction, Command, ConfigAction};
use crate::config::Config;
use crate::error::{CfmpegError, Result};
use crate::parser::Input;
use console::style;
use reqwest::Client;
use serde::Serialize;
use std::process;

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("  {} {}", style("error:").red().bold(), error);
        process::exit(1);
    }
}

async fn run() -> Result<()> {
    let raw_args: Vec<String> = std::env::args().skip(1).collect();

    match cli::parse_args(raw_args)? {
        Command::Auth(action) => {
            let mut config = Config::load()?;

            match action {
                AuthAction::Login => auth::login(&mut config).await,
                AuthAction::Logout => auth::logout(&mut config),
                AuthAction::Status => {
                    auth::status(&config);
                    Ok(())
                }
            }
        }
        Command::Codecs => show_remote_codecs().await,
        Command::Config(action) => handle_config_command(action),
        Command::Encode {
            ffmpeg_args,
            force_local,
        } => run_encode(&ffmpeg_args, force_local).await,
        Command::Help => {
            cli::print_help();
            Ok(())
        }
        Command::Usage => show_usage().await,
        Command::Version => show_version().await,
    }
}

fn handle_config_command(action: Option<ConfigAction>) -> Result<()> {
    match action {
        None | Some(ConfigAction::Path) => {
            println!("{}", Config::config_path()?.display());
            Ok(())
        }
        Some(ConfigAction::Edit) => {
            let path = Config::config_path()?;
            if !path.exists() {
                Config::default().save()?;
            }

            let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".to_string());
            let status = std::process::Command::new(&editor).arg(&path).status()?;

            if status.success() {
                Ok(())
            } else {
                Err(CfmpegError::Config(format!(
                    "{editor} exited with status {}",
                    status
                        .code()
                        .map_or_else(|| "unknown".to_string(), |code| code.to_string())
                )))
            }
        }
        Some(ConfigAction::Show) => {
            let config = Config::load()?;
            let display_config = DisplayConfig {
                api_key: config.api_key().map(|value| mask_secret(&value)),
                api_base: config.api_base(),
                local_fallback: config.local_fallback,
            };

            let contents = toml::to_string_pretty(&display_config).map_err(|error| {
                CfmpegError::Config(format!("failed to render config: {error}"))
            })?;
            println!("{contents}");
            Ok(())
        }
    }
}

async fn run_encode(ffmpeg_args: &[String], force_local: bool) -> Result<()> {
    let config = Config::load()?;

    if force_local {
        eprintln!("  warning: running locally because --local was provided.");
        return fallback::run_local(ffmpeg_args).await;
    }

    let parsed = parser::parse_ffmpeg_args(ffmpeg_args)?;
    let api = match ApiClient::from_config(&config) {
        Ok(api) => api,
        Err(CfmpegError::NotAuthenticated) if config.local_fallback => {
            eprintln!("  warning: not authenticated; falling back to local ffmpeg.");
            eprintln!("  run `cfmpeg auth login` to use cloud execution.");
            eprintln!();
            return fallback::run_local(ffmpeg_args).await;
        }
        Err(error) => return Err(error),
    };

    if !api.health_check().await {
        if config.local_fallback {
            eprintln!("  warning: api unreachable; falling back to local ffmpeg.");
            eprintln!();
            return fallback::run_local(ffmpeg_args).await;
        }

        return Err(CfmpegError::ApiUnreachable(config.api_base()));
    }

    let http_client = Client::builder()
        .user_agent(format!("cfmpeg/{}", env!("CARGO_PKG_VERSION")))
        .build()?;

    let job_inputs: Vec<JobInput> = parsed
        .inputs
        .iter()
        .map(|input| match input {
            Input::LocalFile { path, size } => JobInput {
                filename: path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("input")
                    .to_string(),
                size_bytes: *size,
                source: "local".to_string(),
                url: None,
            },
            Input::Special(value) => JobInput {
                filename: value.clone(),
                size_bytes: 0,
                source: "special".to_string(),
                url: None,
            },
            Input::Url(url) => JobInput {
                filename: url.clone(),
                size_bytes: 0,
                source: "url".to_string(),
                url: Some(url.clone()),
            },
        })
        .collect();

    let outputs: Vec<String> = parsed
        .outputs
        .iter()
        .map(|output| {
            output
                .path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("output")
                .to_string()
        })
        .collect();

    let job = api
        .create_job(&CreateJobRequest {
            ffmpeg_args: parsed.sandbox_args.clone(),
            inputs: job_inputs,
            outputs,
        })
        .await?;

    eprintln!("  {} job {}", style("->").cyan(), style(&job.job_id).dim());

    let local_inputs: Vec<_> = parsed
        .inputs
        .iter()
        .filter_map(|input| match input {
            Input::LocalFile { path, .. } => Some(path.clone()),
            _ => None,
        })
        .collect();

    if job.uploads.len() < local_inputs.len() {
        return Err(CfmpegError::Protocol(
            "api returned fewer upload targets than local inputs".to_string(),
        ));
    }

    if !local_inputs.is_empty() {
        eprintln!();
        for (index, path) in local_inputs.iter().enumerate() {
            upload::upload_file(&http_client, path, &job.uploads[index]).await?;
        }
        eprintln!();
    }

    api.start_job(&job.job_id).await?;
    job::wait_for_completion(&api, &http_client, &job.job_id).await?;

    let outputs = api.get_outputs(&job.job_id).await?;

    eprintln!();
    download::download_outputs(&http_client, &outputs.outputs, &parsed.outputs).await?;
    eprintln!();
    eprintln!("  {} complete.", style("ok").green().bold());

    Ok(())
}

async fn show_remote_codecs() -> Result<()> {
    let config = Config::load()?;
    let api = ApiClient::from_config(&config)?;

    if !api.health_check().await {
        return Err(CfmpegError::ApiUnreachable(config.api_base()));
    }

    println!("Remote ffmpeg codec listing is not implemented yet.");
    Ok(())
}

async fn show_usage() -> Result<()> {
    let config = Config::load()?;
    let api = ApiClient::from_config(&config)?;
    let usage = api.get_usage().await?;

    println!("Current billing period");
    println!("  {} to {}", usage.period_start, usage.period_end);
    println!("  CPU encoding: {:.1} minutes", usage.cpu_minutes);
    println!("  GPU encoding: {:.1} minutes", usage.gpu_minutes);
    println!("  Total jobs:   {}", usage.jobs_count);
    println!(
        "  Total cost:   ${:.2}",
        usage.total_cost_cents as f64 / 100.0
    );

    Ok(())
}

async fn show_version() -> Result<()> {
    println!("cfmpeg {}", env!("CARGO_PKG_VERSION"));

    if let Ok(config) = Config::load() {
        println!("api base {}", config.api_base());
    }

    Ok(())
}

fn mask_secret(secret: &str) -> String {
    if secret.len() <= 8 {
        return "*".repeat(secret.len());
    }

    format!("{}...{}", &secret[..4], &secret[secret.len() - 4..])
}

#[derive(Serialize)]
struct DisplayConfig {
    api_key: Option<String>,
    api_base: String,
    local_fallback: bool,
}
