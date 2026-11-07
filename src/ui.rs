use std::cell::RefCell;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::rc::Rc;
use std::time::Duration;

use anyhow::Result;
use crossterm::cursor::Show;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use pulldown_cmark::{Event as MarkdownEvent, Parser, Tag, TagEnd};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::{Frame, Terminal};

use crate::agent::{Agent, AgentEvent};
use crate::client::ModelInfo;
use crate::session::{Session, SessionStore};
use crate::{SLASH_COMMANDS, handle_command, save_session};

pub struct Interface {
    core: Rc<RefCell<Core>>,
}

struct Core {
    terminal: Terminal<CrosstermBackend<io::Stdout>>,
    state: UiState,
}

#[derive(Default)]
struct UiState {
    entries: Vec<ConversationEntry>,
    input: InputState,
    model: String,
    session: String,
    permissions: String,
    context: String,
    working: bool,
    stream_agent: bool,
    expand_tools: bool,
    scroll_back: usize,
    page_size: usize,
    approval: Option<String>,
    model_picker: Option<ModelPicker>,
}

struct ModelPicker {
    models: Vec<ModelInfo>,
    selected: usize,
}

enum ConversationEntry {
    User(String),
    Agent(String),
    Tool {
        name: String,
        output: Option<String>,
    },
    Status(String),
    Error(String),
}

#[derive(Default)]
struct InputState {
    text: String,
    cursor: usize,
    history: Vec<String>,
    history_index: Option<usize>,
    draft: String,
}

enum IdleAction {
    None,
    Submit(String),
    Exit,
}

