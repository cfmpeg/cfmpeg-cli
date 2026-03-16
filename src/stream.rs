use crate::api::{parse_error_response, JobIngest};
use crate::error::{CfmpegError, Result};
use reqwest::header::CONTENT_TYPE;
use reqwest::Client;
use std::path::Path;
use std::process::Stdio;
use tokio::io::{AsyncReadExt, BufReader};
use tokio::process::Command;
use tokio_util::io::ReaderStream;

pub async fn stream_input(client: &Client, ingest: &JobIngest, input_path: &Path) -> Result<()> {
    let stream_url = ingest.stream_url.as_deref().ok_or_else(|| {
        CfmpegError::Protocol("direct stream jobs require a stream_url".to_string())
    })?;
    let claim_url = ingest.claim_url.as_deref().ok_or_else(|| {
        CfmpegError::Protocol("direct stream jobs require a claim_url".to_string())
    })?;
    let input_format = ingest.input_format.as_deref().ok_or_else(|| {
        CfmpegError::Protocol("direct stream jobs require an input_format".to_string())
    })?;
    let stream_strategy = ingest.stream_strategy.as_deref().ok_or_else(|| {
        CfmpegError::Protocol("direct stream jobs require a stream_strategy".to_string())
    })?;

    let content_type = stream_content_type(input_format);

    match stream_strategy {
        "passthrough" => {
            let file = tokio::fs::File::open(input_path).await?;
            let body = reqwest::Body::wrap_stream(ReaderStream::new(file));

            handle_stream_response(client, stream_url, claim_url, content_type, body).await
        }
        "copy_remux" => {
            let mut child = Command::new("ffmpeg")
                .arg("-hide_banner")
                .arg("-loglevel")
                .arg("error")
                .arg("-i")
                .arg(input_path)
                .arg("-c")
                .arg("copy")
                .arg("-f")
                .arg(input_format)
                .arg("pipe:1")
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()?;

            let stdout = child.stdout.take().ok_or_else(|| {
                CfmpegError::Protocol("ffmpeg remux process did not expose stdout".to_string())
            })?;
            let stderr = child.stderr.take().ok_or_else(|| {
                CfmpegError::Protocol("ffmpeg remux process did not expose stderr".to_string())
            })?;

            let stderr_task = tokio::spawn(async move {
                let mut reader = BufReader::new(stderr);
                let mut output = String::new();
                reader.read_to_string(&mut output).await?;

                Ok::<String, std::io::Error>(output)
            });

            let body = reqwest::Body::wrap_stream(ReaderStream::new(stdout));
            let response =
                handle_stream_response(client, stream_url, claim_url, content_type, body).await;

            let status = child.wait().await?;
            let stderr_output = stderr_task.await.map_err(|error| {
                CfmpegError::JobFailed(format!("local remux stderr task failed: {error}"))
            })??;

            if !status.success() && response.is_ok() {
                let detail = stderr_output
                    .lines()
                    .rev()
                    .find(|line| !line.trim().is_empty())
                    .map(str::trim)
                    .unwrap_or("ffmpeg remux failed");

                return Err(CfmpegError::JobFailed(format!(
                    "local remux failed before upload completed: {detail}"
                )));
            }

            response
        }
        _ => Err(CfmpegError::Protocol(format!(
            "unsupported direct stream strategy: {stream_strategy}"
        ))),
    }
}

async fn handle_stream_response(
    client: &Client,
    stream_url: &str,
    claim_url: &str,
    content_type: &str,
    body: reqwest::Body,
) -> Result<()> {
    let response = client
        .post(stream_url)
        .header("X-Cfmpeg-Claim-Url", claim_url)
        .header(CONTENT_TYPE, content_type)
        .body(body)
        .send()
        .await?;

    if response.status().is_success() {
        return Ok(());
    }

    let status = response.status();
    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let body = response.text().await.unwrap_or_default();
    let (message, code) = parse_error_response(status.as_u16(), content_type.as_deref(), &body);

    Err(CfmpegError::Api {
        status: status.as_u16(),
        code,
        message,
    })
}

fn stream_content_type(input_format: &str) -> &str {
    match input_format {
        "mpegts" => "video/mp2t",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::stream_content_type;

    #[test]
    fn uses_mpegts_content_type_for_streamed_ts_inputs() {
        assert_eq!(stream_content_type("mpegts"), "video/mp2t");
    }

    #[test]
    fn falls_back_to_octet_stream_for_unknown_formats() {
        assert_eq!(stream_content_type("unknown"), "application/octet-stream");
    }
}
