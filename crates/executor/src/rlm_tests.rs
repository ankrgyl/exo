use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use crate::{
    ExecutionStreamEvent, ModelClient, ModelRequest, ModelResponse, ModelResponseStream,
    PendingToolCall, SendRequest,
};
use anyhow::{anyhow, bail};
use async_trait::async_trait;
use exoharness::{
    BasicExoHarness, Binding, EventData, EventQuery, EventQueryDirection, ExoHarness,
    PutSecretRequest, SandboxProvider, Secret, ToolRequest, Uuid7,
};
use lingua::universal::{AssistantContent, UserContent};
use lingua::{Message, UniversalStreamChunk};
use serde_json::{Map, Value};
use tempfile::TempDir;
use tokio_stream::StreamExt;

use crate::test_support::local_test_config;
use crate::{CreateAgentRequest, CreateConversationRequest, Harness, RlmHarness};

#[tokio::test(flavor = "current_thread")]
async fn rlm_send_executes_repl_steps_and_persists_final_answer() {
    let tempdir = TempDir::new().expect("tempdir should exist");
    let exoharness = Arc::new(
        BasicExoHarness::new(local_test_config(tempdir.path().join("exoharness")))
            .await
            .expect("basic exoharness should initialize"),
    ) as Arc<dyn ExoHarness>;
    let model = Arc::new(FakeModelClient::new(vec![
        ModelResponse {
            provider_cost_usd: None,
            response_id: Some(Uuid7::now()),
            messages: vec![assistant_message("Inspecting the transcript.")],
            tool_calls: vec![PendingToolCall {
                tool_call_id: "repl-1".to_string(),
                request: ToolRequest {
                    function_name: "repl_execute".to_string(),
                    arguments: Map::from_iter([(
                        "code".to_string(),
                        Value::String(
                            "globalThis.snippet = context.slice(0, 32);\nprint(globalThis.snippet);"
                                .to_string(),
                        ),
                    )]),
                },
            }],
            usage: None,
            model: None,
            ttft: None,
            duration: None,
        },
        ModelResponse {
            provider_cost_usd: None,
            response_id: Some(Uuid7::now()),
            messages: vec![assistant_message("FINAL(done)")],
            tool_calls: Vec::new(),
            usage: None,
            model: None,
            ttft: None,
            duration: None,
        },
    ]));
    let harness = RlmHarness::new(exoharness, model);
    register_test_model(harness.exoharness_handle().as_ref()).await;

    let agent = harness
        .create_agent(CreateAgentRequest {
            slug: "demo".to_string(),
            name: Some("Demo".to_string()),
            harness: crate::AgentHarnessKind::Rlm,
            typescript: None,
            enable_agent_tool_creation: true,
            sandbox_image: None,
            sandbox_provider: SandboxProvider::LocalProcess,
            sandbox_scope: None,
            enable_networking: false,
            model: "gpt-5.4".to_string(),
            max_output_tokens: Some(512),
            max_tool_round_trips: Some(4),
            braintrust: None,
        })
        .await
        .expect("agent should be created");
    let conversation = agent
        .create_conversation(CreateConversationRequest::default())
        .await
        .expect("conversation should be created");

    conversation
        .send(SendRequest {
            input: vec![user_message("say done after you inspect context")],
            session_id: None,
        })
        .await
        .expect("send should succeed");

    let messages = conversation.messages().await.expect("messages should load");
    assert_eq!(messages.len(), 2);
    assert!(matches!(messages[0], Message::User { .. }));
    assert_eq!(assistant_text(&messages[1]), "done");

    let events = conversation
        .exoharness_handle()
        .get_events(Some(EventQuery {
            cursor: None,
            direction: Some(EventQueryDirection::Asc),
            limit: None,
            session_id: None,
            turn_id: None,
            types: None,
        }))
        .await
        .expect("events should load")
        .events;

    assert!(events.iter().any(|event| {
        matches!(
            &event.data,
            EventData::Custom { event_type, .. } if event_type == "rlm_context_initialized"
        )
    }));
    assert!(events.iter().any(|event| {
        matches!(
            &event.data,
            EventData::Custom { event_type, .. } if event_type == "rlm_tool_result"
        )
    }));
}

