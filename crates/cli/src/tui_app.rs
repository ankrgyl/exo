use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event,
    EventStream, KeyCode, KeyEventKind, KeyModifiers, MouseEventKind,
};
use executor::{
    EventId, EventQuery, EventQueryDirection, ExecutionStreamEvent, HarnessAgent,
    HarnessConversation, SendRequest, SessionId,
};
use lingua::Message;
use lingua::universal::UserContent;
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use tokio::sync::mpsc;
use tokio_stream::StreamExt;

use crate::render::{
    ASSISTANT_LABEL, Verbosity, compact_result_status, compact_timestamp, render_tool_call,
    render_tool_result, render_transcript_lines,
};
use crate::tui::{
    chunk_text, cost_lines, render_external_event, rewind_lines, shell_lines, snapshot_lines,
    snapshots_lines, teleport_lines,
};

const SPINNER: [&str; 4] = ["|", "/", "-", "\\"];

// Slash-command registry for the TUI: names, aliases, arguments, and help all
// live in this clap tree. `multicall` makes the first token the command name,
// and clap's built-in `help` subcommand serves `/help`.
#[derive(Debug, clap::Parser)]
#[command(
    name = "repl",
    multicall = true,
    disable_help_flag = true,
    about = "repl slash commands"
)]
struct ReplCli {
    #[command(subcommand)]
    command: ReplCommand,
}

// `override_usage` keeps generated usage in slash form; `disable_help_flag`
// drops the `-h, --help` flag noise (per-command help stays available via
// `/help <command>`).
#[derive(Debug, PartialEq, clap::Subcommand)]
enum ReplCommand {
    /// Exit the repl
    #[command(
        visible_alias = "exit",
        override_usage = "/quit",
        disable_help_flag = true
    )]
    Quit,
    /// Summarize token usage and dollar cost
    #[command(
        visible_alias = "usage",
        override_usage = "/cost",
        disable_help_flag = true
    )]
    Cost,
    /// Show or set how much tool detail is printed
    #[command(
        override_usage = "/verbosity [minimal|compact|full]",
        disable_help_flag = true
    )]
    Verbosity {
        #[arg(value_enum)]
        level: Option<Verbosity>,
    },
    /// Run a command in the conversation's sandbox
    #[command(
        visible_alias = "sandbox",
        override_usage = "/shell <command>",
        disable_help_flag = true
    )]
    Shell {
        /// Shell source, passed to the sandbox verbatim
        command: String,
    },
    /// Snapshot a sandbox in this conversation (defaults to the latest one)
    #[command(override_usage = "/snapshot [<sandbox-id>]", disable_help_flag = true)]
    Snapshot { sandbox_id: Option<String> },
    /// List snapshots taken in this conversation
    #[command(override_usage = "/snapshots", disable_help_flag = true)]
    Snapshots,
    /// Restore the sandbox to a previous snapshot
    #[command(override_usage = "/rewind <snapshot-id>", disable_help_flag = true)]
    Rewind { snapshot_id: String },
    /// Move the live sandbox to another provider (e.g. daytona: snapshot + restore there)
    #[command(override_usage = "/teleport <provider>", disable_help_flag = true)]
    Teleport { provider: String },
}

/// `/help` listing, generated from the clap tree but formatted for the
/// transcript instead of clap's CLI-shaped help screen.
fn command_help_lines() -> Vec<String> {
    use clap::CommandFactory;

    let mut cmd = ReplCli::command();
    let mut entries: Vec<(String, String)> = Vec::new();
    for sub in cmd
        .get_subcommands_mut()
        .filter(|sub| sub.get_name() != "help")
    {
        let aliases: Vec<String> = sub
            .get_visible_aliases()
            .map(|alias| format!("/{alias}"))
            .collect();
        let mut about = sub.get_about().map(ToString::to_string).unwrap_or_default();
        if !aliases.is_empty() {
            about = format!("{about} (alias {})", aliases.join(", "));
        }
        let synopsis = sub
            .render_usage()
            .to_string()
            .trim_start_matches("Usage:")
            .trim()
            .to_string();
        entries.push((synopsis, about));
    }
    entries.push((
        "/help [<command>]".to_string(),
        "show this message".to_string(),
    ));

    let width = entries
        .iter()
        .map(|(synopsis, _)| synopsis.chars().count())
        .max()
        .unwrap_or(0);
    let mut lines = vec!["commands:".to_string()];
    for (synopsis, about) in entries {
        lines.push(format!("  {synopsis:<width$}  {about}"));
    }
    lines
}

