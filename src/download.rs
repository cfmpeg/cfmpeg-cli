use crate::api::OutputFile;
use crate::error::{CfmpegError, Result};
use crate::parser::Output;
use futures::StreamExt;
use indicatif::{ProgressBar, ProgressStyle};
use reqwest::Client;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tokio::io::AsyncWriteExt;

pub async fn download_outputs(
    client: &Client,
    remote_outputs: &[OutputFile],
    requested_outputs: &[Output],
) -> Result<()> {
    if remote_outputs.is_empty() {
        return Err(CfmpegError::Protocol(
            "job completed without any downloadable outputs".to_string(),
        ));
    }

    let default_dir = requested_outputs
        .first()
        .and_then(|output| output.path.parent())
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();

    let mut requested_by_name: HashMap<String, PathBuf> = requested_outputs
        .iter()
        .map(|output| (output.remote_name.clone(), output.path.clone()))
        .collect();

    for (index, remote_output) in remote_outputs.iter().enumerate() {
        let target_path = requested_by_name
            .remove(&remote_output.filename)
            .or_else(|| {
                requested_outputs
                    .get(index)
                    .map(|output| output.path.clone())
            })
            .unwrap_or_else(|| default_dir.join(&remote_output.filename));

        download_file(client, remote_output, &target_path).await?;
    }

    Ok(())
}

async fn download_file(
    client: &Client,
    remote_output: &OutputFile,
    target_path: &Path,
) -> Result<()> {
    if let Some(parent) = target_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    let response = client
        .get(&remote_output.download_url)
        .send()
        .await
        .map_err(|error| CfmpegError::Download {
            filename: remote_output.filename.clone(),
            reason: error.to_string(),
        })?;

    if !response.status().is_success() {
        return Err(CfmpegError::Download {
            filename: remote_output.filename.clone(),
            reason: format!("unexpected status {}", response.status()),
        });
    }

    let total_bytes = remote_output
        .size_bytes
        .max(response.content_length().unwrap_or_default());

    let progress = ProgressBar::new(total_bytes);
    progress.set_style(
        ProgressStyle::with_template(
            "  Downloading {msg} {bar:40.cyan/blue} {bytes}/{total_bytes}",
        )
        .expect("progress template")
        .progress_chars("##-"),
    );
    progress.set_message(
        target_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("output")
            .to_string(),
    );

    let mut file = tokio::fs::File::create(target_path).await?;
    let mut stream = response.bytes_stream();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| CfmpegError::Download {
            filename: remote_output.filename.clone(),
            reason: error.to_string(),
        })?;

        file.write_all(&chunk).await?;
        progress.inc(chunk.len() as u64);
    }

    file.flush().await?;
    progress.finish_with_message(
        target_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("output")
            .to_string(),
    );

    Ok(())
}
