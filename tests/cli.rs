use std::collections::VecDeque;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use base64::Engine;
use serde_json::{Value, json};
use tempfile::TempDir;

struct MockResponse {
    status: u16,
    content_type: &'static str,
    body: String,
    retry_after: Option<String>,
}

impl MockResponse {
    fn json(status: u16, body: Value) -> Self {
        Self {
            status,
            content_type: "application/json",
            body: body.to_string(),
            retry_after: None,
        }
    }

    fn sse(message: Value) -> Self {
        let chunk = json!({"choices": [{"delta": message}]});
        Self {
            status: 200,
            content_type: "text/event-stream",
            body: format!("data: {chunk}\n\ndata: [DONE]\n\n"),
            retry_after: None,
        }
    }

    fn text(content: &str) -> Self {
        Self::sse(json!({"role": "assistant", "content": content}))
    }

    fn tool_call(id: &str, name: &str, arguments: Value) -> Self {
        Self::sse(json!({
            "role": "assistant",
            "content": null,
            "tool_calls": [{
                "id": id,
                "type": "function",
                "function": {"name": name, "arguments": arguments.to_string()}
            }]
        }))
    }

    fn retry_after(mut self, value: &str) -> Self {
        self.retry_after = Some(value.to_string());
        self
    }
}

struct RecordedRequest {
    method: String,
    path: String,
    body: Value,
}

struct MockProvider {
    base_url: String,
    responses: Arc<std::sync::Mutex<VecDeque<MockResponse>>>,
    requests: Arc<std::sync::Mutex<Vec<RecordedRequest>>>,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl MockProvider {
    fn start(responses: Vec<MockResponse>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock provider");
        let address = listener.local_addr().expect("mock provider address");
        listener
            .set_nonblocking(true)
            .expect("nonblocking listener");
        let responses = Arc::new(std::sync::Mutex::new(VecDeque::from(responses)));
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let responses = responses.clone();
            let requests = requests.clone();
            let stop = stop.clone();
            thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            let request = read_request(&mut stream);
                            requests.lock().unwrap().push(request);
                            let response =
                                responses.lock().unwrap().pop_front().unwrap_or_else(|| {
                                    MockResponse::json(500, json!({"error": "no mock response"}))
                                });
                            write_response(&mut stream, response);
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(10));
                        }
                        Err(_) => break,
                    }
                }
            })
        };
        Self {
            base_url: format!("http://{address}/v1"),
            responses,
            requests,
            stop,
            thread: Some(thread),
        }
    }

    fn requests(&self) -> Vec<RecordedRequest> {
        self.requests.lock().unwrap().clone()
    }

    fn remaining(&self) -> usize {
        self.responses.lock().unwrap().len()
    }
}

impl Drop for MockProvider {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let address = self
            .base_url
            .trim_start_matches("http://")
            .trim_end_matches("/v1");
        let _ = TcpStream::connect(address);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Clone for RecordedRequest {
    fn clone(&self) -> Self {
        Self {
            method: self.method.clone(),
            path: self.path.clone(),
            body: self.body.clone(),
        }
    }
}

fn read_request(stream: &mut TcpStream) -> RecordedRequest {
    let mut data = Vec::new();
    let mut buffer = [0_u8; 8192];
    let header_end;
    loop {
        let count = stream.read(&mut buffer).expect("read request");
        if count == 0 {
            panic!("connection ended before request headers");
        }
        data.extend_from_slice(&buffer[..count]);
        if let Some(position) = data.windows(4).position(|window| window == b"\r\n\r\n") {
            header_end = position + 4;
            break;
        }
    }
    let headers = String::from_utf8_lossy(&data[..header_end]);
    let mut lines = headers.lines();
    let request_line = lines.next().expect("request line");
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("").to_string();
    let content_length = lines
        .find_map(|line| {
            line.to_lowercase()
                .strip_prefix("content-length:")
                .and_then(|value| value.trim().parse::<usize>().ok())
        })
        .unwrap_or(0);
    while data.len() < header_end + content_length {
        let count = stream.read(&mut buffer).expect("read request body");
        if count == 0 {
            break;
        }
        data.extend_from_slice(&buffer[..count]);
    }
    let body = if content_length == 0 {
        Value::Null
    } else {
        serde_json::from_slice(&data[header_end..header_end + content_length])
            .unwrap_or(Value::Null)
    };
    RecordedRequest { method, path, body }
}

fn write_response(stream: &mut TcpStream, response: MockResponse) {
    let mut headers = format!(
        "HTTP/1.1 {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n",
        response.status,
        response.content_type,
        response.body.len()
    );
    if let Some(retry_after) = response.retry_after {
        headers.push_str(&format!("Retry-After: {retry_after}\r\n"));
    }
    headers.push_str("\r\n");
    stream
        .write_all(headers.as_bytes())
        .expect("write response headers");
    stream
        .write_all(response.body.as_bytes())
        .expect("write response body");
}

struct TestProject {
    _temp: TempDir,
    root: PathBuf,
    workspace: PathBuf,
    sessions: PathBuf,
    config: PathBuf,
}

impl TestProject {
    fn new(provider: &MockProvider) -> Self {
        let temp = tempfile::tempdir().expect("temporary project");
        let root = temp.path().to_path_buf();
        let workspace = root.join("workspace");
        let sessions = root.join("sessions");
        fs::create_dir(&workspace).expect("create workspace");
        let config = root.join("config.toml");
        let project = Self {
            _temp: temp,
            root,
            workspace,
            sessions,
            config,
        };
        project.write_config(&provider.base_url, "");
        project
    }