/// clap's error text is written for a shell CLI; trim the parts that make no
/// sense inside the repl (help-flag hints and the bare top-level usage line).
fn clap_error_lines(error: &clap::Error) -> Vec<String> {
    let rendered = error.to_string();
    let mut lines: Vec<String> = rendered
        .lines()
        .filter(|line| !line.trim_start().starts_with("For more information"))
        .filter(|line| line.trim() != "Usage: <COMMAND>")
        .map(str::to_string)
        .collect();
    lines.dedup_by(|a, b| a.trim().is_empty() && b.trim().is_empty());
    while lines.last().is_some_and(|line| line.trim().is_empty()) {
        lines.pop();
    }
    while lines.first().is_some_and(|line| line.trim().is_empty()) {
        lines.remove(0);
    }
    lines
}

/// What a line of repl input means.
#[derive(Debug, PartialEq)]
enum ReplInput {
    Empty,
    /// Plain chat text for the model.
    Chat(String),
    Command(ReplCommand),
}

fn parse_repl_input(line: &str) -> Result<ReplInput, clap::Error> {
    use clap::{CommandFactory, Parser};

    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Ok(ReplInput::Empty);
    }
    let Some(stripped) = trimmed.strip_prefix('/') else {
        return Ok(ReplInput::Chat(trimmed.to_string()));
    };
    // `//text` escapes to the chat message `/text`.
    if stripped.starts_with('/') {
        return Ok(ReplInput::Chat(stripped.to_string()));
    }
    // A lone `/` shows the command list.
    if stripped.is_empty() {
        return Err(ReplCli::try_parse_from(["help"])
            .expect_err("the help subcommand always short-circuits into an error"));
    }

    let (head, rest) = match stripped.split_once(char::is_whitespace) {
        Some((head, rest)) => (head, rest.trim()),
        None => (stripped, ""),
    };
    // Shell source must reach the sandbox verbatim: shlex-splitting and
    // rejoining would change quoting, expansion, pipes, and redirections.
    let argv: Vec<String> = if matches!(head, "shell" | "sandbox") && !rest.is_empty() {
        vec![head.to_string(), rest.to_string()]
    } else {
        shlex::split(stripped).ok_or_else(|| {
            ReplCli::command().error(
                clap::error::ErrorKind::InvalidValue,
                "unbalanced quotes in command input",
            )
        })?
    };
    Ok(ReplInput::Command(ReplCli::try_parse_from(argv)?.command))
}

/// The input box grows with wrapped content up to this many text rows, then
/// scrolls vertically.
const MAX_INPUT_ROWS: u16 = 8;

/// Wrap input at exact character boundaries so the cursor position stays a
/// simple row/column computation (word-wrap would make it unpredictable).
/// Embedded newlines (pasted or Alt+Enter) are hard breaks. The cursor sits
/// at the end, so a line that exactly fills the width rolls over to a fresh
/// empty line.
fn wrap_input_chars(input: &str, width: usize) -> Vec<String> {
    let mut lines = vec![String::new()];
    let mut column = 0;
    for ch in input.chars() {
        if ch == '\n' || column == width {
            lines.push(String::new());
            column = 0;
            if ch == '\n' {
                continue;
            }
        }
        lines.last_mut().expect("lines never empty").push(ch);
        column += 1;
    }
    if column == width {
        lines.push(String::new());
    }
    lines
}

/// Everything the UI task can be woken by besides key presses.
enum AppEvent {
    Stream(ExecutionStreamEvent),
    StreamError(String),
    /// A send or background command finished; clear the busy state.
    StreamDone,
    /// Finished output of a background slash command.
    CommandOutput(Vec<String>),
    External(Vec<String>),
}

