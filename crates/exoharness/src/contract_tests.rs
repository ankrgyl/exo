use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use futures::io::{AsyncReadExt, AsyncWriteExt};
use lingua::Message;
use lingua::universal::{AssistantContent, UserContent};
use tokio::time::timeout;
use tracing::info;

use crate::{
    AddEventsRequest, BeginTurnRequest, Binding, EventData, EventKind, EventQuery,
    EventQueryDirection, ExoHarness, ForkConversationRequest, ListConversationsRequest,
    ListThreadsRequest, ManagedSandboxBackend, ManagedSandboxHandle, NewAgentRequest,
    NewConversationRequest, NewThreadRequest, SandboxCommand, SandboxRequest, ThreadHandle, Uuid7,
    WriteArtifactRequest,
};

pub async fn supports_thread_api_and_conversation_compatibility(harness: Arc<dyn ExoHarness>) {
    let agent = harness
        .new_agent(NewAgentRequest {
            slug: unique_slug("agent"),
            name: "Agent".to_string(),
        })
        .await
        .expect("agent should be created");
    let thread: Arc<dyn ThreadHandle> = agent
        .new_thread(NewThreadRequest {
            slug: Some(unique_slug("thread")),
            name: Some("Thread".to_string()),
        })
        .await
        .expect("thread should be created");
    let thread_id = thread.record().id;
    let created = thread
        .get_events(None)
        .await
        .expect("thread events should load")
        .events
        .into_iter()
        .find(|event| matches!(event.data, EventData::ThreadCreated { .. }))
        .expect("thread creation event should use the compatible event schema");
    assert_eq!(created.thread_id, thread_id);
    assert_eq!(created.data.kind(), EventKind::THREAD_CREATED);
    for kind in [EventKind::THREAD_CREATED, EventKind::CONVERSATION_CREATED] {
        let events = thread
            .get_events(Some(EventQuery {
                types: Some(vec![kind]),
                ..Default::default()
            }))
            .await
            .expect("thread event filter should succeed")
            .events;
        assert!(
            events
                .iter()
                .any(|event| matches!(event.data, EventData::ThreadCreated { .. }))
        );
    }

    assert!(
        agent
            .get_conversation(&thread_id)
            .await
            .expect("legacy get conversation should succeed")
            .is_some()
    );
    assert!(
        agent
            .list_conversations(ListConversationsRequest::default())
            .await
            .expect("legacy list conversations should succeed")
            .conversations
            .iter()
            .any(|candidate| candidate.record().id == thread_id)
    );

    let conversation = agent
        .new_conversation(NewConversationRequest {
            slug: Some(unique_slug("conversation")),
            name: Some("Conversation".to_string()),
        })
        .await
        .expect("legacy conversation should be created");
    assert!(
        agent
            .get_thread(&conversation.record().id)
            .await
            .expect("get thread should read a legacy conversation")
            .is_some()
    );
    assert!(
        agent
            .list_threads(ListThreadsRequest::default())
            .await
            .expect("list threads should read legacy conversations")
            .threads
            .iter()
            .any(|candidate| candidate.record().id == conversation.record().id)
    );

    assert!(
        agent
            .delete_thread(&thread_id)
            .await
            .expect("delete thread should succeed")
    );
    assert!(
        agent
            .delete_conversation(&conversation.record().id)
            .await
            .expect("legacy delete conversation should succeed")
    );
}