impl Interface {
    pub fn new() -> Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        if let Err(error) = execute!(stdout, EnterAlternateScreen) {
            let _ = disable_raw_mode();
            return Err(error.into());
        }
        let terminal = match Terminal::new(CrosstermBackend::new(stdout)) {
            Ok(terminal) => terminal,
            Err(error) => {
                let _ = disable_raw_mode();
                let _ = execute!(io::stdout(), LeaveAlternateScreen);
                return Err(error.into());
            }
        };
        Ok(Self {
            core: Rc::new(RefCell::new(Core {
                terminal,
                state: UiState::default(),
            })),
        })
    }

    pub fn approval_handler(&self) -> Box<dyn FnMut(&str) -> bool> {
        let core = self.core.clone();
        Box::new(move |action| approve_in_terminal(core.clone(), action).unwrap_or(false))
    }

    pub async fn run(
        self,
        agent: &mut Agent,
        store: &SessionStore,
        session: &mut Session,
        workspace_root: &Path,
        history_path: &Path,
        discover_context_window: bool,
    ) -> Result<()> {
        {
            let mut core = self.core.borrow_mut();
            core.state.input.history = read_history(history_path);
            core.state.entries.push(ConversationEntry::Status(
                "Welcome to Latigo. Use /help for commands.".to_string(),
            ));
        }
        self.render(agent, session)?;
        loop {
            let prompt = match self.read_input()? {
                IdleAction::Submit(prompt) => prompt,
                IdleAction::Exit => {
                    save_session(store, session, agent)?;
                    return Ok(());
                }
                IdleAction::None => continue,
            };
            append_history(history_path, &prompt)?;
            if prompt.starts_with('/') {
                let command = prompt
                    .split_whitespace()
                    .next()
                    .unwrap_or_default()
                    .to_string();
                let outcome = if prompt == "/models" {
                    self.run_model_picker(
                        agent,
                        store,
                        session,
                        workspace_root,
                        discover_context_window,
                    )
                    .await
                } else {
                    self.run_command(
                        &prompt,
                        agent,
                        store,
                        session,
                        workspace_root,
                        discover_context_window,
                    )
                    .await
                };
                match outcome {
                    Some(Ok(result)) => {
                        if matches!(command.as_str(), "/new" | "/clear" | "/resume") {
                            self.core.borrow_mut().state.entries.clear();
                        }
                        if !result.output.trim().is_empty() {
                            self.push_entry(ConversationEntry::Status(result.output));
                        }
                        self.render(agent, session)?;
                        if result.exit {
                            return Ok(());
                        }
                    }
                    Some(Err(error)) => {
                        self.push_entry(ConversationEntry::Error(format!("{error:#}")));
                        self.render(agent, session)?;
                    }
                    None => {
                        self.push_entry(ConversationEntry::Status(
                            "Command interrupted.".to_string(),
                        ));
                        self.render(agent, session)?;
                    }
                }
                continue;
            }

            self.run_agent(&prompt, agent).await;
            if let Err(error) = save_session(store, session, agent) {
                self.push_entry(ConversationEntry::Error(format!(
                    "Could not save session: {error:#}"
                )));
            }
            self.render(agent, session)?;
        }
    }

    fn render(&self, agent: &Agent, session: &Session) -> Result<()> {
        {
            let mut core = self.core.borrow_mut();
            core.state.model = agent.model().to_string();
            core.state.session = session.short_id().to_string();
            core.state.permissions = agent.permission_mode().to_string();
            core.state.context = format!(
                "~{} / {} tokens",
                agent.estimated_tokens(),
                agent.context_window()
            );
        }
        self.core.borrow_mut().draw()
    }

    fn push_entry(&self, entry: ConversationEntry) {
        let mut core = self.core.borrow_mut();
        core.state.entries.push(entry);
        core.state.scroll_back = 0;
    }

    fn record_event(&self, event: AgentEvent) {
        {
            let mut core = self.core.borrow_mut();
            match event {
                AgentEvent::Content(content) => {
                    if !core.state.stream_agent {
                        core.state
                            .entries
                            .push(ConversationEntry::Agent(String::new()));
                        core.state.stream_agent = true;
                    }
                    if let Some(ConversationEntry::Agent(text)) = core.state.entries.last_mut() {
                        text.push_str(&content);
                    }
                }
                AgentEvent::ToolCall(name) => {
                    core.state.stream_agent = false;
                    core.state
                        .entries
                        .push(ConversationEntry::Tool { name, output: None });
                }
                AgentEvent::ToolResult { name, output } => {
                    if let Some(ConversationEntry::Tool {
                        output: destination,
                        ..
                    }) = core.state.entries.iter_mut().rev().find(|entry| {
                        matches!(
                            entry,
                            ConversationEntry::Tool { name: tool_name, .. }
                                if *tool_name == name
                        )
                    }) {
                        *destination = Some(output);
                    }
                }
                AgentEvent::Compacted { before, after } => {
                    core.state.stream_agent = false;
                    core.state.entries.push(ConversationEntry::Status(format!(
                        "Context compacted: ~{before} -> ~{after} tokens."
                    )));
                }
            }
        }
        let _ = self.core.borrow_mut().draw();
    }

    fn read_input(&self) -> Result<IdleAction> {
        self.core.borrow_mut().draw()?;
        loop {
            if let Event::Key(key) = event::read()?
                && is_key_press(key)
            {
                let action = self.handle_idle_key(key);
                if !matches!(action, IdleAction::None) {
                    return Ok(action);
                }
                self.core.borrow_mut().draw()?;
            }
        }
    }

    fn handle_idle_key(&self, key: KeyEvent) -> IdleAction {
        let mut core = self.core.borrow_mut();
        let state = &mut core.state;
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('c') => return IdleAction::Exit,
                KeyCode::Char('t') => {
                    state.expand_tools = !state.expand_tools;
                    return IdleAction::None;
                }
                _ => return IdleAction::None,
            }
        }
        match key.code {
            KeyCode::Enter => {
                let prompt = state.input.text.trim().to_string();
                if prompt.is_empty() {
                    return IdleAction::None;
                }
                state.input.text.clear();
                state.input.cursor = 0;
                state.input.history_index = None;
                if state.input.history.last() != Some(&prompt) {
                    state.input.history.push(prompt.clone());
                }
                state.scroll_back = 0;
                IdleAction::Submit(prompt)
            }
            KeyCode::Up => {
                state.input.previous_history();
                IdleAction::None
            }
            KeyCode::Down => {
                state.input.next_history();
                IdleAction::None
            }
            KeyCode::Left => {
                state.input.move_left();
                IdleAction::None
            }
            KeyCode::Right => {
                state.input.move_right();
                IdleAction::None
            }
            KeyCode::Home => {
                state.input.cursor = 0;
                IdleAction::None
            }
            KeyCode::End => {
                state.input.cursor = state.input.text.len();
                IdleAction::None
            }
            KeyCode::Backspace => {
                state.input.backspace();
                IdleAction::None
            }
            KeyCode::Delete => {
                state.input.delete();
                IdleAction::None
            }
            KeyCode::Tab => {
                state.input.complete_command();
                IdleAction::None
            }
            KeyCode::PageUp => {
                state.scroll_back = state.scroll_back.saturating_add(state.page_size.max(1));
                IdleAction::None
            }
            KeyCode::PageDown => {
                state.scroll_back = state.scroll_back.saturating_sub(state.page_size.max(1));
                IdleAction::None
            }
            KeyCode::Char(character) => {
                state.input.insert(character);
                IdleAction::None
            }
            _ => IdleAction::None,
        }
    }

    fn handle_work_key(&self, key: KeyEvent) -> bool {
        if !is_key_press(key) {
            return false;
        }
        let mut core = self.core.borrow_mut();
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('c') => return true,
                KeyCode::Char('t') => {
                    core.state.expand_tools = !core.state.expand_tools;
                }
                _ => {}
            }
        } else {
            match key.code {
                KeyCode::PageUp => {
                    core.state.scroll_back = core
                        .state
                        .scroll_back
                        .saturating_add(core.state.page_size.max(1));
                }
                KeyCode::PageDown => {
                    core.state.scroll_back = core
                        .state
                        .scroll_back
                        .saturating_sub(core.state.page_size.max(1));
                }
                _ => {}
            }
        }
        false
    }

    async fn run_agent(&self, prompt: &str, agent: &mut Agent) {
        self.push_entry(ConversationEntry::User(prompt.to_string()));
        {
            let mut core = self.core.borrow_mut();
            core.state.working = true;
            core.state.stream_agent = false;
        }
        let checkpoint = agent.checkpoint();
        let outcome = {
            let mut callback = |event| self.record_event(event);
            let future = agent.run(prompt, &mut callback);
            tokio::pin!(future);
            loop {
                tokio::select! {
                    result = &mut future => break Some(result),
                    _ = tokio::time::sleep(Duration::from_millis(50)) => {
                        if self.poll_work_events().unwrap_or(true) {
                            break None;
                        }
                        let _ = self.core.borrow_mut().draw();
                    }
                }
            }
        };
        {
            let mut core = self.core.borrow_mut();
            core.state.working = false;
            core.state.stream_agent = false;
        }
        match outcome {
            Some(Ok(_)) => {}
            Some(Err(error)) => self.push_entry(ConversationEntry::Error(format!("{error:#}"))),
            None => {
                agent.rollback(checkpoint);
                self.push_entry(ConversationEntry::Status(
                    "Interrupted. Partial conversation was rolled back.".to_string(),
                ));
            }
        }
    }

    async fn run_model_picker(
        &self,
        agent: &mut Agent,
        store: &SessionStore,
        session: &mut Session,
        workspace_root: &Path,
        discover_context_window: bool,
    ) -> Option<Result<crate::CommandResult>> {
        self.push_entry(ConversationEntry::User("/models".to_string()));
        self.core.borrow_mut().state.working = true;
        let outcome = {
            let future = agent.list_models();
            tokio::pin!(future);
            loop {
                tokio::select! {
                    result = &mut future => break Some(result),
                    _ = tokio::time::sleep(Duration::from_millis(50)) => {
                        if self.poll_work_events().unwrap_or(true) {
                            break None;
                        }
                        let _ = self.core.borrow_mut().draw();
                    }
                }
            }
        };
        self.core.borrow_mut().state.working = false;
        let models = match outcome {
            Some(Ok(models)) => models,
            Some(Err(error)) => return Some(Err(error)),
            None => {
                return Some(Ok(crate::CommandResult {
                    output: "Model selection canceled.\n".to_string(),
                    ..crate::CommandResult::default()
                }));
            }
        };
        if models.is_empty() {
            return Some(Ok(crate::CommandResult {
                output: "The provider returned no models.\n".to_string(),
                ..crate::CommandResult::default()
            }));
        }
        let selected = models
            .iter()
            .position(|model| model.id == agent.model())
            .unwrap_or(0);
        self.core.borrow_mut().state.model_picker = Some(ModelPicker { models, selected });
        match self.read_model_picker() {
            Ok(Some(model)) => {
                self.run_command(
                    &format!("/model {model}"),
                    agent,
                    store,
                    session,
                    workspace_root,
                    discover_context_window,
                )
                .await
            }
            Ok(None) => Some(Ok(crate::CommandResult {
                output: "Model selection canceled.\n".to_string(),
                ..crate::CommandResult::default()
            })),
            Err(error) => Some(Err(error)),
        }
    }

    fn read_model_picker(&self) -> Result<Option<String>> {
        loop {
            self.core.borrow_mut().draw()?;
            if let Event::Key(key) = event::read()?
                && is_key_press(key)
            {
                if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c')
                    || key.code == KeyCode::Esc
                {
                    self.core.borrow_mut().state.model_picker = None;
                    return Ok(None);
                }
                let mut core = self.core.borrow_mut();
                let Some(picker) = core.state.model_picker.as_mut() else {
                    return Ok(None);
                };
                match key.code {
                    KeyCode::Up => {
                        picker.selected = if picker.selected == 0 {
                            picker.models.len() - 1
                        } else {
                            picker.selected - 1
                        };
                    }
                    KeyCode::Down => {
                        picker.selected = (picker.selected + 1) % picker.models.len();
                    }
                    KeyCode::Home => picker.selected = 0,
                    KeyCode::End => picker.selected = picker.models.len() - 1,
                    KeyCode::Enter => {
                        let model = picker.models[picker.selected].id.clone();
                        core.state.model_picker = None;
                        return Ok(Some(model));
                    }
                    _ => {}
                }
            }
        }
    }

    async fn run_command(
        &self,
        prompt: &str,
        agent: &mut Agent,
        store: &SessionStore,
        session: &mut Session,
        workspace_root: &Path,
        discover_context_window: bool,
    ) -> Option<Result<crate::CommandResult>> {
        self.push_entry(ConversationEntry::User(prompt.to_string()));
        self.core.borrow_mut().state.working = true;
        let outcome = {
            let mut approval = self.approval_handler();
            let future = handle_command(
                prompt,
                agent,
                store,
                session,
                workspace_root,
                discover_context_window,
                &mut approval,
            );
            tokio::pin!(future);
            loop {
                tokio::select! {
                    result = &mut future => break Some(result),
                    _ = tokio::time::sleep(Duration::from_millis(50)) => {
                        if self.poll_work_events().unwrap_or(true) {
                            break None;
                        }
                        let _ = self.core.borrow_mut().draw();
                    }
                }
            }
        };
        self.core.borrow_mut().state.working = false;
        outcome
    }

    fn poll_work_events(&self) -> Result<bool> {
        while event::poll(Duration::ZERO)? {
            if let Event::Key(key) = event::read()?
                && self.handle_work_key(key)
            {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

impl Drop for Interface {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let mut core = self.core.borrow_mut();
        let _ = execute!(core.terminal.backend_mut(), LeaveAlternateScreen, Show);
    }
}

impl Core {
    fn draw(&mut self) -> Result<()> {
        self.state.page_size = (self.terminal.size()?.height as usize)
            .saturating_sub(4)
            .max(1);
        self.terminal
            .draw(|frame| draw_frame(frame, &self.state))
            .map(|_| ())
            .map_err(Into::into)
    }
}

fn approve_in_terminal(core: Rc<RefCell<Core>>, action: &str) -> Result<bool> {
    {
        let mut borrowed = core.borrow_mut();
        borrowed.state.approval = Some(action.to_string());
    }
    let decision = (|| {
        loop {
            core.borrow_mut().draw()?;
            if let Event::Key(key) = event::read()?
                && is_key_press(key)
            {
                let accepted = matches!(key.code, KeyCode::Char('y') | KeyCode::Char('Y'));
                let answered = accepted
                    || matches!(
                        key.code,
                        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc | KeyCode::Enter
                    )
                    || (key.modifiers.contains(KeyModifiers::CONTROL)
                        && key.code == KeyCode::Char('c'));
                if answered {
                    return Ok(accepted);
                }
            }
        }
    })();
    let mut borrowed = core.borrow_mut();
    borrowed.state.approval = None;
    let _ = borrowed.draw();
    decision
}

fn draw_frame(frame: &mut Frame, state: &UiState) {
    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(2),
        Constraint::Length(1),
    ])
    .split(frame.area());
    let status = if state.working { "working" } else { "ready" };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("Latigo", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(format!(
                " | {} | session {} | {} | {} | {}",
                state.model, state.session, state.permissions, state.context, status
            )),
        ])),
        chunks[0],
    );

    let text = conversation_text(&state.entries, state.expand_tools);
    let total_lines = wrapped_line_count(&text, chunks[1].width);
    let conversation = Paragraph::new(text).wrap(Wrap { trim: false });
    let height = chunks[1].height as usize;
    let max_scroll_back = total_lines.saturating_sub(height);
    let scroll_back = state.scroll_back.min(max_scroll_back);
    let top = total_lines.saturating_sub(height + scroll_back);
    frame.render_widget(conversation.scroll((top as u16, 0)), chunks[1]);

    let input_block = Block::default().borders(Borders::TOP).title("you");
    let input_inner = input_block.inner(chunks[2]);
    let cursor_width = Span::raw(&state.input.text[..state.input.cursor]).width();
    let available = (input_inner.width as usize).saturating_sub(3);
    let horizontal_scroll = cursor_width.saturating_sub(available);
    let input = Paragraph::new(Line::from(vec![
        Span::styled("> ", Style::default().fg(Color::Cyan)),
        Span::raw(state.input.text.clone()),
    ]))
    .block(input_block)
    .scroll((0, horizontal_scroll as u16));
    frame.render_widget(input, chunks[2]);
    if !state.working && state.approval.is_none() && state.model_picker.is_none() {
        let cursor_x = input_inner.x as usize + 2 + cursor_width - horizontal_scroll;
        frame.set_cursor_position((cursor_x as u16, input_inner.y));
    }
    frame.render_widget(
        Paragraph::new(
            "Enter send · Tab complete · PgUp/PgDn scroll · Ctrl-T tools · Ctrl-C cancel/exit",
        )
        .style(Style::default().fg(Color::DarkGray)),
        chunks[3],
    );

    if let Some(action) = &state.approval {
        let area = centered_rect(80, 60, frame.area());
        let prompt = Paragraph::new(format!("{action}\n\nAllow? [y/N]"))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("Approval required"),
            )
            .wrap(Wrap { trim: false });
        frame.render_widget(Clear, area);
        frame.render_widget(prompt, area);
    }
    if let Some(picker) = &state.model_picker {
        let area = centered_rect(80, 70, frame.area());
        let block = Block::default()
            .borders(Borders::ALL)
            .title("Select model · ↑/↓ move · Enter select · Esc cancel");
        let inner = block.inner(area);
        let lines = model_picker_lines(picker, &state.model, inner.height as usize);
        frame.render_widget(Clear, area);
        frame.render_widget(block, area);
        frame.render_widget(Paragraph::new(lines), inner);
    }
}

