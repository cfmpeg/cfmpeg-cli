use crate::api::{ApiClient, JobProgress, JobState, JobStatus, ProgressEvent};
use crate::error::{CfmpegError, Result};
use futures::StreamExt;
use indicatif::{ProgressBar, ProgressStyle};
use reqwest::Client;
use reqwest_eventsource::{Event, EventSource};
use std::time::Duration;

/// Maximum time to wait for a job to complete (60 minutes).
const JOB_TIMEOUT_SECS: u64 = 3600;

/// Poll interval when SSE is unavailable.
const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Wait for a job to complete, streaming progress in real time.
///
/// Attempts SSE first for real-time updates, falls back to polling.
pub async fn wait_for_completion(
    api: &ApiClient,
    http_client: &Client,
    job_id: &str,
) -> Result<()> {
    let pb = ProgressBar::new(100);
    pb.set_style(
        ProgressStyle::default_bar()
            .template("  Encoding {bar:40.yellow/dim} {percent}%  {msg}")
            .unwrap()
            .progress_chars("██░"),
    );
    pb.set_message("starting...");

    // Try SSE streaming first
    let result = stream_progress(api, http_client, job_id, &pb).await;

    match result {
        Ok(()) => {
            pb.finish_with_message("done ✓");
            Ok(())
        }
        Err(_) => {
            // Fall back to polling
            poll_progress(api, job_id, &pb).await
        }
    }
}

/// Stream job progress via Server-Sent Events.
async fn stream_progress(
    api: &ApiClient,
    http_client: &Client,
    job_id: &str,
    pb: &ProgressBar,
) -> Result<()> {
    let url = api.stream_url(job_id);

    let request = http_client
        .get(&url)
        .bearer_auth(api.api_key())
        .build()?;

    let mut es = EventSource::new(request)
        .map_err(|e| CfmpegError::JobFailed(format!("SSE connection failed: {}", e)))?;

    let timeout = tokio::time::Instant::now() + Duration::from_secs(JOB_TIMEOUT_SECS);

    loop {
        if tokio::time::Instant::now() > timeout {
            return Err(CfmpegError::JobTimeout(JOB_TIMEOUT_SECS));
        }

        match es.next().await {
            Some(Ok(Event::Message(msg))) => {
                if let Ok(event) = serde_json::from_str::<ProgressEvent>(&msg.data) {
                    update_progress_bar(pb, &event.progress);

                    match event.status {
                        JobState::Completed => {
                            pb.finish_with_message("done ✓");
                            return Ok(());
                        }
                        JobState::Failed => {
                            pb.finish_with_message("failed ✗");
                            return Err(CfmpegError::JobFailed(
                                "Encoding failed on remote".into(),
                            ));
                        }
                        _ => {}
                    }
                }
            }
            Some(Ok(Event::Open)) => {
                // Connection established
            }
            Some(Err(_)) => {
                // SSE error — break out and fall back to polling
                return Err(CfmpegError::JobFailed("SSE stream interrupted".into()));
            }
            None => {
                // Stream ended
                break;
            }
        }
    }

    Ok(())
}

/// Poll job status at a fixed interval (fallback when SSE is unavailable).
async fn poll_progress(api: &ApiClient, job_id: &str, pb: &ProgressBar) -> Result<()> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(JOB_TIMEOUT_SECS);

    loop {
        if tokio::time::Instant::now() > deadline {
            return Err(CfmpegError::JobTimeout(JOB_TIMEOUT_SECS));
        }

        let status = api.get_job_status(job_id).await?;

        match status.status {
            JobState::Completed => {
                pb.set_position(100);
                pb.finish_with_message("done ✓");
                return Ok(());
            }
            JobState::Failed => {
                pb.finish_with_message("failed ✗");
                let error_msg = status.error.unwrap_or_else(|| "Unknown error".into());
                return Err(CfmpegError::JobFailed(error_msg));
            }
            JobState::Cancelled => {
                pb.finish_with_message("cancelled");
                return Err(CfmpegError::JobFailed("Job was cancelled".into()));
            }
            _ => {
                if let Some(progress) = &status.progress {
                    update_progress_bar(pb, progress);
                }
            }
        }

        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// Update the progress bar with the latest ffmpeg progress info.
fn update_progress_bar(pb: &ProgressBar, progress: &JobProgress) {
    if let Some(percent) = progress.percent {
        pb.set_position(percent as u64);
    }

    let mut msg_parts = Vec::new();

    if let Some(frame) = progress.frame {
        msg_parts.push(format!("frame={}", frame));
    }
    if let Some(fps) = progress.fps {
        msg_parts.push(format!("fps={:.1}", fps));
    }
    if let Some(time) = &progress.time {
        msg_parts.push(format!("time={}", time));
    }
    if let Some(speed) = &progress.speed {
        msg_parts.push(format!("speed={}", speed));
    }

    if !msg_parts.is_empty() {
        pb.set_message(msg_parts.join(" "));
    }
}
