//! Rendering of conversation messages and tool activity for the CLI, with a
//! user-selectable verbosity level.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

use clap::ValueEnum;
use lingua::Message;
use lingua::universal::{
    AssistantContent, AssistantContentPart, ToolCallArguments, ToolContentPart, UserContent,
};
use serde_json::{Map, Value};

/// Longest compact tool-call summary before it is truncated with an ellipsis.
const COMPACT_SUMMARY_MAX_CHARS: usize = 100;

/// Argument keys that identify what a tool call does, in priority order; the
/// first one present is shown inline in compact mode (e.g. the shell command).
const PRIMARY_ARGUMENT_KEYS: [&str; 3] = ["command", "cmd", "code"];

/// How much of the tool machinery the repl prints.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum)]
pub(crate) enum Verbosity {
    /// Only user and assistant text; tool activity is hidden entirely.
    Minimal,
    /// One line per tool call (name plus its primary argument) and one line
    /// per tool result.
    #[default]
    Compact,
    /// Every tool call argument and result field, fully expanded.
    Full,
}

impl fmt::Display for Verbosity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Verbosity::Minimal => "minimal",
            Verbosity::Compact => "compact",
            Verbosity::Full => "full",
        })
    }
}

impl FromStr for Verbosity {
    type Err = anyhow::Error;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        match input {
            "minimal" => Ok(Verbosity::Minimal),
            "compact" => Ok(Verbosity::Compact),
            "full" => Ok(Verbosity::Full),
            other => {
                anyhow::bail!("unknown verbosity `{other}` (expected minimal, compact, or full)")
            }
        }
    }
}

pub(crate) fn print_message(message: &Message, verbosity: Verbosity) {
    for line in render_message_lines(message, verbosity, None) {
        println!("{line}");
    }
}

/// Print a whole transcript. In compact mode, each tool result is folded into
/// its originating call line (`→ shell: ls ✓`) instead of printing separately.
pub(crate) fn print_transcript(messages: &[Message], verbosity: Verbosity) {
    for line in render_transcript_lines(messages, verbosity) {
        println!("{line}");
    }
}

/// A whole transcript as display lines, with compact-mode result folding.
pub(crate) fn render_transcript_lines(messages: &[Message], verbosity: Verbosity) -> Vec<String> {
    let fold = (verbosity == Verbosity::Compact).then(|| TranscriptFold::new(messages));
    messages
        .iter()
        .flat_map(|message| render_message_lines(message, verbosity, fold.as_ref()))
        .collect()
}

/// Result statuses collected across a transcript so compact call lines can
/// carry their outcome inline.
struct TranscriptFold {
    /// Status (`✓`, `✗ exit 2`, ...) by tool call id.
    statuses: HashMap<String, String>,
    /// Call ids that have a rendered call line to fold into.
    calls: HashSet<String>,
}