pub async fn supports_agent_and_conversation_crud(harness: Arc<dyn ExoHarness>) {
    let agent_slug = unique_slug("agent");
    let conversation_slug = unique_slug("conversation");
    let agent = harness
        .new_agent(NewAgentRequest {
            slug: agent_slug.clone(),
            name: "Agent".to_string(),
        })
        .await
        .expect("agent should be created");
    let conversation = agent
        .new_conversation(NewConversationRequest {
            slug: Some(conversation_slug),
            name: Some("Conversation".to_string()),
        })
        .await
        .expect("conversation should be created");
    let events = conversation
        .get_events(None)
        .await
        .expect("get conversation events")
        .events;
    assert!(
        events
            .iter()
            .any(|event| matches!(event.data, EventData::ThreadCreated { .. }))
    );

    assert!(
        harness
            .list_agents()
            .await
            .expect("list agents")
            .iter()
            .any(|candidate| candidate.record().id == agent.record().id)
    );
    assert!(
        agent
            .list_conversations(crate::ListConversationsRequest::default())
            .await
            .expect("list conversations")
            .conversations
            .iter()
            .any(|candidate| candidate.record().id == conversation.record().id)
    );

    assert!(
        agent
            .delete_conversation(&conversation.record().id)
            .await
            .expect("delete conversation")
    );
    assert!(
        harness
            .delete_agent(&agent.record().id)
            .await
            .expect("delete agent")
    );

    // Deleting an agent must release its slug marker for reuse.
    let reused = harness
        .new_agent(NewAgentRequest {
            slug: agent_slug,
            name: "Agent".to_string(),
        })
        .await
        .expect("slug should be reusable after agent deletion");
    assert!(
        harness
            .delete_agent(&reused.record().id)
            .await
            .expect("delete reused agent")
    );
}

pub async fn list_conversations_returns_recent_first_and_paginates(harness: Arc<dyn ExoHarness>) {
    let agent = harness
        .new_agent(NewAgentRequest {
            slug: unique_slug("agent"),
            name: "Agent".to_string(),
        })
        .await
        .expect("agent should be created");
    let first = agent
        .new_conversation(NewConversationRequest {
            slug: Some(unique_slug("first")),
            name: Some("First".to_string()),
        })
        .await
        .expect("first conversation");
    tokio::time::sleep(Duration::from_millis(2)).await;
    let second = agent
        .new_conversation(NewConversationRequest {
            slug: Some(unique_slug("second")),
            name: Some("Second".to_string()),
        })
        .await
        .expect("second conversation");
    tokio::time::sleep(Duration::from_millis(2)).await;
    let third = agent
        .new_conversation(NewConversationRequest {
            slug: Some(unique_slug("third")),
            name: Some("Third".to_string()),
        })
        .await
        .expect("third conversation");
    tokio::time::sleep(Duration::from_millis(2)).await;
    first
        .add_events(AddEventsRequest {
            session_id: None,
            turn_id: None,
            data: vec![EventData::Custom {
                event_type: "touch".to_string(),
                payload: serde_json::Value::Null,
            }],
        })
        .await
        .expect("touch first conversation");

    let page = agent
        .list_conversations(ListConversationsRequest {
            cursor: None,
            limit: Some(2),
        })
        .await
        .expect("first page");
    let page_ids: Vec<_> = page
        .conversations
        .iter()
        .map(|conversation| conversation.record().id)
        .collect();
    assert_eq!(page_ids, vec![first.record().id, third.record().id]);
    assert_eq!(
        page.next_cursor,
        Some(third.record().latest_event_id.unwrap_or(third.record().id))
    );

    let next_page = agent
        .list_conversations(ListConversationsRequest {
            cursor: page.next_cursor,
            limit: Some(2),
        })
        .await
        .expect("second page");
    let next_page_ids: Vec<_> = next_page
        .conversations
        .iter()
        .map(|conversation| conversation.record().id)
        .collect();
    assert_eq!(next_page_ids, vec![second.record().id]);
    assert_eq!(next_page.next_cursor, None);
}

