use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};
use std::io::{self, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use clap::{CommandFactory, Parser};
use executor::{
    ConversationHandle, EventData, EventId, EventKind, EventQuery, EventQueryDirection,
    ExecutionStreamEvent, HarnessAgent, HarnessConversation, SandboxId, SandboxProvider,
    SendRequest, SessionId, SnapshotId, StartSandboxRequest,
};
use lingua::universal::{UserContent, UserContentPart};
use lingua::{Message, UniversalStreamChunk};
use rustyline::error::ReadlineError;
use rustyline::history::{History, MemHistory, SearchDirection, SearchResult};
use rustyline::{Cmd, Config, Editor, ExternalPrinter, KeyCode, KeyEvent, Modifiers};
use tokio::runtime::Handle;
use tokio::task::JoinHandle;
use tokio_stream::StreamExt;

use crate::render::{
    Verbosity, compact_result_status, compact_timestamp, print_transcript,
    render_assistant_content, render_tool_call, render_tool_result,
};
use crate::run_sandbox_shell_command;

const DEFAULT_SHELL_PROGRAM: &str = "/bin/bash";
const REMOTE_HISTORY_BASE: usize = 1_000_000;
const REMOTE_HISTORY_PAGE_SIZE: u32 = 32;

// Slash-command registry: names, aliases, arguments, and help all live in
// this clap tree. `multicall` makes the first token the command name, and
// clap's built-in `help` subcommand serves `/help`.
#[derive(Debug, clap::Parser)]
#[command(name = "repl", multicall = true, about = "repl slash commands")]
struct ReplCli {
    #[command(subcommand)]
    command: ReplCommand,
}

#[derive(Debug, PartialEq, clap::Subcommand)]
enum ReplCommand {
    /// Exit the repl
    #[command(alias = "exit")]
    Quit,
    /// Reprint the conversation transcript
    History,
    /// Summarize token usage and dollar cost
    #[command(alias = "usage")]
    Cost,
    /// Show or set how much tool detail is printed
    Verbosity {
        #[arg(value_enum)]
        level: Option<Verbosity>,
    },
    /// Run a command in the conversation's sandbox
    #[command(alias = "sandbox")]
    Shell {
        /// Shell source, passed to the sandbox verbatim
        command: String,
    },
    /// Snapshot a sandbox in this conversation (defaults to the latest one)
    Snapshot { sandbox_id: Option<String> },
    /// List snapshots taken in this conversation
    Snapshots,
    /// Restore the sandbox to a previous snapshot
    Rewind { snapshot_id: String },
    /// Move the live sandbox to another provider (e.g. daytona: snapshot + restore there)
    Teleport { provider: String },
}

/// What a line of repl input means.
#[derive(Debug, PartialEq)]
enum ReplInput {
    Empty,
    /// Plain chat text for the model.
    Chat(String),
    Command(ReplCommand),
}

/// Whether the repl keeps reading input after a command.
enum ReplFlow {
    Continue,
    Quit,
}

fn parse_repl_input(line: &str) -> Result<ReplInput, clap::Error> {
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

pub async fn run_chat_repl(
    agent: Arc<dyn HarnessAgent>,
    conversation: Arc<dyn HarnessConversation>,
    verbosity: Verbosity,
) -> Result<()> {
    let mut repl = ChatRepl::new(agent, conversation, verbosity)?;
    repl.print_transcript().await?;
    repl.run().await
}

struct ChatHistory {
    state: Mutex<ChatHistoryState>,
}

struct ChatHistoryState {
    conversation: Arc<dyn ConversationHandle>,
    remote_cutoff: Option<EventId>,
    remote_cursor: Option<EventId>,
    remote_entries: Vec<String>,
    remote_exhausted: bool,
    local_history: MemHistory,
}

impl ChatHistory {
    fn new(conversation: Arc<dyn ConversationHandle>, remote_cutoff: Option<EventId>) -> Self {
        Self {
            state: Mutex::new(ChatHistoryState {
                conversation,
                remote_cutoff,
                remote_cursor: None,
                remote_entries: Vec::new(),
                remote_exhausted: false,
                local_history: MemHistory::with_config(Config::default()),
            }),
        }
    }

    fn remote_enabled(state: &ChatHistoryState) -> bool {
        state.remote_cutoff.is_some()
    }

    fn remote_len_sentinel(state: &ChatHistoryState) -> usize {
        if Self::remote_enabled(state) {
            REMOTE_HISTORY_BASE
        } else {
            0
        }
    }

    fn history_len(state: &ChatHistoryState) -> usize {
        Self::remote_len_sentinel(state).saturating_add(state.local_history.len())
    }

    fn ensure_remote_position_loaded(&self, remote_position: usize) -> rustyline::Result<()> {
        loop {
            let (conversation, cursor, cutoff, loaded_len, exhausted, enabled) = {
                let state = self.state.lock().expect("chat history poisoned");
                (
                    Arc::clone(&state.conversation),
                    state.remote_cursor,
                    state.remote_cutoff,
                    state.remote_entries.len(),
                    state.remote_exhausted,
                    Self::remote_enabled(&state),
                )
            };

            if !enabled || loaded_len > remote_position || exhausted {
                return Ok(());
            }

            let page = fetch_remote_user_messages(&conversation, cursor, cutoff)?;
            let mut state = self.state.lock().expect("chat history poisoned");
            state.remote_cursor = page.cursor;
            if page.cursor.is_none() {
                state.remote_exhausted = true;
            }
            if page.entries.is_empty() {
                if state.remote_exhausted {
                    return Ok(());
                }
                continue;
            }
            state.remote_entries.extend(page.entries);
        }
    }

    fn get_entry(
        &self,
        index: usize,
        dir: SearchDirection,
    ) -> rustyline::Result<Option<SearchResult<'_>>> {
        let remote_len_sentinel = {
            let state = self.state.lock().expect("chat history poisoned");
            Self::remote_len_sentinel(&state)
        };

        if index >= remote_len_sentinel {
            let state = self.state.lock().expect("chat history poisoned");
            let local_index = index - remote_len_sentinel;
            return Ok(state
                .local_history
                .get(local_index, dir)?
                .map(|result| SearchResult {
                    entry: Cow::Owned(result.entry.into_owned()),
                    idx: result.idx,
                    pos: result.pos,
                }));
        }

        let remote_position = remote_len_sentinel - 1 - index;
        self.ensure_remote_position_loaded(remote_position)?;

        let state = self.state.lock().expect("chat history poisoned");
        Ok(state
            .remote_entries
            .get(remote_position)
            .cloned()
            .map(|entry| SearchResult {
                entry: Cow::Owned(entry),
                idx: index,
                pos: 0,
            }))
    }
}

impl History for ChatHistory {
    fn get(
        &self,
        index: usize,
        dir: SearchDirection,
    ) -> rustyline::Result<Option<SearchResult<'_>>> {
        self.get_entry(index, dir)
    }

    fn add(&mut self, line: &str) -> rustyline::Result<bool> {
        self.add_owned(line.to_string())
    }

    fn add_owned(&mut self, line: String) -> rustyline::Result<bool> {
        let mut state = self.state.lock().expect("chat history poisoned");
        state.local_history.add_owned(line)
    }

    fn len(&self) -> usize {
        let state = self.state.lock().expect("chat history poisoned");
        Self::history_len(&state)
    }

    fn is_empty(&self) -> bool {
        let state = self.state.lock().expect("chat history poisoned");
        !Self::remote_enabled(&state) && state.local_history.is_empty()
    }

    fn set_max_len(&mut self, len: usize) -> rustyline::Result<()> {
        let mut state = self.state.lock().expect("chat history poisoned");
        state.local_history.set_max_len(len)
    }

    fn ignore_dups(&mut self, yes: bool) -> rustyline::Result<()> {
        let mut state = self.state.lock().expect("chat history poisoned");
        state.local_history.ignore_dups(yes)
    }

    fn ignore_space(&mut self, yes: bool) {
        let mut state = self.state.lock().expect("chat history poisoned");
        state.local_history.ignore_space(yes);
    }

    fn save(&mut self, path: &Path) -> rustyline::Result<()> {
        let mut state = self.state.lock().expect("chat history poisoned");
        state.local_history.save(path)
    }

    fn append(&mut self, path: &Path) -> rustyline::Result<()> {
        let mut state = self.state.lock().expect("chat history poisoned");
        state.local_history.append(path)
    }

    fn load(&mut self, path: &Path) -> rustyline::Result<()> {
        let mut state = self.state.lock().expect("chat history poisoned");
        state.local_history.load(path)
    }

    fn clear(&mut self) -> rustyline::Result<()> {
        let mut state = self.state.lock().expect("chat history poisoned");
        state.remote_cursor = None;
        state.remote_entries.clear();
        state.remote_exhausted = false;
        state.local_history.clear()
    }

    fn search(
        &self,
        term: &str,
        start: usize,
        dir: SearchDirection,
    ) -> rustyline::Result<Option<SearchResult<'_>>> {
        let mut index = start;
        loop {
            let Some(result) = self.get(index, dir)? else {
                return Ok(None);
            };
            if let Some(pos) = result.entry.find(term) {
                return Ok(Some(SearchResult {
                    entry: result.entry,
                    idx: result.idx,
                    pos,
                }));
            }
            match dir {
                SearchDirection::Forward => {
                    index += 1;
                    if index >= self.len() {
                        return Ok(None);
                    }
                }
                SearchDirection::Reverse => {
                    if index == 0 {
                        return Ok(None);
                    }
                    index -= 1;
                }
            }
        }
    }

    fn starts_with(
        &self,
        term: &str,
        start: usize,
        dir: SearchDirection,
    ) -> rustyline::Result<Option<SearchResult<'_>>> {
        let mut index = start;
        loop {
            let Some(result) = self.get(index, dir)? else {
                return Ok(None);
            };
            if result.entry.starts_with(term) {
                return Ok(Some(SearchResult {
                    entry: result.entry,
                    idx: result.idx,
                    pos: 0,
                }));
            }
            match dir {
                SearchDirection::Forward => {
                    index += 1;
                    if index >= self.len() {
                        return Ok(None);
                    }
                }
                SearchDirection::Reverse => {
                    if index == 0 {
                        return Ok(None);
                    }
                    index -= 1;
                }
            }
        }
    }
}

