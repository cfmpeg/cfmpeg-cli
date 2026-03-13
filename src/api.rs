use crate::config::Config;
use crate::error::{CfmpegError, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::time::Duration;

pub struct ApiClient {
    client: Client,
    base_url: String,
    api_key: String,
}

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
    pub source: String,
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

#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
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
    pub percent: Option<f64>,
    #[serde(default)]
    pub size_kb: Option<u64>,
    #[serde(default)]
    pub speed: Option<String>,
    #[serde(default)]
    pub time: Option<String>,
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
struct ApiErrorResponse {
    pub error: String,
}

#[derive(Debug, Deserialize)]
pub struct ProgressEvent {
    #[serde(flatten)]
    pub progress: JobProgress,
    pub status: JobState,
}

impl ApiClient {
    pub fn from_config(config: &Config) -> Result<Self> {
        let api_key = config.require_api_key()?;
        let base_url = config.api_base();
        let client = Client::builder()
            .user_agent(format!("cfmpeg/{}", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(30))
            .build()?;

        Ok(Self {
            client,
            base_url,
            api_key,
        })
    }

    pub async fn create_job(&self, request: &CreateJobRequest) -> Result<CreateJobResponse> {
        let response = self
            .client
            .post(format!("{}/jobs", self.base_url))
            .bearer_auth(&self.api_key)
            .json(request)
            .send()
            .await
            .map_err(|error| {
                if error.is_connect() || error.is_timeout() {
                    CfmpegError::ApiUnreachable(self.base_url.clone())
                } else {
                    CfmpegError::Http(error)
                }
            })?;

        self.handle_response(response).await
    }

    pub async fn start_job(&self, job_id: &str) -> Result<JobStatus> {
        let response = self
            .client
            .post(format!("{}/jobs/{job_id}/start", self.base_url))
            .bearer_auth(&self.api_key)
            .send()
            .await?;

        self.handle_response(response).await
    }

    pub async fn get_job_status(&self, job_id: &str) -> Result<JobStatus> {
        let response = self
            .client
            .get(format!("{}/jobs/{job_id}", self.base_url))
            .bearer_auth(&self.api_key)
            .send()
            .await?;

        self.handle_response(response).await
    }

    pub fn stream_url(&self, job_id: &str) -> String {
        format!("{}/jobs/{job_id}/stream", self.base_url)
    }

    pub fn api_key(&self) -> &str {
        &self.api_key
    }

    pub async fn get_outputs(&self, job_id: &str) -> Result<JobOutputResponse> {
        let response = self
            .client
            .get(format!("{}/jobs/{job_id}/output", self.base_url))
            .bearer_auth(&self.api_key)
            .send()
            .await?;

        self.handle_response(response).await
    }

    pub async fn get_usage(&self) -> Result<UsageResponse> {
        let response = self
            .client
            .get(format!("{}/usage", self.base_url))
            .bearer_auth(&self.api_key)
            .send()
            .await?;

        self.handle_response(response).await
    }

    pub async fn health_check(&self) -> bool {
        self.client
            .get(format!("{}/health", self.base_url))
            .timeout(Duration::from_secs(5))
            .send()
            .await
            .map(|response| response.status().is_success())
            .unwrap_or(false)
    }

    async fn handle_response<T: serde::de::DeserializeOwned>(
        &self,
        response: reqwest::Response,
    ) -> Result<T> {
        let status = response.status();

        if status.is_success() {
            return response.json::<T>().await.map_err(CfmpegError::Http);
        }

        let status_code = status.as_u16();
        let body = response.text().await.unwrap_or_default();
        let message = serde_json::from_str::<ApiErrorResponse>(&body)
            .map(|error| error.error)
            .unwrap_or(body);

        Err(CfmpegError::Api {
            status: status_code,
            message,
        })
    }
}
