//! Driver for the Groundhog-backend demo (`demo/groundhog-backend/demo.sh`).
//!
//! Each invocation is a fresh harness process, so every `read` after a `seed`
//! demonstrates restart-from-log. Subcommands:
//!
//! - `seed`             create an agent + conversation and one worked turn
//! - `read`             replay the conversation through the harness API
//! - `append <text>`    add one note event
//! - `retired-append`   raw-append to the predecessor identity (must fail)
//! - `lineage`          print the recorded succession chain
//!
//! Environment: `DEMO_ROOT` (harness root + state file), `EXO_GROUNDHOG_SOCKET`,
//! optional `EXO_GROUNDHOG_KERNEL_CONFIG` (enables kernel-bound identity).

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use exoharness::groundhog::{GroundhogClient, IngestBatch, IngestEvent, ReplayQuery};
use exoharness::{
    AddEventsRequest, BasicExoHarness, BasicExoHarnessConfig, ConversationHandle, EventData,
    ExoHarness, GroundhogStoreConfig, NewAgentRequest, NewConversationRequest,
    SandboxBackendRegistration, SandboxProvider, SecretBackendChoice, ToolRequest, Uuid7,
};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
struct DemoState {
    agent_id: Uuid7,
    conversation_id: Uuid7,
    first_event_id: Uuid7,
}

fn main() -> Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(run())
}

async fn run() -> Result<()> {
    let command = std::env::args().nth(1).unwrap_or_default();
    match command.as_str() {
        "seed" => seed().await,
        "read" => read().await,
        "seed-local" => seed_local().await,
        "read-local" => read_local().await,
        "append" => {
            let text = std::env::args().nth(2).context("append needs a note")?;
            append(&text).await
        }
        "retired-append" => retired_append().await,
        "lineage" => lineage().await,
        other => bail!("unknown subcommand {other:?}"),
    }
}

fn demo_root() -> Result<PathBuf> {
    Ok(PathBuf::from(
        std::env::var_os("DEMO_ROOT").context("DEMO_ROOT not set")?,
    ))
}

fn socket() -> Result<PathBuf> {
    Ok(PathBuf::from(
        std::env::var_os("EXO_GROUNDHOG_SOCKET").context("EXO_GROUNDHOG_SOCKET not set")?,
    ))
}

async fn harness() -> Result<BasicExoHarness> {
    BasicExoHarness::new(BasicExoHarnessConfig {
        root: demo_root()?.join("exoharness"),
        secret_backend: SecretBackendChoice::Static([7u8; 32]),
        sandbox_default: SandboxProvider::LocalProcess,
        sandbox_backends: vec![SandboxBackendRegistration::local_process()],
        groundhog: Some(GroundhogStoreConfig {
            socket: socket()?,
            source: "exo".to_string(),
            kernel_config: std::env::var_os("EXO_GROUNDHOG_KERNEL_CONFIG").map(PathBuf::from),
        }),
    })
    .await
}

fn state_path() -> Result<PathBuf> {
    Ok(demo_root()?.join("demo-state.json"))
}

async fn conversation(
    harness: &BasicExoHarness,
) -> Result<(DemoState, Arc<dyn ConversationHandle>)> {
    let state: DemoState = serde_json::from_slice(&std::fs::read(state_path()?)?)?;
    let agent = harness
        .get_agent(&state.agent_id)
        .await?
        .context("agent missing")?;
    let conversation = agent
        .get_conversation(&state.conversation_id)
        .await?
        .context("conversation missing")?;
    Ok((state, conversation))
}

async fn seed() -> Result<()> {
    let harness = harness().await?;
    let agent = harness
        .new_agent(NewAgentRequest {
            slug: "pilot".into(),
            name: "Pilot".into(),
        })
        .await?;
    let conversation = agent
        .new_conversation(NewConversationRequest::default())
        .await?;
    let added = conversation
        .add_events(AddEventsRequest {
            session_id: None,
            turn_id: None,
            data: vec![
                EventData::TurnStarted,
                EventData::ToolRequested {
                    tool_call_id: "call-1".into(),
                    response_id: None,
                    request: ToolRequest {
                        function_name: "web_search".into(),
                        arguments: serde_json::Map::from_iter([(
                            "query".to_string(),
                            serde_json::json!("groundhog event history engine"),
                        )]),
                    },
                },
                EventData::ToolResult {
                    tool_call_id: "call-1".into(),
                    result: serde_json::json!({"top_hit": "groundhog.so"}),
                },
                EventData::Custom {
                    event_type: "memory_note".into(),
                    payload: serde_json::json!({"note": "the user prefers audited tools"}),
                },
                EventData::TurnEnded,
            ],
        })
        .await?;
    let state = DemoState {
        agent_id: agent.record().id,
        conversation_id: conversation.record().id,
        first_event_id: added.event_ids[0],
    };
    std::fs::write(state_path()?, serde_json::to_vec_pretty(&state)?)?;
    println!(
        "seeded agent {} conversation {} with {} events",
        state.agent_id,
        state.conversation_id,
        added.event_ids.len()
    );
    Ok(())
}

