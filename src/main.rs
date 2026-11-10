mod api;
mod auth;
mod cli;
mod config;
mod download;
mod error;
mod fallback;
mod ffmpeg_options;
mod interrupt;
mod job;
mod job_download;
mod local_output;
mod local_references;
mod media_tools;
mod output_support;
mod parser;
mod remote;
mod stream;
mod transfer;
mod upload;

use crate::api::{
    ApiClient, CompleteIngestRequest, CompletedMultipartUpload, CreateJobRequest,
    CreateJobResponse, IngestMode, InputSource, JobInput, JobOutputResponse, OutputDeliveryMode,
    OutputDeliveryRequest, StartJobRequest, UsageResponse,
};
use crate::cli::{AuthAction, Command, ConfigAction};
use crate::config::Config;
use crate::error::{CfmpegError, Result};
use crate::interrupt::Interrupts;
use crate::parser::{Input, ParsedCommand};
use crate::remote::RemoteExecutionOptions;
use console::style;
use reqwest::Client;
use serde::Serialize;
use std::cell::Cell;
use std::process;
use std::sync::atomic::{AtomicBool, Ordering};

#[tokio::main]
async fn main() {
    let exit_code = match run().await {
        Ok(exit_code) => exit_code,
        Err(error) => {
            // An interrupt has been reported already, when the remote job was
            // cancelled.
            if !matches!(error, CfmpegError::Interrupted(_)) {
                eprintln!("  {} {}", style("error:").red().bold(), error);
            }
            error.exit_code()
        }
    };

    process::exit(exit_code);
}

/// Runs the command line and returns the process exit code.
async fn run() -> Result<i32> {
    let raw_args: Vec<String> = std::env::args().skip(1).collect();

    match cli::parse_args(raw_args)? {
        Command::Auth(action) => {
            let mut config = Config::load()?;

            match action {
                AuthAction::Login => auth::login(&mut config).await?,
                AuthAction::Logout => auth::logout(&mut config)?,
                AuthAction::Status => auth::status(&config),
            }
        }
        Command::Cancel { job_id } => cancel_job(&job_id).await?,
        Command::Config(action) => handle_config_command(action)?,
        Command::Download {
            job_id,
            paths,
            overwrite,
        } => {
            let outputs = job_download::run(&job_id, &paths, overwrite).await?;
            eprintln!();
            print_local_completion(&outputs);
        }
        Command::Encode {
            ffmpeg_args,
            force_local,
            no_download,
            allow_fallback,
            remote,
        } => {
            return run_encode(
                &ffmpeg_args,
                EncodeOptions {
                    force_local,
                    no_download,
                    allow_fallback,
                    remote,
                },
            )
            .await;
        }
        Command::Help {
            topic,
            exit_success,
        } => {
            cli::print_help(topic);
            if !exit_success {
                return Err(CfmpegError::ParseError(
                    "no command or ffmpeg arguments provided".to_string(),
                ));
            }
        }
        Command::Usage => show_usage().await?,
        Command::Version => show_version().await?,
    }

    Ok(0)
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
                api_key: config.api_key().map(|value| auth::mask_secret(&value)),
                api_base: config.api_base(),
                allow_fallback: config.allow_fallback()?,
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

/// cfmpeg's own flags for an encode run.
struct EncodeOptions {
    force_local: bool,
    no_download: bool,
    allow_fallback: bool,
    remote: RemoteExecutionOptions,
}

/// What to do when remote execution is unavailable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fallback {
    /// Run the command with local ffmpeg.
    Local,
    /// Fail and explain how to allow the fallback. The default.
    Explain,
    /// Fail with the plain error: `--remote` or `--cf-*` require a remote run.
    Never,
}

/// How the client side of a created job ended.
enum JobOutcome {
    Completed,
    /// The API refused to start the job for a reason that allows a fallback.
    /// Nothing has run remotely.
    StartRefused(CfmpegError),
}

