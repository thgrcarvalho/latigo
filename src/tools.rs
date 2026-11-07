use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::io::Cursor;
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::str::FromStr;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use clap::ValueEnum;
use glob::Pattern;
use image::{GenericImageView, ImageFormat, ImageReader};
use serde::Deserialize;
use tokio::process::Command;
use walkdir::{DirEntry, WalkDir};

use crate::tasks::TaskManager;
use crate::util::truncate;

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum PermissionMode {
    #[default]
    Ask,
    AutoEdit,
    Auto,
}

impl PermissionMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ask => "ask",
            Self::AutoEdit => "auto-edit",
            Self::Auto => "auto",
        }
    }
}

impl fmt::Display for PermissionMode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for PermissionMode {
    type Err = String;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        match value {
            "ask" => Ok(Self::Ask),
            "auto-edit" => Ok(Self::AutoEdit),
            "auto" => Ok(Self::Auto),
            _ => Err(format!(
                "Unknown permission mode: {value}. Use ask, auto-edit, or auto."
            )),
        }
    }
}

#[derive(Clone, Default, Deserialize)]
#[serde(default)]
pub struct CommandRules {
    pub allow: Vec<String>,
    pub deny: Vec<String>,
}

impl CommandRules {
    pub fn validate(&self) -> Result<()> {
        for pattern in self.allow.iter().chain(&self.deny) {
            Pattern::new(pattern).with_context(|| format!("Invalid command pattern: {pattern}"))?;
        }
        Ok(())
    }

    fn check(&self, command: &str) -> Result<()> {
        for pattern in &self.deny {
            if Pattern::new(pattern)?.matches(command) {
                bail!("Command denied by rule: {pattern}");
            }
        }
        if !self.allow.is_empty()
            && !self
                .allow
                .iter()
                .any(|pattern| Pattern::new(pattern).is_ok_and(|pattern| pattern.matches(command)))
        {
            bail!("Command does not match any allow rule");
        }
        Ok(())
    }
}

pub struct ImageAttachment {
    pub mime: String,
    pub data: String,
    pub width: u32,
    pub height: u32,
}

pub struct Workspace {
    pub root: PathBuf,
    approve: Box<dyn FnMut(&str) -> bool>,
    permission_mode: PermissionMode,
    command_rules: CommandRules,
    tasks: TaskManager,
}

impl Workspace {
    pub fn new(
        root: &Path,
        approve: Box<dyn FnMut(&str) -> bool>,
        permission_mode: PermissionMode,
        command_rules: CommandRules,
        tasks_dir: PathBuf,
    ) -> Result<Self> {
        let root = root
            .canonicalize()
            .with_context(|| format!("Invalid workspace: {}", root.display()))?;
        command_rules.validate()?;
        Ok(Self {
            root,
            approve,
            permission_mode,
            command_rules,
            tasks: TaskManager::new(tasks_dir),
        })
    }

    pub fn permission_mode(&self) -> PermissionMode {
        self.permission_mode
    }

    pub fn set_permission_mode(&mut self, mode: PermissionMode) {
        self.permission_mode = mode;
    }