async fn read() -> Result<()> {
    let harness = harness().await?;
    let (state, conversation) = conversation(&harness).await?;
    let events = conversation.get_events(None).await?;
    println!(
        "replayed {} events through the harness API:",
        events.events.len()
    );
    for event in &events.events {
        println!("  {}  {}", event.id, event.data.kind().as_str());
    }
    let looked_up = conversation.get_event(state.first_event_id).await?;
    println!(
        "point lookup of first event: {}",
        if looked_up.is_some() {
            "found"
        } else {
            "MISSING"
        }
    );
    Ok(())
}

async fn append(text: &str) -> Result<()> {
    let harness = harness().await?;
    let (_, conversation) = conversation(&harness).await?;
    conversation
        .add_events(AddEventsRequest {
            session_id: None,
            turn_id: None,
            data: vec![EventData::Custom {
                event_type: "memory_note".into(),
                payload: serde_json::json!({ "note": text }),
            }],
        })
        .await?;
    println!("appended note: {text}");
    Ok(())
}

/// The contrast case: exo's default store, one pretty-printed JSON file per
/// event under `events/`, with nothing that would notice an edit.
async fn local_harness() -> Result<BasicExoHarness> {
    BasicExoHarness::new(BasicExoHarnessConfig {
        root: demo_root()?.join("exoharness"),
        secret_backend: SecretBackendChoice::Static([7u8; 32]),
        sandbox_default: SandboxProvider::LocalProcess,
        sandbox_backends: vec![SandboxBackendRegistration::local_process()],
        groundhog: None,
    })
    .await
}

async fn seed_local() -> Result<()> {
    let harness = local_harness().await?;
    let agent = harness
        .new_agent(NewAgentRequest {
            slug: "pilot".into(),
            name: "Pilot".into(),
        })
        .await?;
    let conversation = agent
        .new_conversation(NewConversationRequest::default())
        .await?;
    let added = conversation
        .add_events(AddEventsRequest {
            session_id: None,
            turn_id: None,
            data: vec![EventData::Custom {
                event_type: "memory_note".into(),
                payload: serde_json::json!({"note": "the user prefers audited tools"}),
            }],
        })
        .await?;
    let state = DemoState {
        agent_id: agent.record().id,
        conversation_id: conversation.record().id,
        first_event_id: added.event_ids[0],
    };
    std::fs::write(state_path()?, serde_json::to_vec_pretty(&state)?)?;
    println!("seeded local-files conversation {}", state.conversation_id);
    Ok(())
}

async fn read_local() -> Result<()> {
    let harness = local_harness().await?;
    let (_, conversation) = conversation(&harness).await?;
    for event in conversation.get_events(None).await?.events {
        if let EventData::Custom {
            event_type,
            payload,
        } = &event.data
            && event_type == "memory_note"
        {
            println!("  memory_note: {}", payload["note"]);
        }
    }
    Ok(())
}

/// The source the current kernel config resolves to (same derivation as the
/// harness: `exo.k` + first 12 hex of the file's SHA-256).
fn current_source() -> Result<String> {
    use sha2::Digest;
    let path = std::env::var_os("EXO_GROUNDHOG_KERNEL_CONFIG")
        .context("EXO_GROUNDHOG_KERNEL_CONFIG not set")?;
    let digest = sha2::Sha256::digest(std::fs::read(PathBuf::from(path))?);
    let suffix: String = digest[..6]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    Ok(format!("exo.k{suffix}"))
}

/// Identity sources present in the log, oldest first.
async fn identity_sources(client: &GroundhogClient) -> Result<Vec<String>> {
    let mut sources: Vec<String> = client
        .streams(None)
        .await?
        .into_iter()
        .map(|info| info.source)
        .filter(|source| source.starts_with("exo.k"))
        .collect();
    sources.sort();
    sources.dedup();
    Ok(sources)
}

async fn retired_append() -> Result<()> {
    let client = GroundhogClient::new(socket()?);
    let current = current_source()?;
    let Some(retired) = identity_sources(&client)
        .await?
        .into_iter()
        .find(|source| *source != current)
    else {
        bail!("no predecessor identity in the log yet");
    };
    println!("appending to retired identity {retired} ...");
    let refused = client
        .append(IngestBatch {
            batch_id: "demo-tamper-append".into(),
            source: retired,
            events: vec![IngestEvent {
                stream: "x1".into(),
                record_key: "x1".into(),
                kind: "memory_note".into(),
                occurred_at: None,
                payload: serde_json::json!({"note": "history revision attempt"}),
            }],
            stream_precondition: None,
        })
        .await;
    match refused {
        Err(error) => {
            println!("engine refused: {error}");
            Ok(())
        }
        Ok(_) => bail!("the retired identity accepted an append; this is a bug"),
    }
}

async fn lineage() -> Result<()> {
    let client = GroundhogClient::new(socket()?);
    for source in identity_sources(&client).await? {
        let markers = client
            .replay_all(&ReplayQuery {
                source: Some(source.clone()),
                stream: Some(exoharness::groundhog::LINEAGE_STREAM.to_owned()),
                kind: Some(exoharness::groundhog::LINEAGE_KIND.to_owned()),
                ..Default::default()
            })
            .await?;
        match markers.first() {
            Some(envelope) => {
                let marker: exoharness::groundhog::LineageMarker =
                    serde_json::from_value(envelope.payload.clone())?;
                println!(
                    "{source}\n  succeeds {} at its final frontier {}",
                    marker.predecessor_source, marker.predecessor_final_frontier
                );
            }
            None => println!("{source}\n  first of its line"),
        }
    }
    Ok(())
}
