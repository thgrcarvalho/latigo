use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::hooks::HooksConfig;
use crate::mcp::McpConfig;

#[derive(Default)]
pub struct PluginManager {
    plugins: Vec<PluginInfo>,
    commands: BTreeMap<String, PluginCommand>,
    pub mcp: McpConfig,
    pub hooks: HooksConfig,
}

pub struct PluginInfo {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
    pub command_count: usize,
}

#[derive(Clone, Deserialize)]
pub struct PluginCommand {
    #[serde(default)]
    pub description: String,
    pub prompt: String,
}

#[derive(Deserialize)]
struct PluginManifest {
    name: Option<String>,
    #[serde(default)]
    description: String,
    #[serde(default)]
    commands: BTreeMap<String, PluginCommand>,
    #[serde(default)]
    mcp: McpConfig,
    #[serde(default)]
    hooks: HooksConfig,
}

impl PluginManager {
    pub fn load(
        configured_paths: &[PathBuf],
        config_dir: &Path,
        user_plugins_dir: &Path,
    ) -> Result<Self> {
        let mut manifests = Vec::new();
        if user_plugins_dir.join("plugin.toml").is_file() {
            manifests.push(user_plugins_dir.join("plugin.toml"));
        } else if user_plugins_dir.is_dir() {
            for entry in fs::read_dir(user_plugins_dir)? {
                let path = entry?.path().join("plugin.toml");
                if path.is_file() {
                    manifests.push(path);
                }
            }
        }
        for path in configured_paths {
            let path = if path.is_absolute() {
                path.clone()
            } else {
                config_dir.join(path)
            };
            let manifest = if path.is_dir() {
                path.join("plugin.toml")
            } else {
                path
            };
            if !manifest.is_file() {
                bail!("Plugin manifest does not exist: {}", manifest.display());
            }
            manifests.push(manifest);
        }
        manifests.sort();
        manifests.dedup();

        let mut manager = Self::default();
        for manifest_path in manifests {
            manager.load_manifest(&manifest_path)?;
        }
        Ok(manager)
    }

    pub fn expand_command(&self, input: &str) -> Result<Option<String>> {
        let (name, arguments) = input
            .strip_prefix('/')
            .unwrap_or(input)
            .split_once(char::is_whitespace)
            .map_or((input.trim_start_matches('/'), ""), |(name, arguments)| {
                (name, arguments.trim())
            });
        let Some(command) = self.commands.get(name) else {
            return Ok(None);
        };
        let mut prompt = command
            .prompt
            .replace("$ARGUMENTS", arguments)
            .replace("{{input}}", arguments);
        if !arguments.is_empty()
            && !command.prompt.contains("$ARGUMENTS")
            && !command.prompt.contains("{{input}}")
        {
            prompt.push_str("\n\n");
            prompt.push_str(arguments);
        }
        if prompt.trim().is_empty() {
            bail!("Plugin command /{name} produced an empty prompt");
        }
        Ok(Some(prompt))
    }

    pub fn status(&self) -> Vec<String> {
        if self.plugins.is_empty() {
            return vec!["No plugins loaded.".to_string()];
        }
        let mut status = self
            .plugins
            .iter()
            .map(|plugin| {
                format!(
                    "{}: {} | {} command(s) | {}",
                    plugin.name,
                    if plugin.description.is_empty() {
                        "No description"
                    } else {
                        &plugin.description
                    },
                    plugin.command_count,
                    plugin.path.display()
                )
            })
            .collect::<Vec<_>>();
        status.extend(self.commands.iter().map(|(name, command)| {
            format!(
                "/{name}: {}",
                if command.description.is_empty() {
                    "Custom prompt command"
                } else {
                    &command.description
                }
            )
        }));
        status
    }

    fn load_manifest(&mut self, path: &Path) -> Result<()> {
        let content = fs::read_to_string(path)
            .with_context(|| format!("Could not read plugin: {}", path.display()))?;
        let manifest: PluginManifest = toml::from_str(&content)
            .with_context(|| format!("Could not parse plugin: {}", path.display()))?;
        let plugin_dir = path.parent().unwrap_or_else(|| Path::new("."));
        let fallback_name = plugin_dir
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("plugin");
        let name = manifest.name.unwrap_or_else(|| fallback_name.to_string());
        validate_name(&name, "Plugin")?;
        if self.plugins.iter().any(|plugin| plugin.name == name) {
            bail!("Duplicate plugin name: {name}");
        }

        let command_count = manifest.commands.len();
        for (command_name, command) in manifest.commands {
            validate_name(&command_name, "Plugin command")?;
            if command.prompt.trim().is_empty() {
                bail!("Plugin command /{command_name} has an empty prompt");
            }
            if self
                .commands
                .insert(command_name.clone(), command)
                .is_some()
            {
                bail!("Duplicate plugin command: /{command_name}");
            }
        }

        for (server_name, mut server) in manifest.mcp.servers {
            validate_name(&server_name, "MCP server")?;
            if server.cwd.is_none() {
                server.cwd = Some(plugin_dir.to_path_buf());
            } else if let Some(cwd) = server.cwd.as_mut()
                && cwd.is_relative()
            {
                *cwd = plugin_dir.join(&*cwd);
            }
            let namespaced = format!("{}_{}", namespace(&name), server_name.replace('-', "_"));
            if self
                .mcp
                .servers
                .insert(namespaced.clone(), server)
                .is_some()
            {
                bail!("Duplicate plugin MCP server: {namespaced}");
            }
        }

        let hook_timeout = manifest.hooks.timeout_seconds;
        let mut hooks = manifest.hooks;
        for hook in hooks.commands_mut() {
            if hook.timeout_seconds.is_none() {
                hook.timeout_seconds = Some(hook_timeout);
            }
            hook.cwd = Some(match hook.cwd.take() {
                Some(cwd) if cwd.is_absolute() => cwd,
                Some(cwd) => plugin_dir.join(cwd),
                None => plugin_dir.to_path_buf(),
            });
        }
        self.hooks.extend(hooks);
        self.plugins.push(PluginInfo {
            name,
            description: manifest.description,
            path: path.to_path_buf(),
            command_count,
        });
        Ok(())
    }
}

fn validate_name(name: &str, kind: &str) -> Result<()> {
    if name.is_empty()
        || !name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'))
    {
        bail!("{kind} name must contain only ASCII letters, numbers, '_' or '-': {name}");
    }
    Ok(())
}

fn namespace(name: &str) -> String {
    name.replace('-', "_")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_plugin_command_arguments() {
        let mut manager = PluginManager::default();
        manager.commands.insert(
            "review".to_string(),
            PluginCommand {
                description: String::new(),
                prompt: "Review $ARGUMENTS".to_string(),
            },
        );
        assert_eq!(
            manager
                .expand_command("/review src main")
                .unwrap()
                .as_deref(),
            Some("Review src main")
        );
        assert!(manager.expand_command("/unknown").unwrap().is_none());
    }
}
