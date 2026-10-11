use crate::api::{ApiClient, OutputBatch, OutputDeliveryMode, OutputFile};
use crate::error::{CfmpegError, Result};
use crate::media_tools::ffmpeg_binary;
use crate::parser::Output;
use crate::transfer::{file_name_or, retry_with_backoff, transfer_progress_bar};
use futures::{stream, StreamExt};
use indicatif::ProgressBar;
use reqwest::header::CONTENT_RANGE;
use reqwest::Client;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use tokio::io::{AsyncSeekExt, AsyncWriteExt};
use tokio::process::Command;

const PARALLEL_DOWNLOAD_THRESHOLD_BYTES: u64 = 128 * 1024 * 1024;
const PARALLEL_DOWNLOAD_CHUNK_SIZE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_CONCURRENT_DOWNLOADS: usize = 8;
const PROGRESSIVE_BATCH_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

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

pub fn print_output_urls(remote_outputs: &[OutputFile]) {
    for line in format_output_urls(remote_outputs) {
        println!("{line}");
    }
}

pub async fn download_progressive_batches(
    api: &ApiClient,
    client: &Client,
    job_id: &str,
    target_path: &Path,
    stop: Arc<AtomicBool>,
) -> Result<()> {
    let temp_dir = tempfile::Builder::new()
        .prefix("cfmpeg-output-batches-")
        .tempdir()?;

    let mut downloaded: HashMap<u32, PathBuf> = HashMap::new();

    loop {
        let response = api.get_output_batches(job_id).await?;

        if response.delivery_mode != OutputDeliveryMode::ProgressiveBatches {
            return Err(CfmpegError::Protocol(
                "job does not expose progressive batch outputs".to_string(),
            ));
        }

        download_missing_batches(client, temp_dir.path(), &mut downloaded, response.batches)
            .await?;

        if stop.load(Ordering::Relaxed) || response.complete {
            break;
        }

        tokio::time::sleep(PROGRESSIVE_BATCH_POLL_INTERVAL).await;
    }

    let response = api.get_outputs(job_id).await?;

    if response.delivery_mode != OutputDeliveryMode::ProgressiveBatches {
        return Err(CfmpegError::Protocol(
            "job does not expose progressive batch outputs".to_string(),
        ));
    }

    download_missing_batches(client, temp_dir.path(), &mut downloaded, response.batches).await?;

    let mut ordered_batches: Vec<(u32, PathBuf)> = downloaded.into_iter().collect();
    ordered_batches.sort_by_key(|(index, _)| *index);

    if ordered_batches.is_empty() {
        return Err(CfmpegError::Protocol(
            "job completed without any progressive output batches".to_string(),
        ));
    }

    concat_progressive_batches(
        &ordered_batches
            .iter()
            .map(|(_, path)| path.clone())
            .collect::<Vec<_>>(),
        target_path,
    )
    .await
}