struct RemoteHistoryPage {
    entries: Vec<String>,
    cursor: Option<EventId>,
}

fn fetch_remote_user_messages(
    conversation: &Arc<dyn ConversationHandle>,
    cursor: Option<EventId>,
    cutoff: Option<EventId>,
) -> rustyline::Result<RemoteHistoryPage> {
    let runtime = Handle::try_current().map_err(|error| {
        ReadlineError::Io(io::Error::other(format!(
            "failed to access tokio runtime for chat history: {error}"
        )))
    })?;
    let result = tokio::task::block_in_place(|| {
        runtime.block_on(conversation.get_events(Some(EventQuery {
            cursor,
            direction: Some(EventQueryDirection::Desc),
            limit: Some(REMOTE_HISTORY_PAGE_SIZE),
            session_id: None,
            turn_id: None,
            types: None,
        })))
    })
    .map_err(|error| {
        ReadlineError::Io(io::Error::other(format!(
            "failed to fetch chat history events: {error}"
        )))
    })?;

    let mut entries = Vec::new();
    for event in result.events {
        if cutoff.is_some_and(|event_id| event.id > event_id) {
            continue;
        }
        if let EventData::Messages { messages, .. } = event.data {
            for message in messages.into_iter().rev() {
                if let Message::User { content } = message {
                    let text = render_user_content_for_history(&content);
                    if !text.trim().is_empty() {
                        entries.push(text);
                    }
                }
            }
        }
    }

    Ok(RemoteHistoryPage {
        entries,
        cursor: result.cursor,
    })
}

