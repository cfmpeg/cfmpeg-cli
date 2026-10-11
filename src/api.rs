use crate::config::Config;
use crate::error::{describe_http_error, CfmpegError, Result};
use crate::remote::RemoteExecutionOptions;
use reqwest::header::CONTENT_TYPE;
use reqwest::{Client, RequestBuilder, Response, Url};
use serde::de::{self, DeserializeOwned, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;
use std::time::Duration;

#[derive(Clone)]
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_delivery: Option<OutputDeliveryRequest>,
}

#[derive(Debug, Serialize)]
pub struct JobInput {
    pub filename: String,
    pub size_bytes: u64,
    pub source: InputSource,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CreateJobResponse {
    pub job_id: String,
    #[serde(default)]
    pub uploads: Vec<UploadTarget>,
    #[serde(default)]
    pub ingest: JobIngest,
    #[serde(default)]
    pub output_delivery: JobOutputDelivery,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InputSource {
    Local,
    Special,
    Url,
}

#[derive(Debug, Default, Deserialize)]
pub struct JobIngest {
    #[serde(default)]
    pub mode: IngestMode,
    #[serde(default)]
    pub stream_url: Option<String>,
    #[serde(default)]
    pub claim_url: Option<String>,
    #[serde(default)]
    pub input_format: Option<String>,
    #[serde(default)]
    pub stream_strategy: Option<StreamStrategy>,
    #[serde(default)]
    pub segment_duration_seconds: Option<u64>,
}

#[derive(Debug, Default, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IngestMode {
    DirectStream,
    SegmentedUpload,
    #[default]
    #[serde(other)]
    Durable,
}

#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StreamStrategy {
    Passthrough,
    CopyRemux,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Default, Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OutputDeliveryMode {
    ProgressiveBatches,
    #[default]
    #[serde(other)]
    FinalArtifact,
}

#[derive(Debug, Serialize)]
pub struct OutputDeliveryRequest {
    pub mode: OutputDeliveryMode,
}

#[derive(Debug, Default, Deserialize)]
pub struct JobOutputDelivery {
    #[serde(default)]
    pub mode: OutputDeliveryMode,
}

#[derive(Debug, Deserialize, Clone)]
pub struct SegmentUploadTarget {
    pub index: u32,
    pub upload_url: String,
    #[serde(default, deserialize_with = "deserialize_headers")]
    pub headers: HashMap<String, String>,
}

#[derive(Debug, Deserialize)]
pub struct SegmentUploadTargetsResponse {
    #[serde(default)]
    pub targets: Vec<SegmentUploadTarget>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct UploadTarget {
    pub file_id: u64,
    pub filename: String,
    pub upload_url: String,
    pub method: UploadMethod,
    pub part_size: u64,
    pub part_urls: Vec<String>,
    #[serde(default, deserialize_with = "deserialize_headers")]
    pub headers: HashMap<String, String>,
}

#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UploadMethod {
    Multipart,
    #[serde(other)]
    Direct,
}

#[derive(Debug, Serialize, Default)]
pub struct StartJobRequest {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub multipart_uploads: Vec<CompletedMultipartUpload>,
}

#[derive(Debug, Serialize)]
pub struct CompleteIngestRequest {
    pub segment_count: u32,
}

#[derive(Debug, Serialize)]
pub struct IngestProgressRequest {
    pub uploaded_segment_count: u32,
}

#[derive(Debug, Serialize)]
pub struct SegmentUploadTargetsRequest {
    pub start_index: u32,
    pub count: u32,
}

#[derive(Debug, Serialize, Clone)]
pub struct CompletedMultipartUpload {
    pub file_id: u64,
    pub parts: Vec<CompletedMultipartPart>,
}

#[derive(Debug, Serialize, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct CompletedMultipartPart {
    pub part_number: u32,
    pub etag: String,
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

#[derive(Debug, Deserialize, Clone)]
pub struct JobProgress {
    #[serde(default)]
    pub detail: Option<String>,
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
    pub stage: Option<String>,
    #[serde(default)]
    pub time: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct JobOutputResponse {
    #[serde(default)]
    pub delivery_mode: OutputDeliveryMode,
    #[serde(default)]
    pub complete: bool,
    #[serde(default)]
    pub outputs: Vec<OutputFile>,
    #[serde(default)]
    pub batches: Vec<OutputBatch>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct OutputFile {
    pub filename: String,
    pub download_url: String,
    pub size_bytes: u64,
}

#[derive(Debug, Deserialize, Clone)]
pub struct OutputBatch {
    pub index: u32,
    pub filename: String,
    pub download_url: String,
    pub size_bytes: u64,
}

#[derive(Debug, Deserialize)]
pub struct UsageResponse {
    pub period_start: String,
    pub period_end: String,
    pub cpu_minutes: f64,
    #[serde(default)]
    pub total_cost_millicents: i64,
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

fn parse_error_response(
    status_code: u16,
    content_type: Option<&str>,
    body: &str,
) -> (String, Option<String>) {
    if let Ok(parsed) = serde_json::from_str::<ApiErrorResponse>(body) {
        return (parsed.error, parsed.code);
    }

    let trimmed = body.trim();
    let is_html = content_type.is_some_and(|value| {
        value.contains("text/html") || value.contains("application/xhtml+xml")
    }) || trimmed.starts_with("<!DOCTYPE html")
        || trimmed.starts_with("<html");

    if is_html {
        let message = if status_code >= 500 {
            "server returned an HTML error page instead of JSON".to_string()
        } else {
            "api returned an unexpected HTML response".to_string()
        };

        return (message, Some("unexpected_html_response".to_string()));
    }

    let single_line = trimmed
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("request failed");

    (truncate_error_message(single_line, 240), None)
}

fn truncate_error_message(message: &str, max_len: usize) -> String {
    if message.chars().count() <= max_len {
        return message.to_string();
    }

    message.chars().take(max_len).collect::<String>() + "..."
}

#[derive(Debug, Deserialize)]
pub struct ProgressEvent {
    #[serde(flatten)]
    pub progress: JobProgress,
    pub status: JobState,
    #[serde(default)]
    pub error: Option<String>,
}

const NO_BODY: Option<&()> = None;

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
            .post_request("/jobs", Some(request))
            .send()
            .await
            .map_err(|error| {
                if error.is_connect() || error.is_timeout() {
                    CfmpegError::ApiUnreachable(format!(
                        "{} ({})",
                        self.base_url,
                        describe_http_error(&error)
                    ))
                } else {
                    CfmpegError::from(error)
                }
            })?;

        handle_response(response).await
    }

    pub async fn start_job(&self, job_id: &str, request: &StartJobRequest) -> Result<JobStatus> {
        self.post(&format!("/jobs/{job_id}/start"), Some(request))
            .await
    }

    pub async fn prepare_job(&self, job_id: &str) -> Result<JobStatus> {
        self.post(&format!("/jobs/{job_id}/prepare"), NO_BODY).await
    }

    pub async fn cancel_job(&self, job_id: &str) -> Result<JobStatus> {
        self.post(&format!("/jobs/{job_id}/cancel"), NO_BODY).await
    }

    pub async fn request_segment_upload_targets(
        &self,
        job_id: &str,
        request: &SegmentUploadTargetsRequest,
    ) -> Result<Vec<SegmentUploadTarget>> {
        let payload: SegmentUploadTargetsResponse = self
            .post(
                &format!("/jobs/{job_id}/ingest/segments/upload-targets"),
                Some(request),
            )
            .await?;

        Ok(payload.targets)
    }

    pub async fn complete_segmented_ingest(
        &self,
        job_id: &str,
        request: &CompleteIngestRequest,
    ) -> Result<JobStatus> {
        self.post(&format!("/jobs/{job_id}/ingest/complete"), Some(request))
            .await
    }

    pub async fn report_segmented_ingest_progress(
        &self,
        job_id: &str,
        request: &IngestProgressRequest,
    ) -> Result<JobStatus> {
        self.post(&format!("/jobs/{job_id}/ingest/progress"), Some(request))
            .await
    }

    pub async fn get_job_status(&self, job_id: &str) -> Result<JobStatus> {
        self.get(&format!("/jobs/{job_id}")).await
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
        self.get(&format!("/jobs/{job_id}/output")).await
    }

    pub async fn get_output_batches(&self, job_id: &str) -> Result<JobOutputResponse> {
        self.get(&format!("/jobs/{job_id}/output-batches")).await
    }

    pub async fn get_usage(&self) -> Result<UsageResponse> {
        self.get("/usage").await
    }

    pub async fn health_check(&self) -> Result<()> {
        let response = self
            .client
            .get(format!("{}/health", self.base_url))
            .timeout(Duration::from_secs(5))
            .send()
            .await
            .map_err(|error| {
                CfmpegError::ApiUnreachable(format!(
                    "{} ({})",
                    self.base_url,
                    describe_http_error(&error)
                ))
            })?;

        if response.status().is_success() {
            Ok(())
        } else {
            Err(CfmpegError::ApiUnreachable(format!(
                "{} (health returned {})",
                self.base_url,
                response.status()
            )))
        }
    }

    async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        let response = self
            .client
            .get(format!("{}{path}", self.base_url))
            .bearer_auth(&self.api_key)
            .send()
            .await?;

        handle_response(response).await
    }

