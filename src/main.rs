mod api;
mod auth;
mod cli;
mod config;
mod download;
mod error;
mod fallback;
mod job;
mod media_tools;
mod parser;
mod remote;
mod stream;
mod upload;

use crate::api::{
    ApiClient, CompleteIngestRequest, CompletedMultipartUpload, CreateJobRequest, JobInput,
    OutputDeliveryRequest, StartJobRequest, UsageResponse,
};
use crate::cli::{AuthAction, Command, ConfigAction};
use crate::config::Config;
use crate::error::{CfmpegError, Result};
use crate::parser::Input;
use console::style;
use reqwest::Client;
use serde::Serialize;
use std::process;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

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
        Command::Cancel { job_id } => cancel_job(&job_id).await,
        Command::Config(action) => handle_config_command(action),
        Command::Encode {
            ffmpeg_args,
            force_local,
            no_download,
            remote,
        } => run_encode(&ffmpeg_args, force_local, no_download, remote).await,
        Command::Help {
            topic,
            exit_success,
        } => {
            cli::print_help(topic);
            if exit_success {
                Ok(())
            } else {
                Err(CfmpegError::ParseError(
                    "no command or ffmpeg arguments provided".to_string(),
                ))
            }
        }
        Command::Usage => show_usage().await,
        Command::Version => show_version().await,
    }
}

async fn cancel_job(job_id: &str) -> Result<()> {
    let config = Config::load()?;
    let api = ApiClient::from_config(&config)?;
    let status = api.cancel_job(job_id).await?;

    println!("{} {}", style("cancelled").yellow().bold(), status.job_id);

    Ok(())
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
                remote_profile: config.remote_profile.clone(),
                remote_cpu: config.remote_cpu,
                remote_memory_mb: config.remote_memory_mb,
                remote_timeout_seconds: config.remote_timeout_seconds,
            };

            let contents = toml::to_string_pretty(&display_config).map_err(|error| {
                CfmpegError::Config(format!("failed to render config: {error}"))
            })?;
            println!("{contents}");
            Ok(())
        }
    }
}