struct ChatRepl {
    agent: Arc<dyn HarnessAgent>,
    conversation: Arc<dyn HarnessConversation>,
    editor: Editor<(), ChatHistory>,
    session_id: Option<SessionId>,
    watch_after: Arc<Mutex<Option<EventId>>>,
    verbosity: Verbosity,
}

impl ChatRepl {
    fn new(
        agent: Arc<dyn HarnessAgent>,
        conversation: Arc<dyn HarnessConversation>,
        verbosity: Verbosity,
    ) -> Result<Self> {
        let latest_event_id = conversation.record().latest_event_id;
        let history = ChatHistory::new(conversation.exoharness_handle(), latest_event_id);
        let mut editor = Editor::with_history(Config::default(), history)?;
        editor.bind_sequence(KeyEvent(KeyCode::Enter, Modifiers::ALT), Cmd::Newline);
        Ok(Self {
            agent,
            conversation,
            editor,
            session_id: None,
            watch_after: Arc::new(Mutex::new(latest_event_id)),
            verbosity,
        })
    }

    async fn print_transcript(&self) -> Result<()> {
        let messages = self.conversation.messages().await?;
        print_transcript(&messages, self.verbosity);
        Ok(())
    }

    /// Summarize token usage and dollar cost for this conversation from the
    /// `usage` records on its `messages` events. Paginates so it covers the
    /// whole conversation, not just one page.
    async fn print_cost(&self) -> Result<()> {
        let handle = self.conversation.exoharness_handle();
        let mut cursor: Option<EventId> = None;
        let mut per_model: BTreeMap<String, ModelCost> = BTreeMap::new();
        let mut unpriced = 0usize;
        loop {
            let result = handle
                .get_events(Some(EventQuery {
                    cursor,
                    direction: Some(EventQueryDirection::Desc),
                    limit: Some(REMOTE_HISTORY_PAGE_SIZE),
                    session_id: None,
                    turn_id: None,
                    types: Some(vec![EventKind::MESSAGES]),
                }))
                .await?;
            for event in &result.events {
                if let EventData::Messages {
                    usage: Some(usage), ..
                } = &event.data
                {
                    let entry = per_model.entry(usage.model.clone()).or_default();
                    entry.calls += 1;
                    entry.prompt += usage.prompt_tokens.unwrap_or(0);
                    entry.cached += usage.prompt_cached_tokens.unwrap_or(0);
                    entry.completion += usage.completion_tokens.unwrap_or(0);
                    match usage.cost_usd {
                        Some(cost) => entry.cost += cost,
                        None => unpriced += 1,
                    }
                }
            }
            match result.cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }

        if per_model.is_empty() {
            println!("no recorded model usage in this conversation yet");
            return Ok(());
        }

        let mut total = ModelCost::default();
        println!(
            "{:<28} {:>6} {:>10} {:>10} {:>10} {:>12}",
            "MODEL", "CALLS", "PROMPT", "CACHED", "OUT", "COST"
        );
        for (model, c) in &per_model {
            println!(
                "{:<28} {:>6} {:>10} {:>10} {:>10} {:>12}",
                truncate(model, 28),
                c.calls,
                c.prompt,
                c.cached,
                c.completion,
                fmt_usd(c.cost),
            );
            total.calls += c.calls;
            total.prompt += c.prompt;
            total.cached += c.cached;
            total.completion += c.completion;
            total.cost += c.cost;
        }
        println!(
            "{:<28} {:>6} {:>10} {:>10} {:>10} {:>12}",
            "TOTAL",
            total.calls,
            total.prompt,
            total.cached,
            total.completion,
            fmt_usd(total.cost),
        );
        if unpriced > 0 {
            println!(
                "note: {unpriced} call(s) had no price (model not in the price table); cost excludes them"
            );
        }
        Ok(())
    }

