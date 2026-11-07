use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

const MAX_INSTRUCTION_FILE_BYTES: u64 = 128 * 1024;

#[derive(Clone, Default)]
pub struct InstructionSet {
    files: Vec<InstructionFile>,
}

#[derive(Clone)]
struct InstructionFile {
    path: PathBuf,
    content: String,
}

impl InstructionSet {
    pub fn load(workspace: &Path, user_config_dir: &Path) -> Result<Self> {
        let mut candidates = vec![user_config_dir.join("AGENTS.md")];
        let mut directories = workspace.ancestors().collect::<Vec<_>>();
        directories.reverse();
        for directory in directories {
            candidates.push(directory.join("AGENTS.md"));
            candidates.push(directory.join(".latigo/AGENTS.md"));
        }

        let mut seen = BTreeSet::new();
        let mut files = Vec::new();
        for path in candidates {
            if !path.is_file() {
                continue;
            }
            let canonical = path.canonicalize().with_context(|| {
                format!("Could not resolve instruction file: {}", path.display())
            })?;
            if !seen.insert(canonical.clone()) {
                continue;
            }
            let metadata = fs::metadata(&canonical)?;
            if metadata.len() > MAX_INSTRUCTION_FILE_BYTES {
                bail!(
                    "Instruction file exceeds the 128 KiB limit: {}",
                    canonical.display()
                );
            }
            let content = fs::read_to_string(&canonical).with_context(|| {
                format!("Could not read instruction file: {}", canonical.display())
            })?;
            if !content.trim().is_empty() {
                files.push(InstructionFile {
                    path: canonical,
                    content,
                });
            }
        }
        Ok(Self { files })
    }

    pub fn system_prompt(&self) -> String {
        self.files
            .iter()
            .map(|file| {
                format!(
                    "\n\nInstructions from {}:\n{}",
                    file.path.display(),
                    file.content.trim()
                )
            })
            .collect()
    }

    pub fn status(&self) -> Vec<String> {
        if self.files.is_empty() {
            return vec!["No AGENTS.md instruction files loaded.".to_string()];
        }
        self.files
            .iter()
            .map(|file| file.path.display().to_string())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_user_and_hierarchical_project_instructions_in_order() {
        let temp = tempfile::tempdir().unwrap();
        let user = temp.path().join("user");
        let project = temp.path().join("project");
        let nested = project.join("nested");
        fs::create_dir_all(&user).unwrap();
        fs::create_dir_all(&nested).unwrap();
        fs::write(user.join("AGENTS.md"), "user rules").unwrap();
        fs::write(project.join("AGENTS.md"), "project rules").unwrap();
        fs::create_dir(project.join(".latigo")).unwrap();
        fs::write(project.join(".latigo/AGENTS.md"), "local rules").unwrap();
        fs::write(nested.join("AGENTS.md"), "nested rules").unwrap();

        let instructions = InstructionSet::load(&nested, &user).unwrap();
        let prompt = instructions.system_prompt();
        let user_index = prompt.find("user rules").unwrap();
        let project_index = prompt.find("project rules").unwrap();
        let local_index = prompt.find("local rules").unwrap();
        let nested_index = prompt.find("nested rules").unwrap();
        assert!(user_index < project_index);
        assert!(project_index < local_index);
        assert!(local_index < nested_index);
        assert_eq!(instructions.status().len(), 4);
    }
}