pub async fn begin_turn_tracks_events_through_finish(harness: Arc<dyn ExoHarness>) {
    let agent = harness
        .new_agent(NewAgentRequest {
            slug: unique_slug("agent"),
            name: "Agent".to_string(),
        })
        .await
        .expect("agent");
    let conversation = agent
        .new_conversation(NewConversationRequest::default())
        .await
        .expect("conversation");

    let turn = conversation
        .begin_turn(BeginTurnRequest {
            session_id: None,
            input: vec![user_message("ping")],
        })
        .await
        .expect("turn");
    turn.add_events(vec![EventData::Messages {
        messages: vec![assistant_message("pong")],
        response_id: None,
        usage: None,
    }])
    .await
    .expect("append assistant message");
    let latest_event_id = turn.finish().await.expect("finish turn");

    let events = conversation
        .get_events(Some(EventQuery {
            cursor: None,
            direction: Some(EventQueryDirection::Asc),
            limit: None,
            session_id: None,
            turn_id: Some(turn.record().id),
            types: None,
        }))
        .await
        .expect("get events")
        .events;

    assert!(
        events
            .iter()
            .any(|event| matches!(event.data, EventData::SessionStarted))
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event.data, EventData::TurnStarted))
    );
    assert!(
        events
            .iter()
            .filter(|event| matches!(event.data, EventData::Messages { .. }))
            .count()
            >= 2
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event.data, EventData::TurnEnded))
    );
    assert_eq!(events.last().expect("turn ended").id, latest_event_id);
}

pub async fn turn_events_continue_after_artifact_writes(harness: Arc<dyn ExoHarness>) {
    let agent = harness
        .new_agent(NewAgentRequest {
            slug: unique_slug("agent"),
            name: "Agent".to_string(),
        })
        .await
        .expect("agent");
    let conversation = agent
        .new_conversation(NewConversationRequest::default())
        .await
        .expect("conversation");

    let turn = conversation
        .begin_turn(BeginTurnRequest {
            session_id: None,
            input: vec![user_message("ping")],
        })
        .await
        .expect("turn");
    turn.write_artifact(WriteArtifactRequest {
        path: "tool-results/example.json".to_string(),
        contents: br#"{"ok":true}"#.to_vec(),
    })
    .await
    .expect("write artifact");
    turn.add_events(vec![EventData::Messages {
        messages: vec![assistant_message("pong")],
        response_id: None,
        usage: None,
    }])
    .await
    .expect("append after artifact write");
    turn.finish().await.expect("finish after artifact write");

    let events = conversation
        .get_events(Some(EventQuery {
            cursor: None,
            direction: Some(EventQueryDirection::Asc),
            limit: None,
            session_id: None,
            turn_id: None,
            types: Some(vec![EventKind::ARTIFACT_WRITTEN]),
        }))
        .await
        .expect("artifact event")
        .events;
    let artifact_event = events.first().expect("artifact_written event");
    assert_eq!(artifact_event.session_id, Some(turn.record().session_id));
    assert_eq!(artifact_event.turn_id, Some(turn.record().id));
}

pub async fn conversation_scope_overrides_agent_scope_and_fork_copies_bindings(
    harness: Arc<dyn ExoHarness>,
) {
    let agent = harness
        .new_agent(NewAgentRequest {
            slug: unique_slug("agent"),
            name: "Agent".to_string(),
        })
        .await
        .expect("agent");
    let conversation = agent
        .new_conversation(NewConversationRequest {
            slug: Some(unique_slug("base")),
            name: Some("Base".to_string()),
        })
        .await
        .expect("conversation");

    agent
        .put_binding(Binding::Env {
            name: "OPENAI_API_KEY".to_string(),
            env_var: "OPENAI_API_KEY".to_string(),
            secret_id: Uuid7::now(),
        })
        .await
        .expect("agent binding");

    let conversation_binding_id = conversation
        .put_binding(Binding::Env {
            name: "OPENAI_API_KEY".to_string(),
            env_var: "OPENAI_API_KEY".to_string(),
            secret_id: Uuid7::now(),
        })
        .await
        .expect("conversation binding");

    let effective_binding = conversation
        .list_bindings()
        .await
        .expect("list bindings")
        .into_iter()
        .find(|binding| binding.name == "OPENAI_API_KEY")
        .expect("effective binding");
    assert_eq!(effective_binding.id, conversation_binding_id);

    let forked = conversation
        .fork(ForkConversationRequest {
            up_to_inclusive: None,
            slug: Some(unique_slug("fork")),
            name: Some("Fork".to_string()),
        })
        .await
        .expect("fork");
    let forked_binding = forked
        .list_bindings()
        .await
        .expect("list forked bindings")
        .into_iter()
        .find(|binding| binding.name == "OPENAI_API_KEY")
        .expect("forked effective binding");
    assert_eq!(forked_binding.name, "OPENAI_API_KEY");
    let events = forked
        .get_events(Some(EventQuery {
            cursor: None,
            direction: Some(EventQueryDirection::Asc),
            limit: None,
            session_id: None,
            turn_id: None,
            types: None,
        }))
        .await
        .expect("get forked events")
        .events;
    assert!(
        events
            .iter()
            .any(|event| matches!(event.data, EventData::ThreadForked { .. }))
    );
}

