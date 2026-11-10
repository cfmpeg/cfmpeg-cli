use crate::error::{CfmpegError, Result};
use crate::interrupt::{Interrupt, Interrupts};
use crate::media_tools;
use std::process::Stdio;
use tokio::process::Command;

/// Runs the user's command with local ffmpeg and returns ffmpeg's exit code.
pub async fn run_local(ffmpeg_args: &[String]) -> Result<i32> {
    let ffmpeg_binary = media_tools::user_ffmpeg_binary()?;
    // Holding the listener keeps cfmpeg alive on Ctrl-C, so that it can report
    // the exit code ffmpeg ends with.
    let mut interrupts = Interrupts::listen()?;
    let mut child = Command::new(ffmpeg_binary)
        .args(ffmpeg_args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()?;

    let status = loop {
        tokio::select! {
            status = child.wait() => break status?,
            interrupt = interrupts.next() => match interrupt {
                // The terminal sends Ctrl-C to ffmpeg as well; ffmpeg finishes
                // the file it is writing and exits on its own.
                Interrupt::CtrlC => {}
                // A termination request reaches cfmpeg alone.
                Interrupt::Terminate => {
                    child.kill().await?;
                    return Err(CfmpegError::Interrupted(interrupt));
                }
            },
        }
    };

    if let Some(code) = status.code() {
        return Ok(code);
    }

    // No exit code means a signal ended ffmpeg; shells report that as 128 plus
    // the signal number.
    #[cfg(unix)]
    if let Some(signal) = std::os::unix::process::ExitStatusExt::signal(&status) {
        return Ok(128 + signal);
    }

    Ok(1)
}
