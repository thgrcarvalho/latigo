use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::client::RetryConfig;
use crate::hooks::{HookRunner, HooksConfig};
use crate::mcp::McpConfig;
use crate::tools::{CommandRules, PermissionMode};
use crate::web::WebResearchConfig;

#[derive(Default, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub model: Option<String>,
    pub base_url: Option<String>,
    pub reasoning_effort: Option<String>,
    pub max_steps: Option<usize>,
    pub context_window: Option<usize>,
    pub auto_compact: Option<bool>,
    pub permission_mode: Option<PermissionMode>,
    pub sessions_dir: Option<PathBuf>,
    pub plugins: Vec<PathBuf>,
    pub commands: CommandRules,
    pub web_research: WebResearchConfig,
    pub mcp: McpConfig,
    pub hooks: HooksConfig,
    pub agents: BTreeMap<String, crate::subagents::SubagentConfig>,
    pub retry: RetryConfig,
}

impl AppConfig {
    pub fn load(path: &Path, required: bool) -> Result<Self> {
        if !path.exists() {
            if required {
                bail!("Config file does not exist: {}", path.display());
            }
            return Ok(Self::default());
        }
        let content = fs::read_to_string(path)
            .with_context(|| format!("Could not read config file: {}", path.display()))?;
        let config: Self = toml::from_str(&content)
            .with_context(|| format!("Could not parse config file: {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        self.commands.validate()?;
        self.retry.validate()?;
        self.mcp.validate()?;
        crate::subagents::validate(&self.agents)?;
        HookRunner::new(self.hooks.clone(), Path::new("."))?;
        if self.max_steps == Some(0) {
            bail!("max_steps must be at least 1");
        }
        if self.context_window == Some(0) {
            bail!("context_window must be at least 1");
        }
        if self.reasoning_effort.as_deref().is_some_and(str::is_empty) {
            bail!("reasoning_effort cannot be empty");
        }
        Ok(())
    }
}