pub async fn run_chat_tui(
    agent: Arc<dyn HarnessAgent>,
    conversation: Arc<dyn HarnessConversation>,
    verbosity: Verbosity,
) -> Result<()> {
    let terminal = ratatui::init();
    // Capture the mouse so wheel motion reaches us as scroll events; without
    // this the terminal fakes arrow keys, which would page the input history.
    // Bracketed paste keeps pasted newlines literal instead of sending the
    // message once per line.
    let _ = crossterm::execute!(std::io::stdout(), EnableMouseCapture, EnableBracketedPaste);
    let result = TuiApp::new(agent, conversation, verbosity)
        .run(terminal)
        .await;
    let _ = crossterm::execute!(
        std::io::stdout(),
        DisableMouseCapture,
        DisableBracketedPaste
    );
    ratatui::restore();
    result
}

struct TuiApp {
    agent: Arc<dyn HarnessAgent>,
    conversation: Arc<dyn HarnessConversation>,
    verbosity: Verbosity,
    transcript: Vec<Line<'static>>,
    input: String,
    input_history: Vec<String>,
    history_pos: Option<usize>,
    /// Lines scrolled up from the bottom; 0 means follow new output.
    scrollback: u16,
    /// A turn or command is in flight. Shared with the event watcher, which
    /// pauses while set so it never re-renders messages the streaming path
    /// already displayed.
    busy: Arc<AtomicBool>,
    /// Whether the terminal reports mouse events to us (wheel scrolling).
    /// Toggled off to let the terminal's native text selection work.
    mouse_captured: bool,
    spinner_frame: usize,
    session_id: Option<SessionId>,
    watch_after: Arc<Mutex<Option<EventId>>>,
    /// Transcript index of the assistant line currently streaming.
    open_assistant: Option<usize>,
    /// Whether the streaming assistant message already got its prefix line.
    assistant_prefixed: bool,
    /// Transcript index of each unresolved tool-call line, by call id.
    open_calls: HashMap<String, usize>,
    /// Tool names by call id, for full-mode results and fallbacks.
    pending_tool_names: HashMap<String, String>,
}

impl TuiApp {
    fn new(
        agent: Arc<dyn HarnessAgent>,
        conversation: Arc<dyn HarnessConversation>,
        verbosity: Verbosity,
    ) -> Self {
        let watch_after = Arc::new(Mutex::new(conversation.record().latest_event_id));
        Self {
            agent,
            conversation,
            verbosity,
            transcript: Vec::new(),
            input: String::new(),
            input_history: Vec::new(),
            history_pos: None,
            scrollback: 0,
            busy: Arc::new(AtomicBool::new(false)),
            mouse_captured: true,
            spinner_frame: 0,
            session_id: None,
            watch_after,
            open_assistant: None,
            assistant_prefixed: false,
            open_calls: HashMap::new(),
            pending_tool_names: HashMap::new(),
        }
    }

    async fn run(mut self, mut terminal: ratatui::DefaultTerminal) -> Result<()> {
        self.load_transcript().await?;

        let (tx, mut rx) = mpsc::unbounded_channel::<AppEvent>();
        let watcher = self.spawn_event_watcher(tx.clone());
        let mut keys = EventStream::new();
        let mut tick = tokio::time::interval(Duration::from_millis(120));

        let outcome = loop {
            terminal.draw(|frame| self.draw(frame))?;
            tokio::select! {
                key = keys.next() => match key {
                    Some(Ok(event)) => {
                        if self.handle_terminal_event(event, &tx).await? {
                            break Ok(());
                        }
                    }
                    Some(Err(error)) => break Err(error.into()),
                    None => break Ok(()),
                },
                app_event = rx.recv() => {
                    if let Some(app_event) = app_event {
                        self.handle_app_event(app_event);
                    }
                }
                _ = tick.tick() => {
                    if self.is_busy() {
                        self.spinner_frame = self.spinner_frame.wrapping_add(1);
                    }
                }
            }
        };

        watcher.abort();
        if let Some(session_id) = self.session_id.take() {
            self.conversation.close_session(session_id).await?;
        }
        outcome
    }