fn model_picker_lines(
    picker: &ModelPicker,
    current_model: &str,
    visible_height: usize,
) -> Vec<Line<'static>> {
    let visible_height = visible_height.max(1);
    let start = picker
        .selected
        .saturating_add(1)
        .saturating_sub(visible_height);
    picker
        .models
        .iter()
        .enumerate()
        .skip(start)
        .take(visible_height)
        .map(|(index, model)| {
            let selected = index == picker.selected;
            let selector = if selected { ">" } else { " " };
            let current = if model.id == current_model { "*" } else { " " };
            let name = if model.display_name == model.id {
                model.id.clone()
            } else {
                format!("{} ({})", model.display_name, model.id)
            };
            let context = model
                .context_length
                .map(|value| format!(" · {value} tokens"))
                .unwrap_or_default();
            let style = if selected {
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else if model.id == current_model {
                Style::default().fg(Color::Green)
            } else {
                Style::default()
            };
            Line::styled(format!("{selector} {current} {name}{context}"), style)
        })
        .collect()
}

fn wrapped_line_count(text: &Text, width: u16) -> usize {
    let width = width as usize;
    if width == 0 {
        return text.lines.len();
    }
    text.lines
        .iter()
        .map(|line| {
            let mut rows = 1;
            let mut column = 0;
            for span in &line.spans {
                for token in span.content.split_inclusive(char::is_whitespace) {
                    let mut remaining = Span::raw(token).width();
                    if column > 0 && column + remaining > width {
                        rows += 1;
                        column = 0;
                    }
                    while remaining > 0 {
                        let consumed = remaining.min(width - column);
                        column += consumed;
                        remaining -= consumed;
                        if column == width && remaining > 0 {
                            rows += 1;
                            column = 0;
                        }
                    }
                }
            }
            rows
        })
        .sum()
}

