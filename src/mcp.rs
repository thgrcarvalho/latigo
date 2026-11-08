use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

#[derive(Clone, Default, Deserialize)]
#[serde(default)]
pub struct McpConfig {
    pub servers: BTreeMap<String, McpServerConfig>,
}

#[derive(Clone, Deserialize)]
#[serde(default)]
pub struct McpServerConfig {
    pub command: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub cwd: Option<PathBuf>,
    pub timeout_seconds: u64,
}

impl Default for McpServerConfig {
    fn default() -> Self {
        Self {
            command: String::new(),
            args: Vec::new(),
            env: BTreeMap::new(),
            cwd: None,
            timeout_seconds: 30,
        }
    }
}

impl McpConfig {
    pub fn validate(&self) -> Result<()> {
        for (name, server) in &self.servers {
            validate_server_name(name)?;
            if server.command.trim().is_empty() {
                bail!("MCP server {name} has an empty command");
            }
            if server.timeout_seconds == 0 {
                bail!("MCP server {name} timeout_seconds must be at least 1");
            }
        }
        Ok(())
    }
}

#[derive(Default)]
pub struct McpManager {
    servers: BTreeMap<String, McpServer>,
    tools: BTreeMap<String, RemoteTool>,
}

struct McpServer {
    client: McpClient,
    tool_count: usize,
}

struct RemoteTool {
    server: String,
    remote_name: String,
    definition: Value,
}

impl McpManager {
    pub async fn connect(config: McpConfig, workspace: &Path) -> Result<Self> {
        config.validate()?;
        let mut manager = Self {
            servers: BTreeMap::new(),
            tools: BTreeMap::new(),
        };
        for (name, server_config) in config.servers {
            let mut client = McpClient::start(&name, &server_config, workspace)
                .await
                .with_context(|| format!("Could not start MCP server {name}"))?;
            let tools = client
                .list_tools()
                .await
                .with_context(|| format!("Could not discover tools from MCP server {name}"))?;
            let mut tool_count = 0;
            for tool in tools {
                let remote_name = tool
                    .get("name")
                    .and_then(Value::as_str)
                    .context("MCP tool is missing a name")?
                    .to_string();
                let description = tool
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or("Tool provided by an MCP server.");
                let parameters = tool
                    .get("inputSchema")
                    .cloned()
                    .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
                let exposed_name = format!("mcp__{name}__{}", exposed_tool_name(&remote_name));
                if manager.tools.contains_key(&exposed_name) {
                    bail!("MCP tool name collision: {exposed_name}");
                }
                manager.tools.insert(
                    exposed_name.clone(),
                    RemoteTool {
                        server: name.clone(),
                        remote_name,
                        definition: json!({
                            "type": "function",
                            "function": {
                                "name": exposed_name,
                                "description": format!("[MCP: {name}] {description}"),
                                "parameters": parameters
                            }
                        }),
                    },
                );
                tool_count += 1;
            }
            manager
                .servers
                .insert(name, McpServer { client, tool_count });
        }
        Ok(manager)
    }

    pub fn tool_definitions(&self) -> Vec<Value> {
        self.tools
            .values()
            .map(|tool| tool.definition.clone())
            .collect()
    }

    pub fn contains(&self, name: &str) -> bool {
        self.tools.contains_key(name)
    }

    pub async fn call_tool(&mut self, name: &str, arguments: Value) -> Result<String> {
        let tool = self
            .tools
            .get(name)
            .with_context(|| format!("Unknown MCP tool: {name}"))?;
        let server = self
            .servers
            .get_mut(&tool.server)
            .with_context(|| format!("MCP server is not connected: {}", tool.server))?;
        let result = server
            .client
            .request(
                "tools/call",
                json!({"name": tool.remote_name, "arguments": arguments}),
            )
            .await?;
        format_tool_result(result)
    }

    pub fn status(&self) -> Vec<String> {
        if self.servers.is_empty() {
            return vec!["No MCP servers configured.".to_string()];
        }
        self.servers
            .iter()
            .map(|(name, server)| {
                format!(
                    "{name}: connected, {} tool(s), protocol {}",
                    server.tool_count, server.client.protocol_version
                )
            })
            .collect()
    }
}

struct McpClient {
    name: String,
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
    timeout: Duration,
    protocol_version: String,
}

