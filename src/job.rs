use crate::api::{ApiClient, JobProgress, JobState, ProgressEvent};
use crate::error::{CfmpegError, Result};
use futures::StreamExt;
use indicatif::{ProgressBar, ProgressStyle};
use reqwest::Client;
use reqwest_eventsource::{Event, EventSource};
use std::time::Duration;

pub const DEFAULT_JOB_TIMEOUT_SECS: u64 = 3600;
const POLL_INTERVAL: Duration = Duration::from_secs(2);
const MAX_TRANSIENT_POLL_ERRORS: u8 = 30;

fn terminal_result(state: &JobState, error: Option<String>) -> Option<Result<()>> {
    match state {
        JobState::Completed => Some(Ok(())),
        JobState::Failed => Some(Err(CfmpegError::JobFailed(
            error.unwrap_or_else(|| "remote job failed".to_string()),
        ))),
        JobState::Cancelled => Some(Err(CfmpegError::JobFailed(
            error.unwrap_or_else(|| "remote job was cancelled".to_string()),
        ))),
        _ => None,
    }
}

pub async fn wait_for_completion(
    api: &ApiClient,
    http_client: &Client,
    job_id: &str,
    timeout_seconds: u64,
) -> Result<()> {
    let progress = ProgressBar::new(100);
    progress.set_style(
        ProgressStyle::with_template("  Encoding {bar:40.yellow/blue} {pos:>3}% {msg}")
            .expect("progress template")
            .progress_chars("##-"),
    );
    progress.set_message("starting");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_seconds);

    if api.should_stream_progress() {
        if let Some(result) = stream_progress(
            api,
            http_client,
            job_id,
            &progress,
            deadline,
            timeout_seconds,
        )
        .await
        {
            if result.is_ok() {
                progress.finish_with_message("done");
            }
            return result;
        }
    }

    poll_progress(api, job_id, &progress, deadline, timeout_seconds).await
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
    deadline: tokio::time::Instant,
    timeout_seconds: u64,
) -> Option<Result<()>> {
    let request = http_client
        .get(api.stream_url(job_id))
        .bearer_auth(api.api_key());
    let mut events = EventSource::new(request).ok()?;

    loop {
        if tokio::time::Instant::now() > deadline {
            return Some(Err(CfmpegError::JobTimeout(timeout_seconds)));
        }

        match tokio::time::timeout_at(deadline, events.next()).await {
            Err(_) => return Some(Err(CfmpegError::JobTimeout(timeout_seconds))),
            Ok(Some(Ok(Event::Message(message)))) => {
                if let Ok(event) = serde_json::from_str::<ProgressEvent>(&message.data) {
                    update_progress_bar(progress, &event.progress);

                    if let Some(result) = terminal_result(&event.status, event.error) {
                        return Some(result);
                    }
                }
            }
            Ok(Some(Ok(Event::Open))) => {}
            Ok(Some(Err(_))) | Ok(None) => return None,
        }
    }
}

async fn poll_progress(
    api: &ApiClient,
    job_id: &str,
    progress: &ProgressBar,
    deadline: tokio::time::Instant,
    timeout_seconds: u64,
) -> Result<()> {
    let mut transient_errors = 0u8;

    loop {
        if tokio::time::Instant::now() > deadline {
            return Err(CfmpegError::JobTimeout(timeout_seconds));
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

        if let Some(result) = terminal_result(&status.status, status.error) {
            if result.is_ok() {
                progress.set_position(100);
            }
            return result;
        }

        if let Some(job_progress) = status.progress.as_ref() {
            update_progress_bar(progress, job_progress);
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
    use super::{should_retry_poll_error, terminal_result};
    use crate::api::JobState;
    use crate::error::CfmpegError;

    fn failure_message(state: JobState, error: Option<&str>) -> String {
        match terminal_result(&state, error.map(str::to_string)) {
            Some(Err(CfmpegError::JobFailed(message))) => message,
            _ => panic!("{state:?} should produce a failed terminal result"),
        }
    }

    #[test]
    fn uses_reported_failure_error_message() {
        assert_eq!(
            failure_message(JobState::Failed, Some("ffmpeg failed: no such filter")),
            "ffmpeg failed: no such filter"
        );
    }

    #[test]
    fn falls_back_to_generic_cancelled_message() {
        assert_eq!(
            failure_message(JobState::Cancelled, None),
            "remote job was cancelled"
        );
    }

    #[test]
    fn uses_cancelled_error_message_when_present() {
        assert_eq!(
            failure_message(JobState::Cancelled, Some("job cancelled by user")),
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
