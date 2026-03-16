use crate::api::{CompletedMultipartPart, UploadTarget};
use crate::error::{CfmpegError, Result};
use futures::stream::{self, StreamExt};
use indicatif::{ProgressBar, ProgressStyle};
use reqwest::header::{HeaderMap, ETAG};
use reqwest::Client;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::sync::Mutex;

const MIN_CONCURRENT_UPLOADS: usize = 6;
const MAX_CONCURRENT_UPLOADS: usize = 24;
const MAX_RETRIES: u32 = 3;

pub struct UploadResult {
    pub multipart_parts: Vec<CompletedMultipartPart>,
}

pub async fn upload_file(
    client: &Client,
    file_path: &Path,
    target: &UploadTarget,
) -> Result<UploadResult> {
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

    let upload_result = if should_use_multipart(target) {
        multipart_upload(client, file_path, file_size, target, &progress).await
    } else {
        direct_upload(client, file_path, target, &progress).await
    };

    match upload_result {
        Ok(result) => {
            progress.finish_with_message(filename.clone());
            Ok(result)
        }
        Err(error) => {
            progress.abandon_with_message(filename);
            Err(error)
        }
    }
}

fn should_use_multipart(target: &UploadTarget) -> bool {
    target.method.eq_ignore_ascii_case("multipart") && !target.part_urls.is_empty()
}

async fn direct_upload(
    client: &Client,
    file_path: &Path,
    target: &UploadTarget,
    progress: &ProgressBar,
) -> Result<UploadResult> {
    let data = tokio::fs::read(file_path).await?;

    let mut request = client.put(&target.upload_url);

    for (name, value) in &target.headers {
        if name.eq_ignore_ascii_case("host") {
            continue;
        }

        request = request.header(name, value);
    }

    if !target
        .headers
        .keys()
        .any(|name| name.eq_ignore_ascii_case("content-type"))
    {
        request = request.header("Content-Type", "application/octet-stream");
    }

    let response =
        request
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

    Ok(UploadResult {
        multipart_parts: Vec::new(),
    })
}

async fn multipart_upload(
    client: &Client,
    file_path: &Path,
    file_size: u64,
    target: &UploadTarget,
    progress: &ProgressBar,
) -> Result<UploadResult> {
    let part_size = target.part_size as usize;
    let uploaded_bytes = Arc::new(Mutex::new(0u64));
    let chunks: Vec<(usize, String)> = target
        .part_urls
        .iter()
        .enumerate()
        .map(|(index, url)| (index, url.clone()))
        .collect();
    let concurrency = multipart_upload_concurrency(chunks.len(), file_size);

    let results: Vec<Result<CompletedMultipartPart>> = stream::iter(chunks)
        .map(|(part_index, part_url)| {
            let client = client.clone();
            let file_path = file_path.to_path_buf();
            let uploaded_bytes = Arc::clone(&uploaded_bytes);
            let progress = progress.clone();

            async move {
                let chunk = read_chunk(&file_path, file_size, part_index, part_size).await?;
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
                            let etag =
                                extract_multipart_etag(response.headers()).ok_or_else(|| {
                                    CfmpegError::Upload {
                                        filename: file_path.display().to_string(),
                                        reason: format!(
                                            "part {} succeeded without an ETag header",
                                            part_index + 1
                                        ),
                                    }
                                })?;
                            let mut uploaded = uploaded_bytes.lock().await;
                            *uploaded += chunk_size;
                            progress.set_position(*uploaded);
                            return Ok(CompletedMultipartPart {
                                part_number: (part_index + 1) as u32,
                                etag,
                            });
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
        .buffer_unordered(concurrency)
        .collect()
        .await;

    let mut completed_parts = Vec::with_capacity(results.len());

    for result in results {
        completed_parts.push(result?);
    }

    completed_parts.sort();

    Ok(UploadResult {
        multipart_parts: completed_parts,
    })
}

async fn read_chunk(
    file_path: &PathBuf,
    file_size: u64,
    part_index: usize,
    part_size: usize,
) -> Result<Vec<u8>> {
    let mut file = tokio::fs::File::open(file_path).await?;
    let start = (part_index * part_size) as u64;
    let chunk_size = chunk_size_for_part(file_size, part_index, part_size)?;

    file.seek(std::io::SeekFrom::Start(start)).await?;

    let mut chunk = vec![0; chunk_size];
    file.read_exact(&mut chunk).await?;

    Ok(chunk)
}

fn chunk_size_for_part(file_size: u64, part_index: usize, part_size: usize) -> Result<usize> {
    let start = (part_index * part_size) as u64;

    if start >= file_size {
        return Err(CfmpegError::Upload {
            filename: "input".to_string(),
            reason: format!("part {} starts past end of file", part_index + 1),
        });
    }

    Ok(((file_size - start) as usize).min(part_size))
}

fn multipart_upload_concurrency(part_count: usize, file_size: u64) -> usize {
    if part_count == 0 {
        return 1;
    }

    let target = if file_size >= 5 * 1024 * 1024 * 1024 {
        MAX_CONCURRENT_UPLOADS
    } else if file_size >= 1024 * 1024 * 1024 {
        16
    } else if file_size >= 256 * 1024 * 1024 {
        8
    } else {
        MIN_CONCURRENT_UPLOADS
    };

    target.clamp(1, MAX_CONCURRENT_UPLOADS).min(part_count)
}

fn extract_multipart_etag(headers: &HeaderMap) -> Option<String> {
    headers
        .get(ETAG)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::{chunk_size_for_part, extract_multipart_etag, multipart_upload_concurrency};
    use reqwest::header::{HeaderMap, HeaderValue, ETAG};

    #[test]
    fn extracts_multipart_etag_header() {
        let mut headers = HeaderMap::new();
        headers.insert(ETAG, HeaderValue::from_static("\"etag-123\""));

        assert_eq!(
            extract_multipart_etag(&headers).as_deref(),
            Some("\"etag-123\"")
        );
    }

    #[test]
    fn returns_none_when_multipart_etag_is_missing() {
        let headers = HeaderMap::new();

        assert_eq!(extract_multipart_etag(&headers), None);
    }

    #[test]
    fn computes_full_chunk_sizes_for_non_final_parts() {
        assert_eq!(
            chunk_size_for_part(25 * 1024 * 1024, 0, 10 * 1024 * 1024).unwrap(),
            10 * 1024 * 1024
        );
        assert_eq!(
            chunk_size_for_part(25 * 1024 * 1024, 1, 10 * 1024 * 1024).unwrap(),
            10 * 1024 * 1024
        );
        assert_eq!(
            chunk_size_for_part(25 * 1024 * 1024, 2, 10 * 1024 * 1024).unwrap(),
            5 * 1024 * 1024
        );
    }

    #[test]
    fn scales_upload_concurrency_with_file_size() {
        assert_eq!(multipart_upload_concurrency(2, 512 * 1024 * 1024), 2);
        assert_eq!(multipart_upload_concurrency(6, 128 * 1024 * 1024), 6);
        assert_eq!(multipart_upload_concurrency(12, 512 * 1024 * 1024), 8);
        assert_eq!(multipart_upload_concurrency(20, 2 * 1024 * 1024 * 1024), 16);
        assert_eq!(multipart_upload_concurrency(40, 8 * 1024 * 1024 * 1024), 24);
    }
}
