use crate::config::Config;
use crate::error::{CfmpegError, Result};
use crate::remote::RemoteExecutionOptions;
use reqwest::Client;
use reqwest::Url;
use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;
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
    #[serde(skip_serializing_if = "RemoteExecutionOptions::is_empty")]
    pub execution: RemoteExecutionOptions,
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
    #[serde(default, deserialize_with = "deserialize_headers")]
    pub headers: HashMap<String, String>,
}

fn deserialize_headers<'de, D>(
    deserializer: D,
) -> std::result::Result<HashMap<String, String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct HeadersVisitor;

    impl<'de> Visitor<'de> for HeadersVisitor {
        type Value = HashMap<String, String>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("an object of header key/value pairs or an empty array")
        }

        fn visit_map<A>(self, mut map: A) -> std::result::Result<Self::Value, A::Error>
        where
            A: MapAccess<'de>,
        {
            let mut headers = HashMap::new();

            while let Some((key, value)) = map.next_entry::<String, String>()? {
                headers.insert(key, value);
            }

            Ok(headers)
        }

        fn visit_seq<A>(self, mut seq: A) -> std::result::Result<Self::Value, A::Error>
        where
            A: SeqAccess<'de>,
        {
            if seq.next_element::<serde::de::IgnoredAny>()?.is_some() {
                return Err(de::Error::custom("headers array must be empty"));
            }

            Ok(HashMap::new())
        }
    }

    deserializer.deserialize_any(HeadersVisitor)
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
    Queued,
    Uploading,
    Processing,
    Completed,
    Failed,
    Cancelled,
}

#[cfg(test)]
mod tests {
    use super::{ApiClient, JobState, JobStatus, UploadTarget};

    #[test]
    fn deserializes_queued_job_status() {
        let status: JobStatus = serde_json::from_str(
            r#"{
                "job_id": "job_123",
                "status": "queued"
            }"#,
        )
        .expect("queued status should deserialize");

        assert!(matches!(status.status, JobState::Queued));
    }

    #[test]
    fn deserializes_upload_headers() {
        let target: UploadTarget = serde_json::from_str(
            r#"{
                "filename": "input.mov",
                "upload_url": "https://example.com/upload",
                "method": "direct",
                "part_size": 123,
                "part_urls": [],
                "headers": {
                    "x-test-header": "signed-value"
                }
            }"#,
        )
        .expect("upload target should deserialize");

        assert_eq!(
            target.headers.get("x-test-header").map(String::as_str),
            Some("signed-value")
        );
    }

    #[test]
    fn deserializes_empty_upload_headers_array() {
        let target: UploadTarget = serde_json::from_str(
            r#"{
                "filename": "input.mov",
                "upload_url": "https://example.com/upload",
                "method": "direct",
                "part_size": 123,
                "part_urls": [],
                "headers": []
            }"#,
        )
        .expect("empty header array should deserialize");

        assert!(target.headers.is_empty());
    }

    #[test]
    fn skips_streaming_progress_for_loopback_hosts() {
        let client = ApiClient {
            client: reqwest::Client::new(),
            base_url: "http://127.0.0.1:8000/v1".to_string(),
            api_key: "cfm_testing".to_string(),
        };

        assert!(!client.should_stream_progress());
    }

    #[test]
    fn streams_progress_for_remote_hosts() {
        let client = ApiClient {
            client: reqwest::Client::new(),
            base_url: "https://api.cfmpeg.dev/v1".to_string(),
            api_key: "cfm_testing".to_string(),
        };

        assert!(client.should_stream_progress());
    }
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
    #[serde(default)]
    pub balance_millicents: i64,
    #[serde(default = "default_currency")]
    pub currency: String,
}

#[derive(Debug, Deserialize)]
struct ApiErrorResponse {
    pub error: String,
    #[serde(default)]
    pub code: Option<String>,
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

    pub fn should_stream_progress(&self) -> bool {
        let Ok(url) = Url::parse(&self.base_url) else {
            return true;
        };

        let Some(host) = url.host_str() else {
            return true;
        };

        !matches!(host, "127.0.0.1" | "localhost" | "::1")
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
        let parsed = serde_json::from_str::<ApiErrorResponse>(&body).ok();
        let message = parsed
            .as_ref()
            .map(|error| error.error.clone())
            .unwrap_or(body);
        let code = parsed.and_then(|error| error.code);

        Err(CfmpegError::Api {
            status: status_code,
            code,
            message,
        })
    }
}

fn default_currency() -> String {
    "usd".to_string()
}
