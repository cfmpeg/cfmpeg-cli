//! End-to-end tests: the real `cfmpeg` binary against a stand-in for the API
//! on 127.0.0.1. Nothing here reaches the network: inputs are either URLs that
//! only the (absent) remote worker would fetch, or small local files that are
//! uploaded to the stand-in.

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use tempfile::TempDir;

const INPUT_URL: &str = "https://example.com/input.mov";
const ENCODED: &[u8] = b"encoded output bytes";
/// `{origin}` is replaced with the address of the stand-in API.
const DIRECT_STREAM_JOB: &str = r#"{"job_id":"job_1","uploads":[],"ingest":{"mode":"direct_stream","stream_url":"{origin}/stream","claim_url":"{origin}/claim","input_format":"mpegts","stream_strategy":"passthrough"}}"#;

/// How the stand-in API answers for the one job it knows, `job_1`.
#[derive(Clone)]
struct Scenario {
    /// Status and body of `POST /v1/jobs`.
    create: (u16, &'static str),
    /// The job status every poll reports.
    status: &'static str,
    /// Status and body of `POST /v1/jobs/job_1/cancel`.
    cancel: (u16, &'static str),
    /// The size the output listing reports for the output file.
    listed_size: usize,
    download: Download,
    delivery: Delivery,
    /// Status of `POST /stream`, the direct stream upload.
    stream: u16,
    /// Status and body of `POST /v1/jobs/job_1/start`.
    start: (u16, &'static str),
    /// Whether the retention period of the outputs has passed.
    outputs_expired: bool,
}

#[derive(Clone, Copy)]
enum Delivery {
    /// One finished file per output.
    Final,
    /// Progressive batches; `complete` is what the batch listing reports.
    #[cfg(unix)]
    Batches { complete: bool },
}

#[derive(Clone, Copy)]
enum Download {
    /// The whole file.
    Complete,
    /// Promises 4096 bytes and closes the connection after a few.
    Truncated,
    /// Promises 4096 bytes, sends a few and then nothing.
    #[cfg(unix)]
    Stalled,
}

impl Default for Scenario {
    fn default() -> Self {
        Self {
            create: (200, r#"{"job_id":"job_1","uploads":[]}"#),
            status: "completed",
            cancel: (200, r#"{"job_id":"job_1","status":"cancelled"}"#),
            listed_size: ENCODED.len(),
            download: Download::Complete,
            delivery: Delivery::Final,
            stream: 200,
            start: (200, r#"{"job_id":"job_1","status":"queued"}"#),
            outputs_expired: false,
        }
    }
}

struct MockApi {
    origin: String,
    requests: Arc<Mutex<Vec<String>>>,
}

impl MockApi {
    fn start(scenario: Scenario) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock api");
        let origin = format!(
            "http://{}",
            listener.local_addr().expect("mock api address")
        );
        let requests = Arc::new(Mutex::new(Vec::new()));

        let server_origin = origin.clone();
        let server_requests = Arc::clone(&requests);
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let scenario = scenario.clone();
                let origin = server_origin.clone();
                let requests = Arc::clone(&server_requests);
                thread::spawn(move || serve(stream, &scenario, &origin, &requests));
            }
        });

        Self { origin, requests }
    }

    fn api_base(&self) -> String {
        format!("{}/v1", self.origin)
    }

    fn requests(&self) -> Vec<String> {
        self.requests.lock().expect("request log").clone()
    }

    #[cfg(unix)]
    fn wait_for_request(&self, request: &str) {
        wait_until(&format!("request `{request}`"), || {
            self.requests().iter().any(|seen| seen == request)
        });
    }
}

/// Answers one request and closes the connection.
fn serve(stream: TcpStream, scenario: &Scenario, origin: &str, requests: &Mutex<Vec<String>>) {
    let mut reader = BufReader::new(stream);
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
        return;
    }

    let mut content_length = 0usize;
    let mut chunked = false;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header).unwrap_or(0) == 0 || header.trim().is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                content_length = value.trim().parse().unwrap_or(0);
            }
            if name.eq_ignore_ascii_case("transfer-encoding") {
                chunked = value.trim().eq_ignore_ascii_case("chunked");
            }
        }
    }
    // The request body is read to its end and discarded.
    loop {
        let mut chunk_size = String::new();
        if chunked {
            if reader.read_line(&mut chunk_size).unwrap_or(0) == 0 {
                return;
            }
            content_length = usize::from_str_radix(chunk_size.trim(), 16).unwrap_or(0);
        }
        let mut body = vec![0u8; content_length + if chunked { 2 } else { 0 }];
        if reader.read_exact(&mut body).is_err() {
            return;
        }
        if !chunked || content_length == 0 {
            break;
        }
    }

    let mut parts = request_line.split_whitespace();
    let request = format!(
        "{} {}",
        parts.next().unwrap_or_default(),
        parts.next().unwrap_or_default()
    );
    requests.lock().expect("request log").push(request.clone());

    let json = |status: u16, body: &str| {
        format!(
            "HTTP/1.1 {status} Mock\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    };
    let file = |declared: usize, sent: &[u8]| {
        let mut response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {declared}\r\nConnection: close\r\n\r\n"
        )
        .into_bytes();
        response.extend_from_slice(sent);
        response
    };

    let job_status = format!(r#"{{"job_id":"job_1","status":"{}"}}"#, scenario.status);
    let response = match (request.as_str(), scenario.delivery) {
        ("GET /v1/health", _) => json(200, "{}"),
        ("POST /v1/jobs", _) => json(
            scenario.create.0,
            &scenario.create.1.replace("{origin}", origin),
        ),
        ("GET /v1/jobs/job_1", _) => json(200, &job_status),
        ("POST /v1/jobs/job_1/cancel", _) => json(scenario.cancel.0, scenario.cancel.1),
        ("POST /v1/jobs/job_1/ingest/segments/upload-targets", _) => json(
            200,
            &format!(
                r#"{{"targets":[{{"index":0,"upload_url":"{origin}/segments/0","headers":{{}}}}]}}"#
            ),
        ),
        ("PUT /segments/0", _) => json(200, "{}"),
        ("POST /stream", _) => json(scenario.stream, r#"{"error":"stream refused"}"#),
        ("POST /v1/jobs/job_1/start", _) => json(scenario.start.0, scenario.start.1),
        // prepare and the ingest progress calls all answer with the job.
        (request, _) if request.starts_with("POST /v1/jobs/job_1/") => json(200, &job_status),
        ("GET /v1/jobs/job_1/output", _) if scenario.outputs_expired => {
            json(410, r#"{"error":"Retained job outputs have expired."}"#)
        }
        ("GET /v1/jobs/job_1/output", Delivery::Final) => json(
            200,
            &format!(
                r#"{{"delivery_mode":"final_artifact","complete":true,"outputs":[{{"filename":"output_0.mp4","download_url":"{origin}/files/output_0.mp4","size_bytes":{}}}],"batches":[]}}"#,
                scenario.listed_size
            ),
        ),
        #[cfg(unix)]
        (
            "GET /v1/jobs/job_1/output" | "GET /v1/jobs/job_1/output-batches",
            Delivery::Batches { complete },
        ) => json(
            200,
            &format!(
                r#"{{"delivery_mode":"progressive_batches","complete":{complete},"outputs":[],"batches":[{{"index":0,"filename":"output_0.batch_00000.mp4","download_url":"{origin}/files/output_0.mp4","size_bytes":{}}}]}}"#,
                scenario.listed_size
            ),
        ),
        ("GET /files/output_0.mp4", _) => match scenario.download {
            Download::Complete => file(ENCODED.len(), ENCODED),
            Download::Truncated => file(4096, b"partial"),
            #[cfg(unix)]
            Download::Stalled => {
                // Keeps the connection open for longer than any test waits.
                let _ = reader.get_mut().write_all(&file(4096, b"partial"));
                thread::sleep(std::time::Duration::from_secs(120));
                return;
            }
        },
        (_, _) => json(404, r#"{"error":"not found"}"#),
    };

    let mut stream = reader.into_inner();
    let _ = stream.write_all(&response);
    let _ = stream.flush();
}

#[cfg(unix)]
fn wait_until(what: &str, condition: impl Fn() -> bool) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while !condition() {
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// A `cfmpeg` command that can only talk to `api_base` and reads no real config.
fn cfmpeg(api_base: &str, config_dir: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_cfmpeg"));
    command
        .env_clear()
        .env("CFMPEG_API_BASE", api_base)
        .env("CFMPEG_API_KEY", "cfm_test_key")
        .env("CFMPEG_CONFIG_DIR", config_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

/// An API base on a loopback port that nothing listens on.
fn unreachable_api_base() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("address");
    drop(listener);

    format!("http://{address}/v1")
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn file_names(directory: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(directory)
        .expect("read dir")
        .map(|entry| {
            entry
                .expect("dir entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    names.sort();
    names
}

/// A config directory plus a separate directory for output files.
fn workspace() -> (TempDir, TempDir) {
    (
        tempfile::tempdir().expect("config dir"),
        tempfile::tempdir().expect("output dir"),
    )
}

#[test]
fn downloads_the_output_through_a_temporary_file() {
    let api = MockApi::start(Scenario::default());
    let (config, outputs) = workspace();
    let target = outputs.path().join("out.mp4");
    fs::write(&target, b"previous contents").expect("existing output");

    let output = cfmpeg(&api.api_base(), config.path())
        .args(["-i", INPUT_URL, "-c:v", "libx264", "-crf", "23"])
        .arg(&target)
        .output()
        .expect("run cfmpeg");

    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(fs::read(&target).expect("read output"), ENCODED);
    assert_eq!(file_names(outputs.path()), vec!["out.mp4"]);
}

#[test]
fn failed_download_keeps_the_existing_file_and_leaves_no_partial_file() {
    let api = MockApi::start(Scenario {
        listed_size: 4096,
        download: Download::Truncated,
        ..Scenario::default()
    });
    let (config, outputs) = workspace();
    let target = outputs.path().join("out.mp4");
    fs::write(&target, b"previous contents").expect("existing output");

    let output = cfmpeg(&api.api_base(), config.path())
        .args(["-y", "-i", INPUT_URL])
        .arg(&target)
        .output()
        .expect("run cfmpeg");

    let stderr = stderr(&output);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("download failed"), "{stderr}");
    // The job is paid for and finished: it is kept, and the way back to its
    // output is shown.
    assert!(
        stderr.contains("remote job job_1 completed and was not cancelled"),
        "{stderr}"
    );
    assert!(stderr.contains("`cfmpeg download job_1`"), "{stderr}");
    assert!(
        stderr.contains(&format!("output_0.mp4\t{}/files/output_0.mp4", api.origin)),
        "{stderr}"
    );
    assert!(!api
        .requests()
        .contains(&"POST /v1/jobs/job_1/cancel".to_string()));
    assert_eq!(
        fs::read(&target).expect("read output"),
        b"previous contents"
    );
    assert_eq!(file_names(outputs.path()), vec!["out.mp4"]);
}

#[test]
fn download_of_unexpected_size_is_not_moved_into_place() {
    let api = MockApi::start(Scenario {
        listed_size: ENCODED.len() + 100,
        ..Scenario::default()
    });
    let (config, outputs) = workspace();
    let target = outputs.path().join("out.mp4");

    let output = cfmpeg(&api.api_base(), config.path())
        .args(["-i", INPUT_URL])
        .arg(&target)
        .output()
        .expect("run cfmpeg");

    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(
        stderr(&output).contains(&format!(
            "received {} bytes, expected {}",
            ENCODED.len(),
            ENCODED.len() + 100
        )),
        "{}",
        stderr(&output)
    );
    assert!(file_names(outputs.path()).is_empty());
}

#[test]
fn never_overwrite_refuses_an_existing_output_before_any_request() {
    let api = MockApi::start(Scenario::default());
    let (config, outputs) = workspace();
    let target = outputs.path().join("out.mp4");
    fs::write(&target, b"previous contents").expect("existing output");

    let output = cfmpeg(&api.api_base(), config.path())
        .args(["-n", "-i", INPUT_URL])
        .arg(&target)
        .output()
        .expect("run cfmpeg");

    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(
        stderr(&output).contains(&format!(
            "File '{}' already exists. Exiting.",
            target.display()
        )),
        "{}",
        stderr(&output)
    );
    assert_eq!(
        fs::read(&target).expect("read output"),
        b"previous contents"
    );
    assert_eq!(api.requests(), Vec::<String>::new());
}

#[test]
fn never_overwrite_still_writes_a_new_output() {
    let api = MockApi::start(Scenario::default());
    let (config, outputs) = workspace();
    let target = outputs.path().join("out.mp4");

    let output = cfmpeg(&api.api_base(), config.path())
        .args(["-n", "-i", INPUT_URL])
        .arg(&target)
        .output()
        .expect("run cfmpeg");

    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(fs::read(&target).expect("read output"), ENCODED);
}

#[test]
fn missing_output_directory_fails_before_any_request() {
    let api = MockApi::start(Scenario::default());
    let (config, outputs) = workspace();
    let target = outputs.path().join("missing").join("out.mp4");

    let output = cfmpeg(&api.api_base(), config.path())
        .args(["-i", INPUT_URL])
        .arg(&target)
        .output()
        .expect("run cfmpeg");

    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("does not exist"),
        "{}",
        stderr(&output)
    );
    assert_eq!(api.requests(), Vec::<String>::new());
}

#[test]
fn unknown_options_and_unsupported_outputs_fail_before_any_request() {
    let api = MockApi::start(Scenario::default());
    let (config, outputs) = workspace();

    for (arguments, expected) in [
        (
            &["-i", INPUT_URL, "-foo", "bar", "out.mp4"][..],
            "does not recognise the ffmpeg option `-foo`",
        ),
        (
            &["-i", INPUT_URL, "-f", "hls", "stream.m3u8"][..],
            "not supported for remote execution yet",
        ),
        (
            &["-i", INPUT_URL, "thumb%03d.jpg"][..],
            "not supported for remote execution yet",
        ),
        (
            &["-i", INPUT_URL, "-f", "mpegts", "pipe:1"][..],
            "not supported for remote execution yet",
        ),
        (
            &["-i", INPUT_URL, "-pass", "1", "out.mp4"][..],
            "`-pass` is not supported for remote execution yet",
        ),
        (
            &["-i", INPUT_URL, "-vf", "subtitles=subs.srt", "out.mp4"][..],
            "the `subtitles` filter reads the local path `subs.srt`",
        ),
        (
            &["-i", INPUT_URL, "-attach", "font.ttf", "out.mkv"][..],
            "`-attach` is not supported for remote execution yet",
        ),
        (
            &[
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440",
                "-t",
                "1",
                "out.wav",
            ][..],
            "lavfi input `sine=frequency=440` is not supported for remote execution yet",
        ),
    ] {
        let output = cfmpeg(&api.api_base(), config.path())
            .current_dir(outputs.path())
            .args(arguments)
            .output()
            .expect("run cfmpeg");

        assert_eq!(output.status.code(), Some(1), "{arguments:?}");
        assert!(stderr(&output).contains(expected), "{}", stderr(&output));
        assert!(stderr(&output).contains("--local"), "{}", stderr(&output));
    }

    assert_eq!(api.requests(), Vec::<String>::new());
    assert!(file_names(outputs.path()).is_empty());
}

#[test]
fn wait_timeout_cancels_the_remote_job() {
    let api = MockApi::start(Scenario {
        status: "processing",
        ..Scenario::default()
    });
    let (config, outputs) = workspace();

    let output = cfmpeg(&api.api_base(), config.path())
        .args(["--cf-timeout", "1s", "-i", INPUT_URL])
        .arg(outputs.path().join("out.mp4"))
        .output()
        .expect("run cfmpeg");

    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("job timed out after 1 seconds"),
        "{}",
        stderr(&output)
    );
    assert!(
        stderr(&output).contains("cancelled remote job job_1"),
        "{}",
        stderr(&output)
    );
    assert!(api
        .requests()
        .contains(&"POST /v1/jobs/job_1/cancel".to_string()));
    assert!(file_names(outputs.path()).is_empty());
}

#[test]
fn failed_cancellation_says_how_to_cancel_by_hand() {
    let api = MockApi::start(Scenario {
        status: "processing",
        cancel: (500, r#"{"error":"cancel is broken"}"#),
        ..Scenario::default()
    });
    let (config, outputs) = workspace();

    let output = cfmpeg(&api.api_base(), config.path())
        .args(["--cf-timeout", "1s", "-i", INPUT_URL])
        .arg(outputs.path().join("out.mp4"))
        .output()
        .expect("run cfmpeg");

    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("could not cancel remote job job_1")
            && stderr(&output).contains("cfmpeg cancel job_1"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn unreachable_api_is_an_error_that_names_the_fallback_flags() {
    let (config, outputs) = workspace();

    let output = cfmpeg(&unreachable_api_base(), config.path())
        .args(["-i", INPUT_URL])
        .arg(outputs.path().join("out.mp4"))
        .output()
        .expect("run cfmpeg");

    let stderr = stderr(&output);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("api unreachable"), "{stderr}");
    assert!(stderr.contains("--allow-fallback"), "{stderr}");
    assert!(stderr.contains("--local"), "{stderr}");
}

#[test]
fn insufficient_balance_is_an_error_that_names_the_fallback_flags() {
    let api = MockApi::start(Scenario {
        create: (
            402,
            r#"{"error":"Insufficient prepaid balance.","code":"insufficient_funds"}"#,
        ),
        ..Scenario::default()
    });
    let (config, outputs) = workspace();

    let output = cfmpeg(&api.api_base(), config.path())
        .args(["-i", INPUT_URL])
        .arg(outputs.path().join("out.mp4"))
        .output()
        .expect("run cfmpeg");

    let stderr = stderr(&output);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("remote balance is too low"), "{stderr}");
    assert!(stderr.contains("--allow-fallback"), "{stderr}");
}

#[test]
fn direct_stream_job_uploads_the_input_and_downloads_the_output() {
    let api = MockApi::start(Scenario {
        create: (200, DIRECT_STREAM_JOB),
        ..Scenario::default()
    });
    let (config, outputs) = workspace();
    let input = config.path().join("input.ts");
    fs::write(&input, vec![7u8; 64 * 1024]).expect("input file");
    let target = outputs.path().join("out.mp4");

    let output = cfmpeg(&api.api_base(), config.path())
        .arg("-i")
        .arg(&input)
        .args(["-c", "copy"])
        .arg(&target)
        .output()
        .expect("run cfmpeg");

    assert!(output.status.success(), "{}", stderr(&output));
    assert!(api.requests().contains(&"POST /stream".to_string()));
    assert_eq!(fs::read(&target).expect("read output"), ENCODED);
    assert_eq!(file_names(outputs.path()), vec!["out.mp4"]);
}

#[test]
fn failed_direct_stream_upload_ends_the_run_without_waiting_for_the_job() {
    let api = MockApi::start(Scenario {
        create: (200, DIRECT_STREAM_JOB),
        status: "processing",
        stream: 500,
        ..Scenario::default()
    });
    let (config, outputs) = workspace();
    let input = config.path().join("input.ts");
    fs::write(&input, b"transport stream").expect("input file");

    // The timeout only bounds this test if the upload failure went unnoticed.
    let output = cfmpeg(&api.api_base(), config.path())
        .args(["--cf-timeout", "20s", "-i"])
        .arg(&input)
        .arg(outputs.path().join("out.mp4"))
        .output()
        .expect("run cfmpeg");

    let stderr = stderr(&output);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(
        stderr.contains("api error (500): stream refused"),
        "{stderr}"
    );
    assert!(stderr.contains("cancelled remote job job_1"), "{stderr}");
    assert!(api
        .requests()
        .contains(&"POST /v1/jobs/job_1/cancel".to_string()));
    assert!(file_names(outputs.path()).is_empty());
}

#[test]
fn failed_start_cancels_the_created_job() {
    let api = MockApi::start(Scenario {
        start: (500, r#"{"error":"start is broken"}"#),
        ..Scenario::default()
    });
    let (config, outputs) = workspace();

    let output = cfmpeg(&api.api_base(), config.path())
        .args(["-i", INPUT_URL])
        .arg(outputs.path().join("out.mp4"))
        .output()
        .expect("run cfmpeg");

    let stderr = stderr(&output);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(
        stderr.contains("api error (500): start is broken"),
        "{stderr}"
    );
    assert!(stderr.contains("cancelled remote job job_1"), "{stderr}");
    assert!(api
        .requests()
        .contains(&"POST /v1/jobs/job_1/cancel".to_string()));
}

#[test]
fn refused_start_cancels_the_created_job_and_names_the_fallback_flags() {
    let api = MockApi::start(Scenario {
        start: (
            402,
            r#"{"error":"Insufficient prepaid balance.","code":"insufficient_funds"}"#,
        ),
        ..Scenario::default()
    });
    let (config, outputs) = workspace();

    let output = cfmpeg(&api.api_base(), config.path())
        .args(["-i", INPUT_URL])
        .arg(outputs.path().join("out.mp4"))
        .output()
        .expect("run cfmpeg");

    let stderr = stderr(&output);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("remote balance is too low"), "{stderr}");
    assert!(stderr.contains("--allow-fallback"), "{stderr}");
    assert!(api
        .requests()
        .contains(&"POST /v1/jobs/job_1/cancel".to_string()));
}

#[test]
fn job_that_failed_remotely_is_not_cancelled() {
    let api = MockApi::start(Scenario {
        status: "failed",
        ..Scenario::default()
    });
    let (config, outputs) = workspace();

    let output = cfmpeg(&api.api_base(), config.path())
        .args(["-i", INPUT_URL])
        .arg(outputs.path().join("out.mp4"))
        .output()
        .expect("run cfmpeg");

    let stderr = stderr(&output);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("job failed"), "{stderr}");
    assert!(!api
        .requests()
        .contains(&"POST /v1/jobs/job_1/cancel".to_string()));
}

#[test]
fn download_fetches_the_outputs_of_a_finished_job_again() {
    let api = MockApi::start(Scenario::default());
    let (config, outputs) = workspace();
    let download = |arguments: &[&str]| {
        cfmpeg(&api.api_base(), config.path())
            .arg("download")
            .args(arguments)
            .current_dir(outputs.path())
            .output()
            .expect("run cfmpeg")
    };

    // Without a path, the name the job stored the output with.
    let by_name = download(&["job_1"]);
    assert!(by_name.status.success(), "{}", stderr(&by_name));
    assert_eq!(
        fs::read(outputs.path().join("output_0.mp4")).expect("read output"),
        ENCODED
    );

    let renamed = download(&["job_1", "final.mp4"]);
    assert!(renamed.status.success(), "{}", stderr(&renamed));
    assert_eq!(
        fs::read(outputs.path().join("final.mp4")).expect("read output"),
        ENCODED
    );

    fs::write(outputs.path().join("final.mp4"), b"kept").expect("overwrite output");
    let refused = download(&["-n", "job_1", "final.mp4"]);
    assert_eq!(refused.status.code(), Some(1), "{}", stderr(&refused));
    assert!(
        stderr(&refused).contains("File 'final.mp4' already exists. Exiting."),
        "{}",
        stderr(&refused)
    );
    assert_eq!(
        fs::read(outputs.path().join("final.mp4")).expect("read output"),
        b"kept"
    );

    let overwritten = download(&["-y", "job_1", "final.mp4"]);
    assert!(overwritten.status.success(), "{}", stderr(&overwritten));
    assert_eq!(
        fs::read(outputs.path().join("final.mp4")).expect("read output"),
        ENCODED
    );

    assert_eq!(
        file_names(outputs.path()),
        vec!["final.mp4", "output_0.mp4"]
    );
    // Nothing is created, started or cancelled: the command only reads.
    assert!(api
        .requests()
        .iter()
        .all(|request| request.starts_with("GET ")));
}

#[test]
fn download_explains_why_a_job_has_nothing_to_download() {
    let (config, outputs) = workspace();

    for (scenario, arguments, expected) in [
        (
            Scenario {
                status: "processing",
                ..Scenario::default()
            },
            &["job_1"][..],
            "cannot download job job_1: the job has not finished yet (status: processing)",
        ),
        (
            Scenario {
                status: "failed",
                ..Scenario::default()
            },
            &["job_1"][..],
            "cannot download job job_1: the job failed and has no outputs",
        ),
        (
            Scenario {
                status: "cancelled",
                ..Scenario::default()
            },
            &["job_1"][..],
            "cannot download job job_1: the job was cancelled and has no outputs",
        ),
        (
            Scenario::default(),
            &["job_unknown"][..],
            "cannot download job job_unknown: no such job exists for this API key",
        ),
        (
            Scenario {
                outputs_expired: true,
                ..Scenario::default()
            },
            &["job_1"][..],
            "cannot download job job_1: its outputs have expired and are no longer in cloud storage",
        ),
        (
            Scenario::default(),
            &["job_1", "first.mp4", "second.mp4"][..],
            "cannot download job job_1: it has 1 output file(s), but 2 path(s) were given",
        ),
        (
            Scenario::default(),
            &["job_1", "missing/out.mp4"][..],
            "does not exist",
        ),
    ] {
        let api = MockApi::start(scenario);

        let output = cfmpeg(&api.api_base(), config.path())
            .arg("download")
            .args(arguments)
            .current_dir(outputs.path())
            .output()
            .expect("run cfmpeg");

        let stderr = stderr(&output);
        assert_eq!(output.status.code(), Some(1), "{arguments:?}: {stderr}");
        assert!(stderr.contains(expected), "{arguments:?}: {stderr}");
        assert!(!api
            .requests()
            .contains(&"GET /files/output_0.mp4".to_string()));
    }

    assert!(file_names(outputs.path()).is_empty());
}

#[test]
fn file_prefix_on_an_output_names_the_plain_path() {
    let api = MockApi::start(Scenario::default());
    let (config, outputs) = workspace();
    let target = outputs.path().join("out.mp4");

    let output = cfmpeg(&api.api_base(), config.path())
        .args(["-i", INPUT_URL])
        .arg(format!("file:{}", target.display()))
        .current_dir(outputs.path())
        .output()
        .expect("run cfmpeg");

    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(fs::read(&target).expect("read output"), ENCODED);
    assert_eq!(file_names(outputs.path()), vec!["out.mp4"]);
}

#[test]
fn output_that_is_also_an_input_fails_before_any_request() {
    let api = MockApi::start(Scenario::default());
    let (config, outputs) = workspace();
    let media = outputs.path().join("clip.mov");
    fs::write(&media, b"the only copy").expect("input file");

    let output = cfmpeg(&api.api_base(), config.path())
        .args(["-y", "-i", "clip.mov", "-c", "copy", "./clip.mov"])
        .current_dir(outputs.path())
        .output()
        .expect("run cfmpeg");

    let stderr = stderr(&output);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(
        stderr.contains("Output ./clip.mov same as Input #0 - exiting"),
        "{stderr}"
    );
    assert_eq!(fs::read(&media).expect("read input"), b"the only copy");
    assert_eq!(api.requests(), Vec::<String>::new());
}

#[test]
fn generated_lavfi_input_runs_without_a_local_file() {
    let api = MockApi::start(Scenario::default());
    let (config, outputs) = workspace();
    let target = outputs.path().join("out.mp4");

    let output = cfmpeg(&api.api_base(), config.path())
        .args(["-f", "lavfi", "-i", "color=c=black:s=64x64", "-t", "1"])
        .arg(&target)
        .output()
        .expect("run cfmpeg");

    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(fs::read(&target).expect("read output"), ENCODED);
}

#[cfg(unix)]
mod unix {
    use super::{
        cfmpeg, file_names, stderr, unreachable_api_base, wait_until, workspace, Delivery,
        Download, MockApi, Scenario, INPUT_URL,
    };
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, Output};

    /// Writes a stand-in `ffmpeg` that records its arguments and exits with 7.
    fn fake_ffmpeg(directory: &Path) -> (PathBuf, PathBuf) {
        let script = directory.join("fake-ffmpeg");
        let log = directory.join("fake-ffmpeg.log");
        fs::write(
            &script,
            format!("#!/bin/sh\necho \"$@\" > '{}'\nexit 7\n", log.display()),
        )
        .expect("write fake ffmpeg");
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).expect("chmod");

        (script, log)
    }

    fn send_signal(child: &Child, signal: &str) {
        let status = Command::new("kill")
            .args([signal, &child.id().to_string()])
            .status()
            .expect("run kill");
        assert!(status.success());
    }

    /// Starts a job that never finishes, signals cfmpeg once it is polling
    /// and returns how cfmpeg ended.
    fn interrupt_running_job(api: &MockApi, signal: &str) -> Output {
        let (config, outputs) = workspace();
        let child = cfmpeg(&api.api_base(), config.path())
            .args(["-i", INPUT_URL])
            .arg(outputs.path().join("out.mp4"))
            .spawn()
            .expect("spawn cfmpeg");

        api.wait_for_request("GET /v1/jobs/job_1");
        send_signal(&child, signal);
        let output = child.wait_with_output().expect("wait for cfmpeg");

        assert!(file_names(outputs.path()).is_empty());
        output
    }

    #[test]
    fn ctrl_c_cancels_the_remote_job_and_exits_130() {
        let api = MockApi::start(Scenario {
            status: "processing",
            ..Scenario::default()
        });

        let output = interrupt_running_job(&api, "-INT");

        assert_eq!(output.status.code(), Some(130), "{}", stderr(&output));
        assert!(
            stderr(&output).contains("cancelled remote job job_1"),
            "{}",
            stderr(&output)
        );
        assert!(!stderr(&output).contains("error:"), "{}", stderr(&output));
        assert!(api
            .requests()
            .contains(&"POST /v1/jobs/job_1/cancel".to_string()));
    }

    #[test]
    fn sigterm_cancels_the_remote_job_and_exits_143() {
        let api = MockApi::start(Scenario {
            status: "processing",
            ..Scenario::default()
        });

        let output = interrupt_running_job(&api, "-TERM");

        assert_eq!(output.status.code(), Some(143), "{}", stderr(&output));
        assert!(
            stderr(&output).contains("cancelled remote job job_1"),
            "{}",
            stderr(&output)
        );
        assert!(api
            .requests()
            .contains(&"POST /v1/jobs/job_1/cancel".to_string()));
    }

    #[test]
    fn ctrl_c_during_download_removes_the_partial_file() {
        let api = MockApi::start(Scenario {
            cancel: (
                409,
                r#"{"error":"Job cannot be cancelled from status [completed]."}"#,
            ),
            listed_size: 4096,
            download: Download::Stalled,
            ..Scenario::default()
        });
        let (config, outputs) = workspace();
        let target = outputs.path().join("out.mp4");
        fs::write(&target, b"previous contents").expect("existing output");

        let child = cfmpeg(&api.api_base(), config.path())
            .args(["-i", INPUT_URL])
            .arg(&target)
            .spawn()
            .expect("spawn cfmpeg");

        // The partial file sits next to the destination while bytes arrive.
        wait_until("the partial download", || {
            file_names(outputs.path())
                .iter()
                .any(|name| name.starts_with(".cfmpeg-") && name.ends_with("-out.mp4"))
        });
        send_signal(&child, "-INT");
        let output = child.wait_with_output().expect("wait for cfmpeg");

        let stderr = stderr(&output);
        assert_eq!(output.status.code(), Some(130), "{stderr}");
        // The job had completed, so there is nothing to cancel.
        assert!(
            stderr.contains("remote job job_1 completed and was not cancelled"),
            "{stderr}"
        );
        assert!(stderr.contains("/files/output_0.mp4"), "{stderr}");
        assert!(!api
            .requests()
            .contains(&"POST /v1/jobs/job_1/cancel".to_string()));
        assert_eq!(
            fs::read(&target).expect("read output"),
            b"previous contents"
        );
        assert_eq!(file_names(outputs.path()), vec!["out.mp4"]);
    }

    const SEGMENTED_JOB: &str = r#"{"job_id":"job_1","uploads":[],"ingest":{"mode":"segmented_upload","segment_duration_seconds":90},"output_delivery":{"mode":"progressive_batches"}}"#;

    /// Writes a stand-in for the bundled helper ffmpeg: asked to segment, it
    /// writes one segment; asked to join batches, it writes the output.
    fn fake_helper_ffmpeg(directory: &Path) -> PathBuf {
        let script = directory.join("fake-helper-ffmpeg");
        fs::write(
            &script,
            concat!(
                "#!/bin/sh\n",
                "for last; do :; done\n",
                "case \" $* \" in\n",
                "*\" -f segment \"*) printf 'segment bytes' > \"${last%\\%06d.mp4}000000.mp4\" ;;\n",
                "*) printf 'joined output' > \"$last\" ;;\n",
                "esac\n",
            ),
        )
        .expect("write fake helper ffmpeg");
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).expect("chmod");

        script
    }

    #[test]
    fn segmented_job_joins_progressive_batches_into_the_output() {
        let api = MockApi::start(Scenario {
            create: (200, SEGMENTED_JOB),
            delivery: Delivery::Batches { complete: true },
            ..Scenario::default()
        });
        let (config, outputs) = workspace();
        let scratch = tempfile::tempdir().expect("scratch dir");
        let input = config.path().join("input.mov");
        fs::write(&input, b"local input").expect("input file");
        let target = outputs.path().join("out.mp4");
        fs::write(&target, b"previous contents").expect("existing output");

        let output = cfmpeg(&api.api_base(), config.path())
            .env("CFMPEG_FFMPEG_BINARY", fake_helper_ffmpeg(config.path()))
            .env("TMPDIR", scratch.path())
            .arg("-i")
            .arg(&input)
            .arg(&target)
            .output()
            .expect("run cfmpeg");

        assert!(output.status.success(), "{}", stderr(&output));
        for request in ["PUT /segments/0", "POST /v1/jobs/job_1/ingest/complete"] {
            assert!(api.requests().contains(&request.to_string()), "{request}");
        }
        assert_eq!(fs::read(&target).expect("read output"), b"joined output");
        assert_eq!(file_names(outputs.path()), vec!["out.mp4"]);
        assert!(file_names(scratch.path()).is_empty());
    }

    #[test]
    fn download_joins_the_batches_of_a_finished_job_again() {
        let api = MockApi::start(Scenario {
            delivery: Delivery::Batches { complete: true },
            ..Scenario::default()
        });
        let (config, outputs) = workspace();
        let scratch = tempfile::tempdir().expect("scratch dir");

        let output = cfmpeg(&api.api_base(), config.path())
            .env("CFMPEG_FFMPEG_BINARY", fake_helper_ffmpeg(config.path()))
            .env("TMPDIR", scratch.path())
            .args(["download", "job_1"])
            .current_dir(outputs.path())
            .output()
            .expect("run cfmpeg");

        assert!(output.status.success(), "{}", stderr(&output));
        // `output_0.batch_00000.mp4` is a part of `output_0.mp4`.
        assert_eq!(
            fs::read(outputs.path().join("output_0.mp4")).expect("read output"),
            b"joined output"
        );
        assert_eq!(file_names(outputs.path()), vec!["output_0.mp4"]);
        assert!(file_names(scratch.path()).is_empty());
    }

    #[test]
    fn ctrl_c_during_download_command_removes_the_partial_file() {
        let api = MockApi::start(Scenario {
            listed_size: 4096,
            download: Download::Stalled,
            ..Scenario::default()
        });
        let (config, outputs) = workspace();

        let child = cfmpeg(&api.api_base(), config.path())
            .args(["download", "job_1"])
            .current_dir(outputs.path())
            .spawn()
            .expect("spawn cfmpeg");

        wait_until("the partial download", || {
            file_names(outputs.path())
                .iter()
                .any(|name| name.starts_with(".cfmpeg-"))
        });
        send_signal(&child, "-INT");
        let output = child.wait_with_output().expect("wait for cfmpeg");

        assert_eq!(output.status.code(), Some(130), "{}", stderr(&output));
        assert!(file_names(outputs.path()).is_empty());
        assert!(!api
            .requests()
            .contains(&"POST /v1/jobs/job_1/cancel".to_string()));
    }

    #[test]
    fn ctrl_c_while_batches_download_cancels_the_job_and_cleans_up() {
        let api = MockApi::start(Scenario {
            create: (200, SEGMENTED_JOB),
            status: "processing",
            delivery: Delivery::Batches { complete: false },
            ..Scenario::default()
        });
        let (config, outputs) = workspace();
        let scratch = tempfile::tempdir().expect("scratch dir");
        let input = config.path().join("input.mov");
        fs::write(&input, b"local input").expect("input file");

        let child = cfmpeg(&api.api_base(), config.path())
            .env("CFMPEG_FFMPEG_BINARY", fake_helper_ffmpeg(config.path()))
            .env("TMPDIR", scratch.path())
            .arg("-i")
            .arg(&input)
            .arg(outputs.path().join("out.mp4"))
            .spawn()
            .expect("spawn cfmpeg");

        // One batch has arrived and cfmpeg is waiting for the job and for more.
        api.wait_for_request("GET /files/output_0.mp4");
        api.wait_for_request("GET /v1/jobs/job_1");
        send_signal(&child, "-INT");
        let output = child.wait_with_output().expect("wait for cfmpeg");

        assert_eq!(output.status.code(), Some(130), "{}", stderr(&output));
        assert!(
            stderr(&output).contains("cancelled remote job job_1"),
            "{}",
            stderr(&output)
        );
        assert!(file_names(outputs.path()).is_empty());
        // The downloaded batches lived in the temporary directory.
        assert!(file_names(scratch.path()).is_empty());
    }

    #[test]
    fn unreachable_api_never_runs_local_ffmpeg_by_default() {
        let (config, outputs) = workspace();
        let (script, log) = fake_ffmpeg(config.path());

        let output = cfmpeg(&unreachable_api_base(), config.path())
            .env("CFMPEG_FFMPEG_BINARY", &script)
            .args(["-i", INPUT_URL])
            .arg(outputs.path().join("out.mp4"))
            .output()
            .expect("run cfmpeg");

        assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
        assert!(!log.exists());
    }

    #[test]
    fn allow_fallback_runs_local_ffmpeg_and_returns_its_exit_code() {
        let (config, outputs) = workspace();
        let (script, log) = fake_ffmpeg(config.path());

        let output = cfmpeg(&unreachable_api_base(), config.path())
            .env("CFMPEG_FFMPEG_BINARY", &script)
            .args(["--allow-fallback", "-i", INPUT_URL, "-c:v", "libx264"])
            .arg(outputs.path().join("out.mp4"))
            .output()
            .expect("run cfmpeg");

        let stderr = stderr(&output);
        assert_eq!(output.status.code(), Some(7), "{stderr}");
        assert!(
            stderr.contains("running locally with ffmpeg because fallback is allowed"),
            "{stderr}"
        );
        assert!(stderr.contains("api unreachable"), "{stderr}");
        assert!(!stderr.contains("error:"), "{stderr}");
        assert_eq!(
            fs::read_to_string(&log).expect("fake ffmpeg log").trim(),
            format!(
                "-i {INPUT_URL} -c:v libx264 {}",
                outputs.path().join("out.mp4").display()
            )
        );
    }

    #[test]
    fn fallback_can_be_allowed_by_environment_and_by_config() {
        let (config, outputs) = workspace();
        let (script, log) = fake_ffmpeg(outputs.path());
        let run = |allow_by_env: bool| {
            let mut command = cfmpeg(&unreachable_api_base(), config.path());
            if allow_by_env {
                command.env("CFMPEG_ALLOW_FALLBACK", "1");
            }
            command
                .env("CFMPEG_FFMPEG_BINARY", &script)
                .args(["-i", INPUT_URL, "out.mp4"])
                .current_dir(outputs.path())
                .output()
                .expect("run cfmpeg")
        };

        assert_eq!(run(true).status.code(), Some(7));
        fs::remove_file(&log).expect("remove log");

        fs::write(config.path().join("config.toml"), "allow_fallback = true\n")
            .expect("write config");
        assert_eq!(run(false).status.code(), Some(7));
        assert!(log.exists());
    }

    #[test]
    fn remote_flag_overrides_a_configured_fallback() {
        let (config, outputs) = workspace();
        let (script, log) = fake_ffmpeg(config.path());

        let output = cfmpeg(&unreachable_api_base(), config.path())
            .env("CFMPEG_ALLOW_FALLBACK", "true")
            .env("CFMPEG_FFMPEG_BINARY", &script)
            .args(["--remote", "-i", INPUT_URL])
            .arg(outputs.path().join("out.mp4"))
            .output()
            .expect("run cfmpeg");

        let stderr = stderr(&output);
        assert_eq!(output.status.code(), Some(1), "{stderr}");
        assert!(stderr.contains("api unreachable"), "{stderr}");
        assert!(!stderr.contains("--allow-fallback"), "{stderr}");
        assert!(!log.exists());
    }

    #[test]
    fn insufficient_balance_falls_back_when_allowed() {
        let api = MockApi::start(Scenario {
            create: (
                402,
                r#"{"error":"Insufficient prepaid balance.","code":"insufficient_funds"}"#,
            ),
            ..Scenario::default()
        });
        let (config, outputs) = workspace();
        let (script, log) = fake_ffmpeg(config.path());

        let output = cfmpeg(&api.api_base(), config.path())
            .env("CFMPEG_FFMPEG_BINARY", &script)
            .args(["--allow-fallback", "-i", INPUT_URL])
            .arg(outputs.path().join("out.mp4"))
            .output()
            .expect("run cfmpeg");

        let stderr = stderr(&output);
        assert_eq!(output.status.code(), Some(7), "{stderr}");
        assert!(stderr.contains("remote balance is too low"), "{stderr}");
        assert!(log.exists());
    }

    #[test]
    fn local_mode_returns_the_exit_code_of_ffmpeg() {
        let (config, outputs) = workspace();
        let (script, log) = fake_ffmpeg(config.path());

        // `-foo` is not a known option: local runs are not parsed by cfmpeg.
        let output = cfmpeg(&unreachable_api_base(), config.path())
            .env("CFMPEG_FFMPEG_BINARY", &script)
            .args(["--local", "-i", "in.mov", "-foo", "bar", "out.m3u8"])
            .current_dir(outputs.path())
            .output()
            .expect("run cfmpeg");

        assert_eq!(output.status.code(), Some(7), "{}", stderr(&output));
        assert!(!stderr(&output).contains("error:"), "{}", stderr(&output));
        assert_eq!(
            fs::read_to_string(&log).expect("fake ffmpeg log").trim(),
            "-i in.mov -foo bar out.m3u8"
        );
    }

    #[test]
    fn local_mode_leaves_options_of_the_local_ffmpeg_to_that_ffmpeg() {
        let (config, outputs) = workspace();
        let (script, log) = fake_ffmpeg(config.path());
        // `-allow_sw` belongs to VideoToolbox, which the workers' ffmpeg lacks.
        let arguments = [
            "-i",
            INPUT_URL,
            "-c:v",
            "h264_videotoolbox",
            "-allow_sw",
            "1",
            "out.mp4",
        ];

        let remote = cfmpeg(&unreachable_api_base(), config.path())
            .env("CFMPEG_FFMPEG_BINARY", &script)
            .args(arguments)
            .current_dir(outputs.path())
            .output()
            .expect("run cfmpeg");
        assert!(
            stderr(&remote).contains("does not recognise the ffmpeg option `-allow_sw`"),
            "{}",
            stderr(&remote)
        );
        assert!(!log.exists());

        let local = cfmpeg(&unreachable_api_base(), config.path())
            .env("CFMPEG_FFMPEG_BINARY", &script)
            .arg("--local")
            .args(arguments)
            .current_dir(outputs.path())
            .output()
            .expect("run cfmpeg");
        assert_eq!(local.status.code(), Some(7), "{}", stderr(&local));
        assert!(!stderr(&local).contains("error:"), "{}", stderr(&local));
        assert_eq!(
            fs::read_to_string(&log).expect("fake ffmpeg log").trim(),
            arguments.join(" ")
        );
    }
}
