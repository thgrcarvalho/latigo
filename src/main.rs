mod agent;
mod client;
mod config;
mod session;
mod tasks;
mod tools;
mod ui;
mod util;
mod web;

use std::env;
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{Context, Result, bail};
use chrono::Utc;
use clap::Parser;
use rustyline::completion::{Completer, Pair};
use rustyline::error::ReadlineError;
use rustyline::highlight::Highlighter;
use rustyline::hint::Hinter;
use rustyline::history::DefaultHistory;
use rustyline::validate::Validator;
use rustyline::{Context as ReadlineContext, Editor, Helper};

use crate::agent::{Agent, AgentEvent};
use crate::client::OpenAICompatibleClient;
use crate::config::AppConfig;
use crate::session::{Session, SessionStore};
use crate::tools::{PermissionMode, Workspace};

#[derive(Parser)]
#[command(
    name = "latigo",
    about = "A CLI agent harness for OpenAI-compatible chat completion APIs.",
    after_help = "Defaults are read from ~/.config/latigo/config.toml. CLI flags override environment variables, which override config values."
)]
struct Args {
    #[arg(long, help = "Config file path")]
    config: Option<PathBuf>,

    #[arg(long, default_value = ".", help = "Workspace directory")]
    workspace: PathBuf,

    #[arg(long, help = "Model ID (overrides OPENAI_MODEL and config)")]
    model: Option<String>,

    #[arg(long, help = "OpenAI-compatible base URL")]
    base_url: Option<String>,

