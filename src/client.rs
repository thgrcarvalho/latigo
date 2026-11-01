use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use rand::Rng;
use reqwest::{Client, Method, Response};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::util::truncate;

#[derive(Clone, Deserialize)]
#[serde(default)]
pub struct RetryConfig {
    pub max_attempts: usize,
    pub initial_delay_ms: u64,
    pub max_delay_ms: u64,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            initial_delay_ms: 500,
            max_delay_ms: 8000,
        }
    }
}

impl RetryConfig {
    fn validate(&self) -> Result<()> {
        if self.max_attempts == 0 {
            bail!("retry.max_attempts must be at least 1");
        }
        if self.initial_delay_ms == 0 || self.max_delay_ms < self.initial_delay_ms {
            bail!(
                "retry delays must be positive and max_delay_ms must not be smaller than initial_delay_ms"
            );
        }
        Ok(())
    }

    fn backoff(&self, retry_number: usize) -> Duration {
        let shift = retry_number.saturating_sub(1).min(16) as u32;
        let base = self
            .initial_delay_ms
            .saturating_mul(1_u64 << shift)
            .min(self.max_delay_ms);
        let jitter = rand::rng().random_range(50..=100);
        Duration::from_millis(base.saturating_mul(jitter) / 100)
    }
}

pub struct ModelInfo {
    pub id: String,
    pub display_name: String,
    pub context_length: Option<usize>,
    pub supports_image_in: Option<bool>,
}

#[derive(Clone)]
pub struct OpenAICompatibleClient {
    client: Client,
    endpoint: String,
    models_endpoint: String,
    search_endpoint: String,
    api_key: String,
    model: String,
    retry: RetryConfig,
}

impl OpenAICompatibleClient {
    pub fn new(base_url: &str, api_key: String, model: String, retry: RetryConfig) -> Result<Self> {
        retry.validate()?;
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(30))
            .build()?;
        let base_url = base_url.trim_end_matches('/');
        Ok(Self {
            client,
            endpoint: format!("{base_url}/chat/completions"),
            models_endpoint: format!("{base_url}/models"),
            search_endpoint: format!("{base_url}/search"),
            api_key,
            model,
            retry,
        })
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn set_model(&mut self, model: String) {
        self.model = model;
    }

    pub async fn list_models(&self) -> Result<Vec<ModelInfo>> {
        let response = self
            .request_with_retry(Method::GET, &self.models_endpoint, None)
            .await?;
        let status = response.status();
        let body = response
            .text()
            .await
            .context("Could not read the provider response")?;
        if !status.is_success() {
            bail!("Provider returned HTTP {status}: {}", truncate(&body, 1000));
        }
        let data: Value = serde_json::from_str(&body).context("Provider returned invalid JSON")?;
        let entries = data
            .get("data")
            .and_then(Value::as_array)
            .with_context(|| {
                format!(
                    "Provider returned an invalid model list: {}",
                    truncate(&body, 1000)
                )
            })?;
        let mut models = entries
            .iter()
            .filter_map(|entry| {
                let id = entry.get("id").and_then(Value::as_str)?;
                let display_name = entry
                    .get("display_name")
                    .and_then(Value::as_str)
                    .unwrap_or(id);
                let context_length = entry
                    .get("context_length")
                    .and_then(Value::as_u64)
                    .or_else(|| {
                        entry
                            .get("limit")
                            .and_then(|limit| limit.get("context"))
                            .and_then(Value::as_u64)
                    })
                    .map(|value| value as usize);
                let supports_image_in = entry
                    .get("supports_image_in")
                    .and_then(Value::as_bool)
                    .or_else(|| {
                        entry
                            .get("modalities")
                            .and_then(|modalities| modalities.get("input"))
                            .and_then(Value::as_array)
                            .map(|inputs| inputs.iter().any(|input| input == "image"))
                    });
                Some(ModelInfo {
                    id: id.to_string(),
                    display_name: display_name.to_string(),
                    context_length,
                    supports_image_in,
                })
            })
            .collect::<Vec<_>>();
        models.sort_by(|left, right| left.id.cmp(&right.id));
        models.dedup_by(|left, right| left.id == right.id);
        Ok(models)
    }

    pub async fn model_context_window(&self) -> Result<Option<usize>> {
        Ok(self
            .list_models()
            .await?
            .into_iter()
            .find(|model| model.id == self.model)
            .and_then(|model| model.context_length))
    }