async fn run_encode(
    ffmpeg_args: &[String],
    force_local: bool,
    no_download: bool,
    remote: remote::RemoteExecutionOptions,
) -> Result<()> {
    let config = Config::load()?;
    let effective_remote = remote.merge_defaults(&config.remote_execution_defaults()?);

    if force_local {
        eprintln!("  warning: running locally because --local was provided.");
        return fallback::run_local(ffmpeg_args).await;
    }

    let parsed = parser::parse_ffmpeg_args(ffmpeg_args)?;
    let api = match ApiClient::from_config(&config) {
        Ok(api) => api,
        Err(CfmpegError::NotAuthenticated) if allows_local_fallback(&config, &effective_remote) => {
            eprintln!("  warning: not authenticated; falling back to local ffmpeg.");
            eprintln!("  run `cfmpeg auth login` to use cloud execution.");
            eprintln!();
            return fallback::run_local(ffmpeg_args).await;
        }
        Err(error) => return Err(error),
    };

    if let Err(error) = api.health_check().await {
        if allows_local_fallback(&config, &effective_remote) {
            eprintln!("  warning: {error}; falling back to local ffmpeg.");
            if !effective_remote.is_empty() {
                eprintln!(
                    "  note: ignoring requested remote execution settings during local fallback."
                );
            }
            eprintln!();
            return fallback::run_local(ffmpeg_args).await;
        }

        return Err(error);
    }
    eprintln!(
        "  {} api reachable; using remote execution",
        style("->").cyan()
    );

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
        .map(|output| output.remote_name.clone())
        .collect();

    let create_job_request = CreateJobRequest {
        ffmpeg_args: parsed.sandbox_args.clone(),
        inputs: job_inputs,
        outputs,
        execution: effective_remote.clone(),
        output_delivery: (!no_download && parsed.outputs.len() == 1).then(|| {
            OutputDeliveryRequest {
                mode: "progressive_batches".to_string(),
            }
        }),
    };

    let job = match api.create_job(&create_job_request).await {
        Ok(job) => job,
        Err(error) => {
            if let Some(result) =
                maybe_fallback_to_local(ffmpeg_args, &config, &effective_remote, &error).await
            {
                return result;
            }

            return Err(error);
        }
    };

    eprintln!("  {} job {}", style("->").cyan(), style(&job.job_id).dim());
    eprintln!("  {} starting remote execution", style("->").cyan());

    let local_inputs: Vec<_> = parsed
        .inputs
        .iter()
        .filter_map(|input| match input {
            Input::LocalFile { path, .. } => Some(path.clone()),
            _ => None,
        })
        .collect();

    if !job.ingest.is_direct_stream()
        && !job.ingest.is_segmented_upload()
        && job.uploads.len() < local_inputs.len()
    {
        return Err(CfmpegError::Protocol(
            "api returned fewer upload targets than local inputs".to_string(),
        ));
    }

    if job.ingest.is_direct_stream() {
        let [input_path] = local_inputs.as_slice() else {
            return Err(CfmpegError::Protocol(
                "direct stream jobs require exactly one local input".to_string(),
            ));
        };

        eprintln!(
            "  {} direct streaming enabled for this job",
            style("->").cyan()
        );
        eprintln!();

        let monitor_api = api.clone();
        let monitor_http_client = http_client.clone();
        let monitor_job_id = job.job_id.clone();
        let monitor = tokio::spawn(async move {
            job::wait_for_completion(&monitor_api, &monitor_http_client, &monitor_job_id).await
        });

        if let Err(error) = stream::stream_input(&http_client, &job.ingest, input_path).await {
            monitor.abort();
            let _ = monitor.await;

            return Err(error);
        }

        monitor.await.map_err(|error| {
            CfmpegError::JobFailed(format!("job monitor task failed: {error}"))
        })??;
    } else if job.ingest.is_segmented_upload() {
        let [input_path] = local_inputs.as_slice() else {
            return Err(CfmpegError::Protocol(
                "segmented upload jobs require exactly one local input".to_string(),
            ));
        };
        let _upload_target_url = job.ingest.upload_target_url.as_deref().ok_or_else(|| {
            CfmpegError::Protocol("segmented uploads require an upload target endpoint".to_string())
        })?;

        eprintln!(
            "  {} segmented ingest enabled for this job",
            style("->").cyan()
        );
        let use_progressive_batches = job.output_delivery.is_progressive_batches() && !no_download;

        if use_progressive_batches {
            eprintln!(
                "  {} progressive batch downloads enabled for this job",
                style("->").cyan()
            );
        }
        eprintln!();

        api.prepare_job(&job.job_id).await?;
        let segment_count =
            upload::upload_segmented_file(&api, &http_client, input_path, &job.job_id, &job.ingest)
                .await?;
        eprintln!();
        api.complete_segmented_ingest(&job.job_id, &CompleteIngestRequest { segment_count })
            .await?;

        let stop_downloads = Arc::new(AtomicBool::new(false));
        let progressive_download = if use_progressive_batches {
            let download_api = api.clone();
            let download_client = http_client.clone();
            let job_id = job.job_id.clone();
            let output_path = parsed.outputs[0].path.clone();
            let stop = Arc::clone(&stop_downloads);

            Some(tokio::spawn(async move {
                download::download_progressive_batches(
                    &download_api,
                    &download_client,
                    &job_id,
                    &output_path,
                    stop,
                )
                .await
            }))
        } else {
            None
        };

        let completion_result = job::wait_for_completion(&api, &http_client, &job.job_id).await;
        stop_downloads.store(true, Ordering::Relaxed);

        match (completion_result, progressive_download) {
            (Ok(()), Some(handle)) => {
                handle.await.map_err(|error| {
                    CfmpegError::JobFailed(format!("progressive download task failed: {error}"))
                })??;

                eprintln!();
                print_local_completion(&parsed.outputs);
                return Ok(());
            }
            (Err(error), Some(handle)) => {
                handle.abort();
                let _ = handle.await;
                return Err(error);
            }
            (result, None) => {
                result?;
            }
        }
    } else {
        if !local_inputs.is_empty() {
            if let Err(error) = api.prepare_job(&job.job_id).await {
                eprintln!(
                    "  warning: remote worker warmup failed; continuing with the normal start path."
                );
                eprintln!("  note: {error}");
                eprintln!();
            }

            eprintln!();
            let mut multipart_uploads = Vec::new();

            for (index, path) in local_inputs.iter().enumerate() {
                let upload_result =
                    upload::upload_file(&http_client, path, &job.uploads[index]).await?;

                if !upload_result.multipart_parts.is_empty() {
                    multipart_uploads.push(CompletedMultipartUpload {
                        file_id: job.uploads[index].file_id,
                        parts: upload_result.multipart_parts,
                    });
                }
            }
            eprintln!();

            let start_request = StartJobRequest { multipart_uploads };

            if let Err(error) = api.start_job(&job.job_id, &start_request).await {
                if let Some(result) =
                    maybe_fallback_to_local(ffmpeg_args, &config, &effective_remote, &error).await
                {
                    return result;
                }

                return Err(error);
            }
        } else if let Err(error) = api
            .start_job(&job.job_id, &StartJobRequest::default())
            .await
        {
            if let Some(result) =
                maybe_fallback_to_local(ffmpeg_args, &config, &effective_remote, &error).await
            {
                return result;
            }

            return Err(error);
        }

        job::wait_for_completion(&api, &http_client, &job.job_id).await?;
    }

    let outputs = api.get_outputs(&job.job_id).await?;

    if no_download {
        eprintln!();
        eprintln!(
            "  {} remote outputs are ready. Skipping local download.",
            style("ok").green().bold()
        );
        download::print_output_urls(&outputs.outputs);
        eprintln!();
        eprintln!("  {} complete.", style("ok").green().bold());
    } else {
        eprintln!();
        eprintln!("  {} downloading outputs", style("->").cyan());
        download::download_outputs(&http_client, &outputs.outputs, &parsed.outputs).await?;
        eprintln!();
        print_local_completion(&parsed.outputs);
    }

    Ok(())
}

