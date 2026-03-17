use crate::api::{
    ApiClient, CompletedMultipartPart, JobIngest, SegmentUploadTarget, SegmentUploadTargetsRequest,
    UploadTarget,
};
use crate::error::{CfmpegError, Result};
use crate::media_tools::ffmpeg_binary;
use futures::stream::{self, StreamExt};
use indicatif::{ProgressBar, ProgressStyle};
use reqwest::header::{HeaderMap, ETAG};
use reqwest::Client;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::process::Command;
use tokio::sync::Mutex;
use uuid::Uuid;

const MIN_CONCURRENT_UPLOADS: usize = 6;
const MAX_CONCURRENT_UPLOADS: usize = 24;
const MIN_CONCURRENT_SEGMENT_UPLOADS: usize = 2;
const MAX_CONCURRENT_SEGMENT_UPLOADS: usize = 8;
const SEGMENT_UPLOAD_TARGET_BATCH_MULTIPLIER: usize = 2;
const MAX_RETRIES: u32 = 3;

pub struct UploadResult {
    pub multipart_parts: Vec<CompletedMultipartPart>,
}

struct SegmentUploadContext<'a> {
    api: &'a ApiClient,
    client: &'a Client,
    job_id: &'a str,
    segment_dir: &'a Path,
    progress: &'a ProgressBar,
    total_bytes: u64,
}

#[derive(Default)]
struct SegmentUploadState {
    next_upload_index: u32,
    uploaded_bytes: u64,
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

pub async fn upload_segmented_file(
    api: &ApiClient,
    client: &Client,
    file_path: &Path,
    job_id: &str,
    ingest: &JobIngest,
) -> Result<u32> {
    let segment_duration_seconds = ingest.segment_duration_seconds.ok_or_else(|| {
        CfmpegError::Protocol("segmented uploads require segment_duration_seconds".to_string())
    })? as usize;

    if segment_duration_seconds == 0 {
        return Err(CfmpegError::Protocol(
            "segmented uploads require a positive segment duration".to_string(),
        ));
    }

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
    progress.set_message(filename.clone());

    let ffmpeg = ffmpeg_binary()?;
    let segment_dir = std::env::temp_dir().join(format!("cfmpeg-segments-{}", Uuid::new_v4()));
    tokio::fs::create_dir_all(&segment_dir).await?;
    let segment_template = segment_dir.join("segment_%06d.mp4");

    let mut segmenter = Command::new(ffmpeg);
    segmenter
        .arg("-hide_banner")
        .arg("-loglevel")
        .arg("error")
        .arg("-nostdin")
        .arg("-y")
        .arg("-i")
        .arg(file_path)
        .arg("-c")
        .arg("copy")
        .arg("-map")
        .arg("0")
        .arg("-f")
        .arg("segment")
        .arg("-segment_time")
        .arg(segment_duration_seconds.to_string())
        .arg("-reset_timestamps")
        .arg("1")
        .arg(segment_template.as_os_str())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());

    let mut child = segmenter.spawn()?;
    let mut state = SegmentUploadState::default();
    let context = SegmentUploadContext {
        api,
        client,
        job_id,
        segment_dir: &segment_dir,
        progress: &progress,
        total_bytes: file_size,
    };

    loop {
        upload_closed_segments(&context, &mut state, false).await?;

        if child.try_wait()?.is_some() {
            break;
        }

        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }

    let output = child.wait_with_output().await?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let _ = tokio::fs::remove_dir_all(&segment_dir).await;
        progress.abandon_with_message(filename);
        return Err(CfmpegError::Upload {
            filename: file_path.display().to_string(),
            reason: if detail.is_empty() {
                format!("segmenter exited with status {}", output.status)
            } else {
                detail
            },
        });
    }

    upload_closed_segments(&context, &mut state, true).await?;

    let _ = tokio::fs::remove_dir_all(&segment_dir).await;

    progress.finish_with_message(filename);

    Ok(state.next_upload_index)
}

fn should_use_multipart(target: &UploadTarget) -> bool {
    target.method.eq_ignore_ascii_case("multipart") && !target.part_urls.is_empty()
}

async fn upload_closed_segments(
    context: &SegmentUploadContext<'_>,
    state: &mut SegmentUploadState,
    include_last_segment: bool,
) -> Result<()> {
    let concurrency = segment_upload_concurrency(context.total_bytes);
    let batch_size = segment_target_batch_size(context.total_bytes);

    loop {
        let ready_segments = ready_segment_paths(
            context.segment_dir,
            state.next_upload_index,
            include_last_segment,
            batch_size,
        );

        if ready_segments.is_empty() {
            return Ok(());
        }

        let targets = context
            .api
            .request_segment_upload_targets(
                context.job_id,
                &SegmentUploadTargetsRequest {
                    start_index: state.next_upload_index,
                    count: ready_segments.len() as u32,
                },
            )
            .await?;

        if targets.len() != ready_segments.len() {
            return Err(CfmpegError::Protocol(format!(
                "segment target batch returned {} targets for {} ready segments",
                targets.len(),
                ready_segments.len()
            )));
        }

        let results: Vec<Result<(u32, PathBuf, u64)>> =
            stream::iter(ready_segments.into_iter().zip(targets.into_iter()))
                .map(|((index, path), target)| {
                    let client = context.client.clone();

                    async move {
                        let bytes_uploaded =
                            upload_segment_file(&client, &path, &target, &path).await?;

                        Ok((index, path, bytes_uploaded))
                    }
                })
                .buffer_unordered(concurrency)
                .collect()
                .await;

        let mut uploaded_segments = Vec::with_capacity(results.len());
        for result in results {
            uploaded_segments.push(result?);
        }

        uploaded_segments.sort_by_key(|(index, _, _)| *index);

        for (index, path, bytes_uploaded) in uploaded_segments {
            if index != state.next_upload_index {
                return Err(CfmpegError::Protocol(format!(
                    "segment uploads completed out of order: expected {}, got {}",
                    state.next_upload_index, index
                )));
            }

            state.uploaded_bytes = (state.uploaded_bytes + bytes_uploaded).min(context.total_bytes);
            context.progress.set_position(state.uploaded_bytes);
            tokio::fs::remove_file(&path).await?;
            state.next_upload_index += 1;
        }
    }
}

