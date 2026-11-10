//! Ctrl-C and termination requests, and cancelling the remote job when cfmpeg
//! stops waiting for it.

use crate::api::ApiClient;
use crate::error::{CfmpegError, Result};
use console::style;
use std::cell::Cell;
use std::future::Future;

/// A request from outside to stop cfmpeg.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Interrupt {
    /// Ctrl-C (SIGINT).
    CtrlC,
    /// SIGTERM, or Ctrl-Break and a closing console on Windows.
    Terminate,
}

impl Interrupt {
    /// The status shells report for a process stopped by the signal: 128 plus
    /// the signal number.
    pub fn exit_code(self) -> i32 {
        match self {
            Self::CtrlC => 130,
            Self::Terminate => 143,
        }
    }
}

/// Listens for interrupts for as long as it is alive.
///
/// Once a listener exists, Ctrl-C and SIGTERM no longer end the process on
/// their own; whoever holds the listener decides how to stop.
pub struct Interrupts {
    #[cfg(unix)]
    interrupt: tokio::signal::unix::Signal,
    #[cfg(unix)]
    terminate: tokio::signal::unix::Signal,
    #[cfg(windows)]
    ctrl_c: tokio::signal::windows::CtrlC,
    #[cfg(windows)]
    ctrl_break: tokio::signal::windows::CtrlBreak,
    #[cfg(windows)]
    ctrl_close: tokio::signal::windows::CtrlClose,
}

impl Interrupts {
    #[cfg(unix)]
    pub fn listen() -> std::io::Result<Self> {
        use tokio::signal::unix::{signal, SignalKind};

        Ok(Self {
            interrupt: signal(SignalKind::interrupt())?,
            terminate: signal(SignalKind::terminate())?,
        })
    }

    #[cfg(windows)]
    pub fn listen() -> std::io::Result<Self> {
        use tokio::signal::windows::{ctrl_break, ctrl_c, ctrl_close};

        Ok(Self {
            ctrl_c: ctrl_c()?,
            ctrl_break: ctrl_break()?,
            ctrl_close: ctrl_close()?,
        })
    }

    /// Waits for the next interrupt. Cancel safe: an interrupt that arrives
    /// while nobody is waiting is delivered to the next call.
    #[cfg(unix)]
    pub async fn next(&mut self) -> Interrupt {
        tokio::select! {
            _ = self.interrupt.recv() => Interrupt::CtrlC,
            _ = self.terminate.recv() => Interrupt::Terminate,
        }
    }

    /// Waits for the next interrupt. Cancel safe: an interrupt that arrives
    /// while nobody is waiting is delivered to the next call.
    #[cfg(windows)]
    pub async fn next(&mut self) -> Interrupt {
        tokio::select! {
            _ = self.ctrl_c.recv() => Interrupt::CtrlC,
            _ = self.ctrl_break.recv() => Interrupt::Terminate,
            _ = self.ctrl_close.recv() => Interrupt::Terminate,
        }
    }
}

/// Runs the client side of a remote job and cancels the job on the server when
/// cfmpeg gives up on it before it has finished: on an interrupt, when the
/// wait times out, and on any other client-side failure such as a failed
/// upload or a status poll that keeps failing.
///
/// A job that has ended on the server is left alone: `work` sets `completed`
/// once the job has completed, and a job that failed or was cancelled remotely
/// is reported as [`CfmpegError::JobFailed`].
///
/// `work` is dropped before the job is cancelled, which removes partial
/// downloads and stops helper processes.
pub async fn run_remote_work<T>(
    api: &ApiClient,
    job_id: &str,
    interrupts: &mut Interrupts,
    completed: &Cell<bool>,
    work: impl Future<Output = Result<T>>,
) -> Result<T> {
    let outcome = {
        tokio::pin!(work);

        tokio::select! {
            result = &mut work => result,
            interrupt = interrupts.next() => Err(CfmpegError::Interrupted(interrupt)),
        }
    };

    let still_running = match &outcome {
        Ok(_) | Err(CfmpegError::JobFailed(_)) => false,
        Err(_) => !completed.get(),
    };
    if still_running {
        cancel_remote_job(api, job_id, interrupts).await;
    }

    outcome
}

/// Asks the server to cancel `job_id` and reports the outcome in one line on
/// stderr. A second interrupt gives up on the request.
pub async fn cancel_remote_job(api: &ApiClient, job_id: &str, interrupts: &mut Interrupts) {
    let warning = style("warning:").yellow().bold();
    let manually = format!("cancel it with `cfmpeg cancel {job_id}`");

    let notice = tokio::select! {
        result = api.cancel_job(job_id) => match result {
            Ok(_) => format!("{} cancelled remote job {job_id}", style("->").cyan()),
            // The server answers 409 for a job that has already finished.
            Err(CfmpegError::Api { status: 409, message, .. }) => format!(
                "{} remote job {job_id} had already finished; nothing to cancel ({message})",
                style("->").cyan()
            ),
            Err(error) => {
                format!("{warning} could not cancel remote job {job_id} ({error}); {manually}")
            }
        },
        _ = interrupts.next() => format!(
            "{warning} interrupted before remote job {job_id} was cancelled; {manually}"
        ),
    };

    eprintln!("  {notice}");
}