    fn write_config(&self, base_url: &str, extra: &str) {
        fs::write(
            &self.config,
            format!(
                "model = \"mock-model\"\nbase_url = \"{base_url}\"\ncontext_window = 262144\n[retry]\nmax_attempts = 3\ninitial_delay_ms = 1\nmax_delay_ms = 2\n{extra}"
            ),
        )
        .expect("write config");
    }

    fn run(&self, input: &str) -> Output {
        let output = Command::new(env!("CARGO_BIN_EXE_latigo"))
            .arg("--config")
            .arg(&self.config)
            .arg("--workspace")
            .arg(&self.workspace)
            .arg("--sessions-dir")
            .arg(&self.sessions)
            .env("OPENAI_API_KEY", "test-key")
            .env("TERM", "dumb")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .and_then(|mut child| {
                use std::io::Write as _;
                child
                    .stdin
                    .take()
                    .unwrap()
                    .write_all(input.as_bytes())
                    .unwrap();
                child.wait_with_output()
            })
            .expect("run harness");
        assert!(
            output.status.success(),
            "stdout: {}\nstderr: {}",
            stdout(&output),
            stderr(&output)
        );
        output
    }

    fn session_files(&self) -> Vec<PathBuf> {
        fs::read_dir(&self.sessions)
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .map(|entry| entry.path())
                    .filter(|path| {
                        path.extension().and_then(|value| value.to_str()) == Some("json")
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn streams_tools_and_resumes_sessions() {
    let provider = MockProvider::start(vec![
        MockResponse::tool_call(
            "write",
            "write_file",
            json!({"path": "result.txt", "content": "saved"}),
        ),
        MockResponse::text("Created"),
        MockResponse::text("History restored"),
    ]);
    let project = TestProject::new(&provider);
    project.run("Create a file\ny\n/exit\n");
    assert_eq!(
        fs::read_to_string(project.workspace.join("result.txt")).unwrap(),
        "saved"
    );
    assert_eq!(project.session_files().len(), 1);
    let session: Value =
        serde_json::from_str(&fs::read_to_string(&project.session_files()[0]).unwrap()).unwrap();
    let id = session["id"].as_str().unwrap();
    let second = project.run(&format!("/resume {id}\nWhat did I ask?\n/exit\n"));
    assert!(stdout(&second).contains("History restored"));
    let requests = provider.requests();
    assert_eq!(requests.len(), 3);
    assert!(requests[2].body.to_string().contains("Create a file"));
    assert_eq!(provider.remaining(), 0);
}

#[test]
fn retries_transient_responses_but_not_authentication_errors() {
    let provider = MockProvider::start(vec![
        MockResponse::json(500, json!({"error": "temporary"})).retry_after("0"),
        MockResponse::json(429, json!({"error": "limited"})).retry_after("0"),
        MockResponse::text("Recovered"),
        MockResponse::json(401, json!({"error": "invalid"})),
    ]);
    let project = TestProject::new(&provider);
    let recovered = project.run("Try again\n/exit\n");
    assert!(stdout(&recovered).contains("Recovered"));
    let rejected = project.run("Authenticate\n/exit\n");
    assert!(stderr(&rejected).contains("401"));
    let requests = provider.requests();
    assert_eq!(requests.len(), 4);
    assert_eq!(requests[3].body["messages"].as_array().unwrap().len(), 2);
}

#[test]
fn rejects_incomplete_stream_without_replaying_it() {
    let provider = MockProvider::start(vec![MockResponse {
        status: 200,
        content_type: "text/event-stream",
        body: "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n".to_string(),
        retry_after: None,
    }]);
    let project = TestProject::new(&provider);
    let output = project.run("Stream\n/exit\n");
    assert!(stderr(&output).contains("stream ended before completion"));
    assert_eq!(provider.requests().len(), 1);
}

#[test]
fn displays_model_metadata_and_discovers_context_window() {
    let provider = MockProvider::start(vec![
        MockResponse::json(
            200,
            json!({
                "data": [{
                    "id": "mock-model",
                    "display_name": "Mock Model",
                    "context_length": 1048576,
                    "supports_image_in": true
                }]
            }),
        ),
        MockResponse::json(
            200,
            json!({
                "data": [{
                    "id": "mock-model",
                    "display_name": "Mock Model",
                    "context_length": 1048576,
                    "supports_image_in": true
                }]
            }),
        ),
    ]);
    let project = TestProject::new(&provider);
    let config = fs::read_to_string(&project.config)
        .unwrap()
        .replace("context_window = 262144\n", "");
    fs::write(&project.config, config).unwrap();
    let output = project.run("/models\n/usage\n/exit\n");
    assert!(stdout(&output).contains("Mock Model (mock-model)"));
    assert!(stdout(&output).contains("1048576 tokens"));
}

#[test]
fn refreshes_discovered_context_window_when_model_changes() {
    let models = || {
        json!({
            "data": [
                {"id": "mock-model", "context_length": 262144},
                {"id": "other-model", "context_length": 200000}
            ]
        })
    };
    let provider = MockProvider::start(vec![
        MockResponse::json(200, models()),
        MockResponse::json(200, models()),
    ]);
    let project = TestProject::new(&provider);
    let config = fs::read_to_string(&project.config)
        .unwrap()
        .replace("context_window = 262144\n", "");
    fs::write(&project.config, config).unwrap();

    let output = project.run("/model other-model\n/usage\n/exit\n");
    assert!(stdout(&output).contains("200000 tokens"));
    assert_eq!(provider.requests().len(), 2);

    project.write_config(&provider.base_url, "");
    let output = project.run("/model other-model\n/usage\n/exit\n");
    assert!(stdout(&output).contains("262144 tokens"));
    assert!(!stdout(&output).contains("200000 tokens"));
    assert_eq!(provider.requests().len(), 2);
}

#[test]
fn blocks_sensitive_files_without_sending_their_contents() {
    let provider = MockProvider::start(vec![
        MockResponse::tool_call("read", "read_file", json!({"path": ".env"})),
        MockResponse::text("Sensitive file blocked"),
    ]);
    let project = TestProject::new(&provider);
    fs::write(project.workspace.join(".env"), "do-not-send").unwrap();
    let output = project.run("Read the secret\n/exit\n");
    assert!(stdout(&output).contains("Sensitive file blocked"));
    let requests = provider.requests();
    assert!(
        requests[1]
            .body
            .to_string()
            .contains("sensitive files is blocked")
    );
    assert!(
        !requests
            .iter()
            .any(|request| request.body.to_string().contains("do-not-send"))
    );
}

#[cfg(unix)]
#[test]
fn runs_background_tasks_and_waits_for_completion() {
    let provider = MockProvider::start(vec![
        MockResponse::tool_call(
            "start",
            "run_command",
            json!({
                "command": "sleep 0.1; echo done",
                "background": true,
                "description": "short task"
            }),
        ),
        MockResponse::tool_call("wait", "wait_for", json!({"timeout": 5})),
        MockResponse::text("Task finished"),
    ]);
    let project = TestProject::new(&provider);
    let output = project.run("Run a background task\ny\n/exit\n");
    assert!(stdout(&output).contains("Task finished"));
    let requests = provider.requests();
    assert!(requests[2].body.to_string().contains("completed"));
    let logs = fs::read_dir(project.root.join("tasks"))
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    assert_eq!(logs.len(), 1);
    assert!(fs::read_to_string(&logs[0]).unwrap().contains("done"));
}

#[test]
fn uses_kimi_hosted_search_without_a_brave_key() {
    let provider = MockProvider::start(vec![
        MockResponse::tool_call("search", "web_search", json!({"query": "Rust"})),
        MockResponse::json(
            200,
            json!({
                "search_results": [{
                    "title": "Rust Book",
                    "url": "https://example.com/rust",
                    "site_name": "Example",
                    "content": "Hosted search content"
                }]
            }),
        ),
        MockResponse::text("Search complete"),
    ]);
    let project = TestProject::new(&provider);
    let output = project.run("Search for Rust\n/exit\n");
    assert!(stdout(&output).contains("Search complete"));
    let requests = provider.requests();
    assert_eq!(requests[1].method, "POST");
    assert_eq!(requests[1].path, "/v1/search");
    assert_eq!(requests[1].body["text_query"], "Rust");
    assert!(
        requests[2]
            .body
            .to_string()
            .contains("Hosted search content")
    );
}

#[test]
fn sends_multimodal_image_attachments() {
    let provider = MockProvider::start(vec![
        MockResponse::json(
            200,
            json!({
                "data": [{"id": "mock-model", "supports_image_in": true}]
            }),
        ),
        MockResponse::text("Image accepted"),
    ]);
    let project = TestProject::new(&provider);
    let png = base64::engine::general_purpose::STANDARD
        .decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==")
        .unwrap();
    fs::write(project.workspace.join("pixel.png"), png).unwrap();
    let output = project.run("Analyze @pixel.png\n/exit\n");
    assert!(stdout(&output).contains("Image accepted"));
    let requests = provider.requests();
    let chat = requests
        .iter()
        .find(|request| request.path == "/v1/chat/completions")
        .unwrap();
    let content = &chat.body["messages"][1]["content"];
    assert!(content.is_array());
    assert_eq!(content[1]["type"], "image_url");
    assert!(
        content[1]["image_url"]["url"]
            .as_str()
            .unwrap()
            .starts_with("data:image/png;base64,")
    );
}

#[test]
fn coding_tools_and_text_attachments_work_together() {
    let provider = MockProvider::start(vec![
        MockResponse::tool_call(
            "read",
            "read_file",
            json!({"path": "src/main.rs", "start_line": 2, "end_line": 3}),
        ),
        MockResponse::tool_call("files", "search_files", json!({"query": "main"})),
        MockResponse::tool_call("content", "search_content", json!({"query": "needle"})),
        MockResponse::tool_call(
            "patch",
            "apply_patch",
            json!({"path": "src/main.rs", "old_string": "beta", "new_string": "BETA"}),
        ),
        MockResponse::text("Coding tools complete"),
    ]);
    let project = TestProject::new(&provider);
    fs::create_dir(project.workspace.join("src")).unwrap();
    fs::write(
        project.workspace.join("src/main.rs"),
        "alpha\nbeta\ngamma\n",
    )
    .unwrap();
    fs::write(project.workspace.join("src/lib.rs"), "needle in library\n").unwrap();
    let output = project.run("Inspect @src/main.rs\ny\n/exit\n");
    assert!(stdout(&output).contains("Coding tools complete"));
    assert_eq!(
        fs::read_to_string(project.workspace.join("src/main.rs")).unwrap(),
        "alpha\nBETA\ngamma\n"
    );
    let requests = provider.requests();
    assert!(
        requests[0]
            .body
            .to_string()
            .contains("Attached file: src/main.rs")
    );
    assert_eq!(
        requests[1].body["messages"]
            .as_array()
            .unwrap()
            .last()
            .unwrap()["content"],
        "beta\ngamma"
    );
}
