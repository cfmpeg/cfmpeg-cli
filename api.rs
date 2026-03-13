use crate::config::Config;
use crate::error::{CfmpegError, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};

/// API client for the cfmpeg backend.
pub struct ApiClient {
    client: Client,
    base_url: String,
    api_key: String,
}

// ── Request / Response Types ────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct CreateJobRequest {
    pub ffmpeg_args: Vec<String>,
    pub inputs: Vec<JobInput>,
    pub outputs: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct JobInput {
    pub filename: String,
    pub size_bytes: u64,
    /// "local" for files that need upload, "url" for remote URLs.
    pub source: String,
    /// For URL inputs, the URL to fetch from.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CreateJobResponse {
    pub job_id: String,
    pub uploads: Vec<UploadTarget>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct UploadTarget {
    pub filename: String,
    pub upload_url: String,
    pub method: String,
    pub part_size: u64,
    pub part_urls: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct JobStatus {
    pub job_id: String,
    pub status: JobState,
    #[serde(default)]
    pub progress: Option<JobProgress>,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Pending,
    Uploading,
    Processing,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Deserialize, Clone)]
pub struct JobProgress {
    #[serde(default)]
    pub frame: Option<u64>,
    #[serde(default)]
    pub fps: Option<f64>,
    #[serde(default)]
    pub time: Option<String>,
    #[serde(default)]
    pub percent: Option<f64>,
    #[serde(default)]
    pub speed: Option<String>,
    #[serde(default)]
    pub size_kb: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub struct JobOutputResponse {
    pub outputs: Vec<OutputFile>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct OutputFile {
    pub filename: String,
    pub download_url: String,
    pub size_bytes: u64,
}

#[derive(Debug, Deserialize)]
pub struct UsageResponse {
    pub period_start: String,
    pub period_end: String,
    pub cpu_minutes: f64,
    pub gpu_minutes: f64,
    pub total_cost_cents: u64,
    pub jobs_count: u64,
}

#[derive(Debug, Deserialize)]
pub struct ApiErrorResponse {
    pub error: String,
    #[serde(default)]
    pub code: Option<String>,
}

// ── SSE Progress Event ──────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct ProgressEvent {
    #[serde(flatten)]
    pub progress: JobProgress,
    pub status: JobState,
}

// ── Client Implementation ───────────────────────────────────────────────

impl ApiClient {
    /// Create a new API client from config.
    pub fn from_config(config: &Config) -> Result<Self> {
        let api_key = config.require_api_key()?.to_string();

        let client = Client::builder()
            .user_agent(format!("cfmpeg/{}", env!("CARGO_PKG_VERSION")))
            .timeout(std::time::Duration::from_secs(30))
            .build()?;

        Ok(Self {
            client,
            base_url: config.api_base.clone(),
            api_key,
        })
    }

    /// Create a new encoding job.
    pub async fn create_job(&self, request: &CreateJobRequest) -> Result<CreateJobResponse> {
        let url = format!("{}/jobs", self.base_url);

        let response = self
            .client
            .post(&url)
            .bearer_auth(&self.api_key)
            .json(request)
            .send()
            .await
            .map_err(|e| {
                if e.is_connect() || e.is_timeout() {
                    CfmpegError::ApiUnreachable(self.base_url.clone())
                } else {
                    CfmpegError::Http(e)
                }
            })?;

        self.handle_response(response).await
    }

    /// Signal that all uploads are complete and start processing.
    pub async fn start_job(&self, job_id: &str) -> Result<JobStatus> {
        let url = format!("{}/jobs/{}/start", self.base_url, job_id);

        let response = self
            .client
            .post(&url)
            .bearer_auth(&self.api_key)
            .send()
            .await?;

        self.handle_response(response).await
    }

    /// Get the current status of a job.
    pub async fn get_job_status(&self, job_id: &str) -> Result<JobStatus> {
        let url = format!("{}/jobs/{}", self.base_url, job_id);

        let response = self
            .client
            .get(&url)
            .bearer_auth(&self.api_key)
            .send()
            .await?;

        self.handle_response(response).await
    }

    /// Get the SSE stream URL for real-time progress.
    pub fn stream_url(&self, job_id: &str) -> String {
        format!("{}/jobs/{}/stream", self.base_url, job_id)
    }

    /// Get the API key (for SSE auth header).
    pub fn api_key(&self) -> &str {
        &self.api_key
    }

    /// Get download URLs for completed job outputs.
    pub async fn get_outputs(&self, job_id: &str) -> Result<JobOutputResponse> {
        let url = format!("{}/jobs/{}/output", self.base_url, job_id);

        let response = self
            .client
            .get(&url)
            .bearer_auth(&self.api_key)
            .send()
            .await?;

        self.handle_response(response).await
    }

    /// Get usage for the current billing period.
    pub async fn get_usage(&self) -> Result<UsageResponse> {
        let url = format!("{}/usage", self.base_url);

        let response = self
            .client
            .get(&url)
            .bearer_auth(&self.api_key)
            .send()
            .await?;

        self.handle_response(response).await
    }

    /// Health check — is the API reachable?
    pub async fn health_check(&self) -> bool {
        let url = format!("{}/health", self.base_url);

        self.client
            .get(&url)
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false)
    }

    /// Generic response handler — parse JSON or return API error.
    async fn handle_response<T: serde::de::DeserializeOwned>(
        &self,
        response: reqwest::Response,
    ) -> Result<T> {
        let status = response.status();

        if status.is_success() {
            let body = response.json::<T>().await?;
            Ok(body)
        } else {
            let status_code = status.as_u16();
            let body = response.text().await.unwrap_or_default();

            let message = serde_json::from_str::<ApiErrorResponse>(&body)
                .map(|e| e.error)
                .unwrap_or(body);

            Err(CfmpegError::Api {
                status: status_code,
                message,
            })
        }
    }
}