    #[arg(
        long,
        default_value = "OPENAI_API_KEY",
        help = "Environment variable containing the API key"
    )]
    api_key_env: String,

    #[arg(long, help = "Maximum model calls per prompt")]
    max_steps: Option<usize>,

    #[arg(long, help = "Model context-window size in tokens")]
    context_window: Option<usize>,

    #[arg(long, help = "Disable automatic context compaction")]
    no_auto_compact: bool,

    #[arg(long, help = "Permission mode: ask, auto-edit, or auto")]
    permission_mode: Option<PermissionMode>,

    #[arg(long, help = "Session storage directory")]
    sessions_dir: Option<PathBuf>,
}

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("Error: {error:#}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let args = Args::parse();
    let config_required = args.config.is_some();
    let config_path = match &args.config {
        Some(path) => path.clone(),
        None => default_config_path()?,
    };
    let config = AppConfig::load(&config_path, config_required)?;

    let model = args
        .model
        .or_else(|| env::var("OPENAI_MODEL").ok())
        .or(config.model)
        .context("Provide --model, set OPENAI_MODEL, or set model in config.toml")?;
    let base_url = args
        .base_url
        .or_else(|| env::var("OPENAI_BASE_URL").ok())
        .or(config.base_url)
        .unwrap_or_else(|| "https://api.openai.com/v1".to_string());
    let max_steps = args
        .max_steps
        .or(env_usize("AGENT_MAX_STEPS")?)
        .or(config.max_steps)
        .unwrap_or(20);
    let context_window_override = args
        .context_window
        .or(env_usize("AGENT_CONTEXT_WINDOW")?)
        .or(config.context_window);
    let permission_mode = args
        .permission_mode
        .or(env_permission_mode()?)
        .or(config.permission_mode)
        .unwrap_or_default();
    let auto_compact = if args.no_auto_compact {
        false
    } else if let Some(value) = env_bool("AGENT_AUTO_COMPACT")? {
        value
    } else {
        config.auto_compact.unwrap_or(true)
    };
    let sessions_dir = args
        .sessions_dir
        .or_else(|| env::var_os("AGENT_SESSIONS_DIR").map(PathBuf::from))
        .or(config.sessions_dir)
        .unwrap_or(default_sessions_dir()?);
    let api_key = env::var(&args.api_key_env)
        .with_context(|| format!("Set the {} environment variable", args.api_key_env))?;
    if !args.workspace.is_dir() {
        bail!("Workspace is not a directory: {}", args.workspace.display());
    }
    if max_steps == 0 {
        bail!("max_steps must be at least 1");
    }
    if context_window_override == Some(0) {
        bail!("context_window must be at least 1");
    }

    let client = OpenAICompatibleClient::new(&base_url, api_key, model, config.retry)?;
    let context_window = match context_window_override {
        Some(value) => value,
        None => match client.model_context_window().await {
            Ok(Some(value)) if value > 0 => value,
            Ok(_) => {
                eprintln!("Could not discover the model context window; using 262144 tokens.");
                262144
            }
            Err(error) => {
                eprintln!(
                    "Could not discover the model context window: {error:#}; using 262144 tokens."
                );
                262144
            }
        },
    };
    let interactive = io::stdin().is_terminal()
        && io::stdout().is_terminal()
        && env::var("TERM").is_ok_and(|term| term != "dumb");
    let interface = if interactive {
        Some(ui::Interface::new()?)
    } else {
        None
    };
    let approval_handler: Box<dyn FnMut(&str) -> bool> = match &interface {
        Some(interface) => interface.approval_handler(),
        None => Box::new(approve),
    };
    let workspace = Workspace::new(
        &args.workspace,
        approval_handler,
        permission_mode,
        config.commands,
        sessions_dir
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("tasks"),
    )?;
    let workspace_root = workspace.root.clone();
    let mut agent = Agent::new(
        client,
        workspace,
        config.web_research,
        max_steps,
        context_window,
        auto_compact,
    )?;
    let history_path = sessions_dir
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("history.txt");
    let store = SessionStore::new(sessions_dir);
    let mut session = Session::new(
        agent.model().to_string(),
        workspace_root.clone(),
        agent.messages().to_vec(),
    );
    if let Some(interface) = interface {
        return interface
            .run(
                &mut agent,
                &store,
                &mut session,
                &workspace_root,
                &history_path,
                context_window_override.is_none(),
            )
            .await;
    }
    let mut editor = Editor::<CommandHelper, DefaultHistory>::new()?;
    editor.set_helper(Some(CommandHelper));
    if history_path.exists()
        && let Err(error) = editor.load_history(&history_path)
    {
        eprintln!("Could not load input history: {error}");
    }

    println!(
        "Latigo | model: {} | workspace: {}",
        agent.model(),
        workspace_root.display()
    );
    println!(
        "Session: {} | permissions: {} | /help for commands",
        session.short_id(),
        agent.permission_mode()
    );

    loop {
        let input = match editor.readline("\nyou> ") {
            Ok(input) => input,
            Err(ReadlineError::Interrupted | ReadlineError::Eof) => {
                println!();
                save_session(&store, &mut session, &agent)?;
                return Ok(());
            }
            Err(error) => return Err(error.into()),
        };
        let prompt = input.trim();
        if prompt.is_empty() {
            continue;
        }
        editor.add_history_entry(prompt)?;
        if let Some(parent) = history_path.parent() {
            fs::create_dir_all(parent)?;
        }
        editor.append_history(&history_path)?;
        if prompt.starts_with('/') {
            match handle_command(
                prompt,
                &mut agent,
                &store,
                &mut session,
                &workspace_root,
                context_window_override.is_none(),
                &mut |action| approve(action),
            )
            .await
            {
                Ok(result) => {
                    print!("{}", result.output);
                    if result.exit {
                        return Ok(());
                    }
                }
                Err(error) => eprintln!("Error: {error:#}"),
            }
            continue;
        }

        let result = run_prompt(&mut agent, prompt).await;
        if let Err(error) = save_session(&store, &mut session, &agent) {
            eprintln!("Could not save session: {error:#}");
        }
        if let Err(error) = result {
            eprintln!("\nError: {error:#}");
        }
        println!(
            "[context: ~{} / {} tokens]",
            agent.estimated_tokens(),
            agent.context_window()
        );
    }
}