    async fn run(&mut self) -> Result<()> {
        loop {
            let prompt = format!("{}> ", self.conversation.record().slug);
            let event_printer = self.spawn_event_printer()?;
            let readline_result = self.editor.readline(&prompt);
            event_printer.abort();
            let _ = event_printer.await;

            match readline_result {
                Ok(line) => match parse_repl_input(&line) {
                    Ok(ReplInput::Empty) => continue,
                    Ok(ReplInput::Chat(text)) => {
                        self.editor.add_history_entry(line.as_str())?;
                        self.send(&text).await?;
                    }
                    Ok(ReplInput::Command(command)) => {
                        if matches!(command, ReplCommand::Shell { .. }) {
                            self.editor.add_history_entry(line.as_str())?;
                        }
                        match self.execute_command(command).await? {
                            ReplFlow::Continue => {}
                            ReplFlow::Quit => break,
                        }
                    }
                    Err(error) => {
                        let _ = error.print();
                    }
                },
                Err(ReadlineError::Interrupted) => {
                    println!();
                    continue;
                }
                Err(ReadlineError::Eof) => {
                    println!();
                    break;
                }
                Err(error) => return Err(error.into()),
            }
        }

        if let Some(session_id) = self.session_id.take() {
            self.conversation.close_session(session_id).await?;
        }

        Ok(())
    }