#[tokio::test(flavor = "current_thread")]
async fn rlm_subquery_variable_can_store_final_answer() {
    let tempdir = TempDir::new().expect("tempdir should exist");
    let exoharness = Arc::new(
        BasicExoHarness::new(local_test_config(tempdir.path().join("exoharness")))
            .await
            .expect("basic exoharness should initialize"),
    ) as Arc<dyn ExoHarness>;
    let model = Arc::new(FakeModelClient::new(vec![
        ModelResponse {
            provider_cost_usd: None,
            response_id: Some(Uuid7::now()),
            messages: vec![assistant_message("Preparing a smaller prompt.")],
            tool_calls: vec![PendingToolCall {
                tool_call_id: "repl-1".to_string(),
                request: ToolRequest {
                    function_name: "repl_execute".to_string(),
                    arguments: Map::from_iter([(
                        "code".to_string(),
                        Value::String("globalThis.snippet = '2 + 2 = ?';".to_string()),
                    )]),
                },
            }],
            usage: None,
            model: None,
            ttft: None,
            duration: None,
        },
        ModelResponse {
            provider_cost_usd: None,
            response_id: Some(Uuid7::now()),
            messages: vec![assistant_message("Delegating the arithmetic.")],
            tool_calls: vec![PendingToolCall {
                tool_call_id: "sub-1".to_string(),
                request: ToolRequest {
                    function_name: "subquery_variable".to_string(),
                    arguments: Map::from_iter([
                        (
                            "variable_name".to_string(),
                            Value::String("snippet".to_string()),
                        ),
                        (
                            "question".to_string(),
                            Value::String("Return only the numeric answer.".to_string()),
                        ),
                        (
                            "target_var".to_string(),
                            Value::String("final_answer".to_string()),
                        ),
                    ]),
                },
            }],
            usage: None,
            model: None,
            ttft: None,
            duration: None,
        },
        ModelResponse {
            provider_cost_usd: None,
            response_id: Some(Uuid7::now()),
            messages: vec![assistant_message("4")],
            tool_calls: Vec::new(),
            usage: None,
            model: None,
            ttft: None,
            duration: None,
        },
        ModelResponse {
            provider_cost_usd: None,
            response_id: Some(Uuid7::now()),
            messages: vec![assistant_message("FINAL_VAR(final_answer)")],
            tool_calls: Vec::new(),
            usage: None,
            model: None,
            ttft: None,
            duration: None,
        },
    ]));
    let harness = RlmHarness::new(exoharness, Arc::clone(&model));
    register_test_model(harness.exoharness_handle().as_ref()).await;

    let agent = harness
        .create_agent(CreateAgentRequest {
            slug: "demo".to_string(),
            name: Some("Demo".to_string()),
            harness: crate::AgentHarnessKind::Rlm,
            typescript: None,
            enable_agent_tool_creation: true,
            sandbox_image: None,
            sandbox_provider: SandboxProvider::LocalProcess,
            sandbox_scope: None,
            enable_networking: false,
            model: "gpt-5.4".to_string(),
            max_output_tokens: Some(512),
            max_tool_round_trips: Some(6),
            braintrust: None,
        })
        .await
        .expect("agent should be created");
    let conversation = agent
        .create_conversation(CreateConversationRequest::default())
        .await
        .expect("conversation should be created");

    conversation
        .send(SendRequest {
            input: vec![user_message("what is 2 + 2?")],
            session_id: None,
        })
        .await
        .expect("send should succeed");

    let messages = conversation.messages().await.expect("messages should load");
    assert_eq!(
        assistant_text(messages.last().expect("assistant message")),
        "4"
    );

    let requests = model.observed_requests();
    assert_eq!(requests.len(), 4);
    assert!(requests[2].tools.is_empty());
}