pub async fn sandbox_handle_start_process_supports_interactive_stdio_and_env(
    handle: Arc<dyn ManagedSandboxHandle>,
) -> crate::Result<()> {
    let result =
        sandbox_handle_start_process_supports_interactive_stdio_and_env_inner(Arc::clone(&handle))
            .await;
    let stop_result = handle.stop().await.context("stop sandbox after contract");
    match (result, stop_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) => Err(error),
        (Ok(()), Err(error)) => Err(error),
        (Err(error), Err(stop_error)) => Err(anyhow!(
            "{error:#}; also failed to stop sandbox after contract: {stop_error:#}"
        )),
    }
}

pub async fn sandbox_handle_start_process_supports_long_running_request_response_protocol(
    handle: Arc<dyn ManagedSandboxHandle>,
) -> crate::Result<()> {
    let result =
        sandbox_handle_start_process_supports_long_running_request_response_protocol_inner(
            Arc::clone(&handle),
        )
        .await;
    let stop_result = handle.stop().await.context("stop sandbox after contract");
    match (result, stop_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) => Err(error),
        (Ok(()), Err(error)) => Err(error),
        (Err(error), Err(stop_error)) => Err(anyhow!(
            "{error:#}; also failed to stop sandbox after contract: {stop_error:#}"
        )),
    }
}

pub async fn sandbox_backend_durable_file_system_survives_stop_and_reacquire(
    backend: Arc<dyn ManagedSandboxBackend>,
    request: SandboxRequest,
) -> crate::Result<()> {
    let mount_path = request
        .spec
        .durable_file_systems
        .first()
        .context("durable filesystem contract requires a durable filesystem")?
        .mount_path
        .clone();
    let marker = format!("durable-{}", Uuid7::now());

    let first = backend
        .acquire(request.clone())
        .await
        .context("acquire sandbox for durable filesystem write")?;
    let write_result = write_durable_marker(Arc::clone(&first), &mount_path, &marker).await;
    stop_after_contract(
        first,
        write_result,
        "stop sandbox after durable filesystem write",
    )
    .await?;

    let second = backend
        .acquire(request)
        .await
        .context("reacquire sandbox for durable filesystem read")?;
    let read_result = read_durable_marker(Arc::clone(&second), &mount_path, &marker).await;
    stop_after_contract(
        second,
        read_result,
        "stop sandbox after durable filesystem read",
    )
    .await
}

pub async fn sandbox_backend_workdir_survives_stop_and_reacquire(
    backend: Arc<dyn ManagedSandboxBackend>,
    request: SandboxRequest,
) -> crate::Result<()> {
    let mount_path = request.spec.default_workdir.clone();
    let marker = format!("workdir-{}", Uuid7::now());

    let first = backend
        .acquire(request.clone())
        .await
        .context("acquire sandbox for workdir write")?;
    let write_result = write_durable_marker(Arc::clone(&first), &mount_path, &marker).await;
    stop_after_contract(first, write_result, "stop sandbox after workdir write").await?;

    let second = backend
        .acquire(request)
        .await
        .context("reacquire sandbox for workdir read")?;
    let read_result = read_durable_marker(Arc::clone(&second), &mount_path, &marker).await;
    stop_after_contract(second, read_result, "stop sandbox after workdir read").await
}