    pub async fn model_supports_image(&self) -> Result<Option<bool>> {
        Ok(self
            .list_models()
            .await?
            .into_iter()
            .find(|model| model.id == self.model)
            .and_then(|model| model.supports_image_in))
    }

    pub async fn hosted_search(
        &self,
        endpoint: Option<&str>,
        query: &str,
        max_results: usize,
    ) -> Result<Value> {
        let endpoint = endpoint.unwrap_or(&self.search_endpoint);
        let response = self
            .request_with_retry(
                Method::POST,
                endpoint,
                Some(&json!({
                    "text_query": query,
                    "num_search_results": max_results,
                    "timeout_seconds": 30,
                })),
            )
            .await?;
        let status = response.status();
        let body = response
            .text()
            .await
            .context("Could not read the Kimi search response")?;
        if !status.is_success() {
            bail!(
                "Kimi search returned HTTP {status}: {}",
                truncate(&body, 1000)
            );
        }
        serde_json::from_str(&body).context("Kimi search returned invalid JSON")
    }

    pub async fn complete(&self, messages: &[Value]) -> Result<String> {
        let payload = json!({
            "model": self.model,
            "messages": messages,
            "stream": false,
        });
        let response = self.send(&payload).await?;
        let status = response.status();
        let body = response
            .text()
            .await
            .context("Could not read the provider response")?;
        if !status.is_success() {
            bail!("Provider returned HTTP {status}: {}", truncate(&body, 1000));
        }
        let data: Value = serde_json::from_str(&body).context("Provider returned invalid JSON")?;
        data.get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| choices.first())
            .and_then(|choice| choice.get("message"))
            .and_then(|message| message.get("content"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .with_context(|| {
                format!(
                    "Provider returned an invalid response: {}",
                    truncate(&body, 1000)
                )
            })
    }

    pub async fn chat_stream(
        &self,
        messages: &[Value],
        tools: &Value,
        on_content: &mut dyn FnMut(&str),
    ) -> Result<Value> {
        let payload = json!({
            "model": self.model,
            "messages": messages,
            "tools": tools,
            "stream": true,
        });
        let response = self.send(&payload).await?;
        let status = response.status();
        if !status.is_success() {
            let body = response
                .text()
                .await
                .context("Could not read the provider response")?;
            bail!("Provider returned HTTP {status}: {}", truncate(&body, 1000));
        }

        let mut stream = response.bytes_stream();
        let mut buffer = Vec::new();
        let mut content = String::new();
        let mut reasoning_content = String::new();
        let mut tool_calls = BTreeMap::<usize, ToolCallBuilder>::new();
        let mut done = false;
        while let Some(chunk) = stream.next().await {
            buffer.extend_from_slice(&chunk.context("Provider stream failed")?);
            while let Some((end, delimiter_len)) = find_event_end(&buffer) {
                let event = buffer.drain(..end).collect::<Vec<_>>();
                buffer.drain(..delimiter_len);
                if process_event(
                    &event,
                    &mut content,
                    &mut reasoning_content,
                    &mut tool_calls,
                    on_content,
                )? {
                    done = true;
                    break;
                }
            }
            if done {
                break;
            }
        }
        if !done && !buffer.is_empty() {
            process_event(
                &buffer,
                &mut content,
                &mut reasoning_content,
                &mut tool_calls,
                on_content,
            )?;
        }
        if !done {
            bail!("Provider stream ended before completion");
        }
        if content.is_empty() && reasoning_content.is_empty() && tool_calls.is_empty() {
            bail!("Provider returned an empty stream");
        }

        let mut message = json!({
            "role": "assistant",
            "content": if content.is_empty() { Value::Null } else { json!(content) },
        });
        if !reasoning_content.is_empty() {
            message["reasoning_content"] = json!(reasoning_content);
        }
        if !tool_calls.is_empty() {
            message["tool_calls"] = Value::Array(
                tool_calls
                    .into_values()
                    .map(ToolCallBuilder::finish)
                    .collect(),
            );
        }
        Ok(message)
    }

    async fn send(&self, payload: &Value) -> Result<Response> {
        self.request_with_retry(Method::POST, &self.endpoint, Some(payload))
            .await
    }

    async fn request_with_retry(
        &self,
        method: Method,
        url: &str,
        payload: Option<&Value>,
    ) -> Result<Response> {
        let mut attempt = 1;
        loop {
            let mut request = self
                .client
                .request(method.clone(), url)
                .bearer_auth(&self.api_key);
            if let Some(payload) = payload {
                request = request.json(payload);
            }
            match request.send().await {
                Ok(response) => {
                    if !is_transient_status(response.status()) || attempt >= self.retry.max_attempts
                    {
                        return Ok(response);
                    }
                    let delay =
                        retry_after(&response).unwrap_or_else(|| self.retry.backoff(attempt - 1));
                    eprintln!(
                        "Transient provider response (HTTP {}). Retry {}/{} in {} ms.",
                        response.status(),
                        attempt,
                        self.retry.max_attempts - 1,
                        delay.as_millis()
                    );
                    attempt += 1;
                    tokio::time::sleep(delay).await;
                }
                Err(error) => {
                    if !(error.is_connect() || error.is_timeout())
                        || attempt >= self.retry.max_attempts
                    {
                        return Err(error)
                            .with_context(|| format!("Could not reach the provider at {url}"));
                    }
                    let delay = self.retry.backoff(attempt - 1);
                    eprintln!(
                        "Provider connection error. Retry {}/{} in {} ms: {}",
                        attempt,
                        self.retry.max_attempts - 1,
                        delay.as_millis(),
                        error
                    );
                    attempt += 1;
                    tokio::time::sleep(delay).await;
                }
            }
        }
    }
}

fn is_transient_status(status: reqwest::StatusCode) -> bool {
    status.as_u16() == 408 || status.as_u16() == 429 || status.is_server_error()
}

fn retry_after(response: &Response) -> Option<Duration> {
    let value = response
        .headers()
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?;
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    let date = httpdate::parse_http_date(value).ok()?;
    date.duration_since(std::time::SystemTime::now()).ok()
}

#[derive(Default)]
struct ToolCallBuilder {
    id: String,
    call_type: String,
    name: String,
    arguments: String,
}

impl ToolCallBuilder {
    fn finish(self) -> Value {
        let call_type = if self.call_type.is_empty() {
            "function"
        } else {
            &self.call_type
        };
        json!({
            "id": self.id,
            "type": call_type,
            "function": {
                "name": self.name,
                "arguments": self.arguments,
            }
        })
    }
}

fn process_event(
    event: &[u8],
    content: &mut String,
    reasoning_content: &mut String,
    tool_calls: &mut BTreeMap<usize, ToolCallBuilder>,
    on_content: &mut dyn FnMut(&str),
) -> Result<bool> {
    let event = std::str::from_utf8(event).context("Provider sent invalid UTF-8")?;
    let data = event
        .lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(str::trim)
        .collect::<Vec<_>>()
        .join("\n");
    if data.is_empty() {
        return Ok(false);
    }
    if data == "[DONE]" {
        return Ok(true);
    }
    let chunk: Value = serde_json::from_str(&data).context("Provider sent invalid stream JSON")?;
    if let Some(error) = chunk.get("error") {
        bail!("Provider stream error: {error}");
    }
    let Some(delta) = chunk
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .and_then(|choice| choice.get("delta"))
    else {
        return Ok(false);
    };
    if let Some(fragment) = delta.get("content").and_then(Value::as_str) {
        content.push_str(fragment);
        on_content(fragment);
    }
    if let Some(fragment) = delta.get("reasoning_content").and_then(Value::as_str) {
        reasoning_content.push_str(fragment);
    }
    if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
        for (position, call) in calls.iter().enumerate() {
            let index = call
                .get("index")
                .and_then(Value::as_u64)
                .unwrap_or(position as u64) as usize;
            let builder = tool_calls.entry(index).or_default();
            if let Some(id) = call.get("id").and_then(Value::as_str) {
                builder.id = id.to_string();
            }
            if let Some(call_type) = call.get("type").and_then(Value::as_str) {
                builder.call_type = call_type.to_string();
            }
            if let Some(function) = call.get("function") {
                if let Some(name) = function.get("name").and_then(Value::as_str) {
                    builder.name = name.to_string();
                }
                if let Some(arguments) = function.get("arguments").and_then(Value::as_str) {
                    builder.arguments.push_str(arguments);
                }
            }
        }
    }
    Ok(false)
}

fn find_event_end(buffer: &[u8]) -> Option<(usize, usize)> {
    buffer
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|position| (position, 4))
        .or_else(|| {
            buffer
                .windows(2)
                .position(|window| window == b"\n\n")
                .map(|position| (position, 2))
        })
}