async fn run_encode(ffmpeg_args: &[String], options: EncodeOptions) -> Result<i32> {
    let config = Config::load()?;
    let effective_remote = options
        .remote
        .merge_defaults(&config.remote_execution_defaults()?);
    let wait_timeout_seconds = remote_wait_timeout_seconds(&effective_remote);

    if options.force_local {
        eprintln!("  warning: running locally because --local was provided.");
        return fallback::run_local(ffmpeg_args).await;
    }

    let parsed = parser::parse_ffmpeg_args(ffmpeg_args)?;
    if !options.no_download {
        local_output::preflight(&parsed.inputs, &parsed.outputs, parsed.overwrite)?;
    }

    let fallback = fallback_policy(
        options.allow_fallback,
        config.allow_fallback()?,
        &effective_remote,
    );
    // From here on an interrupt is handled by cfmpeg, so that a job that
    // exists on the server is always cancelled.
    let mut interrupts = Interrupts::listen()?;

    let api = match ApiClient::from_config(&config) {
        Ok(api) => api,
        Err(error @ CfmpegError::NotAuthenticated) => {
            return run_locally_or_fail(ffmpeg_args, fallback, &config, &effective_remote, error)
                .await;
        }
        Err(error) => return Err(error),
    };

    let health = tokio::select! {
        health = api.health_check() => health,
        interrupt = interrupts.next() => return Err(CfmpegError::Interrupted(interrupt)),
    };
    if let Err(error) = health {
        return run_locally_or_fail(ffmpeg_args, fallback, &config, &effective_remote, error).await;
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
                filename: transfer::file_name_or(path, "input"),
                size_bytes: *size,
                source: InputSource::Local,
                url: None,
            },
            Input::Special(value) => JobInput {
                filename: value.clone(),
                size_bytes: 0,
                source: InputSource::Special,
                url: None,
            },
            Input::Url(url) => JobInput {
                filename: url.clone(),
                size_bytes: 0,
                source: InputSource::Url,
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
        output_delivery: (!options.no_download && parsed.outputs.len() == 1).then_some(
            OutputDeliveryRequest {
                mode: OutputDeliveryMode::ProgressiveBatches,
            },
        ),
    };

    // Not raced against interrupts: giving up on this request could leave a
    // job on the server whose id cfmpeg never learned.
    let job = match api.create_job(&create_job_request).await {
        Ok(job) => job,
        Err(error) if should_fallback_to_local(&error) => {
            return run_locally_or_fail(ffmpeg_args, fallback, &config, &effective_remote, error)
                .await;
        }
        Err(error) => return Err(error),
    };

    eprintln!("  {} job {}", style("->").cyan(), style(&job.job_id).dim());
    eprintln!("  {} starting remote execution", style("->").cyan());

    // Set once the job has completed on the server. From then on a failure is
    // a failed download of a result that was paid for, never a reason to cancel.
    let job_completed = Cell::new(false);
    let work = run_remote_job(
        &api,
        &http_client,
        &job,
        &parsed,
        options.no_download,
        wait_timeout_seconds,
        &job_completed,
    );
    let outcome =
        interrupt::run_remote_work(&api, &job.job_id, &mut interrupts, &job_completed, work).await;

    match outcome {
        Ok(JobOutcome::Completed) => Ok(0),
        Ok(JobOutcome::StartRefused(error)) => {
            // The job was created but never started; cancelling it makes sure
            // that nothing stays behind.
            interrupt::cancel_remote_job(&api, &job.job_id, &mut interrupts).await;

            run_locally_or_fail(ffmpeg_args, fallback, &config, &effective_remote, error).await
        }
        Err(error) => {
            if job_completed.get() {
                // A second interrupt gives up on listing the outputs.
                let listing = tokio::select! {
                    listing = api.get_outputs(&job.job_id) => Some(listing),
                    _ = interrupts.next() => None,
                };
                for line in remaining_output_lines(&job.job_id, listing) {
                    eprintln!("  {line}");
                }
            }

            Err(error)
        }
    }
}

/// Says how to get the outputs of a completed job when cfmpeg could not
/// deliver them: with `cfmpeg download`, or from the signed URLs directly.
fn remaining_output_lines(job_id: &str, listing: Option<Result<JobOutputResponse>>) -> Vec<String> {
    let mut lines = vec![format!(
        "{} remote job {job_id} completed and was not cancelled; fetch its outputs again with `cfmpeg download {job_id}` while cloud storage still has them",
        style("->").cyan()
    )];

    match listing {
        Some(Ok(listing)) if !listing.outputs.is_empty() => {
            lines.push("or download them from these signed URLs:".to_string());
            lines.extend(
                listing
                    .outputs
                    .iter()
                    .map(|output| format!("{}\t{}", output.filename, output.download_url)),
            );
        }
        Some(Ok(listing)) => {
            lines.push(
                "or download its parts from these signed URLs and join them in this order with ffmpeg's concat demuxer:"
                    .to_string(),
            );
            lines.extend(
                listing
                    .batches
                    .iter()
                    .map(|batch| format!("{}\t{}", batch.filename, batch.download_url)),
            );
        }
        Some(Err(error)) => lines.push(format!("could not list their download URLs: {error}")),
        None => {}
    }

    lines
}

