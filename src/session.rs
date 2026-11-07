use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

#[derive(Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub title: String,
    pub model: String,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    pub workspace: PathBuf,
    pub messages: Vec<Value>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Session {
    pub fn new(
        model: String,
        reasoning_effort: Option<String>,
        workspace: PathBuf,
        messages: Vec<Value>,
    ) -> Self {
        let now = Utc::now();
        Self {
            id: Uuid::new_v4().to_string(),
            title: String::new(),
            model,
            reasoning_effort,
            workspace,
            messages,
            created_at: now,
            updated_at: now,
        }
    }

    pub fn short_id(&self) -> &str {
        self.id.get(..8).unwrap_or(&self.id)
    }

    pub fn fork(&self) -> Self {
        let now = Utc::now();
        let mut fork = self.clone();
        fork.id = Uuid::new_v4().to_string();
        fork.title = if self.title.is_empty() {
            "Fork".to_string()
        } else {
            format!("{} (fork)", self.title)
        };
        fork.created_at = now;
        fork.updated_at = now;
        fork
    }
}

pub struct SessionStore {
    root: PathBuf,
}

impl SessionStore {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn save(&self, session: &Session) -> Result<()> {
        fs::create_dir_all(&self.root).with_context(|| {
            format!(
                "Could not create session directory: {}",
                self.root.display()
            )
        })?;
        let path = self.path(&session.id);
        let temporary = self.root.join(format!("{}.tmp", session.id));
        let content = serde_json::to_vec_pretty(session)?;
        fs::write(&temporary, content)?;
        fs::rename(temporary, path)?;
        Ok(())
    }

    pub fn list(&self) -> Result<Vec<Session>> {
        if !self.root.exists() {
            return Ok(Vec::new());
        }
        let mut sessions = Vec::new();
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            if entry.path().extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            let content = fs::read(entry.path())?;
            let session: Session = serde_json::from_slice(&content)
                .with_context(|| format!("Invalid session file: {}", entry.path().display()))?;
            sessions.push(session);
        }
        sessions.sort_by_key(|session| std::cmp::Reverse(session.updated_at));
        Ok(sessions)
    }

    pub fn latest_for_workspace(&self, workspace: &std::path::Path) -> Result<Option<Session>> {
        Ok(self
            .list()?
            .into_iter()
            .find(|session| session.workspace == workspace))
    }

    pub fn load(&self, id_prefix: &str) -> Result<Session> {
        let session = self.find(id_prefix)?;
        let path = self.path(&session.id);
        let content = fs::read(path)?;
        Ok(serde_json::from_slice(&content)?)
    }

    pub fn delete(&self, id_prefix: &str) -> Result<Session> {
        let session = self.find(id_prefix)?;
        fs::remove_file(self.path(&session.id))?;
        Ok(session)
    }

    fn find(&self, id_prefix: &str) -> Result<Session> {
        if id_prefix.is_empty() {
            bail!("Provide a session ID");
        }
        let matches = self
            .list()?
            .into_iter()
            .filter(|session| session.id.starts_with(id_prefix))
            .collect::<Vec<_>>();
        match matches.as_slice() {
            [] => bail!("Session not found: {id_prefix}"),
            [session] => Ok(session.clone()),
            _ => bail!("Session ID is ambiguous: {id_prefix}"),
        }
    }

    fn path(&self, id: &str) -> PathBuf {
        self.root.join(id).with_extension("json")
    }
}
