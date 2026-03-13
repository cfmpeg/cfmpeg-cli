use crate::api::UploadTarget;
use crate::error::{CfmpegError, Result};
use futures::stream::{self, StreamExt};
use indicatif::{ProgressBar, ProgressStyle};
use reqwest::Client;
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::sync::Mutex;

const MAX_CONCURRENT_UPLOADS: usize = 6;
const MAX_RETRIES: u32 = 3;
const DIRECT_UPLOAD_THRESHOLD: u64 = 25 * 1024 * 1024;

pub async fn upload_file(
    client: &Client,
    file_path: &Path,
    target: &UploadTarget,
) -> Result<String> {
    let file_size = std::fs::metadata(file_path)?.len();
    let filename = file_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("input")
        .to_string();

    let progress = ProgressBar::new(file_size);
    progress.set_style(
        ProgressStyle::with_template("  Uploading {msg} {bar:40.cyan/blue} {bytes}/{total_bytes}")
            .expect("progress template")
            .progress_chars("##-"),
    );
    progress.set_message(if target.filename.is_empty() {
        filename.clone()
    } else {
        target.filename.clone()
    });

    let upload_result = if should_use_multipart(file_size, target) {
        multipart_upload(client, file_path, target, &progress).await
    } else {
        direct_upload(client, file_path, &target.upload_url, &progress).await
    };

    match upload_result {
        Ok(()) => {
            progress.finish_with_message(filename.clone());
            hash_file(file_path).await
        }
        Err(error) => {
            progress.abandon_with_message(filename);
            Err(error)
        }
    }
}

fn should_use_multipart(file_size: u64, target: &UploadTarget) -> bool {
    target.method.eq_ignore_ascii_case("multipart")
        && !target.part_urls.is_empty()
        && file_size > DIRECT_UPLOAD_THRESHOLD
}

async fn direct_upload(
    client: &Client,
    file_path: &Path,
    upload_url: &str,
    progress: &ProgressBar,
) -> Result<()> {
    let data = tokio::fs::read(file_path).await?;

    let response = client
        .put(upload_url)
        .header("Content-Type", "application/octet-stream")
        .body(data.clone())
        .send()
        .await
        .map_err(|error| CfmpegError::Upload {
            filename: file_path.display().to_string(),
            reason: error.to_string(),
        })?;

    if !response.status().is_success() {
        return Err(CfmpegError::Upload {
            filename: file_path.display().to_string(),
            reason: format!("unexpected status {}", response.status()),
        });
    }

    progress.set_position(data.len() as u64);

    Ok(())
}

async fn multipart_upload(
    client: &Client,
    file_path: &Path,
    target: &UploadTarget,
    progress: &ProgressBar,
) -> Result<()> {
    let part_size = target.part_size as usize;
    let uploaded_bytes = Arc::new(Mutex::new(0u64));
    let chunks: Vec<(usize, String)> = target
        .part_urls
        .iter()
        .enumerate()
        .map(|(index, url)| (index, url.clone()))
        .collect();

    let results: Vec<Result<()>> = stream::iter(chunks)
        .map(|(part_index, part_url)| {
            let client = client.clone();
            let file_path = file_path.to_path_buf();
            let uploaded_bytes = Arc::clone(&uploaded_bytes);
            let progress = progress.clone();

            async move {
                let chunk = read_chunk(&file_path, part_index, part_size).await?;
                let chunk_size = chunk.len() as u64;

                for attempt in 0..MAX_RETRIES {
                    let response = client
                        .put(&part_url)
                        .header("Content-Type", "application/octet-stream")
                        .body(chunk.clone())
                        .send()
                        .await;

                    match response {
                        Ok(response) if response.status().is_success() => {
                            let mut uploaded = uploaded_bytes.lock().await;
                            *uploaded += chunk_size;
                            progress.set_position(*uploaded);
                            return Ok(());
                        }
                        Ok(response) => {
                            if attempt == MAX_RETRIES - 1 {
                                return Err(CfmpegError::Upload {
                                    filename: file_path.display().to_string(),
                                    reason: format!(
                                        "part {} failed with status {}",
                                        part_index + 1,
                                        response.status()
                                    ),
                                });
                            }
                        }
                        Err(error) => {
                            if attempt == MAX_RETRIES - 1 {
                                return Err(CfmpegError::Upload {
                                    filename: file_path.display().to_string(),
                                    reason: format!("part {} failed: {error}", part_index + 1),
                                });
                            }
                        }
                    }

                    tokio::time::sleep(std::time::Duration::from_millis(500 * 2u64.pow(attempt)))
                        .await;
                }

                Err(CfmpegError::Upload {
                    filename: file_path.display().to_string(),
                    reason: format!("part {} failed after retries", part_index + 1),
                })
            }
        })
        .buffer_unordered(MAX_CONCURRENT_UPLOADS)
        .collect()
        .await;

    for result in results {
        result?;
    }

    Ok(())
}

async fn read_chunk(file_path: &PathBuf, part_index: usize, part_size: usize) -> Result<Vec<u8>> {
    let mut file = tokio::fs::File::open(file_path).await?;
    let start = (part_index * part_size) as u64;

    file.seek(std::io::SeekFrom::Start(start)).await?;

    let mut chunk = vec![0; part_size];
    let bytes_read = file.read(&mut chunk).await?;
    chunk.truncate(bytes_read);

    Ok(chunk)
}

pub async fn hash_file(path: &Path) -> Result<String> {
    let path = path.to_path_buf();
    let display_path = path.display().to_string();

    tokio::task::spawn_blocking(move || {
        let file = std::fs::File::open(&path)?;
        let mut reader = std::io::BufReader::new(file);
        let mut hasher = Sha256::new();
        let mut buffer = [0_u8; 8192];

        loop {
            let bytes_read = reader.read(&mut buffer)?;
            if bytes_read == 0 {
                break;
            }
            hasher.update(&buffer[..bytes_read]);
        }

        Ok::<String, std::io::Error>(format!("{:x}", hasher.finalize()))
    })
    .await
    .map_err(|error| CfmpegError::Upload {
        filename: display_path,
        reason: format!("hashing failed: {error}"),
    })?
    .map_err(CfmpegError::Io)
}
