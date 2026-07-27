use anyhow::{Result, anyhow, bail};
use boa_engine::{Context as BoaContext, Source};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::harness_helpers::HistoryMessage;

const MAX_VARIABLE_NAMES: usize = 128;
const FINAL_PREVIEW_CHARS: usize = 400;

#[derive(Debug, Clone)]
pub struct JsReplState {
    context_json: String,
    history_messages_json: String,
    globals: Map<String, Value>,
}

impl JsReplState {
    pub fn new<T: Serialize>(context: &T, history_messages: &[HistoryMessage]) -> Result<Self> {
        Ok(Self {
            context_json: serde_json::to_string(context)?,
            history_messages_json: serde_json::to_string(history_messages)?,
            globals: Map::new(),
        })
    }

    pub fn execute(&mut self, code: &str) -> Result<JsExecutionResult> {
        let mut context = BoaContext::default();
        let wrapped = format!(
            r#"
globalThis.context = {context_json};
const __rlm_history_messages = {history_messages_json};
const __rlm_clone_json = (value) => JSON.parse(JSON.stringify(value));
const __rlm_copy_messages = (messages) => __rlm_clone_json(messages);
const __rlm_filter_messages = (role) => {{
  if (role == null) {{
    return __rlm_history_messages;
  }}
  const normalized = String(role).toLowerCase();
  return __rlm_history_messages.filter((message) => message.role === normalized);
}};
globalThis.getMessages = Object.freeze((role = null) =>
  __rlm_copy_messages(__rlm_filter_messages(role))
);
for (const [key, value] of Object.entries({state_json})) {{
  globalThis[key] = value;
}}
if (!("Final" in globalThis)) {{
  globalThis.Final = null;
}}
globalThis.__rlm_stdout = [];
globalThis.print = (...args) => {{
  const rendered = args.map((arg) => {{
    if (typeof arg === "string") {{
      return arg;
    }}
    try {{
      return JSON.stringify(arg);
    }} catch (_error) {{
      return String(arg);
    }}
  }});
  globalThis.__rlm_stdout.push(rendered.join(" "));
}};
let __rlm_error = null;
try {{
  (0, eval)({code_json});
}} catch (error) {{
  __rlm_error = String(error && error.stack ? error.stack : error);
}}
const __rlm_globals = {{}};
for (const [key, value] of Object.entries(globalThis)) {{
  if (key === "context" || key === "getMessages" || key === "print" || key.startsWith("__rlm_")) {{
    continue;
  }}
  try {{
    JSON.stringify(value);
    __rlm_globals[key] = value;
  }} catch (_error) {{}}
}}
JSON.stringify({{
  stdout: globalThis.__rlm_stdout.join("\n"),
  error: __rlm_error,
  final_preview: globalThis.Final == null ? null : String(globalThis.Final).slice(0, {final_preview_chars}),
  globals: __rlm_globals,
  variable_names: Object.keys(__rlm_globals).sort().slice(0, {max_variable_names}),
}});
"#,
            context_json = self.context_json,
            history_messages_json = self.history_messages_json,
            state_json = serde_json::to_string(&self.globals)?,
            code_json = serde_json::to_string(code)?,
            final_preview_chars = FINAL_PREVIEW_CHARS,
            max_variable_names = MAX_VARIABLE_NAMES,
        );
        let value = context
            .eval(Source::from_bytes(&wrapped))
            .map_err(|error| anyhow!("failed to execute JS repl code: {error}"))?;
        let json = value
            .to_string(&mut context)
            .map_err(|error| anyhow!("failed to stringify JS repl result: {error}"))?
            .to_std_string_escaped();
        let envelope: JsExecutionEnvelope = serde_json::from_str(&json)?;
        self.globals = envelope.globals.clone();
        Ok(JsExecutionResult {
            stdout: envelope.stdout,
            variable_names: envelope.variable_names,
            error: envelope.error,
            final_preview: envelope.final_preview,
        })
    }

    pub fn read_variable(&self, variable_name: &str) -> Result<String> {
        let Some(value) = self.globals.get(variable_name) else {
            bail!("variable not found: {variable_name}");
        };
        stringify_value(value)
    }

    pub fn set_variable(&mut self, variable_name: &str, value: &str) {
        self.globals
            .insert(variable_name.to_string(), Value::String(value.to_string()));
    }

    pub fn final_value(&self) -> Result<Option<String>> {
        let Some(value) = self.globals.get("Final") else {
            return Ok(None);
        };
        match value {
            Value::Null => Ok(None),
            Value::String(text) => Ok(Some(text.clone())),
            other => Ok(Some(serde_json::to_string(other)?)),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsExecutionResult {
    pub stdout: String,
    pub variable_names: Vec<String>,
    pub error: Option<String>,
    pub final_preview: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct JsExecutionEnvelope {
    stdout: String,
    error: Option<String>,
    final_preview: Option<String>,
    globals: Map<String, Value>,
    variable_names: Vec<String>,
}

fn stringify_value(value: &Value) -> Result<String> {
    match value {
        Value::String(text) => Ok(text.clone()),
        other => Ok(serde_json::to_string(other)?),
    }
}
