use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, ChildStderr, ChildStdout};
use tokio::sync::{Mutex, mpsc};
use uuid::Uuid;

use crate::tools::shell_command;

const MAX_LOG_BYTES: usize = 2_000_000;
const INLINE_OUTPUT_BYTES: usize = 32_768;

#[derive(Clone)]
struct TaskRecord {
    id: String,
    description: String,
    status: String,
    exit_code: Option<i32>,
    output_path: PathBuf,
    created_at: DateTime<Utc>,
    stop: mpsc::Sender<()>,
}

impl TaskRecord {
    fn is_running(&self) -> bool {
        self.status == "running"
    }

    fn summary(&self) -> String {
        format!(
            "{} | {} | {} | exit: {}\nOutput: {}",
            self.id,
            self.status,
            self.description,
            self.exit_code
                .map_or_else(|| "n/a".to_string(), |code| code.to_string()),
            self.output_path.display()
        )
    }
}

pub struct TaskManager {
    root: PathBuf,
    tasks: Arc<Mutex<HashMap<String, TaskRecord>>>,
    notifications: Arc<Mutex<Vec<String>>>,
}

impl TaskManager {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            tasks: Arc::new(Mutex::new(HashMap::new())),
            notifications: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub async fn start(&self, command: &str, description: &str, cwd: &Path) -> Result<String> {
        std::fs::create_dir_all(&self.root)?;
        let id = Uuid::new_v4().to_string();
        let output_path = self.root.join(format!("{id}.log"));
        let log = OutputLog::new(&output_path)?;
        let mut process = shell_command(command);
        #[cfg(unix)]
        process.process_group(0);
        let mut child = process
            .current_dir(cwd)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("Could not start background command: {command}"))?;
        let stdout = child
            .stdout
            .take()
            .context("Could not capture task stdout")?;
        let stderr = child
            .stderr
            .take()
            .context("Could not capture task stderr")?;
        let (stop, stop_receiver) = mpsc::channel(1);
        let record = TaskRecord {
            id: id.clone(),
            description: description.to_string(),
            status: "running".to_string(),
            exit_code: None,
            output_path,
            created_at: Utc::now(),
            stop,
        };
        self.tasks.lock().await.insert(id.clone(), record.clone());
        tokio::spawn(run_task(
            self.tasks.clone(),
            self.notifications.clone(),
            record,
            TaskProcess {
                child,
                stdout,
                stderr,
                log,
                stop: stop_receiver,
            },
        ));
        Ok(id)
    }

    pub async fn list(&self, active_only: bool, limit: usize) -> String {
        let mut records = self
            .tasks
            .lock()
            .await
            .values()
            .filter(|record| !active_only || record.is_running())
            .cloned()
            .collect::<Vec<_>>();
        records.sort_by_key(|record| std::cmp::Reverse(record.created_at));
        if records.is_empty() {
            return "No background tasks.".to_string();
        }
        records
            .into_iter()
            .take(limit)
            .map(|record| format!("{} | {} | {}", record.id, record.status, record.description))
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub async fn output(&self, id_prefix: &str) -> Result<String> {
        let record = self.find(id_prefix).await?;
        let data = std::fs::read(&record.output_path).unwrap_or_default();
        let start = data.len().saturating_sub(INLINE_OUTPUT_BYTES);
        let output = String::from_utf8_lossy(&data[start..]);
        Ok(format!(
            "{}\n\n{}",
            record.summary(),
            if output.is_empty() {
                "No output yet.".to_string()
            } else {
                output.into_owned()
            }
        ))
    }

    pub async fn stop(&self, id_prefix: &str, reason: &str) -> Result<String> {
        let record = self.find(id_prefix).await?;
        if !record.is_running() {
            return Ok(format!("Task {} is already {}.", record.id, record.status));
        }
        record
            .stop
            .send(())
            .await
            .map_err(|_| anyhow::anyhow!("Task is no longer running"))?;
        Ok(format!(
            "Stop requested for task {}: {}",
            record.id,
            if reason.is_empty() {
                "Stopped by user"
            } else {
                reason
            }
        ))
    }

    pub async fn wait_for(&self, id_prefix: Option<&str>, timeout_seconds: u64) -> Result<String> {
        let timeout = Duration::from_secs(timeout_seconds.min(90));
        let deadline = tokio::time::Instant::now() + timeout;
        let watched = match id_prefix {
            Some(prefix) => vec![self.find(prefix).await?.id],
            None => self
                .tasks
                .lock()
                .await
                .values()
                .filter(|record| record.is_running())
                .map(|record| record.id.clone())
                .collect(),
        };
        if watched.is_empty() {
            return Ok("No running background tasks.".to_string());
        }
        loop {
            let tasks = self.tasks.lock().await;
            let any_finished = watched
                .iter()
                .any(|id| tasks.get(id).is_some_and(|record| !record.is_running()));
            if any_finished {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                return Ok(format!(
                    "Wait timed out. Tasks still running:\n{}",
                    self.list(true, 20).await
                ));
            }
            drop(tasks);
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        match id_prefix {
            Some(prefix) => self.output(prefix).await,
            None => Ok(self.list(false, 20).await),
        }
    }

    pub async fn take_notifications(&self) -> Vec<String> {
        std::mem::take(&mut *self.notifications.lock().await)
    }

    async fn find(&self, id_prefix: &str) -> Result<TaskRecord> {
        if id_prefix.is_empty() {
            bail!("Provide a task ID");
        }
        let matches = self
            .tasks
            .lock()
            .await
            .values()
            .filter(|record| record.id.starts_with(id_prefix))
            .cloned()
            .collect::<Vec<_>>();
        match matches.as_slice() {
            [] => bail!("Task not found: {id_prefix}"),
            [record] => Ok(record.clone()),
            _ => bail!("Task ID is ambiguous: {id_prefix}"),
        }
    }
}

struct TaskProcess {
    child: Child,
    stdout: ChildStdout,
    stderr: ChildStderr,
    log: OutputLog,
    stop: mpsc::Receiver<()>,
}

async fn run_task(
    tasks: Arc<Mutex<HashMap<String, TaskRecord>>>,
    notifications: Arc<Mutex<Vec<String>>>,
    record: TaskRecord,
    process: TaskProcess,
) {
    let TaskProcess {
        mut child,
        stdout,
        stderr,
        log,
        mut stop,
    } = process;
    let log = Arc::new(Mutex::new(log));
    let stdout_reader = tokio::spawn(read_output(stdout, log.clone()));
    let stderr_reader = tokio::spawn(read_output(stderr, log));
    let (status, exit_code) = tokio::select! {
        result = child.wait() => match result {
            Ok(status) => {
                if status.success() {
                    ("completed".to_string(), status.code())
                } else {
                    ("failed".to_string(), status.code())
                }
            }
            Err(error) => ("failed".to_string(), {
                let _ = error;
                None
            }),
        },
        _ = stop.recv() => {
            terminate(&mut child).await;
            ("stopped".to_string(), None)
        }
        _ = tokio::time::sleep(Duration::from_secs(600)) => {
            terminate(&mut child).await;
            ("timeout".to_string(), None)
        }
    };
    let _ = stdout_reader.await;
    let _ = stderr_reader.await;
    if let Some(record) = tasks.lock().await.get_mut(&record.id) {
        record.status = status.clone();
        record.exit_code = exit_code;
    }
    notifications.lock().await.push(format!(
        "Background task {} ({}) finished with status: {}",
        record.id, record.description, status
    ));
}

async fn terminate(child: &mut Child) {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
    }
    #[cfg(not(unix))]
    let _ = child.kill().await;
    let _ = child.wait().await;
}

