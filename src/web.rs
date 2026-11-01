use std::env;
use std::net::IpAddr;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use reqwest::{Client, Response, Url, redirect};
use scraper::{Html, Selector};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::client::OpenAICompatibleClient;
use crate::util::truncate;

const DEFAULT_SEARCH_ENDPOINT: &str = "https://api.search.brave.com/res/v1/web/search";
const MAX_RESPONSE_BYTES: usize = 2_000_000;

#[derive(Clone, Deserialize)]
#[serde(default)]
pub struct WebResearchConfig {
    pub provider: String,
    pub api_key_env: String,
    pub max_results: usize,
    pub search_endpoint: Option<String>,
    pub allow_private: bool,
}

impl Default for WebResearchConfig {
    fn default() -> Self {
        Self {
            provider: "kimi".to_string(),
            api_key_env: "BRAVE_SEARCH_API_KEY".to_string(),
            max_results: 5,
            search_endpoint: None,
            allow_private: false,
        }
    }
}

pub struct WebResearch {
    client: Client,
    kimi_client: OpenAICompatibleClient,
    config: WebResearchConfig,
}

impl WebResearch {
    pub fn new(config: WebResearchConfig, kimi_client: OpenAICompatibleClient) -> Result<Self> {
        if !matches!(config.provider.as_str(), "kimi" | "brave") {
            bail!(
                "Unsupported web search provider: {}. Use kimi or brave.",
                config.provider
            );
        }
        if config.max_results == 0 || config.max_results > 20 {
            bail!("web_research.max_results must be between 1 and 20");
        }
        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .redirect(redirect::Policy::none())
            .build()?;
        Ok(Self {
            client,
            kimi_client,
            config,
        })
    }

    pub fn search_tool_definition(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": "web_search",
                "description": "Search the web and return result titles, URLs, and snippets.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "query": {"type": "string", "description": "Web search query."}
                    },
                    "required": ["query"]
                }
            }
        })
    }

    pub async fn search(&self, query: &str) -> Result<String> {
        if query.trim().is_empty() {
            bail!("Search query cannot be empty");
        }
        if self.config.provider == "kimi" {
            return self.kimi_search(query).await;
        }
        let api_key = env::var(&self.config.api_key_env).with_context(|| {
            format!(
                "Set the {} environment variable to use web_search",
                self.config.api_key_env
            )
        })?;
        let endpoint = self
            .config
            .search_endpoint
            .as_deref()
            .unwrap_or(DEFAULT_SEARCH_ENDPOINT);
        let response = self
            .client
            .get(endpoint)
            .query(&[
                ("q", query),
                ("count", &self.config.max_results.to_string()),
            ])
            .header("X-Subscription-Token", api_key)
            .send()
            .await
            .context("Web search request failed")?;
        let body = response_text(response).await?;
        let data: Value =
            serde_json::from_str(&body).context("Search provider returned invalid JSON")?;
        let results = data
            .get("web")
            .and_then(|web| web.get("results"))
            .and_then(Value::as_array)
            .context("Search provider returned an invalid result list")?;
        if results.is_empty() {
            return Ok("No search results.".to_string());
        }
        let mut output = Vec::new();
        for (index, result) in results.iter().enumerate() {
            let title = result
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or("Untitled");
            let url = result.get("url").and_then(Value::as_str).unwrap_or("");
            let snippet = result
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("");
            output.push(format!("{}. {}\n{}\n{}", index + 1, title, url, snippet));
        }
        Ok(truncate(&output.join("\n\n"), 12000))
    }

    async fn kimi_search(&self, query: &str) -> Result<String> {
        let data = self
            .kimi_client
            .hosted_search(
                self.config.search_endpoint.as_deref(),
                query,
                self.config.max_results,
            )
            .await?;
        let results = data
            .get("search_results")
            .and_then(Value::as_array)
            .context("Kimi search returned an invalid result list")?;
        if results.is_empty() {
            return Ok("No search results.".to_string());
        }
        let mut output = Vec::new();
        for (index, result) in results.iter().take(self.config.max_results).enumerate() {
            let title = result
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or("Untitled");
            let url = result.get("url").and_then(Value::as_str).unwrap_or("");
            let site = result
                .get("site_name")
                .and_then(Value::as_str)
                .unwrap_or("");
            let content = result
                .get("content")
                .and_then(Value::as_str)
                .filter(|content| !content.is_empty())
                .or_else(|| result.get("snippet").and_then(Value::as_str))
                .unwrap_or("");
            output.push(format!(
                "{}. {}\n{}\n{}\n{}",
                index + 1,
                title,
                url,
                site,
                truncate(content, 2500)
            ));
        }
        Ok(truncate(&output.join("\n\n"), 12000))
    }

    pub async fn fetch(&self, url: &str) -> Result<String> {
        let mut current = Url::parse(url).context("Invalid URL")?;
        for redirect_count in 0..=5 {
            self.validate_destination(&current).await?;
            let response = self
                .client
                .get(current.clone())
                .send()
                .await
                .with_context(|| format!("Could not fetch {current}"))?;
            if response.status().is_redirection() {
                if redirect_count == 5 {
                    bail!("Too many redirects");
                }
                let location = response
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .context("Redirect is missing a Location header")?
                    .to_str()
                    .context("Redirect Location is not valid text")?;
                current = current.join(location).context("Invalid redirect URL")?;
                continue;
            }
            return self.read_page(current, response).await;
        }
        bail!("Too many redirects")
    }

    async fn read_page(&self, url: Url, response: Response) -> Result<String> {
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            bail!("URL returned HTTP {status}: {}", truncate(&body, 1000));
        }
        if response
            .content_length()
            .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
        {
            bail!("Response exceeds the {MAX_RESPONSE_BYTES}-byte limit");
        }
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .to_lowercase();
        if !content_type.is_empty()
            && !content_type.starts_with("text/")
            && !content_type.contains("json")
            && !content_type.contains("xml")
        {
            bail!("Unsupported content type: {content_type}");
        }
        let mut stream = response.bytes_stream();
        let mut bytes = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("Could not read URL response")?;
            if bytes.len() + chunk.len() > MAX_RESPONSE_BYTES {
                bail!("Response exceeds the {MAX_RESPONSE_BYTES}-byte limit");
            }
            bytes.extend_from_slice(&chunk);
        }
        let body = String::from_utf8_lossy(&bytes);
        if content_type.contains("html")
            || (!content_type.is_empty() && body.trim_start().starts_with('<'))
        {
            return extract_html(&url, &body);
        }
        Ok(format!("URL: {url}\n\n{}", truncate(&body, 20000)))
    }

    async fn validate_destination(&self, url: &Url) -> Result<()> {
        if !matches!(url.scheme(), "http" | "https") {
            bail!("Only HTTP and HTTPS URLs are supported");
        }
        if !url.username().is_empty() || url.password().is_some() {
            bail!("URLs containing credentials are not supported");
        }
        let host = url.host_str().context("URL is missing a host")?;
        if self.config.allow_private {
            return Ok(());
        }
        if host.eq_ignore_ascii_case("localhost")
            || host.to_lowercase().ends_with(".local")
            || host.to_lowercase().ends_with(".internal")
        {
            bail!("Private or local URL destinations are blocked");
        }
        if let Ok(address) = host.parse::<IpAddr>() {
            if is_non_public(address) {
                bail!("Private or local URL destinations are blocked");
            }
            return Ok(());
        }
        let port = url
            .port_or_known_default()
            .context("URL is missing a valid port")?;
        let addresses = tokio::net::lookup_host((host, port))
            .await
            .with_context(|| format!("Could not resolve URL host: {host}"))?;
        for address in addresses {
            if is_non_public(address.ip()) {
                bail!("URL host resolves to a private or local address");
            }
        }
        Ok(())
    }
}

