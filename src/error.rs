use thiserror::Error;

pub type Result<T> = std::result::Result<T, CfmpegError>;

#[derive(Debug, Error)]
pub enum CfmpegError {
    #[error("api unreachable: {0}")]
    ApiUnreachable(String),
    #[error("api error ({status}): {message}")]
    Api { status: u16, message: String },
    #[error("configuration error: {0}")]
    Config(String),
    #[error("download failed for {filename}: {reason}")]
    Download { filename: String, reason: String },
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("input not found: {0}")]
    InputNotFound(String),
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("job failed: {0}")]
    JobFailed(String),
    #[error("job timed out after {0} seconds")]
    JobTimeout(u64),
    #[error("not authenticated; run `cfmpeg auth login` or set CFMPEG_API_KEY")]
    NotAuthenticated,
    #[error("parse error: {0}")]
    ParseError(String),
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("upload failed for {filename}: {reason}")]
    Upload { filename: String, reason: String },
}
