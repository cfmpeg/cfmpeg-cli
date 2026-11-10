use crate::interrupt::Interrupt;
use thiserror::Error;

pub type Result<T> = std::result::Result<T, CfmpegError>;

#[derive(Debug, Error)]
pub enum CfmpegError {
    #[error("api unreachable: {0}")]
    ApiUnreachable(String),
    #[error("api error ({status}): {message}")]
    Api {
        status: u16,
        code: Option<String>,
        message: String,
    },
    #[error("configuration error: {0}")]
    Config(String),
    #[error("download failed for {filename}: {reason}")]
    Download { filename: String, reason: String },
    #[error("http error: {0}")]
    Http(String),
    #[error("input not found: {0}")]
    InputNotFound(String),
    #[error("interrupted")]
    Interrupted(Interrupt),
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    /// The job reached a failed or cancelled state on the server.
    #[error("job failed: {0}")]
    JobFailed(String),
    #[error("cannot download job {job_id}: {reason}")]
    JobNotDownloadable { job_id: String, reason: String },
    #[error("job timed out after {0} seconds")]
    JobTimeout(u64),
    #[error("not authenticated; run `cfmpeg auth login` or set CFMPEG_API_KEY")]
    NotAuthenticated,
    /// Worded like ffmpeg's own refusal, which scripts may already look for.
    #[error("File '{0}' already exists. Exiting.")]
    OutputExists(String),
    /// Worded like ffmpeg's own refusal.
    #[error(
        "Output {output} same as Input #{input_index} - exiting; ffmpeg cannot edit existing files in-place"
    )]
    OutputIsInput { output: String, input_index: usize },
    #[error("cannot write output `{path}`: {reason}")]
    OutputNotWritable { path: String, reason: String },
    #[error("parse error: {0}")]
    ParseError(String),
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error(
        "remote execution is unavailable: {reason}; pass `--allow-fallback` to run with local ffmpeg when this happens, or `--local` to always run locally"
    )]
    RemoteUnavailable { reason: String },
    #[error(
        "cfmpeg does not recognise the ffmpeg option `{0}`; if it is a valid ffmpeg 8 option, please report it at {repository}/issues, and meanwhile run this command with `--local`",
        repository = env!("CARGO_PKG_REPOSITORY")
    )]
    UnknownOption(String),
    #[error(
        "`{option}` is not supported for remote execution yet: {reason}; run this command with `--local`"
    )]
    UnsupportedOption { option: String, reason: String },
    #[error(
        "output `{output}` is not supported for remote execution yet: {reason}; run this command with `--local`"
    )]
    UnsupportedOutput { output: String, reason: String },
    #[error("upload failed for {filename}: {reason}")]
    Upload { filename: String, reason: String },
}

impl CfmpegError {
    /// The process exit status this error ends the CLI with.
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Interrupted(interrupt) => interrupt.exit_code(),
            Self::ApiUnreachable(_)
            | Self::Api { .. }
            | Self::Config(_)
            | Self::Download { .. }
            | Self::Http(_)
            | Self::InputNotFound(_)
            | Self::Io(_)
            | Self::JobFailed(_)
            | Self::JobNotDownloadable { .. }
            | Self::JobTimeout(_)
            | Self::NotAuthenticated
            | Self::OutputExists(_)
            | Self::OutputIsInput { .. }
            | Self::OutputNotWritable { .. }
            | Self::ParseError(_)
            | Self::Protocol(_)
            | Self::RemoteUnavailable { .. }
            | Self::UnknownOption(_)
            | Self::UnsupportedOption { .. }
            | Self::UnsupportedOutput { .. }
            | Self::Upload { .. } => 1,
        }
    }
}

impl From<reqwest::Error> for CfmpegError {
    fn from(error: reqwest::Error) -> Self {
        Self::Http(describe_http_error(&error))
    }
}

pub fn describe_http_error(error: &reqwest::Error) -> String {
    let mut parts = vec![error.to_string()];
    let mut source = std::error::Error::source(error);

    while let Some(error) = source {
        let message = error.to_string();

        if parts.last() != Some(&message) {
            parts.push(message);
        }

        source = std::error::Error::source(error);
    }

    parts.join("; caused by: ")
}
