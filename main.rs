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

use api::{ApiClient, CreateJobRequest, JobInput};
use cli::{AuthAction, Cli, Commands, ConfigAction};
use config::Config;
use console::style;
use error::{CfmpegError, Result};
use parser::Input;
use reqwest::Client;

use clap::Parser;
use std::process;

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("  {} {}", style("error:").red().bold(), e);
        process::exit(1);
    }
}

async fn run() -> Result<()> {
    let raw_args: Vec<String> = std::env::args().skip(1).collect();

    // If args look like ffmpeg passthrough (start with - or aren't a subcommand),
    // skip clap parsing and go straight to the encode flow.
    if cli::is_ffmpeg_passthrough(&raw_args) {
        return run_encode(&raw_args, false).await;
    }

    // Otherwise, parse with clap for subcommands.
    let cli = Cli::parse();

    // --codecs: query remote for available codecs
    if cli.codecs {
        return show_remote_codecs().await;
    }

    match cli.command {
        Some(Commands::Auth { action }) => {
            let mut config = Config::load()?;
            match action {
                AuthAction::Login => auth::login(&mut config).await,
                AuthAction::Status => {
                    auth::status(&config);
                    Ok(())
                }
                AuthAction::Logout => auth::logout(&mut config),
            }
        }

        Some(Commands::Usage) => show_usage().await,

        Some(Commands::Config { action }) => {
            match action {
                Some(ConfigAction::Path) => {
                    let path = Config::config_path()?;
                    println!("{}", path.display());
                }
                Some(ConfigAction::Show) => {
                    let config = Config::load()?;
                    let toml = toml::to_string_pretty(&config)
                        .map_err(|e| CfmpegError::Config(e.to_string()))?;
                    println!("{}", toml);
                }
                Some(ConfigAction::Edit) => {
                    let path = Config::config_path()?;
                    // Ensure file exists
                    if !path.exists() {
                        let config = Config::default();
                        config.save()?;
                    }
                    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "nano".into());
                    let status = std::process::Command::new(&editor)
                        .arg(&path)
                        .status()?;
                    if !status.success() {
                        return Err(CfmpegError::Config(format!(
                            "{} exited with code {}",
                            editor,
                            status.code().unwrap_or(-1)
                        )));
                    }
                }
                None => {
                    let path = Config::config_path()?;
                    println!("{}", path.display());
                }
            }
            Ok(())
        }

        // No subcommand but has ffmpeg args (e.g., `cfmpeg --local -i ...`)
        None if !cli.ffmpeg_args.is_empty() => {
            run_encode(&cli.ffmpeg_args, cli.local).await
        }

        None => {
            // No args at all — print help
            use clap::CommandFactory;
            Cli::command().print_help().ok();
            println!();
            Ok(())
        }
    }
}

