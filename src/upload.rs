use crate::api::{
    ApiClient, CompletedMultipartPart, IngestProgressRequest, JobIngest, SegmentUploadTarget,
    SegmentUploadTargetsRequest, UploadMethod, UploadTarget,
};
use crate::error::{CfmpegError, Result};
use crate::media_tools::ffmpeg_binary;
use crate::transfer::{file_name_or, retry_with_backoff, transfer_progress_bar};
use futures::stream::{self, StreamExt};
use indicatif::ProgressBar;
use reqwest::header::{HeaderMap, CONTENT_LENGTH, ETAG};
use reqwest::{Client, RequestBuilder};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::process::Command;
use tokio_util::io::ReaderStream;

const MIN_CONCURRENT_UPLOADS: usize = 6;
const MAX_CONCURRENT_UPLOADS: usize = 24;
const MIN_CONCURRENT_SEGMENT_UPLOADS: usize = 2;
const MAX_CONCURRENT_SEGMENT_UPLOADS: usize = 8;

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
) -> Result<Vec<CompletedMultipartPart>> {
    let file_size = std::fs::metadata(file_path)?.len();
    let filename = file_name_or(file_path, "input");

    let progress = transfer_progress_bar(
        "Uploading",
        file_size,
        if target.filename.is_empty() {
            filename.clone()
        } else {
            target.filename.clone()
        },
    );

    let upload_result = if should_use_multipart(target) {
        multipart_upload(client, file_path, file_size, target, &progress).await
    } else {
        direct_upload(client, file_path, target, &progress)
            .await
            .map(|()| Vec::new())
    };

    match upload_result {
        Ok(parts) => {
            progress.finish_with_message(filename);
            Ok(parts)
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
    let filename = file_name_or(file_path, "input");
    let progress = transfer_progress_bar("Uploading", file_size, filename.clone());

    let ffmpeg = ffmpeg_binary()?;
    let segment_dir = tempfile::Builder::new()
        .prefix("cfmpeg-segments-")
        .tempdir()?;
    let segment_template = segment_dir.path().join("segment_%06d.mp4");

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
        segment_dir: segment_dir.path(),
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

    progress.finish_with_message(filename);

    Ok(state.next_upload_index)
}

fn should_use_multipart(target: &UploadTarget) -> bool {
    target.method == UploadMethod::Multipart && !target.part_urls.is_empty()
}

async fn upload_closed_segments(
    context: &SegmentUploadContext<'_>,
    state: &mut SegmentUploadState,
    include_last_segment: bool,
) -> Result<()> {
    let concurrency = segment_upload_concurrency(context.total_bytes);

    loop {
        let ready_segments = ready_segment_paths(
            context.segment_dir,
            state.next_upload_index,
            include_last_segment,
            concurrency,
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

        let results: Vec<Result<(PathBuf, u64)>> =
            stream::iter(ready_segments.into_iter().zip(targets))
                .map(|((_, path), target)| {
                    let client = context.client.clone();

                    async move {
                        let bytes_uploaded = upload_segment_file(&client, &path, &target).await?;

                        Ok((path, bytes_uploaded))
                    }
                })
                .buffer_unordered(concurrency)
                .collect()
                .await;

        let uploaded_segments = results.into_iter().collect::<Result<Vec<_>>>()?;

        for (path, bytes_uploaded) in uploaded_segments {
            state.uploaded_bytes = (state.uploaded_bytes + bytes_uploaded).min(context.total_bytes);
            context.progress.set_position(state.uploaded_bytes);
            tokio::fs::remove_file(&path).await?;
            state.next_upload_index += 1;
        }

        context
            .api
            .report_segmented_ingest_progress(
                context.job_id,
                &IngestProgressRequest {
                    uploaded_segment_count: state.next_upload_index,
                },
            )
            .await?;
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
) -> Result<u64> {
    let file_size = tokio::fs::metadata(file_path).await?.len();
    let upload_error = |reason: String| CfmpegError::Upload {
        filename: file_path.display().to_string(),
        reason,
    };

    retry_with_backoff(|| async {
        let input = tokio::fs::File::open(file_path)
            .await
            .map_err(|error| upload_error(error.to_string()))?;
        let body = reqwest::Body::wrap_stream(ReaderStream::new(input));

        let response = signed_put(client, &target.upload_url, &target.headers, "video/mp4")
            .header(CONTENT_LENGTH, file_size.to_string())
            .body(body)
            .send()
            .await
            .map_err(|error| upload_error(format!("segment {} failed: {error}", target.index)))?;

        if !response.status().is_success() {
            return Err(upload_error(format!(
                "segment {} failed with status {}",
                target.index,
                response.status()
            )));
        }

        Ok(())
    })
    .await?;

    Ok(file_size)
}

fn signed_put(
    client: &Client,
    url: &str,
    headers: &HashMap<String, String>,
    default_content_type: &str,
) -> RequestBuilder {
    let mut request = client.put(url);

    for (name, value) in headers {
        if name.eq_ignore_ascii_case("host") {
            continue;
        }

        request = request.header(name, value);
    }

    if !headers
        .keys()
        .any(|name| name.eq_ignore_ascii_case("content-type"))
    {
        request = request.header("Content-Type", default_content_type);
    }

    request
}

async fn direct_upload(
    client: &Client,
    file_path: &Path,
    target: &UploadTarget,
    progress: &ProgressBar,
) -> Result<()> {
    let data = tokio::fs::read(file_path).await?;
    let data_len = data.len() as u64;

    let response = signed_put(
        client,
        &target.upload_url,
        &target.headers,
        "application/octet-stream",
    )
    .body(data)
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

    progress.set_position(data_len);

    Ok(())
}

async fn multipart_upload(
    client: &Client,
    file_path: &Path,
    file_size: u64,
    target: &UploadTarget,
    progress: &ProgressBar,
) -> Result<Vec<CompletedMultipartPart>> {
    let part_size = target.part_size as usize;
    let concurrency = multipart_upload_concurrency(target.part_urls.len(), file_size);

    let results: Vec<Result<CompletedMultipartPart>> =
        stream::iter(target.part_urls.iter().enumerate())
            .map(|(part_index, part_url)| async move {
                let chunk = read_chunk(file_path, file_size, part_index, part_size).await?;
                let chunk_size = chunk.len() as u64;
                let part_number = part_index + 1;
                let upload_error = |reason: String| CfmpegError::Upload {
                    filename: file_path.display().to_string(),
                    reason,
                };

                let response = retry_with_backoff(|| async {
                    let response = client
                        .put(part_url)
                        .header("Content-Type", "application/octet-stream")
                        .body(chunk.clone())
                        .send()
                        .await
                        .map_err(|error| {
                            upload_error(format!("part {part_number} failed: {error}"))
                        })?;

                    if !response.status().is_success() {
                        return Err(upload_error(format!(
                            "part {part_number} failed with status {}",
                            response.status()
                        )));
                    }

                    Ok(response)
                })
                .await?;

                let etag = extract_multipart_etag(response.headers()).ok_or_else(|| {
                    upload_error(format!(
                        "part {part_number} succeeded without an ETag header"
                    ))
                })?;
                progress.inc(chunk_size);

                Ok(CompletedMultipartPart {
                    part_number: part_number as u32,
                    etag,
                })
            })
            .buffer_unordered(concurrency)
            .collect()
            .await;

    let mut completed_parts = results.into_iter().collect::<Result<Vec<_>>>()?;
    completed_parts.sort();

    Ok(completed_parts)
}

async fn read_chunk(
    file_path: &Path,
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
    let target = if file_size >= 5 * 1024 * 1024 * 1024 {
        MAX_CONCURRENT_UPLOADS
    } else if file_size >= 1024 * 1024 * 1024 {
        16
    } else if file_size >= 256 * 1024 * 1024 {
        8
    } else {
        MIN_CONCURRENT_UPLOADS
    };

    target.min(part_count)
}

fn segment_upload_concurrency(file_size: u64) -> usize {
    if file_size >= 5 * 1024 * 1024 * 1024 {
        MAX_CONCURRENT_SEGMENT_UPLOADS
    } else if file_size >= 1024 * 1024 * 1024 {
        6
    } else if file_size >= 256 * 1024 * 1024 {
        4
    } else {
        MIN_CONCURRENT_SEGMENT_UPLOADS
    }
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
        ready_segment_paths, segment_upload_concurrency,
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
