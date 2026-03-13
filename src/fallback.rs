use crate::error::{CfmpegError, Result};
use std::process::Stdio;
use tokio::process::Command;

pub async fn run_local(ffmpeg_args: &[String]) -> Result<()> {
    let status = Command::new("ffmpeg")
        .args(ffmpeg_args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .await
        .map_err(|error| match error.kind() {
            std::io::ErrorKind::NotFound => CfmpegError::Config(
                "local ffmpeg was not found on PATH; install ffmpeg or remove --local".to_string(),
            ),
            _ => CfmpegError::Io(error),
        })?;

    if status.success() {
        return Ok(());
    }

    Err(CfmpegError::JobFailed(format!(
        "local ffmpeg exited with status {}",
        status
            .code()
            .map_or_else(|| "unknown".to_string(), |code| code.to_string())
    )))
}