#[derive(Default)]
struct CommandResult {
    exit: bool,
    output: String,
}

impl CommandResult {
    fn line(&mut self, line: impl std::fmt::Display) {
        self.output.push_str(&line.to_string());
        self.output.push('\n');
    }
}

async fn handle_command(
    input: &str,
    agent: &mut Agent,
    store: &SessionStore,
    session: &mut Session,
    workspace_root: &Path,
    discover_context_window: bool,
    approve_action: &mut dyn FnMut(&str) -> bool,
) -> Result<CommandResult> {
    let (command, argument) = input
        .split_once(char::is_whitespace)
        .map_or((input, ""), |(command, argument)| {
            (command, argument.trim())
        });
    let mut result = CommandResult::default();
    match command {
        "/exit" | "/quit" => {
            save_session(store, session, agent)?;
            result.exit = true;
        }
        "/help" => result.line(
            "Commands:\n  /help                 Show commands\n  /new                  Start a new session\n  /sessions             List saved sessions\n  /resume <id>          Resume a session in this workspace\n  /delete <id>          Delete a saved session\n  /clear                Clear current conversation\n  /compact              Compact current conversation\n  /usage                Show estimated context usage\n  /models               List available models\n  /model [id]           Show or change the model\n  /permissions [mode]   Show or set ask, auto-edit, or auto\n  /exit                 Save and exit",
        ),
        "/new" => {
            save_session(store, session, agent)?;
            agent.clear();
            *session = Session::new(
                agent.model().to_string(),
                workspace_root.to_path_buf(),
                agent.messages().to_vec(),
            );
            result.line(format!("Started session {}", session.short_id()));
        }
        "/sessions" => {
            let sessions = store.list()?;
            if sessions.is_empty() {
                result.line("No saved sessions.");
            }
            for saved in sessions.into_iter().take(20) {
                result.line(format!(
                    "{} | {} | {} | {}",
                    saved.short_id(),
                    saved.updated_at.format("%Y-%m-%d %H:%M"),
                    saved.model,
                    if saved.title.is_empty() {
                        "Untitled"
                    } else {
                        &saved.title
                    }
                ));
            }
        }
        "/resume" => {
            save_session(store, session, agent)?;
            let loaded = store.load(argument)?;
            if loaded.workspace != workspace_root {
                bail!(
                    "Session belongs to workspace {}. Start the harness there to resume it.",
                    loaded.workspace.display()
                );
            }
            agent.set_model(loaded.model.clone());
            agent.restore_messages(loaded.messages.clone())?;
            result.line(format!(
                "Resumed session {} | {}",
                loaded.short_id(),
                loaded.model
            ));
            *session = loaded;
        }
        "/delete" => {
            let deleted = store.load(argument)?;
            if !approve_action(&format!("Delete session {}?", deleted.short_id())) {
                return Ok(result);
            }
            store.delete(argument)?;
            if deleted.id == session.id {
                agent.clear();
                *session = Session::new(
                    agent.model().to_string(),
                    workspace_root.to_path_buf(),
                    agent.messages().to_vec(),
                );
            }
            result.line(format!("Deleted session {}", deleted.short_id()));
        }
        "/clear" => {
            agent.clear();
            save_session(store, session, agent)?;
            result.line("Conversation cleared.");
        }
        "/compact" => {
            let message = run_compaction(agent).await?;
            save_session(store, session, agent)?;
            result.line(message);
        }
        "/usage" => result.line(format!(
            "Session: {}\nModel: {}\nContext: ~{} / {} tokens (estimated)",
            session.short_id(),
            agent.model(),
            agent.estimated_tokens(),
            agent.context_window()
        )),
        "/models" => {
            let models = agent.list_models().await?;
            if models.is_empty() {
                result.line("The provider returned no models.");
            } else {
                result.line("Available models:");
                for model in models {
                    let marker = if model.id == agent.model() { "*" } else { " " };
                    if model.display_name == model.id {
                        result.line(format!("{marker} {}", model.id));
                    } else {
                        result.line(format!("{marker} {} ({})", model.display_name, model.id));
                    }
                }
            }
        }
        "/model" => {
            if argument.is_empty() {
                result.line(format!("Current model: {}", agent.model()));
            } else {
                agent.set_model(argument.to_string());
                result.line(format!("Model changed to {argument}"));
                if discover_context_window {
                    match agent.refresh_context_window().await {
                        Ok(Some(value)) => result.line(format!("Context window: {value} tokens")),
                        Ok(None) => result.line(format!(
                            "Could not discover the model context window; keeping {} tokens.",
                            agent.context_window()
                        )),
                        Err(error) => result.line(format!(
                            "Could not discover the model context window: {error:#}; keeping {} tokens.",
                            agent.context_window()
                        )),
                    }
                }
                save_session(store, session, agent)?;
            }
        }
        "/permissions" => {
            if argument.is_empty() {
                result.line(format!("Permission mode: {}", agent.permission_mode()));
            } else {
                let mode =
                    PermissionMode::from_str(argument).map_err(|error| anyhow::anyhow!(error))?;
                agent.set_permission_mode(mode);
                result.line(format!("Permission mode changed to {mode}"));
                if mode == PermissionMode::Auto {
                    result.line(
                        "Warning: writes, patches, and most commands no longer require approval. Dangerous commands still require approval.",
                    );
                }
            }
        }
        _ => bail!("Unknown command: {command}. Use /help."),
    }
    Ok(result)
}