fn conversation_text(entries: &[ConversationEntry], expand_tools: bool) -> Text<'static> {
    let mut lines = Vec::new();
    for entry in entries {
        if !lines.is_empty() {
            lines.push(Line::default());
        }
        match entry {
            ConversationEntry::User(content) => {
                lines.push(label_line("You", Color::Cyan));
                push_plain_lines(&mut lines, content, Style::default());
            }
            ConversationEntry::Agent(content) => {
                lines.push(label_line("Agent", Color::Green));
                lines.extend(markdown_text(content).lines);
            }
            ConversationEntry::Tool { name, output } => {
                lines.push(label_line(&format!("Tool · {name}"), Color::Magenta));
                match output {
                    Some(output) if expand_tools => {
                        push_plain_lines(&mut lines, output, Style::default().fg(Color::Gray));
                    }
                    Some(output) => {
                        let summary = output.lines().next().unwrap_or("No output").to_string();
                        lines.push(Line::from(vec![
                            Span::styled(summary, Style::default().fg(Color::DarkGray)),
                            Span::styled(
                                "  [Ctrl-T to expand]",
                                Style::default().fg(Color::DarkGray),
                            ),
                        ]));
                    }
                    None => lines.push(Line::styled(
                        "Running…",
                        Style::default().fg(Color::DarkGray),
                    )),
                }
            }
            ConversationEntry::Status(content) => {
                lines.push(label_line("Latigo", Color::Blue));
                push_plain_lines(&mut lines, content, Style::default().fg(Color::Gray));
            }
            ConversationEntry::Error(content) => {
                lines.push(label_line("Error", Color::Red));
                push_plain_lines(&mut lines, content, Style::default().fg(Color::Red));
            }
        }
    }
    Text::from(lines)
}