    async fn load_transcript(&mut self) -> Result<()> {
        let messages = self.conversation.messages().await?;
        self.transcript = render_transcript_lines(&messages, self.verbosity)
            .iter()
            .flat_map(|rendered| rendered.split('\n'))
            .map(|line| style_transcript_line(line.to_string()))
            .collect();
        Ok(())
    }

    /// Returns true when the app should exit.
    async fn handle_terminal_event(
        &mut self,
        event: Event,
        tx: &mpsc::UnboundedSender<AppEvent>,
    ) -> Result<bool> {
        if let Event::Mouse(mouse) = event {
            match mouse.kind {
                MouseEventKind::ScrollUp => self.scrollback = self.scrollback.saturating_add(3),
                MouseEventKind::ScrollDown => self.scrollback = self.scrollback.saturating_sub(3),
                _ => {}
            }
            return Ok(false);
        }
        if let Event::Paste(text) = event {
            // Terminals report pasted line breaks as `\r`.
            self.input.push_str(&text.replace('\r', "\n"));
            return Ok(false);
        }
        let Event::Key(key) = event else {
            return Ok(false);
        };
        if key.kind != KeyEventKind::Press {
            return Ok(false);
        }
        match (key.modifiers, key.code) {
            (KeyModifiers::CONTROL, KeyCode::Char('c') | KeyCode::Char('d')) => return Ok(true),
            (KeyModifiers::CONTROL, KeyCode::Char('u')) => self.input.clear(),
            // Release the mouse so the terminal's native text selection works,
            // at the cost of wheel scrolling; toggle back when done.
            (KeyModifiers::CONTROL, KeyCode::Char('t')) => {
                self.mouse_captured = !self.mouse_captured;
                let _ = if self.mouse_captured {
                    crossterm::execute!(std::io::stdout(), EnableMouseCapture)
                } else {
                    crossterm::execute!(std::io::stdout(), DisableMouseCapture)
                };
            }
            (KeyModifiers::ALT, KeyCode::Enter) => self.input.push('\n'),
            (_, KeyCode::Enter) => return self.submit_input(tx).await,
            (_, KeyCode::Backspace) => {
                self.input.pop();
            }
            (_, KeyCode::Up) => self.history_step(-1),
            (_, KeyCode::Down) => self.history_step(1),
            (_, KeyCode::PageUp) => {
                self.scrollback = self.scrollback.saturating_add(10);
            }
            (_, KeyCode::PageDown) => {
                self.scrollback = self.scrollback.saturating_sub(10);
            }
            (_, KeyCode::Esc) => self.scrollback = 0,
            (_, KeyCode::Char(ch)) => self.input.push(ch),
            _ => {}
        }
        Ok(false)
    }

    /// Returns true when the app should exit.
    async fn submit_input(&mut self, tx: &mpsc::UnboundedSender<AppEvent>) -> Result<bool> {
        let line = std::mem::take(&mut self.input);
        self.history_pos = None;
        // The bare help listing is ours, not clap's CLI-shaped help screen.
        if matches!(line.trim(), "/" | "/help") {
            for rendered in command_help_lines() {
                self.push_notice(&rendered);
            }
            return Ok(false);
        }
        match parse_repl_input(&line) {
            Ok(ReplInput::Empty) => {}
            Ok(ReplInput::Chat(text)) => {
                if self.is_busy() {
                    self.push_notice("still waiting on the previous turn");
                    self.input = line;
                    return Ok(false);
                }
                self.input_history.push(line);
                self.push_user_line(&text);
                self.start_send(text, tx.clone());
            }
            Ok(ReplInput::Command(command)) => {
                self.input_history.push(line);
                return self.execute_command(command, tx).await;
            }
            Err(error) => {
                for rendered in clap_error_lines(&error) {
                    self.push_notice(&rendered);
                }
            }
        }
        Ok(false)
    }