    async fn execute_command(&mut self, command: ReplCommand) -> Result<ReplFlow> {
        match command {
            ReplCommand::Quit => return Ok(ReplFlow::Quit),
            ReplCommand::History => self.print_transcript().await?,
            ReplCommand::Cost => {
                if let Err(error) = self.print_cost().await {
                    println!("cost summary failed: {error:#}");
                }
            }
            ReplCommand::Verbosity { level } => match level {
                Some(verbosity) => {
                    self.verbosity = verbosity;
                    println!("verbosity set to {verbosity}");
                }
                None => println!("verbosity: {}", self.verbosity),
            },
            ReplCommand::Shell { command } => self.run_shell(&command).await?,
            ReplCommand::Snapshot { sandbox_id } => match self.snapshot_sandbox(sandbox_id).await {
                Ok(snapshot_id) => println!("snapshot {snapshot_id}"),
                Err(error) => println!("snapshot failed: {error:#}"),
            },
            ReplCommand::Snapshots => match self.list_snapshots().await {
                Ok(snapshots) if snapshots.is_empty() => {
                    println!("no snapshots yet for this conversation");
                }
                Ok(snapshots) => {
                    println!("SNAPSHOT\tTAKEN\tSANDBOX");
                    for (snapshot_id, sandbox_id) in snapshots {
                        // Snapshot ids are uuid7, so creation time is embedded
                        // in the id itself.
                        let taken = snapshot_id
                            .timestamp()
                            .map(|t| t.format("%Y-%m-%d %H:%M:%S UTC").to_string())
                            .unwrap_or_else(|| "-".to_string());
                        println!("{snapshot_id}\t{taken}\t{sandbox_id}");
                    }
                }
                Err(error) => println!("listing snapshots failed: {error:#}"),
            },
            ReplCommand::Rewind { snapshot_id } => {
                match self.rewind_to_snapshot(&snapshot_id).await {
                    Ok(()) => println!("rewound to snapshot {snapshot_id}"),
                    Err(error) => println!("rewind failed: {error:#}"),
                }
            }
            ReplCommand::Teleport { provider } => match self.teleport_sandbox(&provider).await {
                Ok((sandbox_id, provider)) => {
                    println!("sandbox {sandbox_id} teleported to {provider}");
                }
                Err(error) => println!("teleport failed: {error:#}"),
            },
        }
        Ok(ReplFlow::Continue)
    }

    async fn snapshot_sandbox(&self, explicit_id: Option<SandboxId>) -> Result<SnapshotId> {
        let sandbox_id = match explicit_id {
            Some(id) => id,
            None => latest_sandbox_id(self.conversation.as_ref())
                .await?
                .ok_or_else(|| {
                    anyhow::anyhow!("no sandbox has been created in this conversation yet")
                })?,
        };
        let id = self
            .conversation
            .exoharness_handle()
            .snapshot_sandbox(sandbox_id)
            .await?;
        Ok(id)
    }

    async fn list_snapshots(&self) -> Result<Vec<(SnapshotId, SandboxId)>> {
        list_snapshots(self.conversation.as_ref()).await
    }

    /// Teleport the conversation's live sandbox to another provider: snapshot
    /// it where it runs now, then restore that snapshot under the target
    /// provider. The sandbox id is stable across the move; only the backend
    /// (and the machine actually running the container) changes.
    async fn teleport_sandbox(&self, provider_str: &str) -> Result<(SandboxId, SandboxProvider)> {
        let provider = provider_str
            .parse::<SandboxProvider>()
            .map_err(|error| anyhow::anyhow!("invalid provider `{provider_str}`: {error}"))?;
        let sandbox_id = latest_sandbox_id(self.conversation.as_ref())
            .await?
            .ok_or_else(|| {
                anyhow::anyhow!("no sandbox has been created in this conversation yet")
            })?;
        println!("snapshotting sandbox {sandbox_id}...");
        let snapshot_id = self
            .conversation
            .exoharness_handle()
            .snapshot_sandbox(sandbox_id.clone())
            .await?;
        println!("snapshot {snapshot_id} captured; restoring on {provider}...");
        self.conversation
            .exoharness_handle()
            .start_sandbox(StartSandboxRequest {
                id: sandbox_id.clone(),
                snapshot_id,
                idle_seconds: None,
                provider: Some(provider),
            })
            .await?;
        Ok((sandbox_id, provider))
    }