fn segment_file_path(segment_dir: &Path, index: u32) -> PathBuf {
    segment_dir.join(format!("segment_{index:06}.mp4"))
}

fn ready_segment_paths(
    segment_dir: &Path,
    start_index: u32,
    include_last_segment: bool,
    limit: usize,
) -> Vec<(u32, PathBuf)> {
    let mut ready = Vec::new();
    let mut index = start_index;

    while ready.len() < limit {
        let current_path = segment_file_path(segment_dir, index);
        if !current_path.exists() {
            break;
        }

        let next_path = segment_file_path(segment_dir, index + 1);
        if !include_last_segment && !next_path.exists() {
            break;
        }

        ready.push((index, current_path));
        index += 1;
    }

    ready
}

async fn upload_segment_file(
    client: &Client,
    file_path: &Path,
    target: &SegmentUploadTarget,
    display_path: &Path,
) -> Result<u64> {
    let data = tokio::fs::read(file_path).await?;

    for attempt in 0..MAX_RETRIES {
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
            request = request.header("Content-Type", "video/mp4");
        }

        match request.body(data.clone()).send().await {
            Ok(response) if response.status().is_success() => return Ok(data.len() as u64),
            Ok(response) => {
                if attempt == MAX_RETRIES - 1 {
                    return Err(CfmpegError::Upload {
                        filename: display_path.display().to_string(),
                        reason: format!(
                            "segment {} failed with status {}",
                            target.index,
                            response.status()
                        ),
                    });
                }
            }
            Err(error) => {
                if attempt == MAX_RETRIES - 1 {
                    return Err(CfmpegError::Upload {
                        filename: display_path.display().to_string(),
                        reason: format!("segment {} failed: {error}", target.index),
                    });
                }
            }
        }

        tokio::time::sleep(std::time::Duration::from_millis(500 * 2u64.pow(attempt))).await;
    }

    Err(CfmpegError::Upload {
        filename: display_path.display().to_string(),
        reason: format!("segment {} failed after retries", target.index),
    })
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

fn segment_upload_concurrency(file_size: u64) -> usize {
    let target = if file_size >= 5 * 1024 * 1024 * 1024 {
        MAX_CONCURRENT_SEGMENT_UPLOADS
    } else if file_size >= 1024 * 1024 * 1024 {
        6
    } else if file_size >= 256 * 1024 * 1024 {
        4
    } else {
        MIN_CONCURRENT_SEGMENT_UPLOADS
    };

    target.clamp(
        MIN_CONCURRENT_SEGMENT_UPLOADS,
        MAX_CONCURRENT_SEGMENT_UPLOADS,
    )
}

fn segment_target_batch_size(file_size: u64) -> usize {
    segment_upload_concurrency(file_size) * SEGMENT_UPLOAD_TARGET_BATCH_MULTIPLIER
}

fn extract_multipart_etag(headers: &HeaderMap) -> Option<String> {
    headers
        .get(ETAG)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::{
        chunk_size_for_part, extract_multipart_etag, multipart_upload_concurrency,
        ready_segment_paths, segment_target_batch_size, segment_upload_concurrency,
    };
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

    #[test]
    fn scales_segment_upload_concurrency_with_file_size() {
        assert_eq!(segment_upload_concurrency(128 * 1024 * 1024), 2);
        assert_eq!(segment_upload_concurrency(512 * 1024 * 1024), 4);
        assert_eq!(segment_upload_concurrency(2 * 1024 * 1024 * 1024), 6);
        assert_eq!(segment_upload_concurrency(8 * 1024 * 1024 * 1024), 8);
        assert_eq!(segment_target_batch_size(2 * 1024 * 1024 * 1024), 12);
    }

    #[test]
    fn detects_only_closed_segment_files_until_finalized() {
        let dir =
            std::env::temp_dir().join(format!("cfmpeg-ready-segments-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("segment_000000.mp4"), b"a").unwrap();
        std::fs::write(dir.join("segment_000001.mp4"), b"b").unwrap();
        std::fs::write(dir.join("segment_000002.mp4"), b"c").unwrap();

        let open_ready = ready_segment_paths(&dir, 0, false, 10);
        assert_eq!(
            open_ready
                .iter()
                .map(|(index, _)| *index)
                .collect::<Vec<_>>(),
            vec![0, 1]
        );

        let final_ready = ready_segment_paths(&dir, 0, true, 10);
        assert_eq!(
            final_ready
                .iter()
                .map(|(index, _)| *index)
                .collect::<Vec<_>>(),
            vec![0, 1, 2]
        );

        std::fs::remove_dir_all(dir).unwrap();
    }
}