    async fn post<T: DeserializeOwned, B: Serialize>(
        &self,
        path: &str,
        body: Option<&B>,
    ) -> Result<T> {
        let response = self.post_request(path, body).send().await?;

        handle_response(response).await
    }

    fn post_request<B: Serialize>(&self, path: &str, body: Option<&B>) -> RequestBuilder {
        let request = self
            .client
            .post(format!("{}{path}", self.base_url))
            .bearer_auth(&self.api_key);

        match body {
            Some(body) => request.json(body),
            None => request,
        }
    }
}

async fn handle_response<T: DeserializeOwned>(response: Response) -> Result<T> {
    if response.status().is_success() {
        return response.json::<T>().await.map_err(CfmpegError::from);
    }

    Err(error_from_response(response).await)
}

pub(crate) async fn error_from_response(response: Response) -> CfmpegError {
    let status_code = response.status().as_u16();
    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let body = response.text().await.unwrap_or_default();
    let (message, code) = parse_error_response(status_code, content_type.as_deref(), &body);

    CfmpegError::Api {
        status: status_code,
        code,
        message,
    }
}

fn default_currency() -> String {
    "usd".to_string()
}

#[cfg(test)]
mod tests {
    use super::{
        parse_error_response, ApiClient, CompletedMultipartPart, CreateJobResponse, IngestMode,
        JobOutputResponse, JobState, JobStatus, OutputDeliveryMode, ProgressEvent, StartJobRequest,
        StreamStrategy, UploadMethod, UploadTarget, UsageResponse,
    };

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
                "file_id": 42,
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