async fn response_text(response: Response) -> Result<String> {
    let status = response.status();
    let body = response
        .text()
        .await
        .context("Could not read provider response")?;
    if !status.is_success() {
        bail!("Provider returned HTTP {status}: {}", truncate(&body, 1000));
    }
    Ok(body)
}

fn extract_html(url: &Url, html: &str) -> Result<String> {
    let document = Html::parse_document(html);
    let title_selector =
        Selector::parse("title").map_err(|error| anyhow::anyhow!(error.to_string()))?;
    let content_selector = Selector::parse("h1, h2, h3, h4, p, li, pre, blockquote, td, th")
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    let title = document
        .select(&title_selector)
        .next()
        .map(|element| element.text().collect::<Vec<_>>().join(" "))
        .unwrap_or_else(|| "Untitled".to_string());
    let mut fragments = document
        .select(&content_selector)
        .flat_map(|element| element.text())
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>();
    if fragments.is_empty() {
        let body_selector =
            Selector::parse("body").map_err(|error| anyhow::anyhow!(error.to_string()))?;
        fragments = document
            .select(&body_selector)
            .flat_map(|element| element.text())
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .collect();
    }
    Ok(format!(
        "Title: {}\nURL: {}\n\n{}",
        title.trim(),
        url,
        truncate(&fragments.join("\n"), 20000)
    ))
}

fn is_non_public(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => {
            address.is_private()
                || address.is_loopback()
                || address.is_link_local()
                || address.is_unspecified()
                || address.is_documentation()
                || address.is_broadcast()
                || address.is_multicast()
                || address.octets()[0] == 100 && (64..=127).contains(&address.octets()[1])
        }
        IpAddr::V6(address) => {
            address.is_loopback()
                || address.is_unspecified()
                || address.is_multicast()
                || address.is_unique_local()
                || address.is_unicast_link_local()
        }
    }
}
