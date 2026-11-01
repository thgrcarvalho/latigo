use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::client::RetryConfig;
use crate::tools::{CommandRules, PermissionMode};
use crate::web::WebResearchConfig;

#[derive(Default, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub model: Option<String>,
    pub base_url: Option<String>,
    pub max_steps: Option<usize>,
    pub context_window: Option<usize>,
    pub auto_compact: Option<bool>,
    pub permission_mode: Option<PermissionMode>,
    pub sessions_dir: Option<PathBuf>,
    pub commands: CommandRules,
    pub web_research: WebResearchConfig,
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
        config.commands.validate()?;
        Ok(config)
    }
}