async fn run_prompt(agent: &mut Agent, prompt: &str) -> Result<()> {
    let checkpoint = agent.checkpoint();
    let mut response_started = false;
    let result = {
        let mut callback = |event: AgentEvent| match event {
            AgentEvent::Content(content) => {
                if !response_started {
                    print!("\nagent> ");
                    response_started = true;
                }
                print!("{content}");
                let _ = io::stdout().flush();
            }
            AgentEvent::ToolCall(name) => {
                if response_started {
                    println!();
                    response_started = false;
                }
                println!("\n[tool] {name}");
            }
            AgentEvent::ToolResult { .. } => {}
            AgentEvent::Compacted { before, after } => {
                if response_started {
                    println!();
                    response_started = false;
                }
                println!("\n[context compacted: ~{before} -> ~{after} tokens]");
            }
        };
        let future = agent.run(prompt, &mut callback);
        tokio::pin!(future);
        tokio::select! {
            result = future => Some(result),
            _ = tokio::signal::ctrl_c() => None,
        }
    };
    match result {
        Some(Ok(_)) => {
            println!();
            Ok(())
        }
        Some(Err(error)) => {
            println!();
            Err(error)
        }
        None => {
            agent.rollback(checkpoint);
            println!("\nInterrupted. Partial conversation was rolled back.");
            Ok(())
        }
    }
}

async fn run_compaction(agent: &mut Agent) -> Result<String> {
    let future = agent.compact();
    tokio::pin!(future);
    tokio::select! {
        result = future => {
            let (before, after) = result?;
            Ok(format!("Context compacted: ~{before} -> ~{after} tokens."))
        }
        _ = tokio::signal::ctrl_c() => Ok("Compaction interrupted.".to_string())
    }
}

fn save_session(store: &SessionStore, session: &mut Session, agent: &Agent) -> Result<()> {
    session.messages = agent.messages().to_vec();
    session.model = agent.model().to_string();
    session.updated_at = Utc::now();
    if session.title.is_empty()
        && let Some(title) = agent.messages().iter().find_map(|message| {
            if message.get("role").and_then(serde_json::Value::as_str) == Some("user") {
                message_text(message)
            } else {
                None
            }
        })
    {
        session.title = title
            .lines()
            .next()
            .unwrap_or("")
            .chars()
            .take(60)
            .collect();
    }
    if session.messages.len() > 1 {
        store.save(session)?;
    }
    Ok(())
}

