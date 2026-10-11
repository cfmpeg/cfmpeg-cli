use crate::error::Result;
use indicatif::{ProgressBar, ProgressStyle};
use std::future::Future;
use std::path::Path;
use std::time::Duration;

const MAX_RETRIES: u32 = 3;

pub async fn retry_with_backoff<T, F, Fut>(mut attempt: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    let mut retry = 0;

    loop {
        match attempt().await {
            Ok(value) => return Ok(value),
            Err(error) if retry == MAX_RETRIES - 1 => return Err(error),
            Err(_) => {}
        }

        tokio::time::sleep(Duration::from_millis(500 * 2u64.pow(retry))).await;
        retry += 1;
    }
}

pub fn transfer_progress_bar(verb: &str, total_bytes: u64, message: String) -> ProgressBar {
    let progress = ProgressBar::new(total_bytes);
    progress.set_style(
        ProgressStyle::with_template(&format!(
            "  {verb} {{msg}} {{bar:40.cyan/blue}} {{bytes}}/{{total_bytes}}"
        ))
        .expect("progress template")
        .progress_chars("##-"),
    );
    progress.set_message(message);
    progress
}

pub fn file_name_or(path: &Path, fallback: &str) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(fallback)
        .to_string()
}
