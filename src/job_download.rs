//! `cfmpeg download <job_id>`: fetches the outputs of a finished job again.

use crate::api::{ApiClient, JobState, OutputDeliveryMode};
use crate::config::Config;
use crate::download;
use crate::error::{CfmpegError, Result};
use crate::interrupt::Interrupts;
use crate::local_output;
use crate::parser::{Output, Overwrite};
use reqwest::Client;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

/// Downloads the outputs of the completed job `job_id` and returns where they
/// were written.
///
/// With no `paths`, each output goes to the current directory under the name
/// the job stored it with; otherwise there has to be one path per output.
/// The files are written the way a normal run writes them: checked first,
/// downloaded next to their destination and renamed when complete.
///
/// # Errors
///
/// Fails when the job does not exist, has not completed, or when its outputs
/// have expired, and for everything a download can fail with.
pub async fn run(job_id: &str, paths: &[PathBuf], overwrite: Overwrite) -> Result<Vec<Output>> {
    let config = Config::load()?;
    let api = ApiClient::from_config(&config)?;
    let refuse = |reason: String| CfmpegError::JobNotDownloadable {
        job_id: job_id.to_string(),
        reason,
    };

    let job = api
        .get_job_status(job_id)
        .await
        .map_err(|error| match error {
            CfmpegError::Api { status: 404, .. } => {
                refuse("no such job exists for this API key".to_string())
            }
            other => other,
        })?;
    match job.status {
        JobState::Completed => {}
        JobState::Failed => {
            return Err(refuse(format!(
                "the job failed and has no outputs ({})",
                job.error.as_deref().unwrap_or("no reason was recorded")
            )));
        }
        JobState::Cancelled => {
            return Err(refuse(
                "the job was cancelled and has no outputs".to_string(),
            ));
        }
        JobState::Pending | JobState::Queued | JobState::Uploading | JobState::Processing => {
            return Err(refuse(format!(
                "the job has not finished yet (status: {})",
                format!("{:?}", job.status).to_lowercase()
            )));
        }
    }

    let listing = api.get_outputs(job_id).await.map_err(|error| match error {
        // The server answers 410 once the retention period has passed.
        CfmpegError::Api {
            status: 410,
            message,
            ..
        } => refuse(format!(
            "its outputs have expired and are no longer in cloud storage ({message})"
        )),
        other => other,
    })?;

    // A job delivered in batches has one output; its name is the batch name
    // without the `.batch_00000` part.
    let batched = listing.delivery_mode == OutputDeliveryMode::ProgressiveBatches;
    let stored_names: Vec<String> = if batched {
        listing
            .batches
            .first()
            .map(|batch| match batch.filename.split_once(".batch_") {
                Some((stem, rest)) => {
                    format!(
                        "{stem}{}",
                        rest.trim_start_matches(|c: char| c.is_ascii_digit())
                    )
                }
                None => batch.filename.clone(),
            })
            .into_iter()
            .collect()
    } else {
        listing
            .outputs
            .iter()
            .map(|output| output.filename.clone())
            .collect()
    };

    if stored_names.is_empty() {
        return Err(refuse("the server lists no outputs for it".to_string()));
    }
    if !paths.is_empty() && paths.len() != stored_names.len() {
        return Err(refuse(format!(
            "it has {} output file(s), but {} path(s) were given",
            stored_names.len(),
            paths.len()
        )));
    }

    let outputs = stored_names
        .into_iter()
        .enumerate()
        .map(|(index, stored_name)| {
            let path = match paths.get(index) {
                Some(path) => path.clone(),
                // Only the file name: a stored name never chooses a directory.
                None => Path::new(&stored_name)
                    .file_name()
                    .map(PathBuf::from)
                    .ok_or_else(|| refuse(format!("`{stored_name}` is not a usable file name")))?,
            };

            Ok(Output {
                path,
                remote_name: stored_name,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    local_output::preflight(&[], &outputs, overwrite)?;

    let http_client = Client::builder()
        .user_agent(format!("cfmpeg/{}", env!("CARGO_PKG_VERSION")))
        .build()?;
    // Listening makes an interrupt drop the download, which removes the
    // partial file, instead of ending the process with it in place.
    let mut interrupts = Interrupts::listen()?;
    let transfer = async {
        if batched {
            // The job has finished, so the batch listing is final already.
            let finished = AtomicBool::new(true);
            download::download_progressive_batches(
                &api,
                &http_client,
                job_id,
                &outputs[0].path,
                &finished,
                overwrite,
            )
            .await
        } else {
            download::download_outputs(&http_client, &listing.outputs, &outputs, overwrite).await
        }
    };

    tokio::select! {
        transferred = transfer => transferred?,
        interrupt = interrupts.next() => return Err(CfmpegError::Interrupted(interrupt)),
    }

    Ok(outputs)
}