fn message_text(message: &serde_json::Value) -> Option<&str> {
    let content = message.get("content")?;
    if let Some(text) = content.as_str() {
        return Some(text);
    }
    content.as_array()?.iter().find_map(|part| {
        if part.get("type").and_then(serde_json::Value::as_str) == Some("text") {
            part.get("text").and_then(serde_json::Value::as_str)
        } else {
            None
        }
    })
}

fn home_dir() -> Result<PathBuf> {
    env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .context("Could not determine the home directory")
}

fn default_config_path() -> Result<PathBuf> {
    Ok(home_dir()?.join(".config/latigo/config.toml"))
}

fn default_sessions_dir() -> Result<PathBuf> {
    Ok(home_dir()?.join(".latigo/sessions"))
}

fn env_usize(name: &str) -> Result<Option<usize>> {
    match env::var(name) {
        Ok(value) => {
            let parsed = value
                .parse()
                .with_context(|| format!("{name} must be a nonnegative integer"))?;
            Ok(Some(parsed))
        }
        Err(env::VarError::NotPresent) => Ok(None),
        Err(env::VarError::NotUnicode(_)) => bail!("{name} contains invalid Unicode"),
    }
}

fn env_bool(name: &str) -> Result<Option<bool>> {
    match env::var(name) {
        Ok(value) => match value.to_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => Ok(Some(true)),
            "0" | "false" | "no" | "off" => Ok(Some(false)),
            _ => bail!("{name} must be true or false"),
        },
        Err(env::VarError::NotPresent) => Ok(None),
        Err(env::VarError::NotUnicode(_)) => bail!("{name} contains invalid Unicode"),
    }
}

fn env_permission_mode() -> Result<Option<PermissionMode>> {
    match env::var("AGENT_PERMISSION_MODE") {
        Ok(value) => {
            let mode = PermissionMode::from_str(&value).map_err(|error| anyhow::anyhow!(error))?;
            Ok(Some(mode))
        }
        Err(env::VarError::NotPresent) => Ok(None),
        Err(env::VarError::NotUnicode(_)) => {
            bail!("AGENT_PERMISSION_MODE contains invalid Unicode")
        }
    }
}

const SLASH_COMMANDS: &[&str] = &[
    "/help",
    "/new",
    "/sessions",
    "/resume",
    "/delete",
    "/clear",
    "/compact",
    "/usage",
    "/models",
    "/model",
    "/permissions",
    "/exit",
    "/quit",
];

struct CommandHelper;

impl Completer for CommandHelper {
    type Candidate = Pair;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _context: &ReadlineContext,
    ) -> rustyline::Result<(usize, Vec<Pair>)> {
        let prefix = line.get(..pos).unwrap_or("");
        if !prefix.starts_with('/') || prefix.contains(char::is_whitespace) {
            return Ok((0, Vec::new()));
        }
        let candidates = SLASH_COMMANDS
            .iter()
            .filter(|command| command.starts_with(prefix))
            .map(|command| Pair {
                display: (*command).to_string(),
                replacement: (*command).to_string(),
            })
            .collect();
        Ok((0, candidates))
    }
}

impl Hinter for CommandHelper {
    type Hint = String;

    fn hint(&self, _line: &str, _pos: usize, _context: &ReadlineContext) -> Option<String> {
        None
    }
}

impl Highlighter for CommandHelper {}
impl Validator for CommandHelper {}
impl Helper for CommandHelper {}

fn approve(action: &str) -> bool {
    print!("\n{action}\nAllow? [y/N] ");
    if io::stdout().flush().is_err() {
        return false;
    }
    let mut answer = String::new();
    match io::stdin().read_line(&mut answer) {
        Ok(0) | Err(_) => {
            println!();
            false
        }
        Ok(_) => matches!(answer.trim().to_lowercase().as_str(), "y" | "yes"),
    }
}
