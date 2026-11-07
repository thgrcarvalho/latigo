# Latigo

Latigo is an experimental CLI agent harness written in Rust. It connects OpenAI-compatible chat completion APIs to a local workspace with streaming responses, tool execution, persistent sessions, and configurable permission boundaries.

A latigo is the strap that secures a saddle's cinch. Latigo secures the setup around your working agent.

## Features

- Streaming model responses and tool calls
- Non-interactive print mode with text, JSON, and JSONL output
- Ratatui interface with scrollable history, Markdown rendering, and expandable tool output
- OpenAI-compatible providers, including Kimi
- Persistent sessions with workspace-specific resume
- Hierarchical user and project `AGENTS.md` instructions
- Runtime status and offline configuration diagnostics
- Input history and slash-command completion
- Model listing and model switching
- Metadata-driven reasoning-effort selection with provider defaults
- Model Context Protocol (MCP) stdio servers with dynamic tool discovery
- Lifecycle hooks with tool matchers, blocking decisions, and context injection
- Trusted plugin manifests that contribute commands, hooks, MCP servers, and subagents
- Isolated subagents with configurable instructions, models, and tool restrictions
- Context-window discovery and automatic or manual compaction
- Text and image attachments
- Workspace file search, reading, writing, and patching
- Background command execution and task management
- Kimi-hosted web search, with Brave as an optional alternative
- Local URL fetching with private-address protection
- Configurable retries with exponential backoff and jitter
- Command allow/deny rules and sensitive-file protection
- `ask`, `auto-edit`, and `auto` permission modes

## Requirements

- A recent Rust toolchain with Cargo
- An API key for an OpenAI-compatible provider
- A Unix-like environment for background process management

Latigo is developed and tested on Linux, including Ubuntu under WSL.

## Build

```bash
git clone https://github.com/thgrcarvalho/latigo.git
cd latigo
cargo build --release
```

The executable is created at `target/release/latigo`.

## Quick start with Kimi

Set your API key in the shell environment:

```bash
export KIMI_API_KEY="your-api-key"
```

Start Latigo:

```bash
./target/release/latigo \
  --api-key-env KIMI_API_KEY \
  --base-url "https://api.kimi.ai/coding/v1" \
  --model "k3.1-pro"
```

Use the same persistent environment-variable mechanism you use for other shell credentials so the key remains available after rebooting. Do not store API keys in `config.toml` or commit them to Git.

For another OpenAI-compatible provider, select its base URL, model, and credential variable:

```bash
./target/release/latigo \
  --api-key-env OPENAI_API_KEY \
  --base-url "https://api.openai.com/v1" \
  --model "your-model-id"
```

Provider API usage may consume credits or quota according to that provider's terms.

## Configuration

Latigo reads optional defaults from:

```text
~/.config/latigo/config.toml
```

Example:

```toml
model = "k3.1-pro"
base_url = "https://api.kimi.ai/coding/v1"
max_steps = 20
permission_mode = "ask"
auto_compact = true
# context_window = 262144
# reasoning_effort = "high"

[commands]
allow = []
deny = []

[web_research]
provider = "kimi"
max_results = 5

[retry]
max_attempts = 3
initial_delay_ms = 500
max_delay_ms = 8000
```

Configuration precedence is:

```text
CLI flags -> environment variables -> config.toml -> built-in defaults
```

Supported environment variables include:

- `OPENAI_MODEL`: default model ID
- `OPENAI_BASE_URL`: default provider base URL
- `AGENT_MAX_STEPS`: maximum model calls per prompt
- `AGENT_CONTEXT_WINDOW`: context-window override
- `AGENT_REASONING_EFFORT`: provider reasoning-effort value
- `AGENT_AUTO_COMPACT`: enable or disable automatic compaction
- `AGENT_PERMISSION_MODE`: `ask`, `auto-edit`, or `auto`
- `AGENT_SESSIONS_DIR`: session storage directory

The API credential variable is selected separately with `--api-key-env`; it defaults to `OPENAI_API_KEY`.