#[tokio::test(flavor = "current_thread")]
async fn rlm_send_stream_suppresses_internal_control_text() {
    let tempdir = TempDir::new().expect("tempdir should exist");
    let exoharness = Arc::new(
        BasicExoHarness::new(local_test_config(tempdir.path().join("exoharness")))
            .await
            .expect("basic exoharness should initialize"),
    ) as Arc<dyn ExoHarness>;
    let model = Arc::new(FakeModelClient::with_streams(vec![FakeStreamResponse {
        chunks: vec![
            UniversalStreamChunk::text_delta(0, "FINAL_VAR("),
            UniversalStreamChunk::text_delta(0, "origQ"),
            UniversalStreamChunk::text_delta(0, ")"),
            UniversalStreamChunk::finish(0, "stop"),
        ],
        final_response: ModelResponse {
            provider_cost_usd: None,
            response_id: Some(Uuid7::now()),
            messages: vec![assistant_message("FINAL(2)")],
            tool_calls: Vec::new(),
            usage: None,
            model: None,
            ttft: None,
            duration: None,
        },
    }]));
    let harness = RlmHarness::new(exoharness, model);
    register_test_model(harness.exoharness_handle().as_ref()).await;

    let agent = harness
        .create_agent(CreateAgentRequest {
            slug: "demo".to_string(),
            name: Some("Demo".to_string()),
            harness: crate::AgentHarnessKind::Rlm,
            typescript: None,
            enable_agent_tool_creation: true,
            sandbox_image: None,
            sandbox_provider: SandboxProvider::LocalProcess,
            sandbox_scope: None,
            enable_networking: false,
            model: "gpt-5.4".to_string(),
            max_output_tokens: Some(512),
            max_tool_round_trips: None,
            braintrust: None,
        })
        .await
        .expect("agent should be created");
    let conversation = agent
        .create_conversation(CreateConversationRequest::default())
        .await
        .expect("conversation should be created");

    let mut stream = conversation
        .send_stream(SendRequest {
            input: vec![user_message("what is 1 + 1?")],
            session_id: None,
        })
        .await
        .expect("send stream should succeed");

    let mut saw_chunk = false;
    while let Some(event) = stream.next().await {
        match event.expect("stream event should succeed") {
            ExecutionStreamEvent::FirstChunk { .. } => {}
            ExecutionStreamEvent::Chunk(_) => saw_chunk = true,
            ExecutionStreamEvent::ToolCall { .. } => {}
            ExecutionStreamEvent::ToolResult { .. } => {}
            ExecutionStreamEvent::Completed(_) => {}
        }
    }

    assert!(!saw_chunk, "RLM should not stream raw control text");

    let messages = conversation.messages().await.expect("messages should load");
    assert_eq!(
        assistant_text(messages.last().expect("assistant message")),
        "2"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn rlm_exposes_history_via_get_messages() {
    let tempdir = TempDir::new().expect("tempdir should exist");
    let exoharness = Arc::new(
        BasicExoHarness::new(local_test_config(tempdir.path().join("exoharness")))
            .await
            .expect("basic exoharness should initialize"),
    ) as Arc<dyn ExoHarness>;
    let model = Arc::new(FakeModelClient::new(vec![
        ModelResponse {
            provider_cost_usd: None,
            response_id: Some(Uuid7::now()),
            messages: vec![assistant_message("FINAL(recorded)")],
            tool_calls: Vec::new(),
            usage: None,
            model: None,
            ttft: None,
            duration: None,
        },
        ModelResponse {
            provider_cost_usd: None,
            response_id: Some(Uuid7::now()),
            messages: vec![assistant_message("Checking prior user content.")],
            tool_calls: vec![PendingToolCall {
                tool_call_id: "repl-1".to_string(),
                request: ToolRequest {
                    function_name: "repl_execute".to_string(),
                    arguments: Map::from_iter([(
                        "code".to_string(),
                        Value::String(
                            "const userMessages = getMessages('user');\n\
const latest = userMessages[userMessages.length - 1] ?? null;\n\
const priorMatches = userMessages.filter((message) => message.content.includes('assert!(true)'));\n\
const indicesAreStable = userMessages.every((message, index) => message.index === index * 2);\n\
globalThis.answer = String(\n\
  latest !== null &&\n\
  latest.content.includes('how many assert statements are in that file?') &&\n\
  priorMatches.length === 1 &&\n\
  priorMatches[0].content.includes('assert!(true)') &&\n\
  indicesAreStable\n\
);"
                            .to_string(),
                        ),
                    )]),
                },
            }],
            usage: None,
            model: None,
            ttft: None,
            duration: None,
        },
        ModelResponse {
            provider_cost_usd: None,
            response_id: Some(Uuid7::now()),
            messages: vec![assistant_message("FINAL_VAR(answer)")],
            tool_calls: Vec::new(),
            usage: None,
            model: None,
            ttft: None,
            duration: None,
        },
    ]));
    let harness = RlmHarness::new(exoharness, model);
    register_test_model(harness.exoharness_handle().as_ref()).await;

    let agent = harness
        .create_agent(CreateAgentRequest {
            slug: "demo".to_string(),
            name: Some("Demo".to_string()),
            harness: crate::AgentHarnessKind::Rlm,
            typescript: None,
            enable_agent_tool_creation: true,
            sandbox_image: None,
            sandbox_provider: SandboxProvider::LocalProcess,
            sandbox_scope: None,
            enable_networking: false,
            model: "gpt-5.4".to_string(),
            max_output_tokens: Some(512),
            max_tool_round_trips: None,
            braintrust: None,
        })
        .await
        .expect("agent should be created");
    let conversation = agent
        .create_conversation(CreateConversationRequest::default())
        .await
        .expect("conversation should be created");

    conversation
        .send(SendRequest {
            input: vec![user_message("fn trace_test() { assert!(true); }")],
            session_id: None,
        })
        .await
        .expect("initial send should succeed");

    conversation
        .send(SendRequest {
            input: vec![user_message("how many assert statements are in that file?")],
            session_id: None,
        })
        .await
        .expect("follow-up send should succeed");

    let messages = conversation.messages().await.expect("messages should load");
    assert_eq!(
        assistant_text(messages.last().expect("assistant message")),
        "true"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn rlm_can_finish_by_setting_final_in_repl() {
    let tempdir = TempDir::new().expect("tempdir should exist");
    let exoharness = Arc::new(
        BasicExoHarness::new(local_test_config(tempdir.path().join("exoharness")))
            .await
            .expect("basic exoharness should initialize"),
    ) as Arc<dyn ExoHarness>;
    let model = Arc::new(FakeModelClient::new(vec![ModelResponse {
        provider_cost_usd: None,
        response_id: Some(Uuid7::now()),
        messages: vec![assistant_message("Setting Final in the REPL.")],
        tool_calls: vec![PendingToolCall {
            tool_call_id: "repl-1".to_string(),
            request: ToolRequest {
                function_name: "repl_execute".to_string(),
                arguments: Map::from_iter([(
                    "code".to_string(),
                    Value::String("globalThis.Final = 'done';".to_string()),
                )]),
            },
        }],
        usage: None,
        model: None,
        ttft: None,
        duration: None,
    }]));
    let harness = RlmHarness::new(exoharness, Arc::clone(&model));
    register_test_model(harness.exoharness_handle().as_ref()).await;

    let agent = harness
        .create_agent(CreateAgentRequest {
            slug: "demo".to_string(),
            name: Some("Demo".to_string()),
            harness: crate::AgentHarnessKind::Rlm,
            typescript: None,
            enable_agent_tool_creation: true,
            sandbox_image: None,
            sandbox_provider: SandboxProvider::LocalProcess,
            sandbox_scope: None,
            enable_networking: false,
            model: "gpt-5.4".to_string(),
            max_output_tokens: Some(512),
            max_tool_round_trips: None,
            braintrust: None,
        })
        .await
        .expect("agent should be created");
    let conversation = agent
        .create_conversation(CreateConversationRequest::default())
        .await
        .expect("conversation should be created");

    conversation
        .send(SendRequest {
            input: vec![user_message("say done")],
            session_id: None,
        })
        .await
        .expect("send should succeed");

    let messages = conversation.messages().await.expect("messages should load");
    assert_eq!(
        assistant_text(messages.last().expect("assistant message")),
        "done"
    );

    let requests = model.observed_requests();
    assert_eq!(requests.len(), 1);
}

#[derive(Debug, Default, Clone)]
struct FakeModelClient {
    responses: Arc<Mutex<VecDeque<ModelResponse>>>,
    streams: Arc<Mutex<VecDeque<FakeStreamResponse>>>,
    requests: Arc<Mutex<Vec<ModelRequest>>>,
}

impl FakeModelClient {
    fn new(responses: Vec<ModelResponse>) -> Self {
        Self {
            responses: Arc::new(Mutex::new(VecDeque::from(responses))),
            streams: Arc::new(Mutex::new(VecDeque::new())),
            requests: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn with_streams(streams: Vec<FakeStreamResponse>) -> Self {
        Self {
            responses: Arc::new(Mutex::new(VecDeque::new())),
            streams: Arc::new(Mutex::new(VecDeque::from(streams))),
            requests: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn observed_requests(&self) -> Vec<ModelRequest> {
        self.requests
            .lock()
            .expect("requests should not be poisoned")
            .clone()
    }
}

#[async_trait]
impl ModelClient for FakeModelClient {
    async fn complete(&self, request: ModelRequest) -> exoharness::Result<ModelResponse> {
        self.requests
            .lock()
            .expect("requests should not be poisoned")
            .push(request);
        let mut responses = self
            .responses
            .lock()
            .expect("responses should not be poisoned");
        let Some(response) = responses.pop_front() else {
            bail!("unexpected model request with no queued response");
        };
        Ok(response)
    }

    async fn complete_stream(
        &self,
        request: ModelRequest,
    ) -> exoharness::Result<Box<dyn ModelResponseStream>> {
        self.requests
            .lock()
            .expect("requests should not be poisoned")
            .push(request);
        let mut streams = self.streams.lock().expect("streams should not be poisoned");
        let Some(stream) = streams.pop_front() else {
            return Err(anyhow!("streaming not configured"));
        };
        Ok(Box::new(FakeModelResponseStream {
            chunks: VecDeque::from(stream.chunks),
            final_response: Some(stream.final_response),
        }))
    }
}

#[derive(Debug)]
struct FakeStreamResponse {
    chunks: Vec<UniversalStreamChunk>,
    final_response: ModelResponse,
}

struct FakeModelResponseStream {
    chunks: VecDeque<UniversalStreamChunk>,
    final_response: Option<ModelResponse>,
}

#[async_trait]
impl ModelResponseStream for FakeModelResponseStream {
    async fn next_chunk(&mut self) -> exoharness::Result<Option<UniversalStreamChunk>> {
        Ok(self.chunks.pop_front())
    }

    async fn finish(mut self: Box<Self>) -> exoharness::Result<ModelResponse> {
        self.final_response
            .take()
            .ok_or_else(|| anyhow!("stream already finished"))
    }
}

fn user_message(text: &str) -> Message {
    Message::User {
        content: UserContent::String(text.to_string()),
    }
}

fn assistant_message(text: &str) -> Message {
    Message::Assistant {
        content: AssistantContent::String(text.to_string()),
        id: None,
    }
}

fn assistant_text(message: &Message) -> String {
    let Message::Assistant { content, .. } = message else {
        return String::new();
    };
    match content {
        AssistantContent::String(text) => text.clone(),
        AssistantContent::Array(_) => String::new(),
    }
}

async fn register_test_model(exoharness: &dyn ExoHarness) {
    let secret_id = exoharness
        .put_secret(PutSecretRequest {
            name: "test-openai".to_string(),
            secret: Secret::Key {
                value: "test-key".to_string(),
            },
        })
        .await
        .expect("test secret should register");

    exoharness
        .put_binding(Binding::Llm {
            name: "gpt-5.4".to_string(),
            model: "gpt-5.4".to_string(),
            base_url: None,
            secret_id: Some(secret_id),
        })
        .await
        .expect("test model should register");
}