    /// Returns true when the app should exit.
    async fn execute_command(
        &mut self,
        command: ReplCommand,
        tx: &mpsc::UnboundedSender<AppEvent>,
    ) -> Result<bool> {
        // Sandbox and model operations run on a worker task so the UI keeps
        // drawing; one at a time keeps their output readable.
        if self.is_busy() && !matches!(command, ReplCommand::Quit) {
            self.push_notice("still waiting on the previous operation");
            return Ok(false);
        }
        let conversation = Arc::clone(&self.conversation);
        match command {
            ReplCommand::Quit => return Ok(true),
            ReplCommand::Verbosity { level } => match level {
                Some(verbosity) => {
                    self.verbosity = verbosity;
                    self.push_notice(&format!("verbosity set to {verbosity}"));
                    self.load_transcript().await?;
                }
                None => {
                    let verbosity = self.verbosity;
                    self.push_notice(&format!("verbosity: {verbosity}"));
                }
            },
            ReplCommand::Cost => {
                self.spawn_command(tx, async move { cost_lines(conversation.as_ref()).await })
            }
            ReplCommand::Shell { command } => {
                let agent = Arc::clone(&self.agent);
                self.spawn_command(tx, async move {
                    shell_lines(agent.as_ref(), conversation.as_ref(), command).await
                });
            }
            ReplCommand::Snapshot { sandbox_id } => self.spawn_command(tx, async move {
                snapshot_lines(conversation.as_ref(), sandbox_id).await
            }),
            ReplCommand::Snapshots => {
                self.spawn_command(
                    tx,
                    async move { snapshots_lines(conversation.as_ref()).await },
                )
            }
            ReplCommand::Rewind { snapshot_id } => self.spawn_command(tx, async move {
                rewind_lines(conversation.as_ref(), &snapshot_id).await
            }),
            ReplCommand::Teleport { provider } => self.spawn_command(tx, async move {
                teleport_lines(conversation.as_ref(), &provider).await
            }),
        }
        Ok(false)
    }

    /// Run a slash command on a worker task, feeding its finished output back
    /// through the app-event channel.
    fn spawn_command(
        &mut self,
        tx: &mpsc::UnboundedSender<AppEvent>,
        work: impl Future<Output = Vec<String>> + Send + 'static,
    ) {
        self.busy.store(true, Ordering::Relaxed);
        let tx = tx.clone();
        tokio::spawn(async move {
            let lines = work.await;
            let _ = tx.send(AppEvent::CommandOutput(lines));
            let _ = tx.send(AppEvent::StreamDone);
        });
    }

    fn start_send(&mut self, text: String, tx: mpsc::UnboundedSender<AppEvent>) {
        self.busy.store(true, Ordering::Relaxed);
        self.open_assistant = None;
        self.assistant_prefixed = false;
        self.open_calls.clear();
        let conversation = Arc::clone(&self.conversation);
        let session_id = self.session_id;
        tokio::spawn(async move {
            let request = SendRequest {
                input: vec![Message::User {
                    content: UserContent::String(text),
                }],
                session_id,
            };
            match conversation.send_stream(request).await {
                Ok(mut stream) => {
                    while let Some(event) = stream.next().await {
                        let app_event = match event {
                            Ok(event) => AppEvent::Stream(event),
                            Err(error) => AppEvent::StreamError(format!("{error:#}")),
                        };
                        if tx.send(app_event).is_err() {
                            return;
                        }
                    }
                }
                Err(error) => {
                    let _ = tx.send(AppEvent::StreamError(format!("{error:#}")));
                }
            }
            let _ = tx.send(AppEvent::StreamDone);
        });
    }

    fn handle_app_event(&mut self, event: AppEvent) {
        match event {
            AppEvent::Stream(event) => self.handle_stream_event(event),
            AppEvent::StreamError(error) => self.push_notice(&format!("stream error: {error}")),
            AppEvent::StreamDone => {
                self.busy.store(false, Ordering::Relaxed);
                self.open_assistant = None;
                self.assistant_prefixed = false;
            }
            AppEvent::CommandOutput(lines) => {
                for line in lines {
                    // Tabs come from table-shaped output; ratatui renders
                    // them poorly.
                    self.transcript
                        .push(style_transcript_line(line.replace('\t', "    ")));
                }
            }
            AppEvent::External(lines) => {
                for rendered in lines {
                    for line in rendered.split('\n') {
                        self.transcript
                            .push(style_transcript_line(line.to_string()));
                    }
                }
            }
        }
    }