    /// Restore the conversation's sandbox to a previously-taken snapshot.
    /// Stops the current container, decodes the snapshot payload, and starts
    /// a fresh container from that state.
    async fn rewind_to_snapshot(&self, snapshot_id_str: &str) -> Result<()> {
        let snapshot_id = snapshot_id_str
            .parse::<SnapshotId>()
            .map_err(|error| anyhow::anyhow!("invalid snapshot id `{snapshot_id_str}`: {error}"))?;
        let sandbox_id = sandbox_id_for_snapshot(self.conversation.as_ref(), snapshot_id)
            .await?
            .ok_or_else(|| {
                anyhow::anyhow!("snapshot {snapshot_id} not found in this conversation")
            })?;
        self.conversation
            .exoharness_handle()
            .start_sandbox(StartSandboxRequest {
                id: sandbox_id,
                snapshot_id,
                idle_seconds: None,
                provider: None,
            })
            .await?;
        Ok(())
    }

    async fn send(&mut self, input: &str) -> Result<()> {
        let mut stream = self
            .conversation
            .send_stream(SendRequest {
                input: vec![Message::User {
                    content: UserContent::String(input.to_string()),
                }],
                session_id: self.session_id,
            })
            .await?;
        let mut stdout = io::stdout();
        let mut printed_assistant = false;
        let mut streamed_text = String::new();
        // Tool names by call id, so results (which only carry the id) can be
        // labeled in compact mode.
        let mut pending_tool_calls: HashMap<String, String> = HashMap::new();
        // Compact call line left open (no newline) so the matching result can
        // acknowledge receipt on the same line.
        let mut open_call: Option<String> = None;

        while let Some(event) = stream.next().await {
            match event? {
                ExecutionStreamEvent::FirstChunk { ttft } => {
                    if self.verbosity == Verbosity::Full {
                        print_ttft(ttft);
                    }
                }
                ExecutionStreamEvent::Chunk(chunk) => {
                    let text = chunk_text(&chunk);
                    if text.is_empty() {
                        continue;
                    }
                    if open_call.take().is_some() {
                        println!();
                    }
                    if !printed_assistant {
                        print!("{} assistant: ", compact_timestamp());
                        stdout.flush()?;
                        printed_assistant = true;
                    }
                    stdout.write_all(text.as_bytes())?;
                    stdout.flush()?;
                    streamed_text.push_str(&text);
                }
                ExecutionStreamEvent::ToolCall {
                    tool_call_id,
                    tool_name,
                    arguments,
                } => {
                    if open_call.take().is_some() {
                        println!();
                    }
                    if printed_assistant && !streamed_text.ends_with('\n') {
                        println!();
                        printed_assistant = false;
                        streamed_text.clear();
                    }
                    if let Some(rendered) = render_tool_call(&tool_name, &arguments, self.verbosity)
                    {
                        if self.verbosity == Verbosity::Compact {
                            print!("{rendered}");
                            stdout.flush()?;
                            open_call = Some(tool_call_id.clone());
                        } else {
                            println!("{rendered}");
                        }
                        printed_assistant = false;
                        streamed_text.clear();
                    }
                    pending_tool_calls.insert(tool_call_id, tool_name);
                }
                ExecutionStreamEvent::ToolResult {
                    tool_call_id,
                    result,
                } => {
                    let tool_name = pending_tool_calls
                        .remove(&tool_call_id)
                        .unwrap_or_else(|| "tool".to_string());
                    if open_call.as_deref() == Some(tool_call_id.as_str()) {
                        open_call = None;
                        println!(" {}", compact_result_status(&result));
                    } else if let Some(rendered) =
                        render_tool_result(&tool_name, &result, self.verbosity)
                    {
                        if open_call.take().is_some() {
                            println!();
                        }
                        println!("{rendered}");
                    }
                }
                ExecutionStreamEvent::Completed(result) => {
                    self.session_id = Some(result.session_id);
                    *self.watch_after.lock().expect("chat event watch poisoned") =
                        Some(result.latest_event_id);
                }
            }
        }
        if open_call.is_some() {
            println!();
        }

        if printed_assistant {
            println!();
        } else if let Some(last_message) = self.conversation.messages().await?.last().cloned()
            && let Message::Assistant { content, .. } = last_message
        {
            let rendered = render_assistant_content(&content, self.verbosity);
            if !rendered.is_empty() {
                println!("{} assistant: {}", compact_timestamp(), rendered);
            }
        }
        println!();
        Ok(())
    }

