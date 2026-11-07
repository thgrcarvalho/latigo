use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use glob::Pattern;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;

use crate::tools::shell_command;
use crate::util::truncate;

#[derive(Clone, Deserialize)]
#[serde(default)]
pub struct HooksConfig {
    pub timeout_seconds: u64,
    pub session_start: Vec<HookCommand>,
    pub session_end: Vec<HookCommand>,
    pub user_prompt_submit: Vec<HookCommand>,
    pub pre_tool_use: Vec<HookCommand>,
    pub post_tool_use: Vec<HookCommand>,
    pub pre_compact: Vec<HookCommand>,
    pub stop: Vec<HookCommand>,
}

impl Default for HooksConfig {
    fn default() -> Self {
        Self {
            timeout_seconds: 10,
            session_start: Vec::new(),
            session_end: Vec::new(),
            user_prompt_submit: Vec::new(),
            pre_tool_use: Vec::new(),
            post_tool_use: Vec::new(),
            pre_compact: Vec::new(),
            stop: Vec::new(),
        }
    }
}

#[derive(Clone, Default, Deserialize)]
#[serde(default)]
pub struct HookCommand {
    pub command: String,
    pub matcher: Option<String>,
    pub timeout_seconds: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HookEvent {
    SessionStart,
    SessionEnd,
    UserPromptSubmit,
    PreToolUse,
    PostToolUse,
    PreCompact,
    Stop,
}

impl HookEvent {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SessionStart => "session_start",
            Self::SessionEnd => "session_end",
            Self::UserPromptSubmit => "user_prompt_submit",
            Self::PreToolUse => "pre_tool_use",
            Self::PostToolUse => "post_tool_use",
            Self::PreCompact => "pre_compact",
            Self::Stop => "stop",
        }
    }

    fn is_tool_event(self) -> bool {
        matches!(self, Self::PreToolUse | Self::PostToolUse)
    }
}

#[derive(Default)]
pub struct HookOutcome {
    pub blocked: Option<String>,
    pub context: Vec<String>,
}

pub struct HookRunner {
    config: HooksConfig,
    workspace: PathBuf,
}

impl HookRunner {
    pub fn new(config: HooksConfig, workspace: &Path) -> Result<Self> {
        if config.timeout_seconds == 0 {
            bail!("hooks.timeout_seconds must be at least 1");
        }
        for command in config.commands() {
            if command.command.trim().is_empty() {
                bail!("Hook command cannot be empty");
            }
            if command.timeout_seconds == Some(0) {
                bail!("Hook timeout_seconds must be at least 1");
            }
            if let Some(matcher) = &command.matcher {
                Pattern::new(matcher)
                    .with_context(|| format!("Invalid hook matcher: {matcher}"))?;
            }
        }
        Ok(Self {
            config,
            workspace: workspace.to_path_buf(),
        })
    }

    pub async fn run(
        &self,
        event: HookEvent,
        session_id: &str,
        tool_name: Option<&str>,
        mut payload: Value,
    ) -> Result<HookOutcome> {
        let commands = self.config.for_event(event);
        if commands.is_empty() {
            return Ok(HookOutcome::default());
        }
        let object = payload
            .as_object_mut()
            .context("Hook payload must be a JSON object")?;
        object.insert("hook_event_name".to_string(), json!(event.as_str()));
        object.insert("session_id".to_string(), json!(session_id));
        object.insert(
            "workspace".to_string(),
            json!(self.workspace.display().to_string()),
        );
        let mut outcome = HookOutcome::default();
        for command in commands {
            if let Some(matcher) = &command.matcher {
                if !event.is_tool_event() {
                    bail!("Hook matchers are only supported for tool events");
                }
                if !tool_name.is_some_and(|name| {
                    Pattern::new(matcher).is_ok_and(|pattern| pattern.matches(name))
                }) {
                    continue;
                }
            }
            let command_outcome = self
                .execute(command, event, &payload)
                .await
                .with_context(|| format!("{} hook failed: {}", event.as_str(), command.command))?;
            if let Some(reason) = command_outcome.blocked {
                outcome.blocked = Some(reason);
                break;
            }
            outcome.context.extend(command_outcome.context);
        }
        Ok(outcome)
    }

