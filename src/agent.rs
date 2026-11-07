use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};

use crate::client::{ModelInfo, OpenAICompatibleClient};
use crate::mcp::McpManager;
use crate::tools::{PermissionMode, Workspace};
use crate::web::{WebResearch, WebResearchConfig};

pub enum AgentEvent {
    Content(String),
    ToolCall(String),
    ToolResult { name: String, output: String },
    Compacted { before: usize, after: usize },
}

pub struct Agent {
    client: OpenAICompatibleClient,
    workspace: Workspace,
    web: WebResearch,
    mcp: McpManager,
    max_steps: usize,
    context_window: usize,
    auto_compact: bool,
    messages: Vec<Value>,
    image_support: Option<bool>,
}

impl Agent {
    pub fn new(
        client: OpenAICompatibleClient,
        workspace: Workspace,
        web_config: WebResearchConfig,
        mcp: McpManager,
        max_steps: usize,
        context_window: usize,
        auto_compact: bool,
    ) -> Result<Self> {
        let system_message = json!({
            "role": "system",
            "content": format!(
                "You are a coding assistant running in a small CLI harness. Use the available tools when needed. The workspace is {}. Keep answers concise and report important tool results.",
                workspace.root.display()
            ),
        });
        Ok(Self {
            client: client.clone(),
            workspace,
            web: WebResearch::new(web_config, client)?,
            mcp,
            max_steps,
            context_window,
            auto_compact,
            messages: vec![system_message],
            image_support: None,
        })
    }

    pub fn messages(&self) -> &[Value] {
        &self.messages
    }

    pub fn restore_messages(&mut self, messages: Vec<Value>) -> Result<()> {
        if messages
            .first()
            .and_then(|message| message.get("role"))
            .and_then(Value::as_str)
            != Some("system")
        {
            bail!("Session does not contain a valid system message");
        }
        self.messages = messages;
        Ok(())
    }

    pub fn model(&self) -> &str {
        self.client.model()
    }

    pub fn set_model(&mut self, model: String) {
        self.client.set_model(model);
        self.image_support = None;
    }

    pub fn reasoning_effort(&self) -> Option<&str> {
        self.client.reasoning_effort()
    }

    pub fn set_reasoning_effort(&mut self, effort: Option<String>) {
        self.client.set_reasoning_effort(effort);
    }

    pub async fn refresh_context_window(&mut self) -> Result<Option<usize>> {
        let context_window = self.client.model_context_window().await?;
        if let Some(value) = context_window.filter(|value| *value > 0) {
            self.context_window = value;
            Ok(Some(value))
        } else {
            Ok(None)
        }
    }

    pub async fn list_models(&self) -> Result<Vec<ModelInfo>> {
        self.client.list_models().await
    }

    pub fn mcp_status(&self) -> Vec<String> {
        self.mcp.status()
    }

    pub fn permission_mode(&self) -> PermissionMode {
        self.workspace.permission_mode()
    }

    pub fn set_permission_mode(&mut self, mode: PermissionMode) {
        self.workspace.set_permission_mode(mode);
    }

    pub fn clear(&mut self) {
        self.messages.truncate(1);
    }

    pub fn checkpoint(&self) -> usize {
        self.messages.len()
    }

    pub fn rollback(&mut self, checkpoint: usize) {
        self.messages.truncate(checkpoint);
    }

    pub fn estimated_tokens(&self) -> usize {
        let messages = sanitized_messages(&self.messages);
        let image_tokens = count_image_attachments(&self.messages) * 1000;
        serde_json::to_vec(&messages)
            .map(|content| content.len() / 4 + image_tokens)
            .unwrap_or(image_tokens)
    }

    pub fn context_window(&self) -> usize {
        self.context_window
    }

    pub async fn compact(&mut self) -> Result<(usize, usize)> {
        if self.messages.len() <= 1 {
            bail!("There is no conversation to compact");
        }
        let before = self.estimated_tokens();
        let transcript = serde_json::to_string_pretty(&sanitized_messages(&self.messages[1..]))?;
        let request = vec![
            json!({
                "role": "system",
                "content": "You compact coding-agent conversations. Preserve user goals, decisions, important files, tool outcomes, errors, and unfinished work. Return only a concise continuation summary."
            }),
            json!({
                "role": "user",
                "content": format!("Summarize this conversation for continuation:\n\n{transcript}")
            }),
        ];
        let summary = self.client.complete(&request).await?;
        if summary.trim().is_empty() {
            bail!("Provider returned an empty compaction summary");
        }
        self.messages.truncate(1);
        self.messages.push(json!({
            "role": "system",
            "content": format!("Summary of the earlier conversation:\n{summary}")
        }));
        let after = self.estimated_tokens();
        Ok((before, after))
    }