impl McpClient {
    async fn start(name: &str, config: &McpServerConfig, workspace: &Path) -> Result<Self> {
        let mut command = Command::new(&config.command);
        command
            .args(&config.args)
            .envs(&config.env)
            .current_dir(config.cwd.as_deref().unwrap_or(workspace))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let mut child = command.spawn().with_context(|| {
            format!(
                "Could not launch {} {}",
                config.command,
                config.args.join(" ")
            )
        })?;
        let stdin = child
            .stdin
            .take()
            .context("MCP server stdin is unavailable")?;
        let stdout = child
            .stdout
            .take()
            .context("MCP server stdout is unavailable")?;
        let mut client = Self {
            name: name.to_string(),
            child,
            stdin,
            stdout: BufReader::new(stdout),
            next_id: 1,
            timeout: Duration::from_secs(config.timeout_seconds),
            protocol_version: "2024-11-05".to_string(),
        };
        let initialized = client
            .request(
                "initialize",
                json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": {"name": "latigo", "version": env!("CARGO_PKG_VERSION")}
                }),
            )
            .await?;
        if let Some(version) = initialized.get("protocolVersion").and_then(Value::as_str) {
            client.protocol_version = version.to_string();
        }
        client
            .notify("notifications/initialized", json!({}))
            .await?;
        Ok(client)
    }

    async fn list_tools(&mut self) -> Result<Vec<Value>> {
        let result = self.request("tools/list", json!({})).await?;
        result
            .get("tools")
            .and_then(Value::as_array)
            .cloned()
            .context("MCP tools/list response does not contain a tools array")
    }

    async fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        self.write_message(json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params
        }))
        .await
    }

    async fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        self.write_message(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        }))
        .await?;
        let response = tokio::time::timeout(self.timeout, self.read_response(id))
            .await
            .map_err(|_| {
                anyhow::anyhow!(
                    "MCP server {} timed out after {} seconds during {method}",
                    self.name,
                    self.timeout.as_secs()
                )
            })??;
        if let Some(error) = response.get("error") {
            bail!(
                "MCP server {} returned an error for {method}: {error}",
                self.name
            );
        }
        response
            .get("result")
            .cloned()
            .context("MCP response is missing result")
    }

    async fn write_message(&mut self, message: Value) -> Result<()> {
        let mut encoded = serde_json::to_vec(&message)?;
        encoded.push(b'\n');
        self.stdin.write_all(&encoded).await?;
        self.stdin.flush().await?;
        Ok(())
    }

    async fn read_response(&mut self, expected_id: u64) -> Result<Value> {
        loop {
            if self.child.try_wait()?.is_some() {
                bail!("MCP server {} exited unexpectedly", self.name);
            }
            let mut line = String::new();
            let bytes = self.stdout.read_line(&mut line).await?;
            if bytes == 0 {
                bail!("MCP server {} closed its output", self.name);
            }
            if line.trim().is_empty() {
                continue;
            }
            let message: Value = serde_json::from_str(&line)
                .with_context(|| format!("Invalid JSON from MCP server {}", self.name))?;
            if message.get("id").and_then(Value::as_u64) == Some(expected_id) {
                return Ok(message);
            }
        }
    }
}

fn format_tool_result(result: Value) -> Result<String> {
    let is_error = result
        .get("isError")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let content = result
        .get("content")
        .and_then(Value::as_array)
        .map(|parts| {
            parts
                .iter()
                .map(|part| match part.get("type").and_then(Value::as_str) {
                    Some("text") => part
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    _ => serde_json::to_string(part)
                        .unwrap_or_else(|_| "Unknown content".to_string()),
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .filter(|content| !content.is_empty())
        .unwrap_or_else(|| serde_json::to_string_pretty(&result).unwrap_or_default());
    if is_error {
        bail!("MCP tool reported an error: {content}");
    }
    Ok(content)
}

fn validate_server_name(name: &str) -> Result<()> {
    if name.is_empty()
        || !name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'))
    {
        bail!("MCP server name must contain only ASCII letters, numbers, '_' or '-': {name}");
    }
    Ok(())
}

fn exposed_tool_name(name: &str) -> String {
    name.chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '_' | '-') {
                character
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_text_and_rejects_mcp_errors() {
        let result = json!({"content": [{"type": "text", "text": "hello"}]});
        assert_eq!(format_tool_result(result).unwrap(), "hello");
        let error = json!({"isError": true, "content": [{"type": "text", "text": "failed"}]});
        assert!(
            format_tool_result(error)
                .unwrap_err()
                .to_string()
                .contains("failed")
        );
    }

    #[test]
    fn validates_and_normalizes_exposed_names() {
        assert!(validate_server_name("local-server_1").is_ok());
        assert!(validate_server_name("../bad").is_err());
        assert_eq!(exposed_tool_name("read.file"), "read_file");
    }
}