    fn spawn_event_printer(&mut self) -> Result<JoinHandle<()>> {
        let conversation = self.conversation.exoharness_handle();
        let watch_after = Arc::clone(&self.watch_after);
        let verbosity = self.verbosity;
        let mut printer = self.editor.create_external_printer()?;
        Ok(tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(1));
            loop {
                interval.tick().await;
                let cursor = *watch_after.lock().expect("chat event watch poisoned");
                match conversation
                    .get_events(Some(EventQuery {
                        cursor,
                        direction: Some(EventQueryDirection::Asc),
                        limit: Some(100),
                        session_id: None,
                        turn_id: None,
                        types: None,
                    }))
                    .await
                {
                    Ok(result) => {
                        for event in result.events {
                            *watch_after.lock().expect("chat event watch poisoned") =
                                Some(event.id);
                            for rendered in render_external_event(&event.data, verbosity) {
                                let _ = printer.print(format!("{rendered}\n"));
                            }
                        }
                    }
                    Err(error) => {
                        let _ = printer.print(format!("event watcher error: {error}\n"));
                        break;
                    }
                }
            }
        }))
    }

    async fn run_shell(&self, command: &str) -> Result<()> {
        let mut config = self.conversation.config().await?;
        if config.shell_program.is_none() {
            config.shell_program = Some(DEFAULT_SHELL_PROGRAM.to_string());
            self.conversation.put_config(config).await?;
        }
        let output = run_sandbox_shell_command(
            self.agent.as_ref(),
            self.conversation.as_ref(),
            command.to_string(),
        )
        .await?;
        io::stdout().write_all(output.stdout.as_bytes())?;
        io::stderr().write_all(output.stderr.as_bytes())?;
        if output.exit_code != 0 {
            println!("[exit {}]", output.exit_code);
        }
        Ok(())
    }
}

fn chunk_text(chunk: &UniversalStreamChunk) -> String {
    let mut text = String::new();
    for choice in &chunk.choices {
        if let Some(delta) = choice.delta_view()
            && let Some(content) = delta.content
        {
            text.push_str(&content);
        }
    }
    text
}

fn print_ttft(ttft: Duration) {
    let ttft_ms = ttft.as_millis();
    if ttft_ms == 0 {
        println!("[ttft <1 ms]");
    } else {
        println!("[ttft {ttft_ms} ms]");
    }
}

fn render_external_event(data: &EventData, verbosity: Verbosity) -> Vec<String> {
    let EventData::Messages { messages, .. } = data else {
        return Vec::new();
    };
    messages
        .iter()
        .filter_map(|message| match message {
            Message::User { content } => render_external_user_content(content)
                .map(|rendered| format!("{} user: {rendered}", compact_timestamp())),
            Message::Assistant { content, .. } => {
                let rendered = render_assistant_content(content, verbosity);
                (!rendered.is_empty())
                    .then(|| format!("{} assistant: {rendered}", compact_timestamp()))
            }
            _ => None,
        })
        .collect()
}

fn render_external_user_content(content: &UserContent) -> Option<String> {
    let rendered = render_user_content_for_history(content);
    let trimmed = rendered.trim();
    if trimmed.starts_with("Scheduled task `") {
        Some(trimmed.to_string())
    } else {
        None
    }
}