struct OutputLog {
    file: File,
    remaining: usize,
}

impl OutputLog {
    fn new(path: &Path) -> Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self {
            file,
            remaining: MAX_LOG_BYTES,
        })
    }

    fn write(&mut self, mut data: &[u8]) -> Result<()> {
        if self.remaining == 0 {
            return Ok(());
        }
        if data.len() > self.remaining {
            data = &data[..self.remaining];
        }
        self.file.write_all(data)?;
        self.remaining -= data.len();
        if self.remaining == 0 {
            self.file.write_all(b"\n... log truncated ...\n")?;
        }
        Ok(())
    }
}

async fn read_output(mut reader: impl AsyncRead + Unpin, log: Arc<Mutex<OutputLog>>) {
    let mut buffer = [0_u8; 8192];
    loop {
        match reader.read(&mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(count) => {
                if log.lock().await.write(&buffer[..count]).is_err() {
                    break;
                }
            }
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::fs;

    use super::*;

    struct ProcessGroupGuard(i32);

    impl Drop for ProcessGroupGuard {
        fn drop(&mut self) {
            unsafe {
                libc::kill(-self.0, libc::SIGKILL);
            }
        }
    }

    #[tokio::test]
    async fn stopping_a_task_kills_its_process_group_and_closes_output() {
        let temp = tempfile::tempdir().expect("temporary task directory");
        let shell_pid_path = temp.path().join("shell.pid");
        let child_pid_path = temp.path().join("child.pid");
        let command = format!(
            "echo $$ > '{}'; sleep 60 & echo $! > '{}'; wait",
            shell_pid_path.display(),
            child_pid_path.display()
        );
        let manager = TaskManager::new(temp.path().join("tasks"));
        let id = manager
            .start(&command, "process group test", temp.path())
            .await
            .expect("start task");
        let shell_pid = read_pid(&shell_pid_path).await;
        let child_pid = read_pid(&child_pid_path).await;
        let _guard = ProcessGroupGuard(shell_pid);
        assert!(process_exists(child_pid));

        manager.stop(&id, "test").await.expect("stop task");
        let output = tokio::time::timeout(Duration::from_secs(5), manager.wait_for(Some(&id), 5))
            .await
            .expect("task output pipes should close after stopping the process group")
            .expect("wait for stopped task");
        assert!(output.contains("stopped"));

        for _ in 0..50 {
            if !process_exists(child_pid) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("child process {child_pid} survived its parent's process-group termination");
    }

    async fn read_pid(path: &Path) -> i32 {
        for _ in 0..100 {
            if let Ok(value) = fs::read_to_string(path)
                && let Ok(pid) = value.trim().parse()
            {
                return pid;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("process ID was not written to {}", path.display());
    }

    fn process_exists(pid: i32) -> bool {
        unsafe { libc::kill(pid, 0) == 0 }
    }
}