async fn download_missing_batches(
    client: &Client,
    temp_dir: &Path,
    downloaded: &mut HashMap<u32, PathBuf>,
    batches: Vec<OutputBatch>,
) -> Result<()> {
    for batch in batches {
        if downloaded.contains_key(&batch.index) {
            continue;
        }

        let batch_path = temp_dir.join(format!("batch_{:05}.mp4", batch.index));
        download_file(
            client,
            &OutputFile {
                filename: batch.filename,
                download_url: batch.download_url,
                size_bytes: batch.size_bytes,
            },
            &batch_path,
        )
        .await?;
        downloaded.insert(batch.index, batch_path);
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

    let total_bytes = remote_output.size_bytes;
    let filename = file_name_or(target_path, "output");
    let progress = transfer_progress_bar("Downloading", total_bytes, filename.clone());

    let download_result = if should_parallel_download(total_bytes)
        && supports_parallel_download(client, &remote_output.download_url, total_bytes).await
    {
        download_file_parallel(client, remote_output, target_path, &progress).await
    } else {
        download_file_sequential(client, remote_output, target_path, &progress).await
    };

    match download_result {
        Ok(()) => {
            progress.finish_with_message(filename);
            Ok(())
        }
        Err(error) => {
            progress.abandon();
            Err(error)
        }
    }
}

fn should_parallel_download(size_bytes: u64) -> bool {
    size_bytes >= PARALLEL_DOWNLOAD_THRESHOLD_BYTES
}

async fn supports_parallel_download(client: &Client, url: &str, size_bytes: u64) -> bool {
    let Ok(response) = client.get(url).header("Range", "bytes=0-0").send().await else {
        return false;
    };

    if response.status().as_u16() != 206 {
        return false;
    }

    let Some(content_range) = response.headers().get(CONTENT_RANGE) else {
        return false;
    };

    let Ok(content_range) = content_range.to_str() else {
        return false;
    };

    content_range.starts_with("bytes 0-0/") && content_range.ends_with(&format!("/{size_bytes}"))
}

async fn download_file_sequential(
    client: &Client,
    remote_output: &OutputFile,
    target_path: &Path,
    progress: &ProgressBar,
) -> Result<()> {
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

    Ok(())
}

async fn download_file_parallel(
    client: &Client,
    remote_output: &OutputFile,
    target_path: &Path,
    progress: &ProgressBar,
) -> Result<()> {
    let total_bytes = remote_output.size_bytes;
    let concurrency = download_concurrency(total_bytes);
    let ranges = build_download_ranges(total_bytes, PARALLEL_DOWNLOAD_CHUNK_SIZE_BYTES);

    let file = tokio::fs::File::create(target_path).await?;
    file.set_len(total_bytes).await?;
    drop(file);

    let results: Vec<Result<()>> = stream::iter(ranges)
        .map(|(start, end)| async move {
            let bytes_downloaded =
                download_range(client, remote_output, target_path, start, end).await?;
            progress.inc(bytes_downloaded);

            Ok(())
        })
        .buffer_unordered(concurrency)
        .collect()
        .await;

    results.into_iter().collect()
}

fn build_download_ranges(size_bytes: u64, chunk_size_bytes: u64) -> Vec<(u64, u64)> {
    let mut ranges = Vec::new();
    let mut start = 0;

    while start < size_bytes {
        let end = (start + chunk_size_bytes - 1).min(size_bytes - 1);
        ranges.push((start, end));
        start = end + 1;
    }

    ranges
}

fn download_concurrency(size_bytes: u64) -> usize {
    if size_bytes >= 1024 * 1024 * 1024 {
        MAX_CONCURRENT_DOWNLOADS
    } else if size_bytes >= 256 * 1024 * 1024 {
        6
    } else {
        4
    }
}

async fn download_range(
    client: &Client,
    remote_output: &OutputFile,
    target_path: &Path,
    start: u64,
    end: u64,
) -> Result<u64> {
    let expected_size = end - start + 1;
    let download_error = |reason: String| CfmpegError::Download {
        filename: remote_output.filename.clone(),
        reason,
    };

    let response = retry_with_backoff(|| async {
        let response = client
            .get(&remote_output.download_url)
            .header("Range", format!("bytes={start}-{end}"))
            .send()
            .await
            .map_err(|error| download_error(error.to_string()))?;

        if response.status().as_u16() != 206 {
            return Err(download_error(format!(
                "range {start}-{end} failed with status {}",
                response.status()
            )));
        }

        Ok(response)
    })
    .await?;

    let bytes = response
        .bytes()
        .await
        .map_err(|error| download_error(error.to_string()))?;

    if bytes.len() as u64 != expected_size {
        return Err(download_error(format!(
            "range {start}-{end} returned {} bytes, expected {expected_size}",
            bytes.len()
        )));
    }

    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .open(target_path)
        .await?;
    file.seek(std::io::SeekFrom::Start(start)).await?;
    file.write_all(&bytes).await?;

    Ok(expected_size)
}

fn format_output_urls(remote_outputs: &[OutputFile]) -> Vec<String> {
    remote_outputs
        .iter()
        .map(|output| format!("{}\t{}", output.filename, output.download_url))
        .collect()
}

async fn concat_progressive_batches(batch_paths: &[PathBuf], target_path: &Path) -> Result<()> {
    if let Some(parent) = target_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    let manifest_file = tempfile::Builder::new()
        .prefix("cfmpeg-output-batches-")
        .suffix(".txt")
        .tempfile()?;
    let manifest = batch_paths
        .iter()
        .map(|path| format!("file '{}'\n", path.display()))
        .collect::<String>();
    tokio::fs::write(manifest_file.path(), manifest).await?;

    let ffmpeg = ffmpeg_binary()?;
    let output = Command::new(ffmpeg)
        .arg("-hide_banner")
        .arg("-loglevel")
        .arg("error")
        .arg("-nostdin")
        .arg("-y")
        .arg("-f")
        .arg("concat")
        .arg("-safe")
        .arg("0")
        .arg("-i")
        .arg(manifest_file.path())
        .arg("-c")
        .arg("copy")
        .arg(target_path)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .await?;

    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(CfmpegError::Download {
            filename: target_path.display().to_string(),
            reason: if detail.is_empty() {
                format!("ffmpeg concat exited with status {}", output.status)
            } else {
                detail
            },
        });
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        build_download_ranges, download_concurrency, format_output_urls, should_parallel_download,
    };
    use crate::api::OutputFile;

    #[test]
    fn formats_output_urls_for_no_download_mode() {
        let lines = format_output_urls(&[
            OutputFile {
                filename: "output-0.mp4".to_string(),
                download_url: "https://example.com/output-0.mp4".to_string(),
                size_bytes: 123,
            },
            OutputFile {
                filename: "output-1.mp4".to_string(),
                download_url: "https://example.com/output-1.mp4".to_string(),
                size_bytes: 456,
            },
        ]);

        assert_eq!(
            lines,
            vec![
                "output-0.mp4\thttps://example.com/output-0.mp4".to_string(),
                "output-1.mp4\thttps://example.com/output-1.mp4".to_string(),
            ]
        );
    }

    #[test]
    fn builds_parallel_download_ranges() {
        assert_eq!(build_download_ranges(10, 4), vec![(0, 3), (4, 7), (8, 9)]);
    }

    #[test]
    fn scales_parallel_download_concurrency_with_size() {
        assert!(!should_parallel_download(64 * 1024 * 1024));
        assert!(should_parallel_download(256 * 1024 * 1024));
        assert_eq!(download_concurrency(128 * 1024 * 1024), 4);
        assert_eq!(download_concurrency(512 * 1024 * 1024), 6);
        assert_eq!(download_concurrency(2 * 1024 * 1024 * 1024), 8);
    }
}
