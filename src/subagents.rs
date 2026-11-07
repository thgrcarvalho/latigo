use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use glob::Pattern;
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Clone, Deserialize)]
#[serde(default)]
pub struct SubagentConfig {
    pub description: String,
    pub prompt: String,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub tools: Vec<String>,
    pub max_steps: Option<usize>,
}

impl Default for SubagentConfig {
    fn default() -> Self {
        Self {
            description: String::new(),
            prompt: String::new(),
            model: None,
            reasoning_effort: None,
            tools: [
                "list_files",
                "search_files",
                "search_content",
                "read_file",
                "web_search",
                "fetch_url",
            ]
            .into_iter()
            .map(ToString::to_string)
            .collect(),
            max_steps: None,
        }
    }
}

impl SubagentConfig {
    pub fn validate(&self, name: &str) -> Result<()> {
        if name.is_empty()
            || !name.chars().all(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '_' | '-')
            })
        {
            bail!("Subagent name must contain only ASCII letters, numbers, '_' or '-': {name}");
        }
        if self.max_steps == Some(0) {
            bail!("Subagent {name} max_steps must be at least 1");
        }
        if self.model.as_deref().is_some_and(str::is_empty) {
            bail!("Subagent {name} model cannot be empty");
        }
        if self.reasoning_effort.as_deref() == Some("") {
            bail!("Subagent {name} reasoning_effort cannot be empty");
        }
        for tool in &self.tools {
            Pattern::new(tool)
                .with_context(|| format!("Invalid tool pattern for subagent {name}: {tool}"))?;
        }
        Ok(())
    }

    pub fn allows_tool(&self, name: &str) -> bool {
        name != "run_subagent"
            && self
                .tools
                .iter()
                .any(|pattern| Pattern::new(pattern).is_ok_and(|pattern| pattern.matches(name)))
    }

    pub fn status(&self, name: &str) -> String {
        format!(
            "{name}: {} | model: {} | tools: {}",
            if self.description.is_empty() {
                "No description"
            } else {
                &self.description
            },
            self.model.as_deref().unwrap_or("parent model"),
            if self.tools.is_empty() {
                "none".to_string()
            } else {
                self.tools.join(", ")
            }
        )
    }
}

pub fn validate(agents: &BTreeMap<String, SubagentConfig>) -> Result<()> {
    for (name, agent) in agents {
        agent.validate(name)?;
    }
    Ok(())
}

pub fn tool_definition(agents: &BTreeMap<String, SubagentConfig>) -> Value {
    let descriptions = agents
        .iter()
        .map(|(name, agent)| {
            format!(
                "{name}: {}",
                if agent.description.is_empty() {
                    "Configured subagent"
                } else {
                    &agent.description
                }
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    json!({
        "type": "function",
        "function": {
            "name": "run_subagent",
            "description": format!("Delegate a self-contained task to a configured subagent with an isolated conversation. The subagent's final report is returned to you. Available subagents:\n{descriptions}"),
            "parameters": {
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "enum": agents.keys().collect::<Vec<_>>(),
                        "description": "Configured subagent name."
                    },
                    "prompt": {
                        "type": "string",
                        "description": "Complete task instructions and relevant context for the subagent."
                    }
                },
                "required": ["name", "prompt"]
            }
        }
    })
}

pub fn filter_tools(tools: &Value, config: &SubagentConfig) -> Value {
    Value::Array(
        tools
            .as_array()
            .into_iter()
            .flatten()
            .filter(|tool| {
                tool.get("function")
                    .and_then(|function| function.get("name"))
                    .and_then(Value::as_str)
                    .is_some_and(|name| config.allows_tool(name))
            })
            .cloned()
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_read_only_tools_and_supports_patterns() {
        let config = SubagentConfig::default();
        assert!(config.allows_tool("read_file"));
        assert!(!config.allows_tool("write_file"));
        assert!(!config.allows_tool("run_subagent"));

        let custom = SubagentConfig {
            tools: vec!["read_*".to_string(), "mcp__docs__*".to_string()],
            ..SubagentConfig::default()
        };
        assert!(custom.allows_tool("read_file"));
        assert!(custom.allows_tool("mcp__docs__search"));
        assert!(!custom.allows_tool("write_file"));
    }

    #[test]
    fn filters_unavailable_tools_from_definitions() {
        let tools = json!([
            {"function": {"name": "read_file"}},
            {"function": {"name": "write_file"}},
            {"function": {"name": "run_subagent"}}
        ]);
        let filtered = filter_tools(&tools, &SubagentConfig::default());
        assert_eq!(filtered.as_array().unwrap().len(), 1);
        assert_eq!(filtered[0]["function"]["name"], "read_file");
    }
}