    pub fn status(&self) -> Vec<String> {
        [
            HookEvent::SessionStart,
            HookEvent::SessionEnd,
            HookEvent::UserPromptSubmit,
            HookEvent::PreToolUse,
            HookEvent::PostToolUse,
            HookEvent::PreCompact,
            HookEvent::Stop,
        ]
        .into_iter()
        .filter_map(|event| {
            let count = self.config.for_event(event).len();
            (count > 0).then(|| format!("{}: {count} hook(s)", event.as_str()))
        })
        .collect()
    }

    async fn execute(
        &self,
        hook: &HookCommand,
        event: HookEvent,
        payload: &Value,
    ) -> Result<HookOutcome> {
        let timeout =
            Duration::from_secs(hook.timeout_seconds.unwrap_or(self.config.timeout_seconds));
        let mut process = shell_command(&hook.command);
        process
            .current_dir(&self.workspace)
            .env("LATIGO_HOOK_EVENT", event.as_str())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = process.spawn()?;
        child
            .stdin
            .take()
            .context("Hook stdin is unavailable")?
            .write_all(serde_json::to_string(payload)?.as_bytes())
            .await?;
        let output = tokio::time::timeout(timeout, child.wait_with_output())
            .await
            .map_err(|_| anyhow::anyhow!("Hook timed out after {} seconds", timeout.as_secs()))??;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        if output.status.code() == Some(2) {
            let reason = if stderr.trim().is_empty() {
                stdout.trim()
            } else {
                stderr.trim()
            };
            return Ok(HookOutcome {
                blocked: Some(if reason.is_empty() {
                    format!("Blocked by {} hook", event.as_str())
                } else {
                    truncate(reason, 4000)
                }),
                context: Vec::new(),
            });
        }
        if !output.status.success() {
            bail!(
                "Hook exited with {}: {}",
                output.status,
                truncate(
                    if stderr.trim().is_empty() {
                        &stdout
                    } else {
                        &stderr
                    },
                    4000
                )
            );
        }
        parse_outcome(&stdout)
    }
}

impl HooksConfig {
    fn commands(&self) -> impl Iterator<Item = &HookCommand> {
        [
            &self.session_start,
            &self.session_end,
            &self.user_prompt_submit,
            &self.pre_tool_use,
            &self.post_tool_use,
            &self.pre_compact,
            &self.stop,
        ]
        .into_iter()
        .flatten()
    }

    fn for_event(&self, event: HookEvent) -> &[HookCommand] {
        match event {
            HookEvent::SessionStart => &self.session_start,
            HookEvent::SessionEnd => &self.session_end,
            HookEvent::UserPromptSubmit => &self.user_prompt_submit,
            HookEvent::PreToolUse => &self.pre_tool_use,
            HookEvent::PostToolUse => &self.post_tool_use,
            HookEvent::PreCompact => &self.pre_compact,
            HookEvent::Stop => &self.stop,
        }
    }
}

fn parse_outcome(stdout: &str) -> Result<HookOutcome> {
    let mut outcome = HookOutcome::default();
    for line in stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        if !line.starts_with('{') {
            continue;
        }
        let value: Value = serde_json::from_str(line).context("Invalid JSON in hook output")?;
        if let Some(context) = value
            .get("additionalContext")
            .or_else(|| value.get("additional_context"))
            .and_then(Value::as_str)
            .filter(|context| !context.trim().is_empty())
        {
            outcome.context.push(context.to_string());
        }
        if value.get("decision").and_then(Value::as_str) == Some("block") {
            outcome.blocked = Some(
                value
                    .get("reason")
                    .and_then(Value::as_str)
                    .unwrap_or("Blocked by hook")
                    .to_string(),
            );
            break;
        }
    }
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_context_and_block_responses() {
        let outcome = parse_outcome(
            "log line\n{\"additionalContext\":\"remember this\"}\n{\"decision\":\"block\",\"reason\":\"not allowed\"}",
        )
        .unwrap();
        assert_eq!(outcome.context, ["remember this"]);
        assert_eq!(outcome.blocked.as_deref(), Some("not allowed"));
    }

    #[test]
    fn rejects_invalid_json_control_output() {
        assert!(parse_outcome("{not-json}").is_err());
        assert!(parse_outcome("ordinary output").unwrap().blocked.is_none());
    }
}