pub async fn sandbox_backend_long_running_process_and_workdir_survive_stop_and_reacquire(
    backend: Arc<dyn ManagedSandboxBackend>,
    request: SandboxRequest,
) -> crate::Result<()> {
    let mount_path = request.spec.default_workdir.clone();
    let marker = format!("protocol-workdir-{}", Uuid7::now());

    info!("contract: acquiring sandbox for initial protocol/workdir check");
    let first = backend
        .acquire(request.clone())
        .await
        .context("acquire sandbox for protocol and workdir write")?;
    info!("contract: acquired sandbox for initial protocol/workdir check");
    let first_result = async {
        info!("contract: starting initial long-running protocol check");
        sandbox_handle_start_process_supports_long_running_request_response_protocol_inner(
            Arc::clone(&first),
        )
        .await?;
        info!("contract: initial protocol check complete; writing durable marker");
        write_durable_marker(Arc::clone(&first), &mount_path, &marker).await
    }
    .await;
    info!("contract: stopping sandbox after initial protocol/workdir check");
    stop_after_contract(
        first,
        first_result,
        "stop sandbox after protocol and workdir write",
    )
    .await?;

    info!("contract: reacquiring sandbox for resumed protocol/workdir check");
    let second = backend
        .acquire(request)
        .await
        .context("reacquire sandbox for protocol and workdir read")?;
    info!("contract: reacquired sandbox for resumed protocol/workdir check");
    let second_result = async {
        info!("contract: reading durable marker after resume");
        read_durable_marker(Arc::clone(&second), &mount_path, &marker).await?;
        info!("contract: starting resumed long-running protocol check");
        sandbox_handle_start_process_supports_long_running_request_response_protocol_inner(
            Arc::clone(&second),
        )
        .await
    }
    .await;
    info!("contract: stopping sandbox after resumed protocol/workdir check");
    stop_after_contract(
        second,
        second_result,
        "stop sandbox after protocol and workdir read",
    )
    .await
}

async fn sandbox_handle_start_process_supports_interactive_stdio_and_env_inner(
    handle: Arc<dyn ManagedSandboxHandle>,
) -> crate::Result<()> {
    let mut env = std::collections::HashMap::new();
    env.insert(
        "EXO_CONTRACT_ENV".to_string(),
        "contract-env-value".to_string(),
    );
    let mut process = handle
        .start_process(&SandboxCommand {
            argv: vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                "printf 'ready\\n'; IFS= read -r line; printf 'env=%s input=%s\\n' \"$EXO_CONTRACT_ENV\" \"$line\"".to_string(),
            ],
            env,
            display_argv: None,
            cwd: None,
            timeout: Some(Duration::from_secs(30)),
        })
        .await
        .context("start_process should start before the command exits")?;

    let mut ready = [0u8; 6];
    timeout(
        Duration::from_secs(10),
        process.stdout.read_exact(&mut ready),
    )
    .await
    .context("process should stream initial stdout before stdin is written")?
    .context("read ready marker")?;
    if &ready != b"ready\n" {
        bail!(
            "unexpected ready marker: {:?}",
            String::from_utf8_lossy(&ready)
        );
    }

    process
        .stdin
        .write_all(b"contract-stdin-value\n")
        .await
        .context("write process stdin")?;
    process.stdin.close().await.context("close process stdin")?;

    let expected_stdout = "env=contract-env-value input=contract-stdin-value\n";
    let mut final_stdout = vec![0u8; expected_stdout.len()];
    timeout(
        Duration::from_secs(10),
        process.stdout.read_exact(&mut final_stdout),
    )
    .await
    .context("process should stream stdout after stdin is written")?
    .context("read final stdout")?;
    let final_stdout = String::from_utf8(final_stdout).context("final stdout should be UTF-8")?;
    if final_stdout != expected_stdout {
        bail!("unexpected stdout: {final_stdout:?}");
    }

    let exit_code = timeout(Duration::from_secs(30), process.wait)
        .await
        .with_context(|| format!("process wait should finish after final stdout {final_stdout:?}"))?
        .context("process wait should succeed")?;

    let mut stderr = String::new();
    timeout(
        Duration::from_secs(5),
        process.stderr.read_to_string(&mut stderr),
    )
    .await
    .context("stderr should drain after process exit")?
    .context("read stderr")?;
    if exit_code != 0 {
        bail!("unexpected process exit code: {exit_code}; stderr: {stderr:?}");
    }
    if !stderr.is_empty() {
        bail!("unexpected stderr: {stderr:?}");
    }
    Ok(())
}

