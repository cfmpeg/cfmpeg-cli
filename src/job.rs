use crate::api::{ApiClient, JobProgress, JobState, ProgressEvent};
use crate::error::{CfmpegError, Result};
use futures::StreamExt;
use indicatif::{ProgressBar, ProgressStyle};
use reqwest::Client;
use reqwest_eventsource::{Event, EventSource};
use std::time::Duration;

const JOB_TIMEOUT_SECS: u64 = 3600;
const POLL_INTERVAL: Duration = Duration::from_secs(2);
const MAX_TRANSIENT_POLL_ERRORS: u8 = 30;

enum StreamProgressOutcome {
    Completed,
    Failed(String),
    Cancelled(String),
}

fn cancelled_message(message: Option<String>) -> String {
    message.unwrap_or_else(|| "remote job was cancelled".to_string())
}

fn terminal_outcome(event: ProgressEvent) -> Option<StreamProgressOutcome> {
    match event.status {
        JobState::Completed => Some(StreamProgressOutcome::Completed),
        JobState::Failed => Some(StreamProgressOutcome::Failed(
            event
                .error
                .unwrap_or_else(|| "remote job failed".to_string()),
        )),
        JobState::Cancelled => Some(StreamProgressOutcome::Cancelled(cancelled_message(
            event.error,
        ))),
        _ => None,
    }
}

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

    if !api.should_stream_progress() {
        return poll_progress(api, job_id, &progress).await;
    }

    match stream_progress(api, http_client, job_id, &progress).await {
        Ok(StreamProgressOutcome::Completed) => {
            progress.finish_with_message("done");
            Ok(())
        }
        Ok(StreamProgressOutcome::Failed(message)) => Err(CfmpegError::JobFailed(message)),
        Ok(StreamProgressOutcome::Cancelled(message)) => Err(CfmpegError::JobFailed(message)),
        Err(_) => poll_progress(api, job_id, &progress).await,
    }
}

fn should_retry_poll_error(error: &CfmpegError) -> bool {
    match error {
        CfmpegError::Http(_) | CfmpegError::ApiUnreachable(_) => true,
        CfmpegError::Api { status, .. } => *status == 429 || *status >= 500,
        _ => false,
    }
}

async fn stream_progress(
    api: &ApiClient,
    http_client: &Client,
    job_id: &str,
    progress: &ProgressBar,
) -> Result<StreamProgressOutcome> {
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

                    if let Some(outcome) = terminal_outcome(event) {
                        return Ok(outcome);
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
    let mut transient_errors = 0u8;

    loop {
        if tokio::time::Instant::now() > deadline {
            return Err(CfmpegError::JobTimeout(JOB_TIMEOUT_SECS));
        }

        let status = match api.get_job_status(job_id).await {
            Ok(status) => {
                transient_errors = 0;
                status
            }
            Err(error) if should_retry_poll_error(&error) => {
                transient_errors = transient_errors.saturating_add(1);

                if transient_errors >= MAX_TRANSIENT_POLL_ERRORS {
                    return Err(error);
                }

                progress.set_message(format!(
                    "retrying status ({transient_errors}/{MAX_TRANSIENT_POLL_ERRORS})"
                ));
                tokio::time::sleep(POLL_INTERVAL).await;
                continue;
            }
            Err(error) => return Err(error),
        };
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
                return Err(CfmpegError::JobFailed(cancelled_message(status.error)));
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
    if let Some(detail) = job_progress.detail.as_deref() {
        parts.push(format!("activity={detail}"));
    } else if let Some(stage) = job_progress.stage.as_deref() {
        parts.push(format!("activity={}", stage.replace('_', " ")));
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

#[cfg(test)]
mod tests {
    use super::{
        cancelled_message, should_retry_poll_error, terminal_outcome, StreamProgressOutcome,
    };
    use crate::api::ProgressEvent;
    use crate::error::CfmpegError;
    use serde_json::json;

    #[test]
    fn uses_streamed_failure_error_message() {
        let event: ProgressEvent = serde_json::from_value(json!({
            "status": "failed",
            "error": "ffmpeg failed: no such filter",
        }))
        .expect("progress event should deserialize");

        let Some(StreamProgressOutcome::Failed(message)) = terminal_outcome(event) else {
            panic!("failed jobs should produce a failed terminal outcome");
        };

        assert_eq!(message, "ffmpeg failed: no such filter");
    }

    #[test]
    fn falls_back_to_generic_cancelled_message() {
        let event: ProgressEvent = serde_json::from_value(json!({
            "status": "cancelled",
        }))
        .expect("progress event should deserialize");

        let Some(StreamProgressOutcome::Cancelled(message)) = terminal_outcome(event) else {
            panic!("cancelled jobs should produce a cancelled terminal outcome");
        };

        assert_eq!(message, "remote job was cancelled");
    }

    #[test]
    fn uses_cancelled_error_message_when_present() {
        let event: ProgressEvent = serde_json::from_value(json!({
            "status": "cancelled",
            "error": "job cancelled by user",
        }))
        .expect("progress event should deserialize");

        let Some(StreamProgressOutcome::Cancelled(message)) = terminal_outcome(event) else {
            panic!("cancelled jobs should produce a cancelled terminal outcome");
        };

        assert_eq!(message, "job cancelled by user");
        assert_eq!(
            cancelled_message(Some("job cancelled by user".to_string())),
            "job cancelled by user"
        );
    }

    #[test]
    fn retries_transient_poll_errors() {
        let error = CfmpegError::Api {
            status: 503,
            code: None,
            message: "upstream unavailable".to_string(),
        };

        assert!(should_retry_poll_error(&error));
    }

    #[test]
    fn does_not_retry_terminal_poll_errors() {
        let error = CfmpegError::Api {
            status: 404,
            code: None,
            message: "missing".to_string(),
        };

        assert!(!should_retry_poll_error(&error));
    }
}