fn label_line(label: &str, color: Color) -> Line<'static> {
    Line::from(Span::styled(
        label.to_string(),
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    ))
}

fn push_plain_lines(lines: &mut Vec<Line<'static>>, content: &str, style: Style) {
    lines.extend(
        content
            .lines()
            .map(|line| Line::from(Span::styled(line.to_string(), style))),
    );
}

fn markdown_text(content: &str) -> Text<'static> {
    let mut writer = MarkdownWriter::default();
    for event in Parser::new(content) {
        writer.event(event);
    }
    writer.finish_line();
    Text::from(writer.lines)
}

#[derive(Default)]
struct MarkdownWriter {
    lines: Vec<Line<'static>>,
    spans: Vec<Span<'static>>,
    style: Style,
    styles: Vec<Style>,
    code_block: bool,
}

impl MarkdownWriter {
    fn event(&mut self, event: MarkdownEvent<'_>) {
        match event {
            MarkdownEvent::Start(tag) => match tag {
                Tag::Paragraph => self.finish_line(),
                Tag::Heading { .. } => {
                    self.finish_line();
                    self.push_style(self.style.add_modifier(Modifier::BOLD).fg(Color::Green));
                }
                Tag::Emphasis => self.push_style(self.style.add_modifier(Modifier::ITALIC)),
                Tag::Strong => self.push_style(self.style.add_modifier(Modifier::BOLD)),
                Tag::Strikethrough => {
                    self.push_style(self.style.add_modifier(Modifier::CROSSED_OUT));
                }
                Tag::Link { .. } => {
                    self.push_style(
                        self.style
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::UNDERLINED),
                    );
                }
                Tag::CodeBlock(_) => {
                    self.finish_line();
                    self.code_block = true;
                }
                Tag::BlockQuote(_) => {
                    self.finish_line();
                    self.spans
                        .push(Span::styled("> ", Style::default().fg(Color::DarkGray)));
                }
                Tag::Item => {
                    self.finish_line();
                    self.spans.push(Span::raw("- "));
                }
                _ => {}
            },
            MarkdownEvent::End(tag) => match tag {
                TagEnd::Paragraph | TagEnd::Item => self.finish_line(),
                TagEnd::Heading(_) => {
                    self.finish_line();
                    self.pop_style();
                }
                TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough | TagEnd::Link => {
                    self.pop_style();
                }
                TagEnd::CodeBlock => {
                    self.finish_line();
                    self.code_block = false;
                }
                TagEnd::BlockQuote(_) => self.finish_line(),
                _ => {}
            },
            MarkdownEvent::Text(text) => self.text(&text),
            MarkdownEvent::Code(code) => self.spans.push(Span::styled(
                code.into_string(),
                Style::default().fg(Color::Yellow),
            )),
            MarkdownEvent::SoftBreak => self.spans.push(Span::raw(" ")),
            MarkdownEvent::HardBreak => self.finish_line(),
            MarkdownEvent::Rule => {
                self.finish_line();
                self.lines.push(Line::styled(
                    "────────────────────────────────────────",
                    Style::default().fg(Color::DarkGray),
                ));
            }
            MarkdownEvent::TaskListMarker(done) => {
                self.spans
                    .push(Span::raw(if done { "[x] " } else { "[ ] " }));
            }
            _ => {}
        }
    }

    fn text(&mut self, text: &str) {
        let style = if self.code_block {
            Style::default().fg(Color::Yellow)
        } else {
            self.style
        };
        let mut parts = text.split('\n').peekable();
        while let Some(part) = parts.next() {
            if !part.is_empty() {
                self.spans.push(Span::styled(part.to_string(), style));
            }
            if parts.peek().is_some() {
                self.finish_line();
            }
        }
    }

    fn push_style(&mut self, style: Style) {
        self.styles.push(self.style);
        self.style = style;
    }

    fn pop_style(&mut self) {
        self.style = self.styles.pop().unwrap_or_default();
    }

    fn finish_line(&mut self) {
        if !self.spans.is_empty() {
            self.lines.push(Line::from(std::mem::take(&mut self.spans)));
        }
    }
}