    fn handle_stream_event(&mut self, event: ExecutionStreamEvent) {
        match event {
            ExecutionStreamEvent::FirstChunk { .. } => {}
            ExecutionStreamEvent::Chunk(chunk) => {
                let text = chunk_text(&chunk);
                if !text.is_empty() {
                    self.append_assistant_text(&text);
                }
            }
            ExecutionStreamEvent::ToolCall {
                tool_call_id,
                tool_name,
                arguments,
            } => {
                self.open_assistant = None;
                self.assistant_prefixed = false;
                if let Some(rendered) = render_tool_call(&tool_name, &arguments, self.verbosity) {
                    for (index, line) in rendered.lines().enumerate() {
                        self.transcript
                            .push(style_transcript_line(line.to_string()));
                        if index == 0 && self.verbosity == Verbosity::Compact {
                            self.open_calls
                                .insert(tool_call_id.clone(), self.transcript.len() - 1);
                        }
                    }
                }
                self.pending_tool_names.insert(tool_call_id, tool_name);
            }
            ExecutionStreamEvent::ToolResult {
                tool_call_id,
                result,
            } => {
                let tool_name = self
                    .pending_tool_names
                    .remove(&tool_call_id)
                    .unwrap_or_else(|| "tool".to_string());
                if let Some(index) = self.open_calls.remove(&tool_call_id) {
                    let status = compact_result_status(&result);
                    self.transcript[index]
                        .spans
                        .push(Span::styled(format!(" {status}"), status_style(&status)));
                } else if let Some(rendered) =
                    render_tool_result(&tool_name, &result, self.verbosity)
                {
                    for line in rendered.lines() {
                        self.transcript
                            .push(style_transcript_line(line.to_string()));
                    }
                }
            }
            ExecutionStreamEvent::Completed(result) => {
                self.session_id = Some(result.session_id);
                *self.watch_after.lock().expect("watch cursor poisoned") =
                    Some(result.latest_event_id);
            }
        }
    }

    fn append_assistant_text(&mut self, text: &str) {
        for (index, piece) in text.split('\n').enumerate() {
            if index > 0 {
                self.open_assistant = None;
            }
            if piece.is_empty() {
                continue;
            }
            let line_index = match self.open_assistant {
                Some(line_index) => line_index,
                None => {
                    // Only the first line of a message carries the prefix;
                    // continuation lines stay bare.
                    let prefix = if self.assistant_prefixed {
                        String::new()
                    } else {
                        self.assistant_prefixed = true;
                        format!("{} {ASSISTANT_LABEL}: ", compact_timestamp())
                    };
                    self.transcript.push(Line::from(vec![
                        Span::styled(prefix, Style::new().dim()),
                        Span::raw(String::new()),
                    ]));
                    let line_index = self.transcript.len() - 1;
                    self.open_assistant = Some(line_index);
                    line_index
                }
            };
            if let Some(span) = self.transcript[line_index].spans.last_mut() {
                span.content.to_mut().push_str(piece);
            }
        }
    }

    fn push_user_line(&mut self, text: &str) {
        // Ratatui lines cannot hold newlines; multi-line messages become one
        // transcript line each, with the prefix only on the first.
        for (index, piece) in text.split('\n').enumerate() {
            let mut spans = Vec::new();
            if index == 0 {
                spans.push(Span::styled(
                    format!("{} user: ", compact_timestamp()),
                    Style::new().dim(),
                ));
            }
            spans.push(Span::styled(piece.to_string(), Style::new().bold()));
            self.transcript.push(Line::from(spans));
        }
        self.scrollback = 0;
    }

    fn push_notice(&mut self, text: &str) {
        self.transcript
            .push(Line::styled(text.to_string(), Style::new().dim().italic()));
    }

    fn is_busy(&self) -> bool {
        self.busy.load(Ordering::Relaxed)
    }

    fn history_step(&mut self, direction: isize) {
        if self.input_history.is_empty() {
            return;
        }
        let last = self.input_history.len() - 1;
        let next = match (self.history_pos, direction) {
            (None, -1) => Some(last),
            (None, _) => None,
            (Some(0), -1) => Some(0),
            (Some(pos), -1) => Some(pos - 1),
            (Some(pos), 1) if pos < last => Some(pos + 1),
            (Some(_), 1) => {
                self.history_pos = None;
                self.input.clear();
                return;
            }
            (pos, _) => pos,
        };
        if let Some(pos) = next {
            self.history_pos = Some(pos);
            self.input = self.input_history[pos].clone();
        }
    }