    fn resolve(&self, value: &str) -> Result<PathBuf> {
        let path = Path::new(value);
        let joined = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.root.join(path)
        };
        let normalized = normalize(&joined);
        let mut ancestor = normalized.as_path();
        let mut suffix = Vec::<OsString>::new();
        while fs::symlink_metadata(ancestor).is_err() {
            let name = ancestor
                .file_name()
                .with_context(|| format!("Invalid path: {value}"))?;
            suffix.push(name.to_os_string());
            ancestor = ancestor
                .parent()
                .with_context(|| format!("Invalid path: {value}"))?;
        }
        let mut resolved = ancestor.canonicalize()?;
        for component in suffix.iter().rev() {
            resolved.push(component);
        }
        if !resolved.starts_with(&self.root) {
            bail!("Path is outside the workspace: {value}");
        }
        Ok(resolved)
    }

    fn ensure_not_sensitive(&self, path: &Path) -> Result<()> {
        let relative = self.relative_path(path);
        if is_sensitive_path(Path::new(&relative)) {
            bail!("Access to sensitive files is blocked: {relative}");
        }
        Ok(())
    }

    fn read_utf8(&self, path: &str) -> Result<(PathBuf, String)> {
        let file_path = self.resolve(path)?;
        self.ensure_not_sensitive(&file_path)?;
        if !file_path.is_file() {
            bail!("Not a file: {path}");
        }
        let data = fs::read(&file_path)?;
        if data.contains(&0) {
            bail!("Binary files are not supported: {path}");
        }
        let content = String::from_utf8(data)
            .map_err(|_| anyhow::anyhow!("File is not valid UTF-8: {path}"))?;
        Ok((file_path, content))
    }

    pub fn is_image_path(path: &str) -> bool {
        matches!(
            Path::new(path)
                .extension()
                .and_then(|extension| extension.to_str())
                .map(|extension| extension.to_lowercase())
                .as_deref(),
            Some("png" | "jpg" | "jpeg" | "webp")
        )
    }

    pub fn read_image(&self, path: &str) -> Result<ImageAttachment> {
        let file_path = self.resolve(path)?;
        self.ensure_not_sensitive(&file_path)?;
        let metadata = fs::metadata(&file_path)?;
        if metadata.len() > 10_000_000 {
            bail!("Image exceeds the 10 MB limit: {path}");
        }
        let data = fs::read(&file_path)?;
        let reader = ImageReader::new(Cursor::new(&data)).with_guessed_format()?;
        let format = reader.format().context("Unsupported image format")?;
        let mime = match format {
            ImageFormat::Png => "image/png",
            ImageFormat::Jpeg => "image/jpeg",
            ImageFormat::WebP => "image/webp",
            _ => bail!("Only PNG, JPEG, and WebP images are supported"),
        };
        let (width, height) = reader.decode()?.dimensions();
        if width == 0 || height == 0 || width > 8000 || height > 8000 {
            bail!("Image dimensions must be between 1x1 and 8000x8000 pixels");
        }
        Ok(ImageAttachment {
            mime: mime.to_string(),
            data: BASE64.encode(data),
            width,
            height,
        })
    }

    fn collect_files(&self, path: &str) -> Result<Vec<PathBuf>> {
        let root = self.resolve(path)?;
        if root.is_file() {
            return Ok(vec![root]);
        }
        if !root.is_dir() {
            bail!("Not a file or directory: {path}");
        }
        let mut files = Vec::new();
        for entry in WalkDir::new(root)
            .follow_links(false)
            .into_iter()
            .filter_entry(|entry| !is_ignored_directory(entry))
        {
            let entry = entry?;
            if entry.file_type().is_file() {
                files.push(entry.into_path());
            }
        }
        files.sort();
        Ok(files)
    }

    fn relative_path(&self, path: &Path) -> String {
        path.strip_prefix(&self.root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/")
    }

    pub fn list_files(&self, path: &str) -> Result<String> {
        let directory = self.resolve(path)?;
        if !directory.is_dir() {
            bail!("Not a directory: {path}");
        }
        let mut entries = fs::read_dir(directory)?.collect::<std::io::Result<Vec<_>>>()?;
        entries.sort_by_key(|entry| entry.file_name());
        if entries.is_empty() {
            return Ok("Directory is empty.".to_string());
        }
        let total = entries.len();
        let mut lines = Vec::new();
        for entry in entries.into_iter().take(200) {
            let suffix = if entry.path().is_dir() { "/" } else { "" };
            lines.push(format!("{}{suffix}", entry.file_name().to_string_lossy()));
        }
        if total > 200 {
            lines.push(format!("... {} more entries", total - 200));
        }
        Ok(lines.join("\n"))
    }

    pub fn search_files(&self, query: &str, path: &str) -> Result<String> {
        if query.is_empty() {
            bail!("Search query cannot be empty");
        }
        let query = query.to_lowercase();
        let matches = self
            .collect_files(path)?
            .into_iter()
            .map(|path| self.relative_path(&path))
            .filter(|path| path.to_lowercase().contains(&query))
            .collect::<Vec<_>>();
        if matches.is_empty() {
            return Ok("No matching files.".to_string());
        }
        let total = matches.len();
        let mut lines = matches.into_iter().take(100).collect::<Vec<_>>();
        if total > 100 {
            lines.push(format!("... {} more matches", total - 100));
        }
        Ok(lines.join("\n"))
    }

    pub fn search_content(&self, query: &str, path: &str, case_sensitive: bool) -> Result<String> {
        if query.is_empty() {
            bail!("Search query cannot be empty");
        }
        let needle = if case_sensitive {
            query.to_string()
        } else {
            query.to_lowercase()
        };
        let mut results = Vec::new();
        let mut truncated = false;
        'files: for file_path in self.collect_files(path)? {
            let relative = self.relative_path(&file_path);
            if is_sensitive_path(Path::new(&relative)) {
                continue;
            }
            if fs::metadata(&file_path)?.len() > 2_000_000 {
                continue;
            }
            let data = fs::read(&file_path)?;
            if data.contains(&0) {
                continue;
            }
            let Ok(content) = String::from_utf8(data) else {
                continue;
            };
            for (index, line) in content.lines().enumerate() {
                let matches = if case_sensitive {
                    line.contains(&needle)
                } else {
                    line.to_lowercase().contains(&needle)
                };
                if matches {
                    results.push(format!(
                        "{}:{}: {}",
                        relative,
                        index + 1,
                        truncate(line, 300)
                    ));
                    if results.len() == 100 {
                        truncated = true;
                        break 'files;
                    }
                }
            }
        }
        if results.is_empty() {
            return Ok("No matching content.".to_string());
        }
        if truncated {
            results.push("... more matches omitted".to_string());
        }
        Ok(results.join("\n"))
    }

    pub fn read_file(
        &self,
        path: &str,
        start_line: Option<usize>,
        end_line: Option<usize>,
    ) -> Result<String> {
        let (_, content) = self.read_utf8(path)?;
        if start_line.is_none() && end_line.is_none() {
            return Ok(truncate(&content, 12000));
        }
        let start = start_line.unwrap_or(1);
        if start == 0 || end_line == Some(0) {
            bail!("Line numbers start at 1");
        }
        let lines = content.lines().collect::<Vec<_>>();
        let end = end_line.unwrap_or(lines.len());
        if start > end {
            bail!("start_line must not exceed end_line");
        }
        if start > lines.len() {
            bail!(
                "start_line exceeds the file length of {} lines",
                lines.len()
            );
        }
        let selected = lines[start - 1..end.min(lines.len())].join("\n");
        Ok(truncate(&selected, 12000))
    }

    pub fn write_file(&mut self, path: &str, content: &str) -> Result<String> {
        let file_path = self.resolve(path)?;
        self.ensure_not_sensitive(&file_path)?;
        if self.permission_mode == PermissionMode::Ask {
            let action = format!("Write {} bytes to {}?", content.len(), file_path.display());
            if !(self.approve)(&action) {
                return Ok("Write rejected by the user.".to_string());
            }
        }
        if let Some(parent) = file_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&file_path, content)?;
        Ok(format!("Wrote {}", file_path.display()))
    }

    pub fn apply_patch(
        &mut self,
        path: &str,
        old_string: &str,
        new_string: &str,
        replace_all: bool,
    ) -> Result<String> {
        if old_string.is_empty() {
            bail!("old_string cannot be empty");
        }
        let (file_path, content) = self.read_utf8(path)?;
        let occurrences = content.match_indices(old_string).count();
        if occurrences == 0 {
            bail!("old_string was not found in {path}");
        }
        if occurrences > 1 && !replace_all {
            bail!("old_string occurs {occurrences} times; provide more context or set replace_all");
        }
        let replacements = if replace_all { occurrences } else { 1 };
        if self.permission_mode == PermissionMode::Ask {
            let action = format!(
                "Apply {replacements} replacement(s) to {}?",
                file_path.display()
            );
            if !(self.approve)(&action) {
                return Ok("Patch rejected by the user.".to_string());
            }
        }
        let updated = if replace_all {
            content.replace(old_string, new_string)
        } else {
            content.replacen(old_string, new_string, 1)
        };
        fs::write(&file_path, updated)?;
        Ok(format!(
            "Patched {} ({replacements} replacement(s))",
            file_path.display()
        ))
    }

    pub fn authorize_mcp_tool(&mut self, name: &str, arguments: &serde_json::Value) -> bool {
        if self.permission_mode == PermissionMode::Auto {
            return true;
        }
        (self.approve)(&format!(
            "Call external MCP tool {name}?\n\n{}",
            truncate(&arguments.to_string(), 4000)
        ))
    }

    fn authorize_command(&mut self, command: &str) -> Result<bool> {
        self.command_rules.check(command)?;
        let requires_approval =
            self.permission_mode != PermissionMode::Auto || is_dangerous_command(command);
        if requires_approval {
            let action = format!("Run command in {}?\n\n{command}", self.root.display());
            if !(self.approve)(&action) {
                return Ok(false);
            }
        }
        Ok(true)
    }

    pub async fn start_background_command(
        &mut self,
        command: &str,
        description: &str,
    ) -> Result<String> {
        let command = command.trim();
        if !self.authorize_command(command)? {
            return Ok("Command rejected by the user.".to_string());
        }
        let description = if description.is_empty() {
            command
        } else {
            description
        };
        let id = self.tasks.start(command, description, &self.root).await?;
        Ok(format!(
            "Background task started: {id}\nDescription: {description}\nUse task_output or wait_for with this task ID."
        ))
    }

    pub async fn task_list(&self, active_only: bool, limit: usize) -> String {
        self.tasks.list(active_only, limit).await
    }

    pub async fn task_output(&self, id: &str) -> Result<String> {
        self.tasks.output(id).await
    }

    pub async fn task_stop(&mut self, id: &str, reason: &str) -> Result<String> {
        let record = self.tasks.output(id).await?;
        if !(self.approve)(&format!("Stop background task {id}?")) {
            return Ok("Task stop rejected by the user.".to_string());
        }
        let _ = record;
        self.tasks.stop(id, reason).await
    }

    pub async fn wait_for_task(&self, id: Option<&str>, timeout_seconds: u64) -> Result<String> {
        self.tasks.wait_for(id, timeout_seconds).await
    }

    pub async fn take_task_notifications(&self) -> Vec<String> {
        self.tasks.take_notifications().await
    }

    pub async fn run_command(&mut self, command: &str) -> Result<String> {
        let command = command.trim();
        if !self.authorize_command(command)? {
            return Ok("Command rejected by the user.".to_string());
        }
        let mut process = shell_command(command);
        process
            .current_dir(&self.root)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let output = tokio::time::timeout(Duration::from_secs(60), process.output())
            .await
            .map_err(|_| anyhow::anyhow!("Command timed out after 60 seconds."))?
            .with_context(|| format!("Could not start command: {command}"))?;
        let code = output
            .status
            .code()
            .map_or_else(|| "unknown".to_string(), |code| code.to_string());
        let mut result = format!("Exit code: {code}");
        if !output.stdout.is_empty() {
            result.push_str(&format!(
                "\n\nstdout:\n{}",
                truncate(&String::from_utf8_lossy(&output.stdout), 12000)
            ));
        }
        if !output.stderr.is_empty() {
            result.push_str(&format!(
                "\n\nstderr:\n{}",
                truncate(&String::from_utf8_lossy(&output.stderr), 12000)
            ));
        }
        Ok(result)
    }
}