/// Uploads the inputs of a created job, waits for it and delivers its outputs.
async fn run_remote_job(
    api: &ApiClient,
    http_client: &Client,
    job: &CreateJobResponse,
    parsed: &ParsedCommand,
    no_download: bool,
    wait_timeout_seconds: u64,
    job_completed: &Cell<bool>,
) -> Result<JobOutcome> {
    let wait_for_job = || async move {
        let finished =
            job::wait_for_completion(api, http_client, &job.job_id, wait_timeout_seconds).await;
        if finished.is_ok() {
            job_completed.set(true);
        }
        finished
    };
    let local_inputs: Vec<_> = parsed
        .inputs
        .iter()
        .filter_map(|input| match input {
            Input::LocalFile { path, .. } => Some(path.clone()),
            Input::Special(_) | Input::Url(_) => None,
        })
        .collect();

    match job.ingest.mode {
        IngestMode::DirectStream => {
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

            let monitor = wait_for_job();
            let upload = stream::stream_input(http_client, &job.ingest, input_path);
            tokio::pin!(monitor, upload);

            // The job is watched while the input streams. A failed upload ends
            // the wait, and a job that fails first ends the upload.
            tokio::select! {
                uploaded = &mut upload => {
                    uploaded?;
                    monitor.await?;
                }
                finished = &mut monitor => {
                    finished?;
                    upload.await?;
                }
            }
        }
        IngestMode::SegmentedUpload => {
            let [input_path] = local_inputs.as_slice() else {
                return Err(CfmpegError::Protocol(
                    "segmented upload jobs require exactly one local input".to_string(),
                ));
            };

            eprintln!(
                "  {} segmented ingest enabled for this job",
                style("->").cyan()
            );
            let use_progressive_batches =
                job.output_delivery.mode == OutputDeliveryMode::ProgressiveBatches && !no_download;

            if use_progressive_batches {
                eprintln!(
                    "  {} progressive batch downloads enabled for this job",
                    style("->").cyan()
                );
            }
            eprintln!();

            api.prepare_job(&job.job_id).await?;
            let segment_count = upload::upload_segmented_file(
                api,
                http_client,
                input_path,
                &job.job_id,
                &job.ingest,
            )
            .await?;
            eprintln!();
            api.complete_segmented_ingest(&job.job_id, &CompleteIngestRequest { segment_count })
                .await?;

            if use_progressive_batches {
                let stop_downloads = AtomicBool::new(false);
                let download = download::download_progressive_batches(
                    api,
                    http_client,
                    &job.job_id,
                    &parsed.outputs[0].path,
                    &stop_downloads,
                    parsed.overwrite,
                );
                let completion = async {
                    let completed = wait_for_job().await;
                    stop_downloads.store(true, Ordering::Relaxed);
                    completed
                };
                tokio::pin!(download, completion);

                // Batches download while the job runs. A failed job drops the
                // download at once; a completed job lets it fetch the rest.
                let mut downloaded = None;
                let completed = loop {
                    tokio::select! {
                        completed = &mut completion => break completed,
                        result = &mut download, if downloaded.is_none() => {
                            downloaded = Some(result);
                        }
                    }
                };
                completed?;
                match downloaded {
                    Some(result) => result?,
                    None => download.await?,
                }

                eprintln!();
                print_local_completion(&parsed.outputs);
                return Ok(JobOutcome::Completed);
            }

            wait_for_job().await?;
        }
        IngestMode::Durable => {
            if job.uploads.len() < local_inputs.len() {
                return Err(CfmpegError::Protocol(
                    "api returned fewer upload targets than local inputs".to_string(),
                ));
            }

            let mut multipart_uploads = Vec::new();

            if !local_inputs.is_empty() {
                if let Err(error) = api.prepare_job(&job.job_id).await {
                    eprintln!(
                        "  warning: remote worker warmup failed; continuing with the normal start path."
                    );
                    eprintln!("  note: {error}");
                    eprintln!();
                }

                eprintln!();

                for (path, target) in local_inputs.iter().zip(&job.uploads) {
                    let parts = upload::upload_file(http_client, path, target).await?;

                    if !parts.is_empty() {
                        multipart_uploads.push(CompletedMultipartUpload {
                            file_id: target.file_id,
                            parts,
                        });
                    }
                }
                eprintln!();
            }

            match api
                .start_job(&job.job_id, &StartJobRequest { multipart_uploads })
                .await
            {
                Ok(_) => {}
                Err(error) if should_fallback_to_local(&error) => {
                    return Ok(JobOutcome::StartRefused(error));
                }
                Err(error) => return Err(error),
            }

            wait_for_job().await?;
        }
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
        download::download_outputs(
            http_client,
            &outputs.outputs,
            &parsed.outputs,
            parsed.overwrite,
        )
        .await?;
        eprintln!();
        print_local_completion(&parsed.outputs);
    }

    Ok(JobOutcome::Completed)
}

