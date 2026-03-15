use crate::api::{ApiClient, JobProgress, JobState, ProgressEvent};
use crate::error::{CfmpegError, Result};
use futures::StreamExt;
use indicatif::{ProgressBar, ProgressStyle};
use reqwest::Client;
use reqwest_eventsource::{Event, EventSource};
use std::time::Duration;

const JOB_TIMEOUT_SECS: u64 = 3600;
const POLL_INTERVAL: Duration = Duration::from_secs(2);

pub async fn wait_for_completion(
    api: &ApiClient,
    http_client: &Client,
    job_id: &str,
) -> Result<()> {
    let progress = ProgressBar::new(100);
    progress.set_style(
        ProgressStyle::with_template("  Encoding {bar:40.yellow/blue} {pos:>3}% {msg}")
            .expect("progress template")
            .progress_chars("##-"),
    );
    progress.set_message("starting");

    let result = if api.should_stream_progress() {
        stream_progress(api, http_client, job_id, &progress).await
    } else {
        Err(CfmpegError::JobFailed(
            "progress streaming disabled for loopback api hosts".to_string(),
        ))
    };

    match result {
        Ok(()) => {
            progress.finish_with_message("done");
            Ok(())
        }
        Err(_) => poll_progress(api, job_id, &progress).await,
    }
}

async fn stream_progress(
    api: &ApiClient,
    http_client: &Client,
    job_id: &str,
    progress: &ProgressBar,
) -> Result<()> {
    let request = http_client
        .get(api.stream_url(job_id))
        .bearer_auth(api.api_key());
    let mut events = EventSource::new(request).map_err(|error| {
        CfmpegError::JobFailed(format!("unable to start progress stream: {error}"))
    })?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(JOB_TIMEOUT_SECS);

    loop {
        if tokio::time::Instant::now() > deadline {
            return Err(CfmpegError::JobTimeout(JOB_TIMEOUT_SECS));
        }

        match events.next().await {
            Some(Ok(Event::Message(message))) => {
                if let Ok(event) = serde_json::from_str::<ProgressEvent>(&message.data) {
                    update_progress_bar(progress, &event.progress);

                    match event.status {
                        JobState::Completed => return Ok(()),
                        JobState::Failed => {
                            return Err(CfmpegError::JobFailed(
                                "remote job reported failure".to_string(),
                            ));
                        }
                        JobState::Cancelled => {
                            return Err(CfmpegError::JobFailed(
                                "remote job was cancelled".to_string(),
                            ));
                        }
                        _ => {}
                    }
                }
            }
            Some(Ok(Event::Open)) => {}
            Some(Err(error)) => {
                return Err(CfmpegError::JobFailed(format!(
                    "progress stream interrupted: {error}"
                )));
            }
            None => {
                return Err(CfmpegError::JobFailed(
                    "progress stream ended unexpectedly".to_string(),
                ));
            }
        }
    }
}

async fn poll_progress(api: &ApiClient, job_id: &str, progress: &ProgressBar) -> Result<()> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(JOB_TIMEOUT_SECS);

    loop {
        if tokio::time::Instant::now() > deadline {
            return Err(CfmpegError::JobTimeout(JOB_TIMEOUT_SECS));
        }

        let status = api.get_job_status(job_id).await?;
        let _ = &status.job_id;

        match status.status {
            JobState::Completed => {
                progress.set_position(100);
                return Ok(());
            }
            JobState::Failed => {
                return Err(CfmpegError::JobFailed(
                    status
                        .error
                        .unwrap_or_else(|| "remote job failed".to_string()),
                ));
            }
            JobState::Cancelled => {
                return Err(CfmpegError::JobFailed(
                    "remote job was cancelled".to_string(),
                ));
            }
            _ => {
                if let Some(job_progress) = status.progress.as_ref() {
                    update_progress_bar(progress, job_progress);
                }
            }
        }

        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

fn update_progress_bar(progress: &ProgressBar, job_progress: &JobProgress) {
    if let Some(percent) = job_progress.percent {
        progress.set_position(percent.clamp(0.0, 100.0).round() as u64);
    }

    let mut parts = Vec::new();

    if let Some(frame) = job_progress.frame {
        parts.push(format!("frame={frame}"));
    }
    if let Some(fps) = job_progress.fps {
        parts.push(format!("fps={fps:.1}"));
    }
    if let Some(time) = job_progress.time.as_deref() {
        parts.push(format!("time={time}"));
    }
    if let Some(speed) = job_progress.speed.as_deref() {
        parts.push(format!("speed={speed}"));
    }
    if let Some(size_kb) = job_progress.size_kb {
        parts.push(format!("size={}KiB", size_kb));
    }

    if !parts.is_empty() {
        progress.set_message(parts.join(" "));
    }
}