impl InputState {
    fn insert(&mut self, character: char) {
        self.text.insert(self.cursor, character);
        self.cursor += character.len_utf8();
    }

    fn backspace(&mut self) {
        if let Some((index, _)) = self.text[..self.cursor].char_indices().next_back() {
            self.text.remove(index);
            self.cursor = index;
        }
    }

    fn delete(&mut self) {
        if let Some(character) = self.text[self.cursor..].chars().next() {
            self.text
                .drain(self.cursor..self.cursor + character.len_utf8());
        }
    }

    fn move_left(&mut self) {
        if let Some((index, _)) = self.text[..self.cursor].char_indices().next_back() {
            self.cursor = index;
        }
    }

    fn move_right(&mut self) {
        if let Some(character) = self.text[self.cursor..].chars().next() {
            self.cursor += character.len_utf8();
        }
    }

    fn previous_history(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let index = match self.history_index {
            Some(index) => index.saturating_sub(1),
            None => {
                self.draft = self.text.clone();
                self.history.len() - 1
            }
        };
        self.history_index = Some(index);
        self.text = self.history[index].clone();
        self.cursor = self.text.len();
    }

    fn next_history(&mut self) {
        match self.history_index {
            Some(index) if index + 1 < self.history.len() => {
                self.history_index = Some(index + 1);
                self.text = self.history[index + 1].clone();
            }
            Some(_) => {
                self.history_index = None;
                self.text = self.draft.clone();
            }
            None => return,
        }
        self.cursor = self.text.len();
    }