        assert_eq!(target.file_id, 42);
        assert_eq!(target.method, UploadMethod::Direct);
        assert_eq!(
            target.headers.get("x-test-header").map(String::as_str),
            Some("signed-value")
        );
    }

    #[test]
    fn deserializes_empty_upload_headers_array() {
        let target: UploadTarget = serde_json::from_str(
            r#"{
                "file_id": 7,
                "filename": "input.mov",
                "upload_url": "https://example.com/upload",
                "method": "direct",
                "part_size": 123,
                "part_urls": [],
                "headers": []
            }"#,
        )
        .expect("empty header array should deserialize");

        assert_eq!(target.file_id, 7);
        assert!(target.headers.is_empty());
    }

    #[test]
    fn omits_empty_multipart_uploads_from_start_request() {
        let payload = serde_json::to_value(StartJobRequest::default())
            .expect("start request should serialize");

        assert_eq!(payload, serde_json::json!({}));
    }

    #[test]
    fn serializes_completed_multipart_uploads() {
        let payload = serde_json::to_value(StartJobRequest {
            multipart_uploads: vec![super::CompletedMultipartUpload {
                file_id: 9,
                parts: vec![
                    CompletedMultipartPart {
                        part_number: 2,
                        etag: "\"etag-2\"".to_string(),
                    },
                    CompletedMultipartPart {
                        part_number: 1,
                        etag: "\"etag-1\"".to_string(),
                    },
                ],
            }],
        })
        .expect("start request should serialize");

        assert_eq!(
            payload,
            serde_json::json!({
                "multipart_uploads": [{
                    "file_id": 9,
                    "parts": [
                        {"part_number": 2, "etag": "\"etag-2\""},
                        {"part_number": 1, "etag": "\"etag-1\""}
                    ]
                }]
            })
        );
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
            base_url: "https://cfmpeg.com/v1".to_string(),
            api_key: "cfm_testing".to_string(),
        };

        assert!(client.should_stream_progress());
    }

    #[test]
    fn defaults_ingest_mode_to_durable() {
        let response: CreateJobResponse = serde_json::from_str(
            r#"{
                "job_id": "job_123",
                "uploads": []
            }"#,
        )
        .expect("create job response should deserialize");

        assert_eq!(response.ingest.mode, IngestMode::Durable);
    }

    #[test]
    fn deserializes_direct_stream_ingest_targets() {
        let response: CreateJobResponse = serde_json::from_str(
            r#"{
                "job_id": "job_123",
                "uploads": [],
                "ingest": {
                    "mode": "direct_stream",
                    "stream_url": "https://worker.example.test/stream-highcpu",
                    "claim_url": "https://app.example.test/v1/callbacks/jobs/job_123/stream-claim",
                    "input_format": "mpegts",
                    "stream_strategy": "copy_remux"
                }
            }"#,
        )
        .expect("direct stream response should deserialize");

        assert_eq!(response.ingest.mode, IngestMode::DirectStream);
        assert_eq!(
            response.ingest.stream_strategy,
            Some(StreamStrategy::CopyRemux)
        );
        assert_eq!(
            response.ingest.stream_url.as_deref(),
            Some("https://worker.example.test/stream-highcpu")
        );
        assert_eq!(response.ingest.input_format.as_deref(), Some("mpegts"));
    }

    #[test]
    fn deserializes_segmented_ingest_targets() {
        let response: CreateJobResponse = serde_json::from_str(
            r#"{
                "job_id": "job_123",
                "uploads": [],
                "ingest": {
                    "mode": "segmented_upload",
                    "segment_duration_seconds": 90
                }
            }"#,
        )
        .expect("segmented ingest response should deserialize");

        assert_eq!(response.ingest.mode, IngestMode::SegmentedUpload);
        assert_eq!(response.ingest.segment_duration_seconds, Some(90));
    }

    #[test]
    fn prefers_json_api_errors() {
        let (message, code) = parse_error_response(
            422,
            Some("application/json"),
            r#"{"error":"The given data was invalid.","code":"validation_error"}"#,
        );

        assert_eq!(message, "The given data was invalid.");
        assert_eq!(code.as_deref(), Some("validation_error"));
    }

    #[test]
    fn summarizes_html_api_errors() {
        let (message, code) = parse_error_response(
            500,
            Some("text/html; charset=UTF-8"),
            "<!DOCTYPE html><html><body><h1>500</h1></body></html>",
        );

        assert_eq!(
            message,
            "server returned an HTML error page instead of JSON"
        );
        assert_eq!(code.as_deref(), Some("unexpected_html_response"));
    }

    #[test]
    fn truncates_plain_text_api_errors() {
        let long_body = "x".repeat(300);
        let (message, code) = parse_error_response(500, Some("text/plain"), &long_body);

        assert_eq!(message.len(), 243);
        assert!(message.ends_with("..."));
        assert_eq!(code, None);
    }

    #[test]
    fn deserializes_progress_event_error_message() {
        let event: ProgressEvent = serde_json::from_str(
            r#"{
                "status": "failed",
                "percent": 42,
                "speed": "0.9x",
                "error": "ffmpeg failed: no such filter"
            }"#,
        )
        .expect("progress event should deserialize");

        assert!(matches!(event.status, JobState::Failed));
        assert_eq!(event.progress.percent, Some(42.0));
        assert_eq!(
            event.error.as_deref(),
            Some("ffmpeg failed: no such filter")
        );
    }

    #[test]
    fn deserializes_final_output_payload_with_shared_contract() {
        let response: JobOutputResponse = serde_json::from_str(
            r#"{
                "delivery_mode": "final_artifact",
                "complete": true,
                "outputs": [{
                    "filename": "output.mp4",
                    "download_url": "https://example.com/output.mp4",
                    "size_bytes": 42
                }],
                "batches": []
            }"#,
        )
        .expect("final output response should deserialize");

        assert_eq!(response.delivery_mode, OutputDeliveryMode::FinalArtifact);
        assert!(response.complete);
        assert_eq!(response.outputs.len(), 1);
        assert!(response.batches.is_empty());
    }

    #[test]
    fn deserializes_progressive_output_payload_with_shared_contract() {
        let response: JobOutputResponse = serde_json::from_str(
            r#"{
                "delivery_mode": "progressive_batches",
                "complete": false,
                "outputs": [],
                "batches": [{
                    "index": 0,
                    "filename": "output_0.batch_00000.mp4",
                    "download_url": "https://example.com/output_0.batch_00000.mp4",
                    "size_bytes": 128
                }]
            }"#,
        )
        .expect("progressive output response should deserialize");

        assert_eq!(
            response.delivery_mode,
            OutputDeliveryMode::ProgressiveBatches
        );
        assert!(!response.complete);
        assert!(response.outputs.is_empty());
        assert_eq!(response.batches.len(), 1);
        assert_eq!(response.batches[0].index, 0);
    }

    #[test]
    fn deserializes_usage_millicent_costs() {
        let response: UsageResponse = serde_json::from_str(
            r#"{
                "period_start": "2026-06-01",
                "period_end": "2026-06-30",
                "cpu_minutes": 0.5,
                "gpu_minutes": 0,
                "total_cost_cents": 0,
                "total_cost_millicents": 369,
                "jobs_count": 3,
                "balance_cents": 4698,
                "balance_millicents": 4698347,
                "currency": "usd"
            }"#,
        )
        .expect("usage response should deserialize");

        assert_eq!(response.total_cost_millicents, 369);
    }
}