If `context_window` is omitted, Latigo tries to discover it from the provider's model metadata at startup and after `/model` changes. An explicit override remains fixed. If discovery fails, Latigo keeps the previous limit and reports a warning.

Reasoning effort can be set with `--effort`, `AGENT_REASONING_EFFORT`, or `reasoning_effort` in `config.toml`. If none is set, Latigo omits the request field and uses the provider's default.

Run `latigo --help` for all command-line options.

Validate configuration and trusted plugin manifests without an API key, provider request, or MCP process launch:

```bash
latigo doctor
latigo doctor ./candidate-config.toml
```

The doctor checks command rules, retry settings, hooks, MCP declarations, plugins, and subagents. It exits nonzero on invalid or explicitly requested missing files.

## Project instructions

Latigo loads UTF-8 `AGENTS.md` files into the system context at startup:

1. `~/.config/latigo/AGENTS.md`
2. `AGENTS.md` and `.latigo/AGENTS.md` in each ancestor directory, ordered from the filesystem root to the workspace

More specific instructions appear later. Duplicate paths and empty files are skipped, and each file is limited to 128 KiB. Instructions are context rather than hard security rules; use hooks and permission rules for enforcement. Parent and subagent contexts include the loaded instructions, and the root instructions survive compaction. Use `/status` to see exactly which files were loaded.

## Non-interactive execution

Run one prompt without opening the interactive interface:

```bash
latigo -p "Summarize this repository"
latigo -p "List the main modules" --output-format json
latigo -p "Inspect the workspace" --output-format stream-json
```

`text` is the default and writes only assistant text to stdout. `json` writes one final result object. `stream-json` writes newline-delimited assistant, tool-call, tool-result, compaction, and final-result events. In text and JSON modes, tool progress and diagnostics go to stderr. Sessions are persisted normally.

Print mode retains the configured permission mode. If an action requires approval, Latigo denies it with a diagnostic rather than waiting for input that may never arrive. For unattended tasks in a trusted workspace, explicitly select `--permission-mode auto`; dangerous commands can still require approval and will be denied when no interactive handler is available.

## Interactive commands

```text
/help                 Show commands
/new                  Start a new session
/sessions             List saved sessions
/resume <id>          Resume a session in the current workspace
/delete <id>          Delete a saved session after confirmation
/clear                Clear the current conversation
/compact              Compact the current conversation
/usage                Show estimated context usage
/status               Show runtime state and loaded instruction files
/models               Browse and select available models
/model [id]           Show or directly change the model
/effort [value]       Show or set a provider-specific reasoning effort
/effort default       Use the provider's default reasoning effort
/mcp                  Show connected MCP servers and tool counts
/hooks                Show configured lifecycle hooks
/plugins              Show loaded plugins and custom commands
/agents               Show configured subagents and tool restrictions
/permissions [mode]   Show or set ask, auto-edit, or auto
/exit                 Save and exit
/quit                 Alias for /exit
```

Interactive terminals use a full-screen Ratatui interface. Non-interactive runs and `TERM=dumb` retain the line-oriented interface.

Use the up and down arrows to navigate input history and Tab to complete slash commands. In the Ratatui interface, `/models` and `/effort` open pickers: use Up/Down to move, Enter to select, and Escape to cancel. Effort choices and defaults come from the current model's metadata rather than a hardcoded list. Direct `/effort <value>` input remains available for provider-specific values. PageUp and PageDown scroll through conversation history, and Ctrl-T expands or collapses tool output. Ctrl-C cancels active model work; at the prompt, Ctrl-C saves the session and exits. Approval dialogs accept `y` and reject with `n`, Enter, or Escape.

Sessions are stored in `~/.latigo/sessions` by default. Input history and background-task data are stored alongside them under `~/.latigo`.

## Attachments

Reference workspace files with `@path` in a prompt:

```text
Review @src/main.rs and explain the command flow.
```

PNG, JPEG, and WebP images are sent as multimodal attachments when the selected model supports images:

```text
Describe @diagram.png and compare it with @src/agent.rs.
```

Images are limited to 10 MB and 8000 by 8000 pixels. File references are whitespace-separated, so paths containing spaces are not currently supported.