fn is_sensitive_path(path: &Path) -> bool {
    if path.components().any(|component| {
        component
            .as_os_str()
            .to_str()
            .is_some_and(|component| component.eq_ignore_ascii_case(".ssh"))
    }) {
        return true;
    }
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let name = name.to_lowercase();
    name == ".env"
        || name.starts_with(".env.")
        || matches!(
            name.as_str(),
            "id_rsa" | "id_ed25519" | "id_ecdsa" | "id_dsa" | "credentials" | ".netrc" | ".pgpass"
        )
        || matches!(
            path.extension().and_then(|extension| extension.to_str()),
            Some("pem" | "key" | "p12" | "pfx")
        )
}

fn is_dangerous_command(command: &str) -> bool {
    command == "sudo"
        || command.starts_with("sudo ")
        || command == "su"
        || command.starts_with("su ")
        || command.starts_with("shutdown")
        || command.starts_with("reboot")
        || command.starts_with("poweroff")
        || command.starts_with("halt")
        || command.starts_with("mkfs")
        || command.starts_with("dd ")
        || command.starts_with("rm -rf /")
        || command.starts_with("rm -fr /")
}

fn is_ignored_directory(entry: &DirEntry) -> bool {
    entry.depth() > 0
        && entry.file_type().is_dir()
        && matches!(
            entry.file_name().to_str(),
            Some(".git" | "target" | "node_modules")
        )
}

fn normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(part) => normalized.push(part),
        }
    }
    normalized
}

pub(crate) fn shell_command(command: &str) -> Command {
    #[cfg(target_os = "windows")]
    let mut process = {
        let mut process = Command::new("cmd");
        process.arg("/C");
        process
    };
    #[cfg(not(target_os = "windows"))]
    let mut process = {
        let mut process = Command::new("sh");
        process.arg("-c");
        process
    };
    process.arg(command);
    process
}