async fn write_durable_marker(
    handle: Arc<dyn ManagedSandboxHandle>,
    mount_path: &str,
    marker: &str,
) -> crate::Result<()> {
    let mut env = std::collections::HashMap::new();
    env.insert("EXO_DURABLE_MOUNT".to_string(), mount_path.to_string());
    env.insert("EXO_DURABLE_MARKER".to_string(), marker.to_string());
    let output = handle
        .exec(&SandboxCommand {
            argv: vec![
                "/bin/sh".to_string(),
                "-lc".to_string(),
                "test \"$(pwd)\" = \"$EXO_DURABLE_MOUNT\" && mkdir -p .codex-smoke && printf '%s' \"$EXO_DURABLE_MARKER\" > .codex-smoke/marker.txt"
                    .to_string(),
            ],
            env,
            display_argv: None,
            cwd: None,
            timeout: Some(Duration::from_secs(30)),
        })
        .await
        .context("write durable filesystem marker")?;
    if !output.ok {
        bail!(
            "write durable filesystem marker failed with exit code {:?}: {}{}",
            output.exit_code,
            output.stdout,
            output.stderr
        );
    }
    Ok(())
}

async fn read_durable_marker(
    handle: Arc<dyn ManagedSandboxHandle>,
    mount_path: &str,
    expected_marker: &str,
) -> crate::Result<()> {
    let mut env = std::collections::HashMap::new();
    env.insert("EXO_DURABLE_MOUNT".to_string(), mount_path.to_string());
    let output = handle
        .exec(&SandboxCommand {
            argv: vec![
                "/bin/sh".to_string(),
                "-lc".to_string(),
                "test \"$(pwd)\" = \"$EXO_DURABLE_MOUNT\" && cat .codex-smoke/marker.txt"
                    .to_string(),
            ],
            env,
            display_argv: None,
            cwd: None,
            timeout: Some(Duration::from_secs(30)),
        })
        .await
        .context("read durable filesystem marker")?;
    if !output.ok {
        bail!(
            "read durable filesystem marker failed with exit code {:?}: {}{}",
            output.exit_code,
            output.stdout,
            output.stderr
        );
    }
    if output.stdout != expected_marker {
        bail!(
            "durable filesystem marker mismatch: expected {:?}, got {:?}",
            expected_marker,
            output.stdout
        );
    }
    Ok(())
}

async fn stop_after_contract(
    handle: Arc<dyn ManagedSandboxHandle>,
    result: crate::Result<()>,
    context: &str,
) -> crate::Result<()> {
    let stop_result = handle.stop().await.with_context(|| context.to_string());
    match (result, stop_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) => Err(error),
        (Ok(()), Err(error)) => Err(error),
        (Err(error), Err(stop_error)) => Err(anyhow!(
            "{error:#}; also failed to stop sandbox after contract: {stop_error:#}"
        )),
    }
}