    fn complete_command(&mut self) {
        if !self.text.starts_with('/') || self.text.contains(char::is_whitespace) {
            return;
        }
        let mut candidates = SLASH_COMMANDS
            .iter()
            .filter(|command| command.starts_with(&self.text));
        let Some(first) = candidates.next() else {
            return;
        };
        let mut prefix = first.to_string();
        for candidate in candidates {
            prefix = prefix
                .chars()
                .zip(candidate.chars())
                .take_while(|(left, right)| left == right)
                .map(|(character, _)| character)
                .collect();
        }
        if prefix.len() >= self.text.len() {
            self.text = prefix;
            self.cursor = self.text.len();
        }
    }
}

fn read_history(path: &Path) -> Vec<String> {
    fs::read_to_string(path)
        .map(|content| {
            content
                .lines()
                .filter(|line| !line.trim().is_empty())
                .map(ToString::to_string)
                .collect()
        })
        .unwrap_or_default()
}

pub fn append_history(path: &Path, prompt: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    writeln!(file, "{prompt}")?;
    Ok(())
}

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let vertical = Layout::vertical([
        Constraint::Percentage((100 - percent_y) / 2),
        Constraint::Percentage(percent_y),
        Constraint::Percentage((100 - percent_y) / 2),
    ])
    .split(area);
    Layout::horizontal([
        Constraint::Percentage((100 - percent_x) / 2),
        Constraint::Percentage(percent_x),
        Constraint::Percentage((100 - percent_x) / 2),
    ])
    .split(vertical[1])[1]
}