## Built-in tools

Latigo exposes 13 core tools to the model:

- `list_files`: list workspace files
- `search_files`: find files by name or glob pattern
- `search_content`: search text within files
- `read_file`: read UTF-8 files or selected line ranges
- `write_file`: create or replace workspace files
- `apply_patch`: replace exact text in workspace files
- `web_search`: search the web
- `fetch_url`: fetch and extract readable content from a URL
- `run_command`: start a background shell command
- `task_list`: list background tasks
- `task_output`: read a task's output
- `task_stop`: stop a task's process group
- `wait_for`: wait for a background task to finish
- `run_subagent`: delegate to a configured subagent when at least one is configured

File tools are restricted to the selected workspace. Commands run in the workspace directory.

## MCP servers

Latigo starts configured Model Context Protocol servers over stdio, initializes them, and discovers their tools at startup. Tools are exposed as `mcp__server__tool` and retain the JSON schema supplied by the server.

```toml
[mcp.servers.filesystem]
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "."]
timeout_seconds = 30

[mcp.servers.local]
command = "/path/to/server"
args = ["--stdio"]
cwd = "/path/to/working/directory"
env = { PUBLIC_SETTING = "value" }
```

Server names may contain ASCII letters, numbers, underscores, and hyphens. The server working directory defaults to the Latigo workspace. Do not put credentials directly in `env`; use the server process's existing environment or a credential mechanism outside version-controlled configuration.

Use `/mcp` to inspect connected servers and tool counts. MCP calls require approval in `ask` and `auto-edit` modes and are allowed in `auto` mode. Latigo supports newline-delimited JSON-RPC MCP stdio transport, tool discovery, and tool calls; HTTP transport, resources, and prompts are not yet implemented.

## Lifecycle hooks

Hooks are trusted shell commands configured in `config.toml`. They run in the workspace and receive one JSON object on stdin with `hook_event_name`, `session_id`, `workspace`, and event-specific fields such as `prompt`, `tool_name`, `tool_input`, or `tool_result`.

```toml
[hooks]
timeout_seconds = 10

[[hooks.session_start]]
command = "./hooks/session-start.sh"

[[hooks.user_prompt_submit]]
command = "./hooks/check-prompt.sh"

[[hooks.pre_tool_use]]
command = "./hooks/validate-tool.sh"
matcher = "write_*"

[[hooks.post_tool_use]]
command = "./hooks/observe-tool.sh"
matcher = "*"

[[hooks.pre_compact]]
command = "./hooks/compaction-guidance.sh"

[[hooks.stop]]
command = "./hooks/check-completion.sh"

[[hooks.session_end]]
command = "./hooks/cleanup.sh"
```

Supported events are `session_start`, `session_end`, `user_prompt_submit`, `pre_tool_use`, `post_tool_use`, `pre_compact`, and `stop`. Matchers are glob patterns and apply only to tool events. A hook can override the global timeout with its own `timeout_seconds`.

Exit code 0 allows the operation. Exit code 2 blocks it and uses stderr, or stdout when stderr is empty, as the reason. Other exit codes are errors. Successful hooks can emit JSON lines with control responses:

```json
{"additionalContext":"Remember this project convention."}
{"decision":"block","reason":"This operation is not allowed."}
```

Additional context is added to prompts, tool results, session startup, or compaction guidance as appropriate. A blocked stop hook, or one returning context, asks the model to continue. Plain non-JSON stdout is ignored. Use `/hooks` to inspect configured event counts. Hooks execute without a separate approval prompt, so only load configuration and plugins you trust.

## Plugins

A plugin is a directory containing `plugin.toml`. Latigo automatically loads plugins from `~/.config/latigo/plugins`; additional trusted plugin directories or manifest files can be listed explicitly:

```toml
plugins = ["./my-plugin", "/absolute/path/to/another-plugin"]
```

Relative paths are resolved from the directory containing `config.toml`. Workspace plugins are not discovered automatically: a repository cannot gain execution merely because Latigo is started inside it.

Example `plugin.toml`:

