# Latigo

Latigo is an experimental CLI agent harness written in Rust. It connects OpenAI-compatible chat completion APIs to a local workspace with streaming responses, tool execution, persistent sessions, and configurable permission boundaries.

A latigo is the strap that secures a saddle's cinch. Latigo secures the setup around your working agent.

## Features

- Streaming model responses and tool calls
- Ratatui interface with scrollable history, Markdown rendering, and expandable tool output
- OpenAI-compatible providers, including Kimi
- Persistent sessions with workspace-specific resume
- Input history and slash-command completion
- Model listing and model switching
- Metadata-driven reasoning-effort selection with provider defaults
- Model Context Protocol (MCP) stdio servers with dynamic tool discovery
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
/models               Browse and select available models
/model [id]           Show or directly change the model
/effort [value]       Show or set a provider-specific reasoning effort
/effort default       Use the provider's default reasoning effort
/mcp                  Show connected MCP servers and tool counts
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

Latigo exposes 13 tools to the model:

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

The integration tests use a reusable mock provider and cover streaming, sessions, tool calls, retries, model metadata, sensitive-file protection, background tasks, Kimi search, image attachments, and MCP stdio tool calls. Unit tests also cover MCP result handling, Markdown rendering, input editing and completion, tool-output expansion, and process-group shutdown.

## Roadmap

The next planned milestones are:

1. Hooks
2. Plugins
3. Subagents

## Project status

Latigo is an early-stage learning and experimentation project. Expect interfaces, configuration, and behavior to change as the harness evolves.
