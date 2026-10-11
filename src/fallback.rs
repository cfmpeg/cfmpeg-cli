use crate::error::{CfmpegError, Result};
use crate::media_tools;
use std::process::Stdio;
use tokio::process::Command;

pub async fn run_local(ffmpeg_args: &[String]) -> Result<()> {
    let ffmpeg_binary = media_tools::ffmpeg_binary()?;
    let status = Command::new(ffmpeg_binary)
        .args(ffmpeg_args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .await?;

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