```toml
name = "project-tools"
description = "Project-specific agent extensions"

[commands.review]
description = "Review the requested files"
prompt = "Review $ARGUMENTS for correctness and regressions."

[agents.reviewer]
description = "Read-only code reviewer"
prompt = "Find concrete correctness and regression risks."
tools = ["read_file", "search_*"]

[[hooks.session_start]]
command = "./hooks/session-start.sh"

[mcp.servers.local]
command = "./mcp-server"
args = ["--stdio"]
```

Custom command arguments replace `$ARGUMENTS` or `{{input}}`. If the template has neither placeholder, nonempty arguments are appended. Built-in commands cannot be overridden. Plugin MCP server and subagent names are namespaced with the plugin name, and plugin hooks default to running in the plugin directory. Both MCP servers and hooks can execute arbitrary programs, so install only trusted plugins. Use `/plugins` to list loaded plugins and custom commands.

## Subagents

Configure named subagents in `config.toml` or a plugin manifest:

```toml
[agents.reviewer]
description = "Read-only code reviewer"
prompt = "Review the delegated code for correctness and regressions. Return concrete findings."
model = "k3.1-pro"
reasoning_effort = "low"
tools = ["read_file", "search_*", "list_files"]
max_steps = 8
```

The parent model can call `run_subagent` with a subagent name and self-contained task. Each run starts with a fresh message history and returns only its final report to the parent conversation. Model and effort overrides use a separate client and do not change the parent selection. Set `reasoning_effort = "default"` to clear an inherited effort override.

Default tools are read-only file/search and web-research tools. `tools` accepts exact names and glob patterns, including MCP names such as `mcp__docs__*`; use `"*"` to allow all non-delegation tools. An empty list disables tools. Nested `run_subagent` calls are always blocked. Subagents share the parent's workspace, permissions, hooks, and MCP connections, so tool restrictions are not a security sandbox. Use `/agents` to inspect configurations.

## Permissions and safety

The default permission mode is `ask`.

- `ask`: file writes, patches, and shell commands require approval.
- `auto-edit`: file writes and patches do not require approval; shell commands still do.
- `auto`: writes, patches, and most shell commands do not require approval. Commands classified as dangerous still require approval.

Change modes interactively:

```text
/permissions auto-edit
```

Command rules in `config.toml` are glob patterns matched against the full command. Deny rules run first. If the allow list is nonempty, commands must match at least one allow rule.

```toml
[commands]
allow = ["cargo *", "git status*"]
deny = ["cargo clean*"]
```

Latigo also blocks known sensitive-file paths, prevents workspace path escapes, limits fetched response sizes, checks URL redirect destinations, and uses process groups when stopping background tasks. These are safeguards, not a complete sandbox. Review the workspace and permission mode before letting an agent execute commands.

## Web research

Kimi-hosted search is the default and uses the configured Kimi client credentials:

```toml
[web_research]
provider = "kimi"
max_results = 5
```

Brave Search is available as an alternative:

```toml
[web_research]
provider = "brave"
api_key_env = "BRAVE_SEARCH_API_KEY"
max_results = 5
```

When using Brave, set `BRAVE_SEARCH_API_KEY` in the environment. `fetch_url` is implemented locally and does not require a search-provider key.

## Development

Run the standard validation suite:

```bash
cargo fmt -- --check
cargo clippy --all-targets -- -D warnings
cargo test
```

The integration tests use a reusable mock provider and cover streaming, sessions, print-mode output formats, instruction loading, offline diagnostics, tool calls, retries, model metadata, sensitive-file protection, background tasks, Kimi search, image attachments, MCP stdio tool calls, lifecycle hooks, plugin extensions, and isolated subagents. Unit tests also cover instruction precedence, subagent tool filtering, plugin command expansion, hook control responses, MCP result handling, Markdown rendering, input editing and completion, tool-output expansion, and process-group shutdown.

## Roadmap

The initial roadmap is complete: MCP, hooks, plugins, and subagents are implemented. Further milestones will be selected from established CLI-agent workflows and documented with the corresponding changes.

## Project status

Latigo is an early-stage learning and experimentation project. Expect interfaces, configuration, and behavior to change as the harness evolves.
