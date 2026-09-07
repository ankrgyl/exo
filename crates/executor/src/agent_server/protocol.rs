use exoharness::{
    AgentId, AgentRecord, ConversationId, ConversationRecord, EventId, SandboxProvider, SessionId,
    ToolArguments, ToolCallId, ToolResult, TurnId, Uuid7,
};
use lingua::{Message, UniversalStreamChunk};
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

pub(crate) const PROTOCOL_VERSION: u32 = 1;
pub(crate) const RUNTIME_CONTRACT: &str = "exo-agent-server-v1";

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(untagged)]
pub(crate) enum RequestId {
    Number(serde_json::Number),
    String(String),
    Null(()),
    #[serde(skip)]
    #[default]
    Missing,
}

impl RequestId {
    pub(crate) fn is_missing(&self) -> bool {
        matches!(self, Self::Missing)
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct RpcRequest {
    pub(crate) jsonrpc: String,
    #[serde(default)]
    pub(crate) id: RequestId,
    pub(crate) method: String,
    #[serde(default)]
    pub(crate) params: Option<Box<RawValue>>,
}

#[derive(Debug, Serialize)]
pub(crate) struct RpcSuccess {
    jsonrpc: &'static str,
    id: RequestId,
    result: ResponseResult,
}

impl RpcSuccess {
    pub(crate) fn new(id: RequestId, result: ResponseResult) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result,
        }
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct RpcFailure {
    jsonrpc: &'static str,
    id: RequestId,
    error: RpcError,
}

impl RpcFailure {
    pub(crate) fn new(id: RequestId, error: RpcError) -> Self {
        Self {
            jsonrpc: "2.0",
            id: match id {
                RequestId::Missing => RequestId::Null(()),
                id => id,
            },
            error,
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
pub(crate) enum RpcResponse {
    Success(RpcSuccess),
    Failure(RpcFailure),
}

#[derive(Debug, Serialize)]
pub(crate) struct RpcError {
    code: i32,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<ErrorData>,
}

#[derive(Debug, Serialize)]
struct ErrorData {
    kind: &'static str,
}

impl RpcError {
    pub(crate) fn parse() -> Self {
        Self::standard(-32700, "parse error")
    }

    pub(crate) fn invalid_request() -> Self {
        Self::standard(-32600, "invalid request")
    }

    pub(crate) fn method_not_found() -> Self {
        Self::standard(-32601, "method not found")
    }

    pub(crate) fn invalid_params() -> Self {
        Self::standard(-32602, "invalid params")
    }

    pub(crate) fn executor(error: impl std::fmt::Display) -> Self {
        Self::domain(-32000, error.to_string(), "executor_error")
    }

    pub(crate) fn turn_already_active() -> Self {
        Self::domain(
            -32001,
            "conversation already has an active turn",
            "turn_already_active",
        )
    }

    pub(crate) fn not_found(message: String, kind: &'static str) -> Self {
        Self::domain(-32002, message, kind)
    }

    pub(crate) fn unsupported_protocol_version() -> Self {
        Self::domain(
            -32003,
            format!("unsupported protocol version; expected {PROTOCOL_VERSION}"),
            "unsupported_protocol_version",
        )
    }

    pub(crate) fn not_initialized() -> Self {
        Self::domain(-32004, "initialize must be called first", "not_initialized")
    }

    pub(crate) fn cancellation_unsupported() -> Self {
        Self::domain(-32005, "turn cancellation is not supported", "unsupported")
    }

    pub(crate) fn incompatible_harness(message: String) -> Self {
        Self::domain(-32006, message, "incompatible_harness")
    }

    fn standard(code: i32, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    fn domain(code: i32, message: impl Into<String>, kind: &'static str) -> Self {
        Self {
            code,
            message: message.into(),
            data: Some(ErrorData { kind }),
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
pub(crate) enum ResponseResult {
    Initialize(InitializeResult),
    AgentList(AgentListResult),
    AgentGet(AgentGetResult),
    ConversationList(ConversationListResult),
    ConversationGet(ConversationGetResult),
    ConversationCreate(ConversationCreateResult),
    TurnStart(TurnStartResult),
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct InitializeParams {
    pub(crate) protocol_version: u32,
    #[serde(default)]
    pub(crate) client: Option<ClientInfo>,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct ClientInfo {
    pub(crate) name: String,
    pub(crate) version: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct InitializeResult {
    pub(crate) protocol_version: u32,
    pub(crate) runtime_contract: &'static str,
    pub(crate) server: ServerInfo,
    pub(crate) transport: TransportInfo,
    pub(crate) capabilities: Capabilities,
}

impl Default for InitializeResult {
    fn default() -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            runtime_contract: RUNTIME_CONTRACT,
            server: ServerInfo {
                name: "exo",
                version: env!("CARGO_PKG_VERSION"),
            },
            transport: TransportInfo {
                kind: "stdio",
                framing: "jsonl",
                bidirectional: true,
            },
            capabilities: Capabilities {
                agent_list: true,
                agent_get: true,
                conversation_list: true,
                conversation_get: true,
                conversation_create: true,
                turn_start: true,
                turn_stream: true,
                turn_cancel: false,
            },
        }
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct ServerInfo {
    name: &'static str,
    version: &'static str,
}

#[derive(Debug, Serialize)]
pub(crate) struct TransportInfo {
    kind: &'static str,
    framing: &'static str,
    bidirectional: bool,
}

#[derive(Debug, Serialize)]
pub(crate) struct Capabilities {
    agent_list: bool,
    agent_get: bool,
    conversation_list: bool,
    conversation_get: bool,
    conversation_create: bool,
    turn_start: bool,
    turn_stream: bool,
    turn_cancel: bool,
}

#[derive(Debug, Default, Deserialize, Serialize)]
pub(crate) struct EmptyParams {}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct AgentGetParams {
    pub(crate) agent_ref: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct AgentListResult {
    pub(crate) agents: Vec<AgentRecord>,
}

#[derive(Debug, Serialize)]
pub(crate) struct AgentGetResult {
    pub(crate) agent: AgentRecord,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct ConversationListParams {
    pub(crate) agent_ref: String,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct ConversationGetParams {
    pub(crate) agent_ref: String,
    pub(crate) conversation_ref: String,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct ConversationCreateParams {
    pub(crate) agent_ref: String,
    #[serde(default)]
    pub(crate) slug: Option<String>,
    #[serde(default)]
    pub(crate) name: Option<String>,
    #[serde(default)]
    pub(crate) sandbox_image: Option<String>,
    #[serde(default)]
    pub(crate) sandbox_provider: Option<SandboxProvider>,
    #[serde(default)]
    pub(crate) shell_program: Option<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct ConversationListResult {
    pub(crate) conversations: Vec<ConversationRecord>,
}

#[derive(Debug, Serialize)]
pub(crate) struct ConversationGetResult {
    pub(crate) conversation: ConversationRecord,
}

#[derive(Debug, Serialize)]
pub(crate) struct ConversationCreateResult {
    pub(crate) conversation: ConversationRecord,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct TurnStartParams {
    pub(crate) agent_ref: String,
    pub(crate) conversation_ref: String,
    pub(crate) input: Vec<Message>,
    #[serde(default)]
    pub(crate) session_id: Option<SessionId>,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct TurnCancelParams {
    pub(crate) operation_id: Uuid7,
}

#[derive(Debug, Serialize)]
pub(crate) struct TurnStartResult {
    pub(crate) operation_id: Uuid7,
    pub(crate) state: &'static str,
    pub(crate) agent_id: AgentId,
    pub(crate) conversation_id: ConversationId,
}

#[derive(Debug, Serialize)]
pub(crate) struct RpcNotification<P> {
    jsonrpc: &'static str,
    method: &'static str,
    params: P,
}

impl<P> RpcNotification<P> {
    pub(crate) fn new(method: &'static str, params: P) -> Self {
        Self {
            jsonrpc: "2.0",
            method,
            params,
        }
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct TurnStartedParams {
    pub(crate) operation_id: Uuid7,
    pub(crate) sequence: u64,
    pub(crate) agent_id: AgentId,
    pub(crate) conversation_id: ConversationId,
    pub(crate) session_id: Option<SessionId>,
    pub(crate) turn_id: Option<TurnId>,
}

#[derive(Debug, Serialize)]
pub(crate) struct TurnEventParams {
    pub(crate) operation_id: Uuid7,
    pub(crate) sequence: u64,
    pub(crate) event: TurnEvent,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum TurnEvent {
    FirstChunk {
        ttft_ms: u64,
    },
    Chunk {
        chunk: UniversalStreamChunk,
    },
    ToolCall {
        tool_call_id: ToolCallId,
        tool_name: String,
        arguments: ToolArguments,
    },
    ToolResult {
        tool_call_id: ToolCallId,
        result: ToolResult,
    },
}

#[derive(Debug, Serialize)]
pub(crate) struct TurnCompletedParams {
    pub(crate) operation_id: Uuid7,
    pub(crate) sequence: u64,
    pub(crate) agent_id: AgentId,
    pub(crate) conversation_id: ConversationId,
    pub(crate) session_id: SessionId,
    pub(crate) turn_id: TurnId,
    pub(crate) latest_event_id: EventId,
}

#[derive(Debug, Serialize)]
pub(crate) struct TurnFailedParams {
    pub(crate) operation_id: Uuid7,
    pub(crate) sequence: u64,
    pub(crate) agent_id: AgentId,
    pub(crate) conversation_id: ConversationId,
    pub(crate) session_id: Option<SessionId>,
    pub(crate) turn_id: Option<TurnId>,
    pub(crate) latest_event_id: Option<EventId>,
    pub(crate) error: OperationError,
}

#[derive(Debug, Serialize)]
pub(crate) struct OperationError {
    pub(crate) kind: &'static str,
    pub(crate) message: String,
}