async fn show_usage() -> Result<()> {
    let config = Config::load()?;
    let api = ApiClient::from_config(&config)?;
    let usage = api.get_usage().await?;

    for line in format_usage_report(&usage) {
        println!("{line}");
    }

    Ok(())
}

fn format_usage_report(usage: &UsageResponse) -> Vec<String> {
    vec![
        "Current billing period".to_string(),
        format!("  {} to {}", usage.period_start, usage.period_end),
        format!("  CPU encoding: {:.1} minutes", usage.cpu_minutes),
        format!("  Total jobs:   {}", usage.jobs_count),
        format!(
            "  Balance:      {}",
            format_money_from_millicents(usage.balance_millicents, &usage.currency)
        ),
        format!(
            "  Total cost:   {}",
            format_money_from_millicents(usage.total_cost_millicents(), &usage.currency)
        ),
    ]
}

fn print_local_completion(outputs: &[parser::Output]) {
    eprintln!("  {} complete.", style("ok").green().bold());

    for line in local_output_summary_lines(outputs) {
        eprintln!("  {} {line}", style("->").cyan());
    }
}

fn local_output_summary_lines(outputs: &[parser::Output]) -> Vec<String> {
    outputs
        .iter()
        .map(|output| {
            let size = std::fs::metadata(&output.path)
                .ok()
                .map(|metadata| format!(" ({})", format_file_size(metadata.len())))
                .unwrap_or_default();

            format!("{}{}", output.path.display(), size)
        })
        .collect()
}

fn format_file_size(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = KIB * 1024;
    const GIB: u64 = MIB * 1024;

    if bytes < KIB {
        format!("{bytes} B")
    } else if bytes < MIB {
        format!("{} KiB", bytes.div_ceil(KIB))
    } else if bytes < GIB {
        format!("{:.1} MiB", bytes as f64 / MIB as f64)
    } else {
        format!("{:.1} GiB", bytes as f64 / GIB as f64)
    }
}

async fn show_version() -> Result<()> {
    println!("cfmpeg {}", env!("CARGO_PKG_VERSION"));

    if let Ok(config) = Config::load() {
        println!("api base {}", config.api_base());
    }

    Ok(())
}

async fn maybe_fallback_to_local(
    ffmpeg_args: &[String],
    config: &Config,
    effective_remote: &remote::RemoteExecutionOptions,
    error: &CfmpegError,
) -> Option<Result<()>> {
    if !allows_local_fallback(config, effective_remote) || !should_fallback_to_local(error) {
        return None;
    }

    match api_error_code(error) {
        Some("insufficient_funds") => {
            eprintln!("  warning: remote balance is too low; falling back to local ffmpeg.");
            eprintln!("  top up at {}", config.dashboard_billing_url());
        }
        _ => {
            eprintln!(
                "  warning: remote job execution is not enabled yet; falling back to local ffmpeg."
            );
        }
    }

    if !effective_remote.is_empty() {
        eprintln!("  note: ignoring requested remote execution settings during local fallback.");
    }

    eprintln!();

    Some(fallback::run_local(ffmpeg_args).await)
}

fn allows_local_fallback(
    config: &Config,
    effective_remote: &remote::RemoteExecutionOptions,
) -> bool {
    config.local_fallback && !effective_remote.requires_strict_remote()
}