/// Core encode workflow: parse args → upload → run remote → download.
async fn run_encode(ffmpeg_args: &[String], force_local: bool) -> Result<()> {
    let config = Config::load()?;

    // --local flag: run locally
    if force_local {
        return fallback::run_local(ffmpeg_args).await;
    }

    // Parse the ffmpeg arguments to identify inputs and outputs
    let parsed = parser::parse_ffmpeg_args(ffmpeg_args)?;

    // Build the API client
    let api = match ApiClient::from_config(&config) {
        Ok(api) => api,
        Err(CfmpegError::NotAuthenticated) => {
            if config.local_fallback {
                eprintln!(
                    "  {} Not authenticated. Falling back to local ffmpeg.",
                    style("⚠").yellow()
                );
                eprintln!("  Run {} to use cloud encoding.", style("cfmpeg auth login").cyan());
                eprintln!();
                return fallback::run_local(ffmpeg_args).await;
            }
            return Err(CfmpegError::NotAuthenticated);
        }
        Err(e) => return Err(e),
    };

    // Check API health
    if !api.health_check().await {
        if config.local_fallback {
            eprintln!(
                "  {} API unreachable. Falling back to local ffmpeg.",
                style("⚠").yellow()
            );
            eprintln!();
            return fallback::run_local(ffmpeg_args).await;
        }
        return Err(CfmpegError::ApiUnreachable(config.api_base));
    }

    let http_client = Client::builder()
        .user_agent(format!("cfmpeg/{}", env!("CARGO_PKG_VERSION")))
        .build()?;

    // ── Step 1: Create job ──────────────────────────────────────────

    let job_inputs: Vec<JobInput> = parsed
        .inputs
        .iter()
        .map(|input| match input {
            Input::LocalFile { path, size } => JobInput {
                filename: path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("input")
                    .to_string(),
                size_bytes: *size,
                source: "local".to_string(),
                url: None,
            },
            Input::Url(url) => JobInput {
                filename: url.clone(),
                size_bytes: 0,
                source: "url".to_string(),
                url: Some(url.clone()),
            },
            Input::Special(s) => JobInput {
                filename: s.clone(),
                size_bytes: 0,
                source: "special".to_string(),
                url: None,
            },
        })
        .collect();

    let output_names: Vec<String> = parsed
        .outputs
        .iter()
        .map(|o| {
            o.path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("output")
                .to_string()
        })
        .collect();

    let create_request = CreateJobRequest {
        ffmpeg_args: parsed.sandbox_args.clone(),
        inputs: job_inputs,
        outputs: output_names,
    };

    let job = api.create_job(&create_request).await?;
    let job_id = &job.job_id;

    eprintln!(
        "  {} Job {} created.",
        style("→").cyan(),
        style(job_id).dim()
    );

    // ── Step 2: Upload local inputs ─────────────────────────────────

    let local_files: Vec<_> = parsed
        .inputs
        .iter()
        .filter_map(|input| match input {
            Input::LocalFile { path, .. } => Some(path.clone()),
            _ => None,
        })
        .collect();

    if !local_files.is_empty() {
        eprintln!();
        for (i, file_path) in local_files.iter().enumerate() {
            if i < job.uploads.len() {
                upload::upload_file(&http_client, file_path, &job.uploads[i]).await?;
            }
        }
        eprintln!();
    }

    // ── Step 3: Start processing ────────────────────────────────────

    api.start_job(job_id).await?;

    // ── Step 4: Wait for completion ─────────────────────────────────

    job::wait_for_completion(&api, &http_client, job_id).await?;

    // ── Step 5: Download outputs ────────────────────────────────────

    let outputs_response = api.get_outputs(job_id).await?;

    eprintln!();
    download::download_outputs(&http_client, &outputs_response.outputs, &parsed.outputs).await?;
    eprintln!();

    // ── Done ────────────────────────────────────────────────────────

    let total_cost = "—"; // TODO: include cost from job completion response
    eprintln!(
        "  {} Complete. Cost: {}",
        style("✓").green().bold(),
        style(total_cost).dim()
    );
    eprintln!();

    Ok(())
}

/// Show the remote ffmpeg version and codecs.
async fn show_remote_codecs() -> Result<()> {
    let config = Config::load()?;
    let api = ApiClient::from_config(&config)?;

    // TODO: Implement /codecs endpoint on the API
    println!("  Remote ffmpeg version and codecs are not yet available.");
    println!("  This feature is coming soon.");

    Ok(())
}

/// Show usage for the current billing period.
async fn show_usage() -> Result<()> {
    let config = Config::load()?;
    let api = ApiClient::from_config(&config)?;

    let usage = api.get_usage().await?;

    println!();
    println!("  {} Current Billing Period", style("cfmpeg").cyan().bold());
    println!("  {} to {}", usage.period_start, usage.period_end);
    println!();
    println!(
        "  CPU encoding:  {:.1} minutes",
        usage.cpu_minutes
    );
    println!(
        "  GPU encoding:  {:.1} minutes",
        usage.gpu_minutes
    );
    println!(
        "  Total jobs:    {}",
        usage.jobs_count
    );
    println!(
        "  Total cost:    ${:.2}",
        usage.total_cost_cents as f64 / 100.0
    );
    println!();

    Ok(())
}