    fn spawn_event_watcher(
        &self,
        tx: mpsc::UnboundedSender<AppEvent>,
    ) -> tokio::task::JoinHandle<()> {
        let conversation = self.conversation.exoharness_handle();
        let watch_after = Arc::clone(&self.watch_after);
        let busy = Arc::clone(&self.busy);
        let verbosity = self.verbosity;
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(1));
            loop {
                interval.tick().await;
                // While a turn streams, the watcher's cursor lags the events
                // being written; polling now would replay messages the
                // streaming path already rendered. `Completed` advances the
                // cursor before busy clears.
                if busy.load(Ordering::Relaxed) {
                    continue;
                }
                let cursor = *watch_after.lock().expect("watch cursor poisoned");
                let Ok(result) = conversation
                    .get_events(Some(EventQuery {
                        cursor,
                        direction: Some(EventQueryDirection::Asc),
                        limit: Some(100),
                        session_id: None,
                        turn_id: None,
                        types: None,
                    }))
                    .await
                else {
                    return;
                };
                for event in result.events {
                    *watch_after.lock().expect("watch cursor poisoned") = Some(event.id);
                    let lines = render_external_event(&event.data, verbosity);
                    if !lines.is_empty() && tx.send(AppEvent::External(lines)).is_err() {
                        return;
                    }
                }
            }
        })
    }

    fn draw(&mut self, frame: &mut Frame) {
        // The input box wraps and grows with its content (bounded), so its
        // height must be known before the layout is split.
        let input_width = usize::from(frame.area().width.saturating_sub(2)).max(1);
        let input_lines = wrap_input_chars(&self.input, input_width);
        let input_height = (input_lines.len() as u16).min(MAX_INPUT_ROWS) + 2;

        let [transcript_area, input_area, status_area] = Layout::vertical([
            Constraint::Min(1),
            Constraint::Length(input_height),
            Constraint::Length(1),
        ])
        .areas(frame.area());

        let title = format!(
            " {} · {} ",
            self.agent.record().slug,
            self.conversation.record().slug
        );
        // Count lines before attaching the block: line_count includes the
        // block's border rows, which would over-scroll by two.
        let transcript =
            Paragraph::new(Text::from(self.transcript.clone())).wrap(Wrap { trim: false });
        let inner_width = transcript_area.width.saturating_sub(2);
        let inner_height = transcript_area.height.saturating_sub(2);
        let total = transcript.line_count(inner_width) as u16;
        let bottom = total.saturating_sub(inner_height);
        self.scrollback = self.scrollback.min(bottom);
        let scroll = bottom - self.scrollback;
        frame.render_widget(
            transcript
                .scroll((scroll, 0))
                .block(Block::new().borders(Borders::ALL).title(title)),
            transcript_area,
        );

        // Once the input outgrows its bounded height, scroll vertically so
        // the cursor row (always the last line) stays visible.
        let cursor_col = input_lines.last().map_or(0, |line| line.chars().count()) as u16;
        let total_rows = input_lines.len() as u16;
        let input_scroll = total_rows.saturating_sub(MAX_INPUT_ROWS);
        let input = Paragraph::new(Text::from(
            input_lines.into_iter().map(Line::from).collect::<Vec<_>>(),
        ))
        .scroll((input_scroll, 0))
        .block(
            Block::new()
                .borders(Borders::ALL)
                .title(" message or /command "),
        );
        frame.render_widget(input, input_area);
        frame.set_cursor_position(Position::new(
            input_area.x + 1 + cursor_col,
            input_area.y + 1 + (total_rows - 1 - input_scroll),
        ));

        let state = if self.is_busy() {
            format!("{} thinking…", SPINNER[self.spinner_frame % SPINNER.len()])
        } else {
            "idle".to_string()
        };
        let mouse = if self.mouse_captured {
            "Ctrl+T to select text"
        } else {
            "mouse released: select text freely, Ctrl+T to restore scrolling"
        };
        let status = Line::from(format!(
            " {state} · verbosity {} · wheel/PgUp scroll · ↑↓ history · Esc follow · {mouse} · Ctrl+C quit",
            self.verbosity
        ))
        .style(Style::new().dim());
        frame.render_widget(Paragraph::new(status), status_area);
    }
}