fn should_fallback_to_local(error: &CfmpegError) -> bool {
    matches!(
        error,
        CfmpegError::Api { status, code, .. }
            if matches!(status, 402 | 404 | 405 | 501 | 503)
                || code.as_deref() == Some("insufficient_funds")
    )
}

fn api_error_code(error: &CfmpegError) -> Option<&str> {
    match error {
        CfmpegError::Api { code, .. } => code.as_deref(),
        _ => None,
    }
}

fn format_money_from_millicents(value: i64, currency: &str) -> String {
    let sign = if value < 0 { "-" } else { "" };
    let absolute = value.abs();
    let dollars = absolute / 100_000;
    let fractional = absolute % 100_000;

    format!(
        "{sign}{}{:01}.{:05}",
        currency_symbol(currency),
        dollars,
        fractional
    )
}

fn currency_symbol(currency: &str) -> &str {
    match currency.to_ascii_lowercase().as_str() {
        "usd" => "$",
        _ => "",
    }
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
    remote_profile: Option<String>,
    remote_cpu: Option<u16>,
    remote_memory_mb: Option<u32>,
    remote_timeout_seconds: Option<u32>,
}

#[cfg(test)]
mod tests {
    use super::{
        allows_local_fallback, format_file_size, format_money_from_millicents, format_usage_report,
        local_output_summary_lines, should_fallback_to_local,
    };
    use crate::api::UsageResponse;
    use crate::config::Config;
    use crate::error::CfmpegError;
    use crate::parser::Output;
    use crate::remote::RemoteExecutionOptions;
    use std::path::PathBuf;

    #[test]
    fn falls_back_for_insufficient_funds_errors() {
        assert!(should_fallback_to_local(&CfmpegError::Api {
            status: 402,
            code: Some("insufficient_funds".to_string()),
            message: "Insufficient prepaid balance.".to_string(),
        }));
    }

    #[test]
    fn formats_millicent_balances() {
        assert_eq!(format_money_from_millicents(9_833, "usd"), "$0.09833");
        assert_eq!(format_money_from_millicents(-167, "usd"), "-$0.00167");
    }

    #[test]
    fn formats_usage_total_cost_from_millicents() {
        let usage = UsageResponse {
            period_start: "2026-06-01".to_string(),
            period_end: "2026-06-30".to_string(),
            cpu_minutes: 0.5,
            gpu_minutes: 0.0,
            total_cost_cents: 0,
            total_cost_millicents: 369,
            jobs_count: 3,
            balance_millicents: 4_698_347,
            currency: "usd".to_string(),
        };

        let report = format_usage_report(&usage);

        assert!(report.contains(&"  Total cost:   $0.00369".to_string()));
        assert!(report.contains(&"  Balance:      $46.98347".to_string()));
    }

    #[test]
    fn formats_local_output_summary_with_file_size() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let output_path = temp_dir.path().join("output.mp4");
        std::fs::write(&output_path, vec![0u8; 207_820]).expect("write output");

        let lines = local_output_summary_lines(&[Output {
            path: output_path.clone(),
            remote_name: "output.mp4".to_string(),
        }]);

        assert_eq!(lines, vec![format!("{} (203 KiB)", output_path.display())]);
    }

    #[test]
    fn formats_local_output_summary_without_size_when_missing() {
        let lines = local_output_summary_lines(&[Output {
            path: PathBuf::from("/tmp/missing-cfmpeg-output.mp4"),
            remote_name: "output.mp4".to_string(),
        }]);

        assert_eq!(lines, vec!["/tmp/missing-cfmpeg-output.mp4".to_string()]);
    }

    #[test]
    fn formats_file_sizes_for_completion_output() {
        assert_eq!(format_file_size(512), "512 B");
        assert_eq!(format_file_size(1_025), "2 KiB");
        assert_eq!(format_file_size(2 * 1024 * 1024), "2.0 MiB");
    }

    #[test]
    fn allows_local_fallback_for_remote_defaults() {
        let config = Config::default();
        let remote = RemoteExecutionOptions {
            profile: Some("balanced".to_string()),
            ..RemoteExecutionOptions::default()
        };

        assert!(allows_local_fallback(&config, &remote));
    }

    #[test]
    fn strict_remote_requests_disable_local_fallback() {
        let config = Config::default();
        let remote = RemoteExecutionOptions {
            cpu: Some(8),
            strict_remote: true,
            ..RemoteExecutionOptions::default()
        };

        assert!(!allows_local_fallback(&config, &remote));
    }
}