async fn sandbox_handle_start_process_supports_long_running_request_response_protocol_inner(
    handle: Arc<dyn ManagedSandboxHandle>,
) -> crate::Result<()> {
    info!("contract protocol: start_process");
    let mut process = handle
        .start_process(&SandboxCommand {
            argv: vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                [
                    "printf 'protocol-ready\\n'",
                    "while IFS= read -r line; do",
                    "  case \"$line\" in",
                    "    request-one) printf 'response-one\\n' ;;",
                    "    request-two) printf 'protocol-stderr-two\\n' >&2; printf 'response-two\\n' ;;",
                    "    request-three) printf 'response-three\\n'; exit 0 ;;",
                    "    *) printf 'unexpected:%s\\n' \"$line\"; exit 9 ;;",
                    "  esac",
                    "done",
                    "exit 8",
                ]
                .join("\n"),
            ],
            env: std::collections::HashMap::new(),
            display_argv: None,
            cwd: None,
            timeout: Some(Duration::from_secs(30)),
        })
        .await
        .context("start_process should start a long-running protocol process")?;

    info!("contract protocol: waiting for ready marker");
    read_exact_text(
        &mut process.stdout,
        "protocol-ready\n",
        "read protocol ready marker",
    )
    .await?;

    info!("contract protocol: writing request one");
    process
        .stdin
        .write_all(b"request-one\n")
        .await
        .context("write first protocol request")?;
    info!("contract protocol: waiting for response one");
    read_exact_text(
        &mut process.stdout,
        "response-one\n",
        "read first protocol response",
    )
    .await?;

    info!("contract protocol: writing request two");
    process
        .stdin
        .write_all(b"request-two\n")
        .await
        .context("write second protocol request")?;
    info!("contract protocol: waiting for response two");
    read_exact_text(
        &mut process.stdout,
        "response-two\n",
        "read second protocol response",
    )
    .await?;

    info!("contract protocol: writing request three");
    process
        .stdin
        .write_all(b"request-three\n")
        .await
        .context("write shutdown protocol request")?;
    info!("contract protocol: closing stdin");
    process.stdin.close().await.context("close process stdin")?;
    info!("contract protocol: waiting for response three");
    read_exact_text(
        &mut process.stdout,
        "response-three\n",
        "read shutdown protocol response",
    )
    .await?;

    info!("contract protocol: waiting for process exit");
    let exit_code = timeout(Duration::from_secs(30), process.wait)
        .await
        .context("protocol process wait should finish after shutdown")?
        .context("protocol process wait should succeed")?;
    info!(
        exit_code,
        "contract protocol: process exited; draining stderr"
    );
    let mut stderr = String::new();
    timeout(
        Duration::from_secs(5),
        process.stderr.read_to_string(&mut stderr),
    )
    .await
    .context("stderr should drain after protocol process exit")?
    .context("read protocol stderr")?;
    if exit_code != 0 {
        bail!("unexpected protocol process exit code: {exit_code}; stderr: {stderr:?}");
    }
    if stderr != "protocol-stderr-two\n" {
        bail!("unexpected protocol stderr: {stderr:?}");
    }
    info!("contract protocol: complete");
    Ok(())
}

async fn read_exact_text(
    reader: &mut (impl futures::io::AsyncRead + Unpin),
    expected: &str,
    context: &str,
) -> crate::Result<()> {
    let mut bytes = vec![0u8; expected.len()];
    timeout(Duration::from_secs(10), reader.read_exact(&mut bytes))
        .await
        .with_context(|| context.to_string())?
        .with_context(|| context.to_string())?;
    let actual = String::from_utf8(bytes).with_context(|| format!("{context}: invalid UTF-8"))?;
    if actual != expected {
        bail!("{context}: expected {expected:?}, got {actual:?}");
    }
    Ok(())
}

fn user_message(text: &str) -> Message {
    Message::User {
        content: UserContent::String(text.to_string()),
    }
}

fn assistant_message(text: &str) -> Message {
    Message::Assistant {
        id: None,
        content: AssistantContent::String(text.to_string()),
    }
}

fn unique_slug(prefix: &str) -> String {
    format!("{prefix}-{}", Uuid7::now())
}
