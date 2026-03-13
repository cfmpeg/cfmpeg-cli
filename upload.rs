use crate::api::UploadTarget;
use crate::error::{CfmpegError, Result};
use futures::stream::{self, StreamExt};
use indicatif::{ProgressBar, ProgressStyle};
use reqwest::Client;
use sha2::{Digest, Sha256};
use std::path::Path;
use std::sync::Arc;
use tokio::sync::Mutex;

/// Maximum number of concurrent chunk uploads.
const MAX_CONCURRENT_UPLOADS: usize = 6;

/// Maximum retries per chunk.
const MAX_RETRIES: u32 = 3;

/// Size threshold for direct upload vs multipart (25 MB).
const DIRECT_UPLOAD_THRESHOLD: u64 = 25 * 1024 * 1024;

/// Upload a local file to R2 via presigned URLs.
pub async fn upload_file(
    client: &Client,
    file_path: &Path,
    target: &UploadTarget,
) -> Result<String> {
    let file_size = std::fs::metadata(file_path)?.len();
    let filename = file_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown")
        .to_string();

    let pb = ProgressBar::new(file_size);
    pb.set_style(
        ProgressStyle::default_bar()
            .template("  Uploading {msg} {bar:40.cyan/dim} {percent}%  ({bytes}/{total_bytes})")
            .unwrap()
            .progress_chars("██░"),
    );
    pb.set_message(filename.clone());

    let result = if file_size <= DIRECT_UPLOAD_THRESHOLD || target.part_urls.is_empty() {
        direct_upload(client, file_path, &target.upload_url, &pb).await
    } else {
        multipart_upload(client, file_path, target, &pb).await
    };

    pb.finish_with_message(format!("{} ✓", filename));

    // Compute file hash for caching
    let hash = hash_file(file_path).await?;

    result.map(|_| hash)
}

/// Direct single-request upload for small files.
async fn direct_upload(
    client: &Client,
    file_path: &Path,
    upload_url: &str,
    pb: &ProgressBar,
) -> Result<()> {
    let data = tokio::fs::read(file_path).await?;
    let size = data.len() as u64;

    client
        .put(upload_url)
        .header("Content-Type", "application/octet-stream")
        .body(data)
        .send()
        .await
        .map_err(|e| CfmpegError::Upload {
            filename: file_path.display().to_string(),
            reason: e.to_string(),
        })?;

    pb.set_position(size);

    Ok(())
}

/// Multipart upload with parallel chunk uploads.
async fn multipart_upload(
    client: &Client,
    file_path: &Path,
    target: &UploadTarget,
    pb: &ProgressBar,
) -> Result<()> {
    let file_data = tokio::fs::read(file_path).await?;
    let file_data = Arc::new(file_data);
    let part_size = target.part_size as usize;
    let total_parts = target.part_urls.len();
    let uploaded_bytes = Arc::new(Mutex::new(0u64));

    // Build chunk descriptors
    let chunks: Vec<(usize, String)> = target
        .part_urls
        .iter()
        .enumerate()
        .map(|(i, url)| (i, url.clone()))
        .collect();

    // Upload chunks in parallel
    let results: Vec<Result<()>> = stream::iter(chunks)
        .map(|(part_index, part_url)| {
            let client = client.clone();
            let file_data = Arc::clone(&file_data);
            let uploaded_bytes = Arc::clone(&uploaded_bytes);
            let pb = pb.clone();
            let filename = file_path.display().to_string();

            async move {
                let start = part_index * part_size;
                let end = std::cmp::min(start + part_size, file_data.len());
                let chunk = file_data[start..end].to_vec();
                let chunk_size = chunk.len() as u64;

                // Retry loop
                for attempt in 0..MAX_RETRIES {
                    match client
                        .put(&part_url)
                        .header("Content-Type", "application/octet-stream")
                        .body(chunk.clone())
                        .send()
                        .await
                    {
                        Ok(resp) if resp.status().is_success() => {
                            let mut bytes = uploaded_bytes.lock().await;
                            *bytes += chunk_size;
                            pb.set_position(*bytes);
                            return Ok(());
                        }
                        Ok(resp) => {
                            if attempt == MAX_RETRIES - 1 {
                                return Err(CfmpegError::Upload {
                                    filename,
                                    reason: format!(
                                        "Part {} failed with status {} after {} retries",
                                        part_index + 1,
                                        resp.status(),
                                        MAX_RETRIES
                                    ),
                                });
                            }
                            // Exponential backoff
                            tokio::time::sleep(std::time::Duration::from_millis(
                                500 * 2u64.pow(attempt),
                            ))
                            .await;
                        }
                        Err(e) => {
                            if attempt == MAX_RETRIES - 1 {
                                return Err(CfmpegError::Upload {
                                    filename,
                                    reason: format!(
                                        "Part {} failed: {} (after {} retries)",
                                        part_index + 1,
                                        e,
                                        MAX_RETRIES
                                    ),
                                });
                            }
                            tokio::time::sleep(std::time::Duration::from_millis(
                                500 * 2u64.pow(attempt),
                            ))
                            .await;
                        }
                    }
                }

                unreachable!()
            }
        })
        .buffer_unordered(MAX_CONCURRENT_UPLOADS)
        .collect()
        .await;

    // Check for any failures
    for result in results {
        result?;
    }

    Ok(())
}

/// Compute SHA-256 hash of a file (for cache key generation).
pub async fn hash_file(path: &Path) -> Result<String> {
    let data = tokio::fs::read(path).await?;

    let hash = tokio::task::spawn_blocking(move || {
        let mut hasher = Sha256::new();
        hasher.update(&data);
        format!("{:x}", hasher.finalize())
    })
    .await
    .map_err(|e| CfmpegError::Upload {
        filename: path.display().to_string(),
        reason: format!("Hashing failed: {}", e),
    })?;

    Ok(hash)
}