    pub async fn run(
        &mut self,
        prompt: &str,
        on_event: &mut dyn FnMut(AgentEvent),
    ) -> Result<String> {
        let prompt = self.build_user_message(prompt).await?;
        self.messages.push(prompt);
        for _ in 0..self.max_steps {
            for notification in self.workspace.take_task_notifications().await {
                self.messages.push(json!({
                    "role": "user",
                    "content": format!("[Background task completion]\n{notification}")
                }));
            }
            self.maybe_compact(on_event).await?;
            let mut content_callback =
                |content: &str| on_event(AgentEvent::Content(content.to_string()));
            let mut tools = tool_definitions(self.web.search_tool_definition());
            tools
                .as_array_mut()
                .expect("built-in tool definitions are an array")
                .extend(self.mcp.tool_definitions());
            let message = self
                .client
                .chat_stream(&self.messages, &tools, &mut content_callback)
                .await?;
            let tool_calls = message
                .get("tool_calls")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let content = message
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            self.messages.push(message);
            if tool_calls.is_empty() {
                return Ok(content);
            }
            for call in tool_calls {
                let name = call
                    .get("function")
                    .and_then(|function| function.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                on_event(AgentEvent::ToolCall(name.to_string()));
                let result = self.execute_tool(&call).await;
                on_event(AgentEvent::ToolResult {
                    name: name.to_string(),
                    output: result.clone(),
                });
                self.messages.push(json!({
                    "role": "tool",
                    "tool_call_id": call.get("id").and_then(Value::as_str).unwrap_or(""),
                    "name": name,
                    "content": result,
                }));
            }
        }
        bail!(
            "Agent exceeded the limit of {} model calls.",
            self.max_steps
        )
    }

    async fn build_user_message(&mut self, prompt: &str) -> Result<Value> {
        let references = prompt
            .split_whitespace()
            .filter_map(|token| token.strip_prefix('@'))
            .filter(|reference| !reference.is_empty())
            .collect::<Vec<_>>();
        if references
            .iter()
            .any(|reference| Workspace::is_image_path(reference))
        {
            self.ensure_image_support().await?;
        }

        let mut expanded = prompt.to_string();
        let mut image_parts = Vec::new();
        for reference in references {
            if Workspace::is_image_path(reference) {
                let image = self
                    .workspace
                    .read_image(reference)
                    .with_context(|| format!("Could not attach @{reference}"))?;
                expanded.push_str(&format!(
                    "\n\n[Attached image: {reference} ({}x{} pixels)]",
                    image.width, image.height
                ));
                image_parts.push(json!({
                    "type": "image_url",
                    "image_url": {
                        "url": format!("data:{};base64,{}", image.mime, image.data)
                    }
                }));
            } else {
                let content = self
                    .workspace
                    .read_file(reference, None, None)
                    .with_context(|| format!("Could not attach @{reference}"))?;
                expanded.push_str(&format!(
                    "\n\n--- Attached file: {reference} ---\n{content}\n--- End attached file: {reference} ---"
                ));
            }
        }
        if image_parts.is_empty() {
            return Ok(json!({"role": "user", "content": expanded}));
        }
        let mut parts = vec![json!({"type": "text", "text": expanded})];
        parts.extend(image_parts);
        Ok(json!({"role": "user", "content": parts}))
    }

    async fn ensure_image_support(&mut self) -> Result<()> {
        if self.image_support == Some(true) {
            return Ok(());
        }
        if self.image_support == Some(false) {
            bail!("The selected model does not support image input");
        }
        let supported = self.client.model_supports_image().await?.unwrap_or(false);
        self.image_support = Some(supported);
        if !supported {
            bail!(
                "The selected model does not support image input or its capabilities are unknown"
            );
        }
        Ok(())
    }

    async fn maybe_compact(&mut self, on_event: &mut dyn FnMut(AgentEvent)) -> Result<()> {
        let threshold = self.context_window * 70 / 100;
        if !self.auto_compact || self.messages.len() <= 1 || self.estimated_tokens() < threshold {
            return Ok(());
        }
        let (before, after) = self.compact().await?;
        on_event(AgentEvent::Compacted { before, after });
        Ok(())
    }

    async fn execute_tool(&mut self, call: &Value) -> String {
        match self.try_execute_tool(call).await {
            Ok(result) => result,
            Err(error) => format!("Error: {error}"),
        }
    }

    async fn try_execute_tool(&mut self, call: &Value) -> Result<String> {
        let function = call
            .get("function")
            .context("Tool call is missing function")?;
        let name = function.get("name").and_then(Value::as_str).unwrap_or("");
        let arguments = function
            .get("arguments")
            .and_then(Value::as_str)
            .unwrap_or("{}");
        let arguments: Value = serde_json::from_str(arguments).context("Invalid tool arguments")?;
        let arguments = arguments
            .as_object()
            .context("Tool arguments must be a JSON object")?;
        match name {
            "list_files" => self
                .workspace
                .list_files(optional_string(arguments, "path", ".")?),
            "search_files" => self.workspace.search_files(
                required_string(arguments, "query")?,
                optional_string(arguments, "path", ".")?,
            ),
            "search_content" => self.workspace.search_content(
                required_string(arguments, "query")?,
                optional_string(arguments, "path", ".")?,
                optional_bool(arguments, "case_sensitive", true)?,
            ),
            "read_file" => self.workspace.read_file(
                required_string(arguments, "path")?,
                optional_usize(arguments, "start_line")?,
                optional_usize(arguments, "end_line")?,
            ),
            "write_file" => self.workspace.write_file(
                required_string(arguments, "path")?,
                required_string(arguments, "content")?,
            ),
            "apply_patch" => self.workspace.apply_patch(
                required_string(arguments, "path")?,
                required_string(arguments, "old_string")?,
                required_string(arguments, "new_string")?,
                optional_bool(arguments, "replace_all", false)?,
            ),
            "web_search" => self.web.search(required_string(arguments, "query")?).await,
            "fetch_url" => self.web.fetch(required_string(arguments, "url")?).await,
            "run_command" => {
                let command = required_string(arguments, "command")?;
                if optional_bool(arguments, "background", false)? {
                    self.workspace
                        .start_background_command(
                            command,
                            optional_string(arguments, "description", "")?,
                        )
                        .await
                } else {
                    self.workspace.run_command(command).await
                }
            }
            "task_list" => Ok(self
                .workspace
                .task_list(
                    optional_bool(arguments, "active_only", true)?,
                    optional_usize(arguments, "limit")?.unwrap_or(20),
                )
                .await),
            "task_output" => {
                self.workspace
                    .task_output(required_string(arguments, "task_id")?)
                    .await
            }
            "task_stop" => {
                self.workspace
                    .task_stop(
                        required_string(arguments, "task_id")?,
                        optional_string(arguments, "reason", "")?,
                    )
                    .await
            }
            "wait_for" => {
                self.workspace
                    .wait_for_task(
                        optional_string_argument(arguments, "task_id")?,
                        optional_usize(arguments, "timeout")?.unwrap_or(30) as u64,
                    )
                    .await
            }
            _ if self.mcp.contains(name) => {
                let arguments = Value::Object(arguments.clone());
                if !self.workspace.authorize_mcp_tool(name, &arguments) {
                    return Ok("MCP tool call rejected by the user.".to_string());
                }
                self.mcp.call_tool(name, arguments).await
            }
            _ => bail!("Unknown tool: {name}"),
        }
    }
}

fn sanitized_messages(messages: &[Value]) -> Vec<Value> {
    messages
        .iter()
        .map(|message| {
            let mut message = message.clone();
            if let Some(parts) = message.get_mut("content").and_then(Value::as_array_mut) {
                for part in parts {
                    if part.get("type").and_then(Value::as_str) == Some("image_url") {
                        *part = json!({
                            "type": "text",
                            "text": "[Image attachment omitted for compaction]"
                        });
                    }
                }
            }
            message
        })
        .collect()
}

fn count_image_attachments(messages: &[Value]) -> usize {
    messages
        .iter()
        .filter_map(|message| message.get("content").and_then(Value::as_array))
        .flat_map(|parts| parts.iter())
        .filter(|part| part.get("type").and_then(Value::as_str) == Some("image_url"))
        .count()
}

fn optional_string<'a>(
    arguments: &'a Map<String, Value>,
    key: &str,
    default: &'a str,
) -> Result<&'a str> {
    match arguments.get(key) {
        Some(value) => value
            .as_str()
            .with_context(|| format!("Tool argument {key} must be a string")),
        None => Ok(default),
    }
}