impl TranscriptFold {
    fn new(messages: &[Message]) -> Self {
        let mut statuses = HashMap::new();
        let mut calls = HashSet::new();
        for message in messages {
            match message {
                Message::Tool { content } => {
                    for part in content {
                        let ToolContentPart::ToolResult(result) = part;
                        statuses.insert(
                            result.tool_call_id.clone(),
                            compact_result_status(&to_workspace_json(&result.output)),
                        );
                    }
                }
                Message::Assistant {
                    content: AssistantContent::Array(parts),
                    ..
                } => {
                    for part in parts {
                        match part {
                            AssistantContentPart::ToolCall { tool_call_id, .. } => {
                                calls.insert(tool_call_id.clone());
                            }
                            AssistantContentPart::ToolResult {
                                tool_call_id,
                                output,
                                ..
                            } => {
                                statuses.insert(
                                    tool_call_id.clone(),
                                    compact_result_status(&to_workspace_json(output)),
                                );
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }
        Self { statuses, calls }
    }
}

fn render_message_lines(
    message: &Message,
    verbosity: Verbosity,
    fold: Option<&TranscriptFold>,
) -> Vec<String> {
    let timestamp = compact_timestamp();
    match message {
        Message::User { content } => {
            vec![format!(
                "{timestamp} user: {}",
                render_user_content(content)
            )]
        }
        Message::System { content } => {
            vec![format!(
                "{timestamp} system: {}",
                render_user_content(content)
            )]
        }
        Message::Developer { content } => {
            vec![format!(
                "{timestamp} developer: {}",
                render_user_content(content)
            )]
        }
        Message::Assistant { content, .. } => {
            render_assistant_message_lines(&timestamp, content, verbosity, fold)
        }
        Message::Tool { content } => content
            .iter()
            .filter_map(|part| {
                let ToolContentPart::ToolResult(result) = part;
                // Folded results already show on their call line.
                if fold.is_some_and(|fold| fold.calls.contains(&result.tool_call_id)) {
                    return None;
                }
                match verbosity {
                    Verbosity::Full => Some(format!(
                        "{timestamp} tool {}: {}",
                        result.tool_name, result.output
                    )),
                    _ => render_tool_result(
                        &result.tool_name,
                        &to_workspace_json(&result.output),
                        verbosity,
                    ),
                }
            })
            .collect(),
    }
}

fn render_assistant_message_lines(
    timestamp: &str,
    content: &AssistantContent,
    verbosity: Verbosity,
    fold: Option<&TranscriptFold>,
) -> Vec<String> {
    if verbosity == Verbosity::Full {
        return vec![format!(
            "{timestamp} assistant: {}",
            render_assistant_content(content, verbosity)
        )];
    }

    let mut lines = Vec::new();
    let text = render_assistant_content(content, verbosity);
    if !text.trim().is_empty() {
        lines.push(format!("{timestamp} assistant: {text}"));
    }
    if verbosity == Verbosity::Compact
        && let AssistantContent::Array(parts) = content
    {
        for part in parts {
            match part {
                AssistantContentPart::ToolCall {
                    tool_call_id,
                    tool_name,
                    arguments,
                    ..
                } => {
                    let arguments = match arguments {
                        ToolCallArguments::Valid(map) => match to_workspace_json(map) {
                            Value::Object(map) => map,
                            _ => Map::new(),
                        },
                        ToolCallArguments::Invalid(_) => Map::new(),
                    };
                    let mut rendered = render_tool_call(tool_name, &arguments, verbosity);
                    if let Some(line) = rendered.as_mut()
                        && let Some(status) = fold.and_then(|fold| fold.statuses.get(tool_call_id))
                    {
                        line.push(' ');
                        line.push_str(status);
                    }
                    lines.extend(rendered);
                }
                // Skip results already folded into their call line.
                AssistantContentPart::ToolResult {
                    tool_call_id,
                    tool_name,
                    output,
                    ..
                } if !fold.is_some_and(|fold| fold.calls.contains(tool_call_id)) => {
                    lines.extend(render_tool_result(
                        tool_name,
                        &to_workspace_json(output),
                        verbosity,
                    ));
                }
                _ => {}
            }
        }
    }
    lines
}

/// Bridge from lingua's vendored serde_json fork to the workspace serde_json.
/// Must go through JSON text: the fork has `arbitrary_precision` on, so its
/// numbers serialize as a private newtype that `serde_json::to_value` would
/// keep as an object instead of a number.
fn to_workspace_json<T: serde::Serialize>(value: &T) -> Value {
    lingua::serde_json::to_string(value)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or(Value::Null)
}

pub(crate) fn compact_timestamp() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() % 86_400)
        .unwrap_or(0);
    let hours = seconds / 3_600;
    let minutes = (seconds % 3_600) / 60;
    let seconds = seconds % 60;
    format!("[{hours:02}:{minutes:02}:{seconds:02}]")
}

fn render_user_content(content: &UserContent) -> String {
    match content {
        UserContent::String(text) => text.clone(),
        UserContent::Array(parts) => parts
            .iter()
            .map(|part| match part {
                lingua::universal::UserContentPart::Text(text) => text.text.clone(),
                _ => "[non-text user content]".to_string(),
            })
            .collect::<Vec<_>>()
            .join(""),
    }
}

/// Assistant content as a single string. At `Full`, every part (reasoning,
/// tool calls, tool results) is inlined; below that, only the text parts.
pub(crate) fn render_assistant_content(content: &AssistantContent, verbosity: Verbosity) -> String {
    match content {
        AssistantContent::String(text) => text.clone(),
        AssistantContent::Array(parts) => parts
            .iter()
            .filter_map(|part| match part {
                AssistantContentPart::Text(text) => Some(text.text.clone()),
                AssistantContentPart::Reasoning { text, .. } => {
                    (verbosity == Verbosity::Full).then(|| format!("[reasoning] {text}"))
                }
                AssistantContentPart::ToolCall {
                    tool_name,
                    arguments,
                    ..
                } => (verbosity == Verbosity::Full)
                    .then(|| format!("[tool_call {tool_name}] {arguments}")),
                AssistantContentPart::ToolResult {
                    tool_name, output, ..
                } => (verbosity == Verbosity::Full)
                    .then(|| format!("[tool_result {tool_name}] {output}")),
                AssistantContentPart::File { .. } => {
                    (verbosity == Verbosity::Full).then(|| "[file]".to_string())
                }
            })
            .collect::<Vec<_>>()
            .join(""),
    }
}

/// Render a tool call at the given verbosity; `None` means print nothing.
pub(crate) fn render_tool_call(
    tool_name: &str,
    arguments: &Map<String, Value>,
    verbosity: Verbosity,
) -> Option<String> {
    match verbosity {
        Verbosity::Minimal => None,
        Verbosity::Compact => Some(match primary_argument(arguments) {
            Some(summary) => format!("→ {tool_name}: {summary}"),
            None => format!("→ {tool_name}"),
        }),
        Verbosity::Full => {
            let mut lines = vec![format!("tool call {tool_name}")];
            append_object_fields(&mut lines, arguments, "  ");
            Some(lines.join("\n"))
        }
    }
}

/// Render a standalone tool result line at the given verbosity; `None` means
/// print nothing. In compact mode this is the fallback for results whose call
/// line is not available to fold the status into.
pub(crate) fn render_tool_result(
    tool_name: &str,
    result: &Value,
    verbosity: Verbosity,
) -> Option<String> {
    match verbosity {
        Verbosity::Minimal => None,
        Verbosity::Compact => Some(format!("← {tool_name} {}", compact_result_status(result))),
        Verbosity::Full => {
            let mut lines = vec!["tool result".to_string()];
            match result {
                Value::Object(object) => append_object_fields(&mut lines, object, "  "),
                other => lines.push(format!("  {}", render_value_inline(other))),
            }
            Some(lines.join("\n"))
        }
    }
}

fn primary_argument(arguments: &Map<String, Value>) -> Option<String> {
    PRIMARY_ARGUMENT_KEYS
        .iter()
        .find_map(|key| match arguments.get(*key) {
            Some(Value::String(text)) => Some(first_line_summary(text)),
            _ => None,
        })
}

fn first_line_summary(text: &str) -> String {
    let trimmed = text.trim();
    let mut line: String = trimmed.lines().next().unwrap_or("").trim_end().to_string();
    let truncated_width = line.chars().count() > COMPACT_SUMMARY_MAX_CHARS;
    if truncated_width {
        line = line.chars().take(COMPACT_SUMMARY_MAX_CHARS).collect();
    }
    if truncated_width || trimmed.lines().count() > 1 {
        line.push('…');
    }
    line
}

/// One-glyph outcome for compact tool lines: `✓` on success, `✗` plus the
/// failure signal (nonzero exit code, error field) otherwise.
pub(crate) fn compact_result_status(result: &Value) -> String {
    if let Value::Object(object) = result {
        if let Some(code) = object.get("exit_code").and_then(Value::as_i64)
            && code != 0
        {
            return format!("✗ exit {code}");
        }
        if object.get("error").is_some_and(|error| !error.is_null()) {
            return "✗ error".to_string();
        }
    }
    "✓".to_string()
}

fn append_object_fields(lines: &mut Vec<String>, object: &Map<String, Value>, indent: &str) {
    if object.is_empty() {
        lines.push(format!("{indent}{{}}"));
        return;
    }

    for (key, value) in object {
        append_field(lines, key, value, indent);
    }
}

fn append_field(lines: &mut Vec<String>, key: &str, value: &Value, indent: &str) {
    match value {
        Value::String(text) if text.contains('\n') => {
            lines.push(format!("{indent}{key}:"));
            for line in text.lines() {
                lines.push(format!("{indent}  {line}"));
            }
            if text.ends_with('\n') {
                lines.push(format!("{indent}  "));
            }
        }
        Value::Object(object) => {
            lines.push(format!("{indent}{key}:"));
            append_object_fields(lines, object, &format!("{indent}  "));
        }
        other => lines.push(format!("{indent}{key}: {}", render_value_inline(other))),
    }
}

fn render_value_inline(value: &Value) -> String {
    match value {
        Value::String(text) => format!("{text:?}"),
        other => serde_json::to_string(other).unwrap_or_else(|_| "<unrenderable>".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        TranscriptFold, Verbosity, render_message_lines, render_tool_call, render_tool_result,
    };
    use lingua::Message;
    use lingua::universal::{
        AssistantContent, AssistantContentPart, TextContentPart, ToolCallArguments,
        ToolContentPart, ToolResultContentPart,
    };
    use serde_json::{Map, Value, json};

    fn shell_arguments(command: &str) -> Map<String, Value> {
        Map::from_iter([("command".to_string(), Value::String(command.to_string()))])
    }

    #[test]
    fn full_renders_multiline_tool_call_arguments_as_indented_block() {
        let arguments = Map::from_iter([(
            "code".to_string(),
            Value::String("const x = 1;\nconst y = 2;".to_string()),
        )]);

        let rendered = render_tool_call("repl_execute", &arguments, Verbosity::Full)
            .expect("full renders tool calls");

        assert_eq!(
            rendered,
            "tool call repl_execute\n  code:\n    const x = 1;\n    const y = 2;"
        );
    }

    #[test]
    fn full_renders_multiline_tool_result_fields_as_indented_block() {
        let result = json!({"error": null, "stdout": "line 1\nline 2"});

        let rendered = render_tool_result("shell", &result, Verbosity::Full)
            .expect("full renders tool results");

        assert_eq!(
            rendered,
            "tool result\n  error: null\n  stdout:\n    line 1\n    line 2"
        );
    }

    #[test]
    fn compact_tool_call_shows_the_command_on_one_line() {
        let rendered = render_tool_call("shell", &shell_arguments("ls -la"), Verbosity::Compact)
            .expect("compact renders tool calls");

        assert_eq!(rendered, "→ shell: ls -la");
    }

    #[test]
    fn compact_tool_call_truncates_multiline_commands_to_the_first_line() {
        let rendered = render_tool_call(
            "shell",
            &shell_arguments("echo one\necho two"),
            Verbosity::Compact,
        )
        .expect("compact renders tool calls");

        assert_eq!(rendered, "→ shell: echo one…");
    }

    #[test]
    fn compact_tool_call_without_primary_argument_shows_only_the_name() {
        let arguments = Map::from_iter([("path".to_string(), Value::String("/tmp".to_string()))]);

        let rendered = render_tool_call("read_file", &arguments, Verbosity::Compact)
            .expect("compact renders tool calls");

        assert_eq!(rendered, "→ read_file");
    }

    #[test]
    fn compact_tool_result_is_a_single_line_with_failure_signals() {
        assert_eq!(
            render_tool_result("shell", &json!({"stdout": "ok"}), Verbosity::Compact),
            Some("← shell ✓".to_string())
        );
        assert_eq!(
            render_tool_result("shell", &json!({"exit_code": 2}), Verbosity::Compact),
            Some("← shell ✗ exit 2".to_string())
        );
        assert_eq!(
            render_tool_result("shell", &json!({"error": "boom"}), Verbosity::Compact),
            Some("← shell ✗ error".to_string())
        );
    }

    #[test]
    fn minimal_hides_tool_calls_and_results() {
        assert_eq!(
            render_tool_call("shell", &shell_arguments("ls"), Verbosity::Minimal),
            None
        );
        assert_eq!(
            render_tool_result("shell", &json!({}), Verbosity::Minimal),
            None
        );
    }

    fn assistant_message_with_tool_call() -> Message {
        let arguments = lingua::serde_json::Map::from_iter([(
            "command".to_string(),
            lingua::serde_json::Value::String("ls".to_string()),
        )]);
        Message::Assistant {
            content: AssistantContent::Array(vec![
                AssistantContentPart::Text(TextContentPart {
                    text: "running it".to_string(),
                    encrypted_content: None,
                    cache_control: None,
                    provider_options: None,
                }),
                AssistantContentPart::ToolCall {
                    tool_call_id: "call-1".to_string(),
                    tool_name: "shell".to_string(),
                    arguments: ToolCallArguments::Valid(arguments),
                    encrypted_content: None,
                    provider_options: None,
                    provider_executed: None,
                },
            ]),
            id: None,
        }
    }

    #[test]
    fn compact_assistant_message_renders_text_then_tool_lines() {
        let lines = render_message_lines(
            &assistant_message_with_tool_call(),
            Verbosity::Compact,
            None,
        );

        assert_eq!(lines.len(), 2);
        assert!(lines[0].ends_with("assistant: running it"));
        assert_eq!(lines[1], "→ shell: ls");
    }

    #[test]
    fn minimal_assistant_message_renders_only_text() {
        let lines = render_message_lines(
            &assistant_message_with_tool_call(),
            Verbosity::Minimal,
            None,
        );

        assert_eq!(lines.len(), 1);
        assert!(lines[0].ends_with("assistant: running it"));
    }

    fn tool_result_message(exit_code: i64) -> Message {
        Message::Tool {
            content: vec![ToolContentPart::ToolResult(ToolResultContentPart {
                tool_call_id: "call-1".to_string(),
                tool_name: "shell".to_string(),
                output: lingua::serde_json::json!({"exit_code": exit_code, "stdout": "out"}),
                provider_options: None,
            })],
        }
    }

    #[test]
    fn compact_transcript_folds_results_into_their_call_lines() {
        let messages = vec![assistant_message_with_tool_call(), tool_result_message(0)];
        let fold = TranscriptFold::new(&messages);

        let call_lines = render_message_lines(&messages[0], Verbosity::Compact, Some(&fold));
        let result_lines = render_message_lines(&messages[1], Verbosity::Compact, Some(&fold));

        assert_eq!(call_lines[1], "→ shell: ls ✓");
        assert!(result_lines.is_empty());
    }

    #[test]
    fn compact_transcript_marks_failed_calls() {
        let messages = vec![assistant_message_with_tool_call(), tool_result_message(3)];
        let fold = TranscriptFold::new(&messages);

        let call_lines = render_message_lines(&messages[0], Verbosity::Compact, Some(&fold));

        assert_eq!(call_lines[1], "→ shell: ls ✗ exit 3");
    }

    #[test]
    fn compact_transcript_keeps_orphan_results_as_their_own_line() {
        let messages = vec![tool_result_message(0)];
        let fold = TranscriptFold::new(&messages);

        let lines = render_message_lines(&messages[0], Verbosity::Compact, Some(&fold));

        assert_eq!(lines, vec!["← shell ✓".to_string()]);
    }
}