/// Ends a run that cannot execute remotely because of `error`: with local
/// ffmpeg when the fallback is allowed, with an error otherwise.
async fn run_locally_or_fail(
    ffmpeg_args: &[String],
    fallback: Fallback,
    config: &Config,
    effective_remote: &RemoteExecutionOptions,
    error: CfmpegError,
) -> Result<i32> {
    let reason = if api_error_code(&error) == Some("insufficient_funds") {
        format!(
            "remote balance is too low; top up at {}",
            config.dashboard_billing_url()
        )
    } else {
        error.to_string()
    };

    match fallback {
        Fallback::Never => Err(error),
        Fallback::Explain => Err(CfmpegError::RemoteUnavailable { reason }),
        Fallback::Local => {
            eprintln!(
                "  {} remote execution is unavailable ({reason}); running locally with ffmpeg because fallback is allowed.",
                style("warning:").yellow().bold()
            );
            if !effective_remote.is_empty() {
                eprintln!(
                    "  note: ignoring requested remote execution settings during local fallback."
                );
            }
            eprintln!();

            fallback::run_local(ffmpeg_args).await
        }
    }
}

fn fallback_policy(
    allowed_by_flag: bool,
    allowed_by_config: bool,
    effective_remote: &RemoteExecutionOptions,
) -> Fallback {
    if allowed_by_flag || (allowed_by_config && !effective_remote.strict_remote) {
        Fallback::Local
    } else if effective_remote.strict_remote {
        Fallback::Never
    } else {
        Fallback::Explain
    }
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
            format_money_from_millicents(usage.total_cost_millicents, &usage.currency)
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

fn remote_wait_timeout_seconds(effective_remote: &RemoteExecutionOptions) -> u64 {
    effective_remote
        .timeout_seconds
        .map(u64::from)
        .unwrap_or(job::DEFAULT_JOB_TIMEOUT_SECS)
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

#[derive(Serialize)]
struct DisplayConfig {
    api_key: Option<String>,
    api_base: String,
    allow_fallback: bool,
    remote_profile: Option<String>,
    remote_cpu: Option<u16>,
    remote_memory_mb: Option<u32>,
    remote_timeout_seconds: Option<u32>,
}

#[cfg(test)]
mod tests {
    use super::{
        fallback_policy, format_file_size, format_money_from_millicents, format_usage_report,
        local_output_summary_lines, remote_wait_timeout_seconds, should_fallback_to_local,
        Fallback,
    };
    use crate::api::UsageResponse;
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
    fn fallback_is_explained_but_not_taken_by_default() {
        let remote = RemoteExecutionOptions {
            profile: Some("balanced".to_string()),
            ..RemoteExecutionOptions::default()
        };

        assert_eq!(fallback_policy(false, false, &remote), Fallback::Explain);
    }

    #[test]
    fn fallback_is_taken_when_the_flag_or_the_config_allows_it() {
        let remote = RemoteExecutionOptions::default();

        assert_eq!(fallback_policy(true, false, &remote), Fallback::Local);
        assert_eq!(fallback_policy(false, true, &remote), Fallback::Local);
    }

    #[test]
    fn strict_remote_requests_never_fall_back_on_config_alone() {
        let remote = RemoteExecutionOptions {
            cpu: Some(8),
            strict_remote: true,
            ..RemoteExecutionOptions::default()
        };

        assert_eq!(fallback_policy(false, true, &remote), Fallback::Never);
        assert_eq!(fallback_policy(false, false, &remote), Fallback::Never);
    }

    #[test]
    fn remote_wait_timeout_prefers_configured_timeout() {
        let remote = RemoteExecutionOptions {
            timeout_seconds: Some(45),
            ..RemoteExecutionOptions::default()
        };

        assert_eq!(remote_wait_timeout_seconds(&remote), 45);
    }
}
