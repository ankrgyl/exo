use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use exoharness::{AgentHandle, ConversationHandle, Secret, SecretId};
use serde::Deserialize;
use tokio::sync::Notify;
use tokio::task::JoinSet;

use base64::Engine;
use lingua::universal::{TextContentPart, UserContent, UserContentPart};

use super::store::{AdapterStore, stable_target_key};
use super::tools::download_attachment;
use super::trial::{prepare_trial_run, trial_started};
use super::types::{
    AdapterAttachment, AdapterAttachmentKind, AdapterConfig, AdapterDeliveryStatus,
    AdapterEventType, AdapterRecord, AdapterTargetConversationRecord, now_ms,
};
use super::worker::{WorkerCommand, WorkerEvent, run_worker_loop};
use crate::conversation_events::{
    HOST_EVENT_ADAPTER_RUNNER_DRAINING, HOST_EVENT_ADAPTER_RUNNER_STARTED, HOST_EVENT_REBOOT,
    record_host_event,
};
use crate::conversation_wakeup::{send_conversation_wakeup, send_conversation_wakeup_content};
use crate::{CreateConversationRequest, Harness, HarnessAgent, HarnessConversation, SandboxScope};

const INITIAL_RESTART_DELAY: Duration = Duration::from_secs(5);
const MAX_RESTART_DELAY: Duration = Duration::from_secs(300);
// A worker that survived this long was healthy; its next failure starts the
// backoff schedule over instead of inheriting a stale long delay.
const STABLE_RUN_THRESHOLD: Duration = Duration::from_secs(60);

// A reboot notice older than this is stale (e.g. left behind by a runner that
// crashed before claiming it) and should be dropped rather than announced.
const REBOOT_NOTICE_MAX_AGE: Duration = Duration::from_secs(15 * 60);

#[derive(Debug, Clone)]
pub struct AdapterRunOptions {
    pub limit: usize,
    /// When this file appears, the runner claims it (removes the file), stops
    /// starting new work, lets in-flight wakeup turns finish, and exits so a
    /// supervisor can restart it on a fresh build.
    pub drain_marker: Option<PathBuf>,
    /// Written by the service guardian when it restarts the adapter runner.
    /// A fresh runner claims it and wakes the adapter conversations so the
    /// agent can announce externally that it is back up.
    pub reboot_notice: Option<PathBuf>,
}