/// Basic styling for pre-rendered transcript strings, keyed off the line
/// shape the render module produces.
fn style_transcript_line(line: String) -> Line<'static> {
    let style = if line.starts_with('→') || line.starts_with('←') {
        Style::new().cyan()
    } else if line.contains("] user: ") {
        Style::new().bold()
    } else {
        Style::new()
    };
    Line::styled(line, style)
}

fn status_style(status: &str) -> Style {
    if status.starts_with('✓') {
        Style::new().green()
    } else {
        Style::new().red()
    }
}

#[cfg(test)]
mod tests {
    use super::{ReplCommand, ReplInput, parse_repl_input};
    use crate::render::Verbosity;

    #[test]
    fn parses_blank_and_chat_input() {
        assert_eq!(parse_repl_input("   ").unwrap(), ReplInput::Empty);
        assert_eq!(
            parse_repl_input(" hello there ").unwrap(),
            ReplInput::Chat("hello there".to_string())
        );
    }

    #[test]
    fn double_slash_escapes_to_slash_prefixed_chat() {
        assert_eq!(
            parse_repl_input("//quit is a command").unwrap(),
            ReplInput::Chat("/quit is a command".to_string())
        );
    }

    #[test]
    fn parses_commands_and_aliases() {
        assert_eq!(
            parse_repl_input("/quit").unwrap(),
            ReplInput::Command(ReplCommand::Quit)
        );
        assert_eq!(
            parse_repl_input("/exit").unwrap(),
            ReplInput::Command(ReplCommand::Quit)
        );
        assert_eq!(
            parse_repl_input("/usage").unwrap(),
            ReplInput::Command(ReplCommand::Cost)
        );
        assert_eq!(
            parse_repl_input("/verbosity full").unwrap(),
            ReplInput::Command(ReplCommand::Verbosity {
                level: Some(Verbosity::Full)
            })
        );
        assert_eq!(
            parse_repl_input("/snapshot abc").unwrap(),
            ReplInput::Command(ReplCommand::Snapshot {
                sandbox_id: Some("abc".to_string())
            })
        );
    }

    #[test]
    fn shell_source_is_preserved_verbatim() {
        let input = r#"/shell echo "a  b" | grep 'a' > /tmp/out"#;
        assert_eq!(
            parse_repl_input(input).unwrap(),
            ReplInput::Command(ReplCommand::Shell {
                command: r#"echo "a  b" | grep 'a' > /tmp/out"#.to_string()
            })
        );
        assert_eq!(
            parse_repl_input("/sandbox ls -la").unwrap(),
            ReplInput::Command(ReplCommand::Shell {
                command: "ls -la".to_string()
            })
        );
    }

    #[test]
    fn shell_without_source_is_a_usage_error() {
        let error = parse_repl_input("/shell").unwrap_err();
        assert_eq!(
            error.kind(),
            clap::error::ErrorKind::MissingRequiredArgument
        );
    }

    #[test]
    fn unknown_commands_error_instead_of_reaching_the_model() {
        let error = parse_repl_input("/snapshoot abc").unwrap_err();
        assert_eq!(error.kind(), clap::error::ErrorKind::InvalidSubcommand);
        assert!(error.to_string().contains("snapshot"), "suggests the fix");
    }

    #[test]
    fn extra_arguments_are_clap_errors() {
        assert!(parse_repl_input("/snapshot one two").is_err());
        assert!(parse_repl_input("/rewind").is_err());
    }

    #[test]
    fn help_is_generated_from_the_command_tree() {
        let error = parse_repl_input("/help").unwrap_err();
        assert_eq!(error.kind(), clap::error::ErrorKind::DisplayHelp);
        let rendered = error.to_string();
        assert!(rendered.contains("snapshot"));
        assert!(rendered.contains("teleport"));
    }
}