fn optional_string_argument<'a>(
    arguments: &'a Map<String, Value>,
    key: &str,
) -> Result<Option<&'a str>> {
    match arguments.get(key) {
        Some(value) => value
            .as_str()
            .map(Some)
            .with_context(|| format!("Tool argument {key} must be a string")),
        None => Ok(None),
    }
}

fn required_string<'a>(arguments: &'a Map<String, Value>, key: &str) -> Result<&'a str> {
    arguments
        .get(key)
        .and_then(Value::as_str)
        .with_context(|| format!("Tool argument {key} must be a string"))
}

fn optional_bool(arguments: &Map<String, Value>, key: &str, default: bool) -> Result<bool> {
    match arguments.get(key) {
        Some(value) => value
            .as_bool()
            .with_context(|| format!("Tool argument {key} must be a boolean")),
        None => Ok(default),
    }
}

fn optional_usize(arguments: &Map<String, Value>, key: &str) -> Result<Option<usize>> {
    match arguments.get(key) {
        Some(value) => {
            let number = value
                .as_u64()
                .with_context(|| format!("Tool argument {key} must be a nonnegative integer"))?;
            Ok(Some(number as usize))
        }
        None => Ok(None),
    }
}

fn tool_definitions(search_tool: Value) -> Value {
    json!([
        {
            "type": "function",
            "function": {
                "name": "list_files",
                "description": "List files and directories inside the workspace.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "Directory path, relative to the workspace."}
                    },
                    "required": []
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "search_files",
                "description": "Find files whose workspace-relative paths contain a case-insensitive substring.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "query": {"type": "string", "description": "Substring to find in file paths."},
                        "path": {"type": "string", "description": "File or directory to search, relative to the workspace."}
                    },
                    "required": ["query"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "search_content",
                "description": "Search UTF-8 text files for a substring and return matching lines with line numbers.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "query": {"type": "string", "description": "Text to find."},
                        "path": {"type": "string", "description": "File or directory to search, relative to the workspace."},
                        "case_sensitive": {"type": "boolean", "description": "Whether matching is case-sensitive. Defaults to true."}
                    },
                    "required": ["query"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "read_file",
                "description": "Read a UTF-8 text file, optionally selecting an inclusive one-based line range.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "File path, relative to the workspace."},
                        "start_line": {"type": "integer", "minimum": 1, "description": "First line to include."},
                        "end_line": {"type": "integer", "minimum": 1, "description": "Last line to include."}
                    },
                    "required": ["path"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "write_file",
                "description": "Write a UTF-8 text file inside the workspace. Requires user approval.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "File path, relative to the workspace."},
                        "content": {"type": "string", "description": "Complete file content."}
                    },
                    "required": ["path", "content"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "apply_patch",
                "description": "Replace an exact text span in a UTF-8 file. Requires user approval. Fails on ambiguous matches unless replace_all is true.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "File path, relative to the workspace."},
                        "old_string": {"type": "string", "description": "Exact text to replace."},
                        "new_string": {"type": "string", "description": "Replacement text."},
                        "replace_all": {"type": "boolean", "description": "Replace every occurrence. Defaults to false."}
                    },
                    "required": ["path", "old_string", "new_string"]
                }
            }
        },
        search_tool,
        {
            "type": "function",
            "function": {
                "name": "fetch_url",
                "description": "Fetch an HTTP or HTTPS URL and return readable text content.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "url": {"type": "string", "description": "Public URL to fetch."}
                    },
                    "required": ["url"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "run_command",
                "description": "Run a shell command in the foreground or background. Commands require approval according to the permission mode.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "command": {"type": "string", "description": "Shell command to execute."},
                        "background": {"type": "boolean", "description": "Run as a background task and return a task ID immediately. Defaults to false."},
                        "description": {"type": "string", "description": "Short background task description."}
                    },
                    "required": ["command"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "task_list",
                "description": "List background tasks.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "active_only": {"type": "boolean", "description": "Show only running tasks. Defaults to true."},
                        "limit": {"type": "integer", "minimum": 1, "description": "Maximum tasks to return. Defaults to 20."}
                    },
                    "required": []
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "task_output",
                "description": "Show a background task's status and recent output.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "task_id": {"type": "string", "description": "Task ID or unique prefix."}
                    },
                    "required": ["task_id"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "task_stop",
                "description": "Stop a running background task. Always requires user approval.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "task_id": {"type": "string", "description": "Task ID or unique prefix."},
                        "reason": {"type": "string", "description": "Reason for stopping the task."}
                    },
                    "required": ["task_id"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "wait_for",
                "description": "Wait for a background task or any running task to finish, up to 90 seconds.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "task_id": {"type": "string", "description": "Optional task ID or unique prefix. Waits for any task when omitted."},
                        "timeout": {"type": "integer", "minimum": 1, "maximum": 90, "description": "Maximum wait in seconds. Defaults to 30."}
                    },
                    "required": []
                }
            }
        }
    ])
}