fn is_key_press(key: KeyEvent) -> bool {
    matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_renders_headings_emphasis_and_code() {
        let text = markdown_text("# Heading\n\n**bold** and `code`");
        let flattened = text
            .lines
            .iter()
            .flat_map(|line| &line.spans)
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(flattened.contains("Heading"));
        assert!(flattened.contains("bold"));
        assert!(flattened.contains("code"));
        assert!(
            text.lines
                .iter()
                .flat_map(|line| &line.spans)
                .any(|span| span.content == "bold"
                    && span.style.add_modifier.contains(Modifier::BOLD))
        );
        assert!(
            text.lines
                .iter()
                .flat_map(|line| &line.spans)
                .any(|span| span.content == " and "
                    && !span.style.add_modifier.contains(Modifier::BOLD)
                    && span.style.fg != Some(Color::Green))
        );
    }

    #[test]
    fn model_picker_marks_selection_current_model_and_scrolls() {
        let models = vec![
            ModelInfo {
                id: "first".to_string(),
                display_name: "First".to_string(),
                context_length: Some(1000),
                supports_image_in: None,
            },
            ModelInfo {
                id: "second".to_string(),
                display_name: "Second".to_string(),
                context_length: Some(2000),
                supports_image_in: None,
            },
        ];
        let picker = ModelPicker {
            models,
            selected: 1,
        };
        let lines = model_picker_lines(&picker, "second", 2);
        assert_eq!(
            lines[1].spans[0].content.as_ref(),
            "> * Second (second) · 2000 tokens"
        );
        assert_eq!(lines[1].style.bg, Some(Color::Cyan));

        let scrolled = model_picker_lines(&picker, "second", 1);
        assert_eq!(scrolled.len(), 1);
        assert!(scrolled[0].spans[0].content.contains("Second"));
    }

    #[test]
    fn tool_output_can_be_expanded() {
        let entries = vec![ConversationEntry::Tool {
            name: "read_file".to_string(),
            output: Some("summary\nfull output".to_string()),
        }];
        let collapsed = conversation_text(&entries, false);
        let expanded = conversation_text(&entries, true);
        assert!(!format!("{collapsed:?}").contains("full output"));
        assert!(format!("{expanded:?}").contains("full output"));
    }

    #[test]
    fn scroll_height_accounts_for_word_wrapping() {
        assert_eq!(wrapped_line_count(&Text::from("aa bb cc"), 4), 3);
        assert_eq!(wrapped_line_count(&Text::from("abcdef"), 3), 2);
        assert_eq!(wrapped_line_count(&Text::from(""), 10), 1);
    }

    #[test]
    fn input_supports_unicode_editing_history_and_completion() {
        let mut input = InputState::default();
        input.insert('ç');
        assert_eq!(input.cursor, 'ç'.len_utf8());
        input.backspace();
        assert!(input.text.is_empty());

        input.text = "/mo".to_string();
        input.cursor = input.text.len();
        input.complete_command();
        assert_eq!(input.text, "/model");

        input.history = vec!["first".to_string(), "second".to_string()];
        input.previous_history();
        assert_eq!(input.text, "second");
        input.previous_history();
        assert_eq!(input.text, "first");
        input.next_history();
        assert_eq!(input.text, "second");
        input.next_history();
        assert_eq!(input.text, "/model");
    }
}