impl Default for AdapterRunOptions {
    fn default() -> Self {
        Self {
            limit: 10,
            drain_marker: None,
            reboot_notice: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RebootNotice {
    #[serde(default)]
    pub requested_at: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
}

pub async fn run_adapters_watch(
    harness: Arc<dyn Harness>,
    store: AdapterStore,
    options: AdapterRunOptions,
) -> Result<()> {
    let running = Arc::new(Mutex::new(HashSet::<String>::new()));
    let drain = Arc::new(AtomicBool::new(false));
    let mut supervisors = JoinSet::new();
    if let Some(notice) = claim_reboot_notice(options.reboot_notice.as_deref()) {
        // The wakeup turn can take minutes; run it in the background so the
        // adapter workers (and the connections the agent will announce on)
        // start immediately. The durable outbox holds any announcement until
        // the relevant worker is connected.
        let harness = Arc::clone(&harness);
        let store = store.clone();
        tokio::spawn(async move {
            if let Err(error) = announce_reboot(harness, store, notice).await {
                tracing::error!(%error, "failed to announce adapter runner reboot");
            }
        });
    }
    {
        // Record the runner start in the canonical conversation event log. A
        // start without a preceding host_reboot event implies an unplanned
        // restart. Background so worker startup is not delayed.
        let harness = Arc::clone(&harness);
        let store = store.clone();
        tokio::spawn(async move {
            record_host_event_for_adapter_conversations(
                harness.as_ref(),
                &store,
                HOST_EVENT_ADAPTER_RUNNER_STARTED,
                serde_json::json!({ "pid": std::process::id() }),
            )
            .await;
        });
    }
    loop {
        if claim_drain_marker(options.drain_marker.as_deref()) {
            tracing::info!("adapter runner drain requested; waiting for in-flight work");
            drain.store(true, Ordering::SeqCst);
            record_host_event_for_adapter_conversations(
                harness.as_ref(),
                &store,
                HOST_EVENT_ADAPTER_RUNNER_DRAINING,
                serde_json::json!({ "pid": std::process::id() }),
            )
            .await;
            break;
        }
        let adapters = store.enabled_adapters().await?;
        for adapter in adapters.into_iter().take(options.limit) {
            if !running
                .lock()
                .expect("adapter running set poisoned")
                .insert(adapter.id.clone())
            {
                continue;
            }
            let harness = Arc::clone(&harness);
            let store = store.clone();
            let running = Arc::clone(&running);
            let drain = Arc::clone(&drain);
            supervisors.spawn(async move {
                let adapter_id = adapter.id.clone();
                supervise_adapter(harness, store, adapter, drain).await;
                running
                    .lock()
                    .expect("adapter running set poisoned")
                    .remove(&adapter_id);
            });
        }
        // Reap finished supervision tasks so the JoinSet does not grow
        // unboundedly while the runner stays up.
        while supervisors.try_join_next().is_some() {}
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
    while supervisors.join_next().await.is_some() {}
    tracing::info!("adapter runner drained; exiting for restart");
    Ok(())
}

fn claim_drain_marker(marker: Option<&std::path::Path>) -> bool {
    let Some(marker) = marker else {
        return false;
    };
    std::fs::remove_file(marker).is_ok()
}

fn claim_reboot_notice(notice_path: Option<&std::path::Path>) -> Option<RebootNotice> {
    let notice_path = notice_path?;
    let contents = std::fs::read_to_string(notice_path).ok()?;
    let age = std::fs::metadata(notice_path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| modified.elapsed().ok());
    if std::fs::remove_file(notice_path).is_err() {
        // Another claimant got there first.
        return None;
    }
    match age {
        Some(age) if age <= REBOOT_NOTICE_MAX_AGE => {}
        _ => {
            tracing::warn!(
                path = %notice_path.display(),
                "ignoring stale reboot notice"
            );
            return None;
        }
    }
    match serde_json::from_str::<RebootNotice>(&contents) {
        Ok(notice) => Some(notice),
        Err(error) => {
            tracing::warn!(
                path = %notice_path.display(),
                %error,
                "failed to parse reboot notice"
            );
            None
        }
    }
}

/// Appends a host event to the canonical event log of each distinct
/// conversation that has an enabled adapter. Failures are logged per
/// conversation rather than propagated: a missing conversation must not block
/// the event from reaching the others.
async fn record_host_event_for_adapter_conversations(
    harness: &dyn Harness,
    store: &AdapterStore,
    event_type: &str,
    payload: serde_json::Value,
) {
    let adapters = match store.enabled_adapters().await {
        Ok(adapters) => adapters,
        Err(error) => {
            tracing::error!(%error, event_type, "failed to list adapters for host event");
            return;
        }
    };
    let mut seen = HashSet::new();
    for adapter in &adapters {
        if !seen.insert((adapter.agent_id.clone(), adapter.conversation_id.clone())) {
            continue;
        }
        let result = async {
            let agent = require_agent(harness, adapter).await?;
            let conversation = require_conversation(agent.as_ref(), adapter).await?;
            record_host_event(
                conversation.exoharness_handle().as_ref(),
                event_type,
                payload.clone(),
            )
            .await
        }
        .await;
        if let Err(error) = result {
            tracing::error!(
                conversation_id = %adapter.conversation_id,
                %error,
                event_type,
                "failed to record host event"
            );
        }
    }
}

async fn announce_reboot(
    harness: Arc<dyn Harness>,
    store: AdapterStore,
    notice: RebootNotice,
) -> Result<()> {
    let adapters = store.enabled_adapters().await?;
    let mut woken = HashSet::new();
    let reason = notice.reason.as_deref().unwrap_or("unspecified");
    let requested_at = notice.requested_at.as_deref().unwrap_or("unknown time");
    for adapter in &adapters {
        if !woken.insert((adapter.agent_id.clone(), adapter.conversation_id.clone())) {
            continue;
        }
        let adapter_names = adapters
            .iter()
            .filter(|candidate| {
                candidate.agent_id == adapter.agent_id
                    && candidate.conversation_id == adapter.conversation_id
            })
            .map(|candidate| format!("`{}` ({})", candidate.name, candidate.config.adapter_type))
            .collect::<Vec<_>>()
            .join(", ");
        let agent = require_agent(harness.as_ref(), adapter).await?;
        let conversation = require_conversation(agent.as_ref(), adapter).await?;
        // Record the reboot in the canonical event log before the wakeup turn
        // so the immutable history exists even if the announcement turn fails.
        if let Err(error) = record_host_event(
            conversation.exoharness_handle().as_ref(),
            HOST_EVENT_REBOOT,
            serde_json::json!({
                "reason": notice.reason,
                "requestedAt": notice.requested_at,
            }),
        )
        .await
        {
            tracing::error!(
                conversation_id = %adapter.conversation_id,
                %error,
                "failed to record host_reboot event"
            );
        }
        send_conversation_wakeup(
            conversation.as_ref(),
            format!(
                "Host services were restarted (reason: {reason}, requested at {requested_at}) and the adapter runner is back up. Adapter workers for {adapter_names} are reconnecting now. If you announced this reboot externally, or external users should know you are back, announce your return with send_adapter_message on the relevant adapters and targets; outbound messages queue durably and deliver once the adapter reconnects. If no announcement is appropriate, do nothing.",
            ),
        )
        .await
        .with_context(|| {
            format!(
                "reboot announcement wakeup failed for conversation {}",
                adapter.conversation_id
            )
        })?;
    }
    Ok(())
}

async fn supervise_adapter(
    harness: Arc<dyn Harness>,
    store: AdapterStore,
    adapter: AdapterRecord,
    drain: Arc<AtomicBool>,
) {
    let mut restart_delay = INITIAL_RESTART_DELAY;
    loop {
        if drain.load(Ordering::SeqCst) {
            break;
        }
        match store.get_adapter(&adapter.id).await {
            Ok(Some(current)) if current.enabled => {}
            Ok(Some(_)) => {
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }
            Ok(_) => break,
            Err(error) => {
                tracing::error!(
                    adapter_id = %adapter.id,
                    %error,
                    "failed to read adapter record; retrying"
                );
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }
        }
        let started_at = Instant::now();
        if let Err(error) = run_adapter_loop(
            Arc::clone(&harness),
            &store,
            adapter.clone(),
            Arc::clone(&drain),
        )
        .await
        {
            if started_at.elapsed() >= STABLE_RUN_THRESHOLD {
                restart_delay = INITIAL_RESTART_DELAY;
            }
            tracing::error!(
                adapter_id = %adapter.id,
                %error,
                "adapter runtime error; restarting in {}s",
                restart_delay.as_secs()
            );
            if let Err(mark_error) = store.mark_error(&adapter.id, error.to_string()).await {
                tracing::error!(
                    adapter_id = %adapter.id,
                    error = %mark_error,
                    "failed to mark adapter error"
                );
            }
            if let Err(record_error) = store
                .record_event(
                    adapter.id.clone(),
                    AdapterEventType::Error,
                    error.to_string(),
                )
                .await
            {
                tracing::error!(
                    adapter_id = %adapter.id,
                    error = %record_error,
                    "failed to record adapter error"
                );
            }
            tokio::time::sleep(restart_delay).await;
            restart_delay = (restart_delay * 2).min(MAX_RESTART_DELAY);
            continue;
        }
        restart_delay = INITIAL_RESTART_DELAY;
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

pub async fn send_adapter_message_with_handles(
    _agent: &dyn AgentHandle,
    _conversation: &dyn ConversationHandle,
    store: &AdapterStore,
    adapter: &AdapterRecord,
    text: &str,
    target: Option<&str>,
    attachments: Vec<AdapterAttachment>,
) -> Result<String> {
    queue_adapter_message(store, adapter, text, target, attachments).await
}

async fn queue_adapter_message(
    store: &AdapterStore,
    adapter: &AdapterRecord,
    text: &str,
    target: Option<&str>,
    attachments: Vec<AdapterAttachment>,
) -> Result<String> {
    if !adapter.enabled {
        bail!("adapter is disabled: {}", adapter.id);
    }
    if !attachments.is_empty()
        && adapter.config.adapter_type != "whatsapp"
        && adapter.config.adapter_type != "signal"
        && adapter.config.adapter_type != "discord"
    {
        bail!(
            "adapter {} does not support rich attachments",
            adapter.config.adapter_type
        );
    }
    // Note: we intentionally do not write a conversation artifact here.
    // The outbound message is durably queued in the AdapterStore outbox, and
    // the event below records it for audit without adding adapter bookkeeping
    // to the agent-visible conversation history.
    let message = store
        .enqueue_outbound_message(
            adapter.id.clone(),
            text.to_string(),
            target.map(ToOwned::to_owned),
            attachments,
        )
        .await?;
    store
        .record_event(
            adapter.id.clone(),
            AdapterEventType::Outbound,
            format!(
                "queued {} adapter message {}{}",
                adapter.config.adapter_type,
                message.id,
                target.map(|t| format!(" to {t}")).unwrap_or_default(),
            ),
        )
        .await?;
    notify_adapter_outbound(&adapter.id);
    Ok(message.id)
}

async fn run_adapter_loop(
    harness: Arc<dyn Harness>,
    store: &AdapterStore,
    adapter: AdapterRecord,
    drain: Arc<AtomicBool>,
) -> Result<()> {
    let agent = require_agent(harness.as_ref(), &adapter).await?;
    let conversation = require_conversation(agent.as_ref(), &adapter).await?;
    store.requeue_inflight_messages(&adapter.id).await?;
    let config = adapter.config.clone();
    let secret_env = worker_secret_env(agent.exoharness_handle().as_ref(), &config).await?;
    let outbound_notifier = register_adapter_outbound_notifier(&adapter.id);
    let event_store = store.clone();
    let event_adapter = adapter.clone();
    let event_agent = std::sync::Arc::clone(&agent);
    let event_conversation = std::sync::Arc::clone(&conversation);
    let event_config = config.clone();
    let outbound_store = store.clone();
    let outbound_adapter_id = adapter.id.clone();
    let stop_store = store.clone();
    let stop_adapter_id = adapter.id.clone();
    run_worker_loop(
        &adapter.id,
        &config,
        secret_env,
        Arc::clone(&outbound_notifier.notify),
        move |event| {
            let store = event_store.clone();
            let adapter = event_adapter.clone();
            let agent = std::sync::Arc::clone(&event_agent);
            let conversation = std::sync::Arc::clone(&event_conversation);
            let config = event_config.clone();
            async move {
                handle_worker_event(
                    &store,
                    agent.as_ref(),
                    conversation,
                    &adapter,
                    &config,
                    event,
                )
                .await
            }
        },
        move || {
            let store = outbound_store.clone();
            let adapter_id = outbound_adapter_id.clone();
            async move {
                Ok(store
                    .claim_outbound_messages(&adapter_id)
                    .await?
                    .into_iter()
                    .map(|message| WorkerCommand::SendMessage {
                        id: message.id,
                        target: message.target,
                        text: message.text,
                        attachments: message.attachments,
                    })
                    .collect())
            }
        },
        move || {
            let store = stop_store.clone();
            let adapter_id = stop_adapter_id.clone();
            let drain = Arc::clone(&drain);
            async move {
                if drain.load(Ordering::SeqCst) {
                    return Ok(true);
                }
                Ok(store
                    .get_adapter(&adapter_id)
                    .await?
                    .is_none_or(|adapter| !adapter.enabled))
            }
        },
    )
    .await
}

struct AdapterOutboundNotifierGuard {
    adapter_id: String,
    notify: Arc<Notify>,
}

impl Drop for AdapterOutboundNotifierGuard {
    fn drop(&mut self) {
        let mut notifiers = adapter_outbound_notifiers()
            .lock()
            .expect("adapter outbound notifier registry poisoned");
        if let Some(current) = notifiers.get(&self.adapter_id).and_then(Weak::upgrade)
            && Arc::ptr_eq(&current, &self.notify)
        {
            notifiers.remove(&self.adapter_id);
        }
    }
}

fn register_adapter_outbound_notifier(adapter_id: &str) -> AdapterOutboundNotifierGuard {
    let notify = Arc::new(Notify::new());
    adapter_outbound_notifiers()
        .lock()
        .expect("adapter outbound notifier registry poisoned")
        .insert(adapter_id.to_string(), Arc::downgrade(&notify));
    AdapterOutboundNotifierGuard {
        adapter_id: adapter_id.to_string(),
        notify,
    }
}

fn notify_adapter_outbound(adapter_id: &str) {
    let notify = adapter_outbound_notifiers()
        .lock()
        .expect("adapter outbound notifier registry poisoned")
        .get(adapter_id)
        .and_then(Weak::upgrade);
    if let Some(notify) = notify {
        notify.notify_one();
    }
}

fn adapter_outbound_notifiers() -> &'static Mutex<HashMap<String, Weak<Notify>>> {
    static NOTIFIERS: OnceLock<Mutex<HashMap<String, Weak<Notify>>>> = OnceLock::new();
    NOTIFIERS.get_or_init(|| Mutex::new(HashMap::new()))
}

async fn handle_worker_event(
    store: &AdapterStore,
    agent: &dyn HarnessAgent,
    root_conversation: Arc<dyn HarnessConversation>,
    adapter: &AdapterRecord,
    config: &AdapterConfig,
    event: WorkerEvent,
) -> Result<()> {
    match event {
        WorkerEvent::Connected { subject, metadata } => {
            store.mark_connected(&adapter.id).await?;
            record_worker_lifecycle(
                store,
                root_conversation.as_ref(),
                adapter,
                config,
                "connected",
                serde_json::json!({ "subject": subject, "metadata": metadata }),
            )
            .await
        }
        WorkerEvent::Message {
            target,
            sender,
            text,
            message_id,
            metadata,
            attachments,
        } => {
            let conversation = resolve_message_conversation(
                store,
                agent,
                root_conversation,
                adapter,
                config,
                &target,
                &metadata,
            )
            .await?;
            if config.adapter_type == "trial" {
                let prepared = prepare_trial_run(conversation.as_ref(), metadata.clone()).await?;
                let conversation_id = conversation.record().id.to_string();
                let started = trial_started(&prepared, &target, &conversation_id)?;
                // Publish the conversation id before entering the potentially
                // long-running wakeup turn so evaluators can retain it on timeout.
                queue_adapter_message(store, adapter, &started, Some(&target), Vec::new()).await?;
            }
            handle_worker_message(
                store,
                conversation.as_ref(),
                adapter,
                config,
                target,
                sender,
                text,
                message_id,
                metadata,
                attachments,
            )
            .await
        }
        WorkerEvent::Lifecycle { name, metadata } => {
            record_worker_lifecycle(
                store,
                root_conversation.as_ref(),
                adapter,
                config,
                &name,
                metadata,
            )
            .await
        }
        WorkerEvent::Error { message } => {
            store.mark_error(&adapter.id, message.clone()).await?;
            store
                .record_event(adapter.id.clone(), AdapterEventType::Error, message)
                .await?;
            Ok(())
        }
        WorkerEvent::CommandAck { command_id } => {
            let delivered = store
                .acknowledge_outbound_message(&adapter.id, &command_id)
                .await?;
            if let Some(delivered) = delivered {
                store
                    .record_event(
                        adapter.id.clone(),
                        AdapterEventType::Outbound,
                        format!(
                            "delivered adapter message {} on attempt {}",
                            delivered.id, delivered.attempt
                        ),
                    )
                    .await?;
            }
            Ok(())
        }
        WorkerEvent::CommandNack {
            command_id,
            message,
        } => {
            let delivery = store
                .nack_outbound_message(&adapter.id, &command_id, message.clone())
                .await?;
            let Some(delivery) = delivery else {
                return Ok(());
            };
            let terminal = delivery.status == AdapterDeliveryStatus::Failed;
            let summary = format!(
                "{} adapter message {} after attempt {}: {}",
                if terminal { "failed" } else { "retrying" },
                delivery.id,
                delivery.attempt,
                message
            );
            if terminal {
                store.mark_error(&adapter.id, summary.clone()).await?;
            }
            store
                .record_event(adapter.id.clone(), AdapterEventType::Error, summary)
                .await?;
            if !terminal {
                notify_adapter_outbound(&adapter.id);
            }
            Ok(())
        }
        WorkerEvent::Disconnected { reason } => {
            record_worker_lifecycle(
                store,
                root_conversation.as_ref(),
                adapter,
                config,
                "disconnected",
                serde_json::json!({ "reason": reason }),
            )
            .await
        }
    }
}

async fn resolve_message_conversation(
    store: &AdapterStore,
    agent: &dyn HarnessAgent,
    root_conversation: Arc<dyn HarnessConversation>,
    adapter: &AdapterRecord,
    config: &AdapterConfig,
    target: &str,
    _metadata: &serde_json::Value,
) -> Result<Arc<dyn HarnessConversation>> {
    if !uses_target_conversation_scope(config) {
        return Ok(root_conversation);
    }
    if let Some(record) = store.get_target_conversation(&adapter.id, target).await? {
        if let Some(conversation) = agent.get_conversation(&record.conversation_id).await? {
            return Ok(conversation);
        }
        tracing::warn!(
            adapter_id = %adapter.id,
            target,
            conversation_id = %record.conversation_id,
            "adapter target conversation mapping points to missing conversation; recreating"
        );
    }

    let slug = target_conversation_slug(adapter, target);
    let name = format!("{} target {}", adapter.name, target);
    let conversation = match agent
        .create_conversation(CreateConversationRequest {
            slug: Some(slug.clone()),
            name: Some(name),
            sandbox_scope: Some(SandboxScope::Conversation),
            ..Default::default()
        })
        .await
    {
        Ok(conversation) => conversation,
        Err(error) => {
            if let Some(conversation) = agent.get_conversation(&slug).await? {
                conversation
            } else {
                return Err(error).with_context(|| {
                    format!(
                        "failed to create target-scoped conversation for adapter {} target {}",
                        adapter.id, target
                    )
                });
            }
        }
    };
    let record = AdapterTargetConversationRecord::new(
        adapter.id.clone(),
        target.to_string(),
        conversation.record().id.to_string(),
        now_ms(),
    )?;
    store.put_target_conversation(&record).await?;
    Ok(conversation)
}

fn uses_target_conversation_scope(config: &AdapterConfig) -> bool {
    config
        .initialization
        .get("conversationScope")
        .and_then(|value| value.as_str())
        == Some("target")
}

fn target_conversation_slug(adapter: &AdapterRecord, target: &str) -> String {
    format!(
        "adapter-{}-{}",
        short_slug_part(&adapter.id),
        stable_target_key(target)
    )
}

fn short_slug_part(value: &str) -> String {
    value
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .take(12)
        .collect()
}

// Bound on inbound images forwarded to the model per message. Extra images
// are still listed in the prompt text with their URLs.
const MAX_INBOUND_IMAGES: usize = 4;
// Raw-byte cap per inbound image sent to the model. Larger files are listed
// by URL only so a single huge upload cannot blow up the model request.
const MAX_INBOUND_IMAGE_BYTES: usize = 8 * 1024 * 1024;

#[allow(clippy::too_many_arguments)]
async fn handle_worker_message(
    store: &AdapterStore,
    conversation: &dyn HarnessConversation,
    adapter: &AdapterRecord,
    config: &AdapterConfig,
    target: String,
    sender: Option<String>,
    text: String,
    message_id: Option<String>,
    metadata: serde_json::Value,
    attachments: Vec<AdapterAttachment>,
) -> Result<()> {
    if let Some(message_id) = &message_id
        && !store
            .record_inbound_message_once(&adapter.id, &target, message_id)
            .await?
    {
        return Ok(());
    }
    // Note: we intentionally do not write a conversation artifact here.
    // The full inbound text is delivered to the agent via the wakeup prompt,
    // and the event is recorded in the AdapterStore for audit without adding
    // adapter bookkeeping to the agent-visible conversation history.
    store
        .record_event(
            adapter.id.clone(),
            AdapterEventType::Inbound,
            format!(
                "{} adapter message from {} to {}{}",
                config.adapter_type,
                sender.as_deref().unwrap_or("unknown"),
                target,
                if attachments.is_empty() {
                    String::new()
                } else {
                    format!(" with {} attachment(s)", attachments.len())
                },
            ),
        )
        .await?;
    let image_parts = download_inbound_images(&adapter.id, store, &attachments).await;
    let prompt = compose_inbound_wakeup_prompt(
        config,
        adapter,
        &target,
        sender.as_deref(),
        &text,
        &metadata,
        &attachments,
        image_parts.len(),
    );
    let content = if image_parts.is_empty() {
        UserContent::String(prompt)
    } else {
        let mut parts = vec![UserContentPart::Text(TextContentPart {
            text: prompt,
            encrypted_content: None,
            provider_options: None,
            cache_control: None,
        })];
        parts.extend(image_parts);
        UserContent::Array(parts)
    };
    let wakeup_result = send_conversation_wakeup_content(conversation, content).await;
    // A failed model turn must not tear down the worker: the external
    // connection is healthy and dropping it loses every queued message.
    // Record the failure and keep processing events.
    if let Err(error) = wakeup_result {
        tracing::error!(
            adapter_id = %adapter.id,
            %error,
            "adapter wakeup turn failed; worker stays up"
        );
        store
            .mark_error(&adapter.id, format!("wakeup turn failed: {error}"))
            .await?;
        store
            .record_event(
                adapter.id.clone(),
                AdapterEventType::Error,
                format!("wakeup turn failed: {error}"),
            )
            .await?;
    }
    Ok(())
}

fn inbound_image_candidates(attachments: &[AdapterAttachment]) -> Vec<&AdapterAttachment> {
    attachments
        .iter()
        .filter(|attachment| {
            matches!(attachment.kind, AdapterAttachmentKind::Image) && attachment.url.is_some()
        })
        .take(MAX_INBOUND_IMAGES)
        .collect()
}

/// Downloads inbound image attachments and converts them to multimodal
/// message parts. Download failures are recorded but never fail the inbound
/// message: the wakeup prompt always lists attachment URLs as a fallback.
async fn download_inbound_images(
    adapter_id: &str,
    store: &AdapterStore,
    attachments: &[AdapterAttachment],
) -> Vec<UserContentPart> {
    let mut parts = Vec::new();
    for attachment in inbound_image_candidates(attachments) {
        let Some(url) = attachment.url.as_deref() else {
            continue;
        };
        match download_attachment(url).await {
            Ok(bytes) if bytes.len() <= MAX_INBOUND_IMAGE_BYTES => {
                parts.push(UserContentPart::Image {
                    image: lingua::serde_json::Value::String(
                        base64::engine::general_purpose::STANDARD.encode(&bytes),
                    ),
                    media_type: attachment.mime_type.clone(),
                    provider_options: None,
                });
            }
            Ok(bytes) => {
                tracing::warn!(
                    adapter_id,
                    url,
                    bytes = bytes.len(),
                    "inbound image exceeds size cap; passing URL only"
                );
            }
            Err(error) => {
                tracing::warn!(adapter_id, url, %error, "inbound image download failed");
                if let Err(record_error) = store
                    .record_event(
                        adapter_id.to_string(),
                        AdapterEventType::Error,
                        format!("inbound attachment download failed: {error}"),
                    )
                    .await
                {
                    tracing::error!(adapter_id, %record_error, "failed to record download error");
                }
            }
        }
    }
    parts
}

fn attachment_kind_label(kind: AdapterAttachmentKind) -> &'static str {
    match kind {
        AdapterAttachmentKind::Image => "image",
        AdapterAttachmentKind::Video => "video",
        AdapterAttachmentKind::Audio => "audio",
        AdapterAttachmentKind::Document => "document",
    }
}

fn compose_inbound_wakeup_prompt(
    config: &AdapterConfig,
    adapter: &AdapterRecord,
    target: &str,
    sender: Option<&str>,
    text: &str,
    metadata: &serde_json::Value,
    attachments: &[AdapterAttachment],
    attached_image_count: usize,
) -> String {
    let sender = sender.unwrap_or("unknown");
    let mut prompt = format!(
        "{} message received at target `{}` from {} via adapter `{}`:\n\n{}",
        config.adapter_type, target, sender, adapter.name, text,
    );
    if config.adapter_type == "slack" {
        if let Some(dm_target) = metadata.get("dmTarget").and_then(|value| value.as_str()) {
            prompt.push_str(&format!(
                "\n\nSlack sender DM target: `{dm_target}`. Use this only for appropriate private follow-up; do not use DM to bypass safety policy.",
            ));
        }
        if metadata
            .get("isActiveThread")
            .and_then(|value| value.as_bool())
            == Some(true)
        {
            prompt.push_str(
                "\n\nThis Slack message is from an active thread, but it may be ambient conversation. Only call send_adapter_message if the message appears directed at Exo, asks Exo to do something, or clearly needs an Exo response. If no response is needed, do nothing.",
            );
        }
    }
    if !attachments.is_empty() {
        prompt.push_str("\n\nThe message includes these attachments:\n");
        for attachment in attachments {
            let name = attachment.file_name.as_deref().unwrap_or("(unnamed)");
            let mime = attachment.mime_type.as_deref().unwrap_or("unknown type");
            let url = attachment.url.as_deref().unwrap_or("no url");
            prompt.push_str(&format!(
                "- {} `{}` ({}) — {}\n",
                attachment_kind_label(attachment.kind),
                name,
                mime,
                url,
            ));
        }
        if attached_image_count > 0 {
            prompt.push_str(&format!(
                "\n{attached_image_count} image(s) are attached to this message so you can view them directly.",
            ));
        }
        prompt.push_str(
            "\nAttachment URLs may expire quickly; if you need the raw file, download it promptly (e.g. with curl in your sandbox).",
        );
    }
    prompt.push_str(&format!(
        "\n\nThis message came from an external adapter. If you answer this message, you MUST reply externally with send_adapter_message using adapterId `{}` and target `{}`. Do not answer only in the REPL unless you are explicitly deciding that no external reply should be sent. If this asks you to schedule future work whose results should be posted back externally, include adapterId `{}` and target `{}` in the scheduled task reportPrompt.",
        adapter.id, target, adapter.id, target,
    ));
    prompt
}

async fn record_worker_lifecycle(
    store: &AdapterStore,
    _conversation: &dyn HarnessConversation,
    adapter: &AdapterRecord,
    config: &AdapterConfig,
    event_type: &str,
    payload: serde_json::Value,
) -> Result<()> {
    // Note: lifecycle events are recorded only in the AdapterStore so adapter
    // bookkeeping does not pollute the agent-visible conversation history.
    let payload_json =
        serde_json::to_string(&payload).unwrap_or_else(|_| "<unserializable>".into());
    let summary = if payload_json == "null" || payload_json == "{}" {
        format!("{} worker {event_type}", config.adapter_type)
    } else {
        format!(
            "{} worker {event_type}: {payload_json}",
            config.adapter_type
        )
    };
    store
        .record_event(
            adapter.id.clone(),
            match event_type {
                "connected" => AdapterEventType::Connected,
                "disconnected" => AdapterEventType::Disconnected,
                "error" => AdapterEventType::Error,
                _ => AdapterEventType::Lifecycle,
            },
            summary,
        )
        .await?;
    Ok(())
}

async fn require_agent(
    harness: &dyn Harness,
    adapter: &AdapterRecord,
) -> Result<Arc<dyn HarnessAgent>> {
    harness
        .get_agent(&adapter.agent_id)
        .await?
        .ok_or_else(|| anyhow!("adapter agent does not exist: {}", adapter.agent_id))
}

async fn require_conversation(
    agent: &dyn HarnessAgent,
    adapter: &AdapterRecord,
) -> Result<Arc<dyn HarnessConversation>> {
    agent
        .get_conversation(&adapter.conversation_id)
        .await?
        .ok_or_else(|| {
            anyhow!(
                "adapter conversation does not exist: {}",
                adapter.conversation_id
            )
        })
}

async fn worker_secret_env(
    agent: &dyn AgentHandle,
    config: &AdapterConfig,
) -> Result<Vec<(String, String)>> {
    let mut env = Vec::new();
    for secret_env in &config.secret_env {
        let secret_uuid = resolve_secret_id(agent, &secret_env.secret_id).await?;
        let Some(secret) = agent.get_secret(&secret_uuid).await? else {
            bail!("adapter secret not found: {}", secret_env.secret_id);
        };
        let value = match secret {
            Secret::Key { value } => value,
            Secret::Oauth { .. } => bail!("adapter worker secrets must be key secrets"),
        };
        env.push((secret_env.env.clone(), value));
    }
    Ok(env)
}

async fn resolve_secret_id(agent: &dyn AgentHandle, reference: &str) -> Result<SecretId> {
    if let Ok(secret_id) = reference.parse() {
        return Ok(secret_id);
    }
    agent
        .list_secrets()
        .await?
        .into_iter()
        .find(|secret| secret.name == reference)
        .map(|secret| secret.id)
        .ok_or_else(|| anyhow!("adapter secret not found: {reference}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claims_and_parses_fresh_reboot_notice() {
        let tempdir = tempfile::TempDir::new().unwrap();
        let path = tempdir.path().join("adapter-reboot-notice.json");
        std::fs::write(
            &path,
            r#"{"requestedAt":"2026-06-09T19:39:02Z","reason":"restart-all"}"#,
        )
        .unwrap();

        let notice = claim_reboot_notice(Some(&path)).expect("notice should be claimed");
        assert_eq!(notice.reason.as_deref(), Some("restart-all"));
        assert_eq!(notice.requested_at.as_deref(), Some("2026-06-09T19:39:02Z"));
        assert!(!path.exists(), "claiming must remove the notice file");

        assert!(claim_reboot_notice(Some(&path)).is_none());
    }

    #[test]
    fn ignores_missing_and_malformed_reboot_notices() {
        let tempdir = tempfile::TempDir::new().unwrap();
        let path = tempdir.path().join("adapter-reboot-notice.json");
        assert!(claim_reboot_notice(Some(&path)).is_none());
        assert!(claim_reboot_notice(None).is_none());

        std::fs::write(&path, "not json").unwrap();
        assert!(claim_reboot_notice(Some(&path)).is_none());
        assert!(!path.exists(), "malformed notices are still consumed");
    }

    fn config_with_scope(scope: Option<&str>) -> AdapterConfig {
        let initialization = match scope {
            Some(scope) => serde_json::json!({ "conversationScope": scope }),
            None => serde_json::json!({}),
        };
        AdapterConfig {
            adapter_type: "discord".to_string(),
            worker_command: vec!["true".to_string()],
            initialization,
            state_dir: None,
            secret_env: Vec::new(),
        }
    }

    fn test_attachment(kind: AdapterAttachmentKind, url: Option<&str>) -> AdapterAttachment {
        AdapterAttachment {
            kind,
            path: None,
            url: url.map(|url| url.to_string()),
            data: None,
            sandbox_path: None,
            mime_type: Some("image/png".to_string()),
            file_name: Some("photo.png".to_string()),
        }
    }

    fn test_adapter_record() -> AdapterRecord {
        AdapterRecord {
            id: "adapter-1".to_string(),
            agent_id: "agent-1".to_string(),
            conversation_id: "conversation-1".to_string(),
            name: "discord-dev".to_string(),
            source: super::super::types::AdapterSource::Library,
            enabled: true,
            lifecycle_state: super::super::types::AdapterLifecycleState::Running,
            created_at_ms: 0,
            updated_at_ms: 0,
            config: test_adapter_config(),
            last_connected_at_ms: None,
            last_error: None,
        }
    }

    fn test_adapter_config() -> AdapterConfig {
        config_with_scope(None)
    }

    #[test]
    fn target_scope_only_when_explicitly_set() {
        assert!(uses_target_conversation_scope(&config_with_scope(Some(
            "target"
        ))));
        // Default and explicit "adapter" both stay on the root conversation.
        assert!(!uses_target_conversation_scope(&config_with_scope(None)));
        assert!(!uses_target_conversation_scope(&config_with_scope(Some(
            "adapter"
        ))));
    }

    #[test]
    fn target_conversation_slug_is_deterministic_and_target_specific() {
        let adapter = AdapterRecord::new(
            super::super::types::NewAdapter {
                agent_id: "agent".to_string(),
                conversation_id: "conversation".to_string(),
                name: "discord-dev".to_string(),
                source: super::super::types::AdapterSource::Library,
                config: config_with_scope(Some("target")),
            },
            0,
        )
        .unwrap();

        let a1 = target_conversation_slug(&adapter, "channel-A");
        let a2 = target_conversation_slug(&adapter, "channel-A");
        let b = target_conversation_slug(&adapter, "channel-B");
        assert_eq!(a1, a2, "same target must yield the same slug");
        assert_ne!(a1, b, "different targets must yield different slugs");
        assert!(a1.starts_with("adapter-"));
        // Slug must agree with the store's filename key for that target.
        assert!(a1.ends_with(&stable_target_key("channel-A")));
    }

    #[test]
    fn image_candidates_filter_kind_url_and_cap() {
        let attachments = vec![
            test_attachment(AdapterAttachmentKind::Image, Some("https://a/1.png")),
            test_attachment(AdapterAttachmentKind::Document, Some("https://a/doc.pdf")),
            test_attachment(AdapterAttachmentKind::Image, None),
            test_attachment(AdapterAttachmentKind::Image, Some("https://a/2.png")),
            test_attachment(AdapterAttachmentKind::Image, Some("https://a/3.png")),
            test_attachment(AdapterAttachmentKind::Image, Some("https://a/4.png")),
            test_attachment(AdapterAttachmentKind::Image, Some("https://a/5.png")),
        ];
        let candidates = inbound_image_candidates(&attachments);
        assert_eq!(candidates.len(), MAX_INBOUND_IMAGES);
        assert_eq!(candidates[0].url.as_deref(), Some("https://a/1.png"));
        assert_eq!(candidates[3].url.as_deref(), Some("https://a/4.png"));
    }

    #[test]
    fn wakeup_prompt_without_attachments_matches_plain_format() {
        let prompt = compose_inbound_wakeup_prompt(
            &test_adapter_config(),
            &test_adapter_record(),
            "channel-9",
            Some("martin"),
            "hello",
            &serde_json::json!({}),
            &[],
            0,
        );
        assert!(prompt.starts_with(
            "discord message received at target `channel-9` from martin via adapter `discord-dev`:\n\nhello"
        ));
        assert!(!prompt.contains("attachments"));
        assert!(prompt.contains("send_adapter_message using adapterId `adapter-1`"));
    }

    #[test]
    fn wakeup_prompt_lists_attachments_and_attached_images() {
        let attachments = vec![
            test_attachment(AdapterAttachmentKind::Image, Some("https://a/1.png")),
            test_attachment(AdapterAttachmentKind::Document, Some("https://a/doc.pdf")),
        ];
        let prompt = compose_inbound_wakeup_prompt(
            &test_adapter_config(),
            &test_adapter_record(),
            "channel-9",
            None,
            "look at this",
            &serde_json::json!({}),
            &attachments,
            1,
        );
        assert!(prompt.contains("from unknown"));
        assert!(prompt.contains("The message includes these attachments:"));
        assert!(prompt.contains("- image `photo.png` (image/png) — https://a/1.png"));
        assert!(prompt.contains("- document `photo.png` (image/png) — https://a/doc.pdf"));
        assert!(prompt.contains("1 image(s) are attached to this message"));
        assert!(prompt.contains("Attachment URLs may expire quickly"));
    }

    #[test]
    fn wakeup_prompt_includes_slack_dm_target() {
        let mut adapter = test_adapter_record();
        adapter.config.adapter_type = "slack".to_string();
        adapter.name = "slack-dev".to_string();
        let prompt = compose_inbound_wakeup_prompt(
            &adapter.config,
            &adapter,
            "C123:1700000000.000000",
            Some("U123"),
            "hello",
            &serde_json::json!({ "dmTarget": "dm:U123", "isActiveThread": true }),
            &[],
            0,
        );
        assert!(prompt.contains("Slack sender DM target: `dm:U123`"));
        assert!(prompt.contains("do not use DM to bypass safety policy"));
        assert!(prompt.contains("Only call send_adapter_message"));
        assert!(prompt.contains("If no response is needed, do nothing"));
    }
}