fn render_user_content_for_history(content: &UserContent) -> String {
    match content {
        UserContent::String(text) => text.clone(),
        UserContent::Array(parts) => parts
            .iter()
            .filter_map(|part| match part {
                UserContentPart::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(""),
    }
}

/// Per-model usage tally for `/cost`.
#[derive(Default)]
struct ModelCost {
    calls: u64,
    prompt: i64,
    cached: i64,
    completion: i64,
    cost: f64,
}

/// Dynamic precision so sub-cent costs are not rounded to a misleading shape.
fn fmt_usd(cost: f64) -> String {
    if cost >= 1.0 {
        format!("${cost:.2}")
    } else if cost >= 0.01 {
        format!("${cost:.4}")
    } else {
        format!("${cost:.6}")
    }
}

fn truncate(value: &str, max: usize) -> String {
    if value.len() <= max {
        value.to_string()
    } else {
        format!("{}…", &value[..max.saturating_sub(1)])
    }
}

/// Walk the conversation's event log to find the latest `SandboxCreated`
/// event, returning the sandbox id. Returns `None` if no sandbox has been
/// created yet (e.g. nothing has been chatted with).
async fn latest_sandbox_id(conversation: &dyn HarnessConversation) -> Result<Option<SandboxId>> {
    let result = conversation
        .exoharness_handle()
        .get_events(Some(EventQuery {
            cursor: None,
            direction: Some(EventQueryDirection::Desc),
            limit: Some(1),
            session_id: None,
            turn_id: None,
            types: Some(vec![EventKind::SANDBOX_CREATED]),
        }))
        .await?;
    let Some(event) = result.events.into_iter().next() else {
        return Ok(None);
    };
    match event.data {
        EventData::SandboxCreated { sandbox_id, .. } => Ok(Some(sandbox_id)),
        other => anyhow::bail!(
            "type-filtered query for {} returned unexpected variant {}",
            EventKind::SANDBOX_CREATED.as_str(),
            other.kind().as_str(),
        ),
    }
}

/// All snapshots taken in the conversation, oldest-first. Each tuple is
/// `(snapshot_id, sandbox_id_it_was_taken_from)`.
async fn list_snapshots(
    conversation: &dyn HarnessConversation,
) -> Result<Vec<(SnapshotId, SandboxId)>> {
    let mut out = Vec::new();
    let mut cursor: Option<EventId> = None;
    loop {
        let result = conversation
            .exoharness_handle()
            .get_events(Some(EventQuery {
                cursor,
                direction: Some(EventQueryDirection::Asc),
                limit: Some(100),
                session_id: None,
                turn_id: None,
                types: Some(vec![EventKind::SANDBOX_SNAPSHOTTED]),
            }))
            .await?;
        let events_empty = result.events.is_empty();
        for event in result.events {
            match event.data {
                EventData::SandboxSnapshotted {
                    sandbox_id,
                    snapshot_id,
                } => out.push((snapshot_id, sandbox_id)),
                other => {
                    anyhow::bail!(
                        "type-filtered query for {} returned unexpected variant {}",
                        EventKind::SANDBOX_SNAPSHOTTED.as_str(),
                        other.kind().as_str(),
                    );
                }
            }
        }
        if events_empty || result.cursor.is_none() {
            break;
        }
        cursor = result.cursor;
    }
    Ok(out)
}

/// Find the sandbox a particular snapshot was taken from, by scanning the
/// `SandboxSnapshotted` events.
async fn sandbox_id_for_snapshot(
    conversation: &dyn HarnessConversation,
    target: SnapshotId,
) -> Result<Option<SandboxId>> {
    let snapshots = list_snapshots(conversation).await?;
    Ok(snapshots
        .into_iter()
        .find(|(snapshot_id, _)| *snapshot_id == target)
        .map(|(_, sandbox_id)| sandbox_id))
}

#[cfg(test)]
mod tests {
    use super::{
        ReplCommand, ReplInput, Verbosity, parse_repl_input, render_external_event,
        render_user_content_for_history,
    };
    use executor::EventData;
    use lingua::Message;
    use lingua::universal::UserContent;

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

    #[test]
    fn renders_scheduled_task_wakeup_user_messages() {
        let rendered = render_external_event(
            &EventData::Messages {
                messages: vec![Message::User {
                    content: UserContent::String(
                        "Scheduled task `joke` completed.\n\nstdout preview:\nhello".to_string(),
                    ),
                }],
                response_id: None,
                usage: None,
            },
            Verbosity::Compact,
        );

        assert_eq!(rendered.len(), 1);
        assert!(rendered[0].contains("user: Scheduled task `joke` completed."));
        assert!(rendered[0].contains("stdout preview:\nhello"));
    }

    #[test]
    fn renders_user_string_content_for_history() {
        let content = UserContent::String("first second".to_string());

        assert_eq!(render_user_content_for_history(&content), "first second");
    }
}
