use std::collections::{HashMap, HashSet};
use std::ops::Bound;
use std::sync::Arc;

use anyhow::anyhow;
use async_trait::async_trait;
use exoharness::{
    AddEventsRequest, AddEventsResult, AgentHandle, AgentId, Artifact, ArtifactVersion,
    AttachSandboxRequest, Binding, BindingId, BindingRecord, BoxSandboxTcpStream,
    CancelSandboxProcessRequest, CloseSandboxProcessInputRequest, ConversationHandle,
    ConversationId, CreateSandboxRequest, Event, EventData, EventId, EventKind, EventStream,
    ExoHarness, ForkConversationRequest, ForkSandboxRequest, GetEventsResult,
    ListConversationsRequest, ListConversationsResult, NewAgentRequest, NewConversationRequest,
    PutSecretRequest, ReadArtifactRequest, RestoreSandboxRequest, Result, RunInSandboxRequest,
    SandboxAttachment, SandboxHandle, SandboxId, SandboxProcess, SandboxProcessEventQuery,
    SandboxProcessRecord, SandboxProcessStatus, SandboxProvider, SandboxRecord, Secret, SecretId,
    SecretMetadata, SnapshotHandle, SnapshotId, StartSandboxProcessRequest, StartSandboxRequest,
    TurnHandle, TurnRecord, Uuid7, WaitSandboxProcessRequest, WriteArtifactRequest,
    WriteSandboxProcessInputRequest,
};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

const LOCAL_SANDBOX_AGENT_SLUG: &str = "__exo_local_sandbox";
const LOCAL_AGENT_SANDBOX_SLUG_PREFIX: &str = "__exo_local_agent_sandbox";
const LOCAL_SANDBOX_MAP_EVENT: &str = "local_sandbox_mapped";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LocalSandboxMapEvent {
    remote_sandbox_id: SandboxId,
    local_sandbox_id: SandboxId,
}

fn merge_sandbox_records(
    mut remote: Vec<SandboxRecord>,
    local: Vec<SandboxRecord>,
) -> Vec<SandboxRecord> {
    remote.extend(local);
    remote.sort_unstable_by(|left, right| right.id.cmp(&left.id));
    remote.dedup_by(|left, right| left.id == right.id);
    remote
}

pub struct LocalSandboxExoHarness {
    state: Arc<LocalSandboxState>,
}

struct LocalSandboxState {
    remote: Arc<dyn ExoHarness>,
    local: Arc<dyn ExoHarness>,
    agents: Mutex<HashMap<AgentId, Arc<dyn AgentHandle>>>,
    conversations: Mutex<HashMap<ConversationId, Arc<dyn ConversationHandle>>>,
    conversation_init: Mutex<()>,
    sandboxes: Mutex<HashMap<SandboxId, SandboxId>>,
    detached_sandboxes: Mutex<HashMap<SandboxId, SandboxAttachment>>,
    force_local: bool,
    local_providers: HashSet<SandboxProvider>,
}

impl LocalSandboxExoHarness {
    pub fn new(remote: Arc<dyn ExoHarness>, local: Arc<dyn ExoHarness>) -> Self {
        Self::new_with_routing(remote, local, true, HashSet::new())
    }

    pub fn new_with_local_providers(
        remote: Arc<dyn ExoHarness>,
        local: Arc<dyn ExoHarness>,
        local_providers: impl IntoIterator<Item = SandboxProvider>,
    ) -> Self {
        Self::new_with_routing(remote, local, false, local_providers.into_iter().collect())
    }

    fn new_with_routing(
        remote: Arc<dyn ExoHarness>,
        local: Arc<dyn ExoHarness>,
        force_local: bool,
        local_providers: HashSet<SandboxProvider>,
    ) -> Self {
        Self {
            state: Arc::new(LocalSandboxState {
                remote,
                local,
                agents: Mutex::new(HashMap::new()),
                conversations: Mutex::new(HashMap::new()),
                conversation_init: Mutex::new(()),
                sandboxes: Mutex::new(HashMap::new()),
                detached_sandboxes: Mutex::new(HashMap::new()),
                force_local,
                local_providers,
            }),
        }
    }
}

async fn forget_local_sandbox(state: &LocalSandboxState, id: &SandboxId) {
    // Acquire both guards before mutating either map so cancellation cannot
    // leave a terminated sandbox present in only one index.
    let mut sandboxes = state.sandboxes.lock().await;
    let mut detached_sandboxes = state.detached_sandboxes.lock().await;
    sandboxes.remove(id);
    detached_sandboxes.remove(id);
}

struct UncommittedLocalSandbox {
    state: Arc<LocalSandboxState>,
    local: Arc<dyn ConversationHandle>,
    ids: Option<(SandboxId, SandboxId)>,
}

impl UncommittedLocalSandbox {
    fn new(
        state: Arc<LocalSandboxState>,
        local: Arc<dyn ConversationHandle>,
        remote_id: SandboxId,
        local_id: SandboxId,
    ) -> Self {
        Self {
            state,
            local,
            ids: Some((remote_id, local_id)),
        }
    }

    fn disarm(&mut self) {
        self.ids = None;
    }

    async fn cleanup(&mut self) -> Result<()> {
        let (remote_id, local_id) = self
            .ids
            .as_ref()
            .expect("uncommitted sandbox guard is armed")
            .clone();
        forget_local_sandbox(&self.state, &remote_id).await;
        let result = self.local.terminate_sandbox(local_id).await;
        self.disarm();
        result
    }
}

impl Drop for UncommittedLocalSandbox {
    fn drop(&mut self) {
        let Some((remote_id, local_id)) = self.ids.take() else {
            return;
        };
        let state = Arc::clone(&self.state);
        let local = Arc::clone(&self.local);
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            tracing::error!(
                %remote_id,
                %local_id,
                "cannot clean up an uncommitted local sandbox outside a Tokio runtime"
            );
            return;
        };
        drop(runtime.spawn(async move {
            forget_local_sandbox(&state, &remote_id).await;
            if let Err(error) = local.terminate_sandbox(local_id.clone()).await {
                tracing::error!(
                    %remote_id,
                    %local_id,
                    %error,
                    "failed to clean up a cancelled local sandbox creation"
                );
            }
        }));
    }
}

#[async_trait]
impl ExoHarness for LocalSandboxExoHarness {
    async fn list_agents(&self) -> Result<Vec<Arc<dyn AgentHandle>>> {
        Ok(self
            .state
            .remote
            .list_agents()
            .await?
            .into_iter()
            .map(|remote| wrap_agent(Arc::clone(&self.state), remote))
            .collect())
    }

    async fn get_agent(&self, id: &AgentId) -> Result<Option<Arc<dyn AgentHandle>>> {
        Ok(self
            .state
            .remote
            .get_agent(id)
            .await?
            .map(|remote| wrap_agent(Arc::clone(&self.state), remote)))
    }

    async fn new_agent(&self, request: NewAgentRequest) -> Result<Arc<dyn AgentHandle>> {
        let remote = self.state.remote.new_agent(request).await?;
        Ok(wrap_agent(Arc::clone(&self.state), remote))
    }

    async fn delete_agent(&self, id: &AgentId) -> Result<bool> {
        self.state.remote.delete_agent(id).await
    }

    async fn list_bindings(&self) -> Result<Vec<BindingRecord>> {
        self.state.remote.list_bindings().await
    }

    async fn put_binding(&self, binding: Binding) -> Result<BindingId> {
        self.state.remote.put_binding(binding).await
    }

    async fn get_binding(&self, id: &BindingId) -> Result<Option<Binding>> {
        self.state.remote.get_binding(id).await
    }

    async fn list_secrets(&self) -> Result<Vec<SecretMetadata>> {
        self.state.remote.list_secrets().await
    }

    async fn put_secret(&self, request: PutSecretRequest) -> Result<SecretId> {
        self.state.remote.put_secret(request).await
    }

    async fn get_secret(&self, id: &SecretId) -> Result<Option<Secret>> {
        self.state.remote.get_secret(id).await
    }
}

struct LocalSandboxAgent {
    state: Arc<LocalSandboxState>,
    remote: Arc<dyn AgentHandle>,
}

fn wrap_agent(state: Arc<LocalSandboxState>, remote: Arc<dyn AgentHandle>) -> Arc<dyn AgentHandle> {
    Arc::new(LocalSandboxAgent { state, remote })
}

async fn local_agent_for(
    state: &Arc<LocalSandboxState>,
    remote_agent_id: AgentId,
    remote_slug: &str,
) -> Result<Arc<dyn AgentHandle>> {
    {
        let agents = state.agents.lock().await;
        if let Some(agent) = agents.get(&remote_agent_id) {
            return Ok(Arc::clone(agent));
        }
    }

    let slug = format!("{LOCAL_AGENT_SANDBOX_SLUG_PREFIX}-{remote_agent_id}");
    let local_agent = match state
        .local
        .list_agents()
        .await?
        .into_iter()
        .find(|agent| agent.record().slug == slug)
    {
        Some(agent) => agent,
        None => {
            state
                .local
                .new_agent(NewAgentRequest {
                    slug,
                    name: format!("Local agent sandbox for {remote_slug}"),
                })
                .await?
        }
    };

    let mut agents = state.agents.lock().await;
    agents.insert(remote_agent_id, Arc::clone(&local_agent));
    Ok(local_agent)
}

impl LocalSandboxAgent {
    async fn local_agent(&self) -> Result<Arc<dyn AgentHandle>> {
        local_agent_for(
            &self.state,
            self.remote.record().id,
            self.remote.record().slug.as_str(),
        )
        .await
    }

    fn wants_local_sandbox(&self, request: &CreateSandboxRequest) -> bool {
        self.state.force_local || self.state.local_providers.contains(&request.provider)
    }

    async fn local_sandbox_id(&self, sandbox_id: &SandboxId) -> Result<Option<SandboxId>> {
        Ok(self.state.sandboxes.lock().await.get(sandbox_id).cloned())
    }

    async fn map_local_sandbox(&self, remote_id: SandboxId, local_id: SandboxId) {
        self.state
            .sandboxes
            .lock()
            .await
            .insert(remote_id, local_id);
    }
}

#[async_trait]
impl AgentHandle for LocalSandboxAgent {
    fn record(&self) -> &exoharness::AgentRecord {
        self.remote.record()
    }

    async fn list_conversations(
        &self,
        request: ListConversationsRequest,
    ) -> Result<ListConversationsResult<Arc<dyn ConversationHandle>>> {
        let result = self.remote.list_conversations(request).await?;
        Ok(ListConversationsResult {
            conversations: result
                .conversations
                .into_iter()
                .map(|remote| wrap_conversation(Arc::clone(&self.state), remote))
                .collect(),
            next_cursor: result.next_cursor,
        })
    }

    async fn get_conversation(
        &self,
        id: &ConversationId,
    ) -> Result<Option<Arc<dyn ConversationHandle>>> {
        Ok(self
            .remote
            .get_conversation(id)
            .await?
            .map(|remote| wrap_conversation(Arc::clone(&self.state), remote)))
    }

    async fn new_conversation(
        &self,
        request: NewConversationRequest,
    ) -> Result<Arc<dyn ConversationHandle>> {
        let remote = self.remote.new_conversation(request).await?;
        Ok(wrap_conversation(Arc::clone(&self.state), remote))
    }

    async fn delete_conversation(&self, id: &ConversationId) -> Result<bool> {
        self.remote.delete_conversation(id).await
    }

    async fn list_bindings(&self) -> Result<Vec<BindingRecord>> {
        self.remote.list_bindings().await
    }

    async fn put_binding(&self, binding: Binding) -> Result<BindingId> {
        self.remote.put_binding(binding).await
    }

    async fn get_binding(&self, id: &BindingId) -> Result<Option<Binding>> {
        self.remote.get_binding(id).await
    }

    async fn list_secrets(&self) -> Result<Vec<SecretMetadata>> {
        self.remote.list_secrets().await
    }

    async fn put_secret(&self, request: PutSecretRequest) -> Result<SecretId> {
        self.remote.put_secret(request).await
    }

    async fn get_secret(&self, id: &SecretId) -> Result<Option<Secret>> {
        self.remote.get_secret(id).await
    }

    async fn write_artifact(&self, request: WriteArtifactRequest) -> Result<ArtifactVersion> {
        self.remote.write_artifact(request).await
    }

    async fn read_artifact(&self, request: ReadArtifactRequest) -> Result<Option<Artifact>> {
        self.remote.read_artifact(request).await
    }

    async fn list_artifacts(&self) -> Result<Vec<ArtifactVersion>> {
        self.remote.list_artifacts().await
    }
}

#[async_trait]
impl SnapshotHandle for LocalSandboxAgent {
    async fn snapshot_sandbox(&self, id: SandboxId) -> Result<SnapshotId> {
        let Some(local_id) = self.local_sandbox_id(&id).await? else {
            return self.remote.snapshot_sandbox(id).await;
        };
        self.local_agent().await?.snapshot_sandbox(local_id).await
    }

    async fn start_sandbox(&self, request: StartSandboxRequest) -> Result<()> {
        let Some(local_id) = self.local_sandbox_id(&request.id).await? else {
            return self.remote.start_sandbox(request).await;
        };
        self.local_agent()
            .await?
            .start_sandbox(StartSandboxRequest {
                id: local_id,
                snapshot_id: request.snapshot_id,
                idle_seconds: request.idle_seconds,
                provider: request.provider,
            })
            .await
    }
}

#[async_trait]
impl SandboxHandle for LocalSandboxAgent {
    async fn list_sandboxes(&self) -> Result<Vec<SandboxRecord>> {
        Ok(merge_sandbox_records(
            self.remote.list_sandboxes().await?,
            self.local_agent().await?.list_sandboxes().await?,
        ))
    }

    async fn create_sandbox(&self, request: CreateSandboxRequest) -> Result<SandboxId> {
        if !self.wants_local_sandbox(&request) {
            return self.remote.create_sandbox(request).await;
        }

        let local_id = self.local_agent().await?.create_sandbox(request).await?;
        let remote_id = local_id.clone();
        self.map_local_sandbox(remote_id.clone(), local_id).await;
        Ok(remote_id)
    }

    async fn fork_sandbox(&self, mut request: ForkSandboxRequest) -> Result<SandboxId> {
        let Some(local_source_id) = self.local_sandbox_id(&request.source_id).await? else {
            return self.remote.fork_sandbox(request).await;
        };
        request.source_id = local_source_id;
        let local_id = self.local_agent().await?.fork_sandbox(request).await?;
        let remote_id = local_id.clone();
        self.map_local_sandbox(remote_id.clone(), local_id).await;
        Ok(remote_id)
    }

    async fn restore_sandbox(&self, request: RestoreSandboxRequest) -> Result<SandboxId> {
        if !self.wants_local_sandbox(&request.sandbox) {
            return self.remote.restore_sandbox(request).await;
        }

        let local_id = self.local_agent().await?.restore_sandbox(request).await?;
        let remote_id = local_id.clone();
        self.map_local_sandbox(remote_id.clone(), local_id).await;
        Ok(remote_id)
    }

    async fn terminate_sandbox(&self, id: SandboxId) -> Result<()> {
        let Some(local_id) = self.local_sandbox_id(&id).await? else {
            return self.remote.terminate_sandbox(id).await;
        };
        self.local_agent()
            .await?
            .terminate_sandbox(local_id)
            .await?;
        forget_local_sandbox(&self.state, &id).await;
        Ok(())
    }

    async fn attach_sandbox(&self, request: AttachSandboxRequest) -> Result<SandboxId> {
        self.remote.attach_sandbox(request).await
    }

    async fn detach_sandbox(&self, id: SandboxId) -> Result<SandboxAttachment> {
        if let Some(attachment) = self.state.detached_sandboxes.lock().await.get(&id).cloned() {
            return Ok(attachment);
        }
        let Some(local_id) = self.local_sandbox_id(&id).await? else {
            return self.remote.detach_sandbox(id).await;
        };
        let attachment = self.local_agent().await?.detach_sandbox(local_id).await?;
        self.state
            .detached_sandboxes
            .lock()
            .await
            .insert(id, attachment.clone());
        Ok(attachment)
    }

    async fn stop_sandbox(&self, id: SandboxId) -> Result<()> {
        let Some(local_id) = self.local_sandbox_id(&id).await? else {
            return self.remote.stop_sandbox(id).await;
        };
        self.local_agent().await?.stop_sandbox(local_id).await
    }

    async fn sandbox_supports_tcp(&self, id: SandboxId) -> Result<bool> {
        let Some(local_id) = self.local_sandbox_id(&id).await? else {
            return self.remote.sandbox_supports_tcp(id).await;
        };
        self.local_agent()
            .await?
            .sandbox_supports_tcp(local_id)
            .await
    }

    async fn connect_sandbox_tcp(
        &self,
        id: SandboxId,
        port: u16,
    ) -> Result<Option<BoxSandboxTcpStream>> {
        let Some(local_id) = self.local_sandbox_id(&id).await? else {
            return self.remote.connect_sandbox_tcp(id, port).await;
        };
        self.local_agent()
            .await?
            .connect_sandbox_tcp(local_id, port)
            .await
    }

    async fn start_sandbox_process(
        &self,
        request: StartSandboxProcessRequest,
    ) -> Result<SandboxProcessRecord> {
        let Some(local_id) = self.local_sandbox_id(&request.sandbox_id).await? else {
            return self.remote.start_sandbox_process(request).await;
        };
        start_mapped_sandbox_process(self.local_agent().await?, local_id, request).await
    }

    async fn write_sandbox_process_input(
        &self,
        request: WriteSandboxProcessInputRequest,
    ) -> Result<()> {
        let Some(local_id) = self.local_sandbox_id(&request.sandbox_id).await? else {
            return self.remote.write_sandbox_process_input(request).await;
        };
        write_mapped_sandbox_process_input(self.local_agent().await?, local_id, request).await
    }

    async fn close_sandbox_process_input(
        &self,
        request: CloseSandboxProcessInputRequest,
    ) -> Result<()> {
        let Some(local_id) = self.local_sandbox_id(&request.sandbox_id).await? else {
            return self.remote.close_sandbox_process_input(request).await;
        };
        close_mapped_sandbox_process_input(self.local_agent().await?, local_id, request).await
    }

    async fn get_sandbox_process_events(
        &self,
        query: SandboxProcessEventQuery,
    ) -> Result<exoharness::GetSandboxProcessEventsResult> {
        let Some(local_id) = self.local_sandbox_id(&query.sandbox_id).await? else {
            return self.remote.get_sandbox_process_events(query).await;
        };
        get_mapped_sandbox_process_events(self.local_agent().await?, local_id, query).await
    }

    async fn wait_sandbox_process(
        &self,
        request: WaitSandboxProcessRequest,
    ) -> Result<SandboxProcessStatus> {
        let Some(local_id) = self.local_sandbox_id(&request.sandbox_id).await? else {
            return self.remote.wait_sandbox_process(request).await;
        };
        wait_mapped_sandbox_process(self.local_agent().await?, local_id, request).await
    }

    async fn cancel_sandbox_process(
        &self,
        request: CancelSandboxProcessRequest,
    ) -> Result<SandboxProcessStatus> {
        let Some(local_id) = self.local_sandbox_id(&request.sandbox_id).await? else {
            return self.remote.cancel_sandbox_process(request).await;
        };
        cancel_mapped_sandbox_process(self.local_agent().await?, local_id, request).await
    }

    async fn run_in_sandbox(
        &self,
        request: RunInSandboxRequest,
    ) -> Result<Box<dyn SandboxProcess>> {
        let Some(local_id) = self.local_sandbox_id(&request.id).await? else {
            return self.remote.run_in_sandbox(request).await;
        };
        run_in_mapped_sandbox(self.local_agent().await?, local_id, request).await
    }
}

struct LocalSandboxConversation {
    state: Arc<LocalSandboxState>,
    remote: Arc<dyn ConversationHandle>,
}

fn wrap_conversation(
    state: Arc<LocalSandboxState>,
    remote: Arc<dyn ConversationHandle>,
) -> Arc<dyn ConversationHandle> {
    Arc::new(LocalSandboxConversation { state, remote })
}

async fn local_conversation_for(
    state: &Arc<LocalSandboxState>,
    remote_conversation_id: ConversationId,
    remote_slug: &str,
) -> Result<Arc<dyn ConversationHandle>> {
    {
        let conversations = state.conversations.lock().await;
        if let Some(conversation) = conversations.get(&remote_conversation_id) {
            return Ok(Arc::clone(conversation));
        }
    }

    let _init_guard = state.conversation_init.lock().await;
    {
        let conversations = state.conversations.lock().await;
        if let Some(conversation) = conversations.get(&remote_conversation_id) {
            return Ok(Arc::clone(conversation));
        }
    }

    let local_agent = match state
        .local
        .list_agents()
        .await?
        .into_iter()
        .find(|agent| agent.record().slug == LOCAL_SANDBOX_AGENT_SLUG)
    {
        Some(agent) => agent,
        None => {
            state
                .local
                .new_agent(NewAgentRequest {
                    slug: LOCAL_SANDBOX_AGENT_SLUG.to_string(),
                    name: "Local sandbox".to_string(),
                })
                .await?
        }
    };

    let slug = format!("remote-{remote_conversation_id}");
    let local_conversation = match local_agent
        .list_conversations(ListConversationsRequest::default())
        .await?
        .conversations
        .into_iter()
        .find(|conversation| conversation.record().slug == slug)
    {
        Some(conversation) => conversation,
        None => {
            local_agent
                .new_conversation(NewConversationRequest {
                    slug: Some(slug),
                    name: Some(format!("Local sandbox for {remote_slug}")),
                })
                .await?
        }
    };

    let mut conversations = state.conversations.lock().await;
    conversations.insert(remote_conversation_id, Arc::clone(&local_conversation));
    Ok(local_conversation)
}

async fn local_sandbox_id_for(
    state: &Arc<LocalSandboxState>,
    remote_conversation_id: ConversationId,
    sandbox_id: &SandboxId,
) -> Result<Option<SandboxId>> {
    if let Some(local_id) = state.sandboxes.lock().await.get(sandbox_id).cloned() {
        return Ok(Some(local_id));
    }

    let local_conversation = local_conversation_for(
        state,
        remote_conversation_id,
        &remote_conversation_id.to_string(),
    )
    .await?;
    let events = local_conversation
        .get_events(Some(exoharness::EventQuery {
            cursor: None,
            direction: Some(exoharness::EventQueryDirection::Desc),
            limit: None,
            session_id: None,
            turn_id: None,
            types: Some(vec![EventKind::custom(LOCAL_SANDBOX_MAP_EVENT)]),
        }))
        .await?
        .events;

    for event in events {
        let EventData::Custom {
            event_type,
            payload,
        } = event.data
        else {
            continue;
        };
        if event_type != LOCAL_SANDBOX_MAP_EVENT {
            continue;
        }
        let mapping: LocalSandboxMapEvent = serde_json::from_value(payload)?;
        state.sandboxes.lock().await.insert(
            mapping.remote_sandbox_id.clone(),
            mapping.local_sandbox_id.clone(),
        );
        if mapping.remote_sandbox_id == *sandbox_id {
            return Ok(Some(mapping.local_sandbox_id));
        }
    }

    Ok(None)
}

async fn start_mapped_sandbox_process(
    local: Arc<dyn SandboxHandle>,
    local_id: SandboxId,
    request: StartSandboxProcessRequest,
) -> Result<SandboxProcessRecord> {
    let remote_id = request.sandbox_id.clone();
    let mut process = local
        .start_sandbox_process(StartSandboxProcessRequest {
            sandbox_id: local_id,
            ..request
        })
        .await?;
    process.sandbox_id = remote_id;
    Ok(process)
}

async fn write_mapped_sandbox_process_input(
    local: Arc<dyn SandboxHandle>,
    local_id: SandboxId,
    request: WriteSandboxProcessInputRequest,
) -> Result<()> {
    local
        .write_sandbox_process_input(WriteSandboxProcessInputRequest {
            sandbox_id: local_id,
            ..request
        })
        .await
}

async fn close_mapped_sandbox_process_input(
    local: Arc<dyn SandboxHandle>,
    local_id: SandboxId,
    request: CloseSandboxProcessInputRequest,
) -> Result<()> {
    local
        .close_sandbox_process_input(CloseSandboxProcessInputRequest {
            sandbox_id: local_id,
            ..request
        })
        .await
}

async fn get_mapped_sandbox_process_events(
    local: Arc<dyn SandboxHandle>,
    local_id: SandboxId,
    query: SandboxProcessEventQuery,
) -> Result<exoharness::GetSandboxProcessEventsResult> {
    local
        .get_sandbox_process_events(SandboxProcessEventQuery {
            sandbox_id: local_id,
            ..query
        })
        .await
}

async fn wait_mapped_sandbox_process(
    local: Arc<dyn SandboxHandle>,
    local_id: SandboxId,
    request: WaitSandboxProcessRequest,
) -> Result<SandboxProcessStatus> {
    local
        .wait_sandbox_process(WaitSandboxProcessRequest {
            sandbox_id: local_id,
            ..request
        })
        .await
}

async fn cancel_mapped_sandbox_process(
    local: Arc<dyn SandboxHandle>,
    local_id: SandboxId,
    request: CancelSandboxProcessRequest,
) -> Result<SandboxProcessStatus> {
    local
        .cancel_sandbox_process(CancelSandboxProcessRequest {
            sandbox_id: local_id,
            ..request
        })
        .await
}

async fn run_in_mapped_sandbox(
    local: Arc<dyn SandboxHandle>,
    local_id: SandboxId,
    request: RunInSandboxRequest,
) -> Result<Box<dyn SandboxProcess>> {
    local
        .run_in_sandbox(RunInSandboxRequest {
            id: local_id,
            ..request
        })
        .await
}

impl LocalSandboxConversation {
    async fn local_conversation(&self) -> Result<Arc<dyn ConversationHandle>> {
        local_conversation_for(
            &self.state,
            self.remote.record().id,
            self.remote.record().slug.as_str(),
        )
        .await
    }

    fn wants_local_sandbox(&self, request: &CreateSandboxRequest) -> bool {
        self.state.force_local || self.state.local_providers.contains(&request.provider)
    }

    async fn local_sandbox_id(&self, sandbox_id: &SandboxId) -> Result<Option<SandboxId>> {
        local_sandbox_id_for(&self.state, self.remote.record().id, sandbox_id).await
    }

    async fn map_local_sandbox(&self, remote_id: SandboxId, local_id: SandboxId) -> Result<()> {
        self.state
            .sandboxes
            .lock()
            .await
            .insert(remote_id.clone(), local_id.clone());
        self.local_conversation()
            .await?
            .add_events(AddEventsRequest {
                session_id: None,
                turn_id: None,
                data: vec![EventData::Custom {
                    event_type: LOCAL_SANDBOX_MAP_EVENT.to_string(),
                    payload: serde_json::to_value(LocalSandboxMapEvent {
                        remote_sandbox_id: remote_id,
                        local_sandbox_id: local_id,
                    })?,
                }],
            })
            .await?;
        Ok(())
    }

    async fn append_remote_sandbox_events(&self, data: Vec<EventData>) -> Result<()> {
        self.remote
            .add_events(AddEventsRequest {
                session_id: None,
                turn_id: None,
                data,
            })
            .await?;
        Ok(())
    }

    async fn commit_local_sandbox(
        &self,
        local: &Arc<dyn ConversationHandle>,
        remote_id: SandboxId,
        local_id: SandboxId,
        events: Vec<EventData>,
    ) -> Result<()> {
        let mut uncommitted = UncommittedLocalSandbox::new(
            Arc::clone(&self.state),
            Arc::clone(local),
            remote_id.clone(),
            local_id.clone(),
        );
        let result = async {
            self.map_local_sandbox(remote_id.clone(), local_id.clone())
                .await?;
            self.append_remote_sandbox_events(events).await
        }
        .await;
        let Err(error) = result else {
            uncommitted.disarm();
            return Ok(());
        };

        match uncommitted.cleanup().await {
            Ok(()) => Err(error),
            Err(cleanup_error) => Err(anyhow!(
                "{error:#}; cleaning up the uncommitted local sandbox also failed: {cleanup_error:#}"
            )),
        }
    }
}

#[async_trait]
impl ConversationHandle for LocalSandboxConversation {
    fn record(&self) -> &exoharness::ConversationRecord {
        self.remote.record()
    }

    async fn start_session(&self) -> Result<exoharness::SessionId> {
        self.remote.start_session().await
    }

    async fn end_session(&self, id: exoharness::SessionId) -> Result<()> {
        self.remote.end_session(id).await
    }

    async fn begin_turn(
        &self,
        request: exoharness::BeginTurnRequest,
    ) -> Result<Arc<dyn TurnHandle>> {
        Ok(Arc::new(LocalSandboxTurnHandle {
            state: Arc::clone(&self.state),
            conversation_id: self.remote.record().id,
            remote: self.remote.begin_turn(request).await?,
        }))
    }

    async fn turn_handle(&self, record: TurnRecord) -> Result<Arc<dyn TurnHandle>> {
        Ok(Arc::new(LocalSandboxTurnHandle {
            state: Arc::clone(&self.state),
            conversation_id: self.remote.record().id,
            remote: self.remote.turn_handle(record).await?,
        }))
    }

    async fn get_events(&self, query: Option<exoharness::EventQuery>) -> Result<GetEventsResult> {
        self.remote.get_events(query).await
    }

    async fn watch_events(&self, after_exclusive: Bound<EventId>) -> Result<EventStream> {
        self.remote.watch_events(after_exclusive).await
    }

    async fn get_event(&self, id: EventId) -> Result<Option<Event>> {
        self.remote.get_event(id).await
    }

    async fn add_events(&self, request: AddEventsRequest) -> Result<AddEventsResult> {
        self.remote.add_events(request).await
    }

    async fn fork(&self, request: ForkConversationRequest) -> Result<Arc<dyn ConversationHandle>> {
        let remote = self.remote.fork(request).await?;
        Ok(wrap_conversation(Arc::clone(&self.state), remote))
    }

    async fn write_artifact(&self, request: WriteArtifactRequest) -> Result<ArtifactVersion> {
        self.remote.write_artifact(request).await
    }

    async fn read_artifact(&self, request: ReadArtifactRequest) -> Result<Option<Artifact>> {
        self.remote.read_artifact(request).await
    }

    async fn list_artifacts(&self) -> Result<Vec<ArtifactVersion>> {
        self.remote.list_artifacts().await
    }

    async fn list_bindings(&self) -> Result<Vec<BindingRecord>> {
        self.remote.list_bindings().await
    }

    async fn put_binding(&self, binding: Binding) -> Result<BindingId> {
        self.remote.put_binding(binding).await
    }

    async fn get_binding(&self, id: &BindingId) -> Result<Option<Binding>> {
        self.remote.get_binding(id).await
    }

    async fn list_secrets(&self) -> Result<Vec<SecretMetadata>> {
        self.remote.list_secrets().await
    }

    async fn put_secret(&self, request: PutSecretRequest) -> Result<SecretId> {
        self.remote.put_secret(request).await
    }

    async fn get_secret(&self, id: &SecretId) -> Result<Option<Secret>> {
        self.remote.get_secret(id).await
    }
}

#[async_trait]
impl SnapshotHandle for LocalSandboxConversation {
    async fn snapshot_sandbox(&self, id: SandboxId) -> Result<SnapshotId> {
        let Some(local_id) = self.local_sandbox_id(&id).await? else {
            return self.remote.snapshot_sandbox(id).await;
        };
        let snapshot_id = self
            .local_conversation()
            .await?
            .snapshot_sandbox(local_id)
            .await?;
        self.append_remote_sandbox_events(vec![EventData::SandboxSnapshotted {
            sandbox_id: id,
            snapshot_id,
        }])
        .await?;
        Ok(snapshot_id)
    }

    async fn start_sandbox(&self, request: StartSandboxRequest) -> Result<()> {
        let Some(local_id) = self.local_sandbox_id(&request.id).await? else {
            return self.remote.start_sandbox(request).await;
        };
        self.local_conversation()
            .await?
            .start_sandbox(StartSandboxRequest {
                id: local_id,
                snapshot_id: request.snapshot_id,
                idle_seconds: request.idle_seconds,
                provider: request.provider,
            })
            .await?;
        self.append_remote_sandbox_events(vec![EventData::SandboxStarted {
            sandbox_id: request.id,
            snapshot_id: Some(request.snapshot_id),
        }])
        .await
    }
}

#[async_trait]
impl SandboxHandle for LocalSandboxConversation {
    async fn list_sandboxes(&self) -> Result<Vec<SandboxRecord>> {
        let remote = self.remote.list_sandboxes().await?;
        let local_to_remote = self
            .state
            .sandboxes
            .lock()
            .await
            .iter()
            .map(|(remote, local)| (local.clone(), remote.clone()))
            .collect::<HashMap<_, _>>();
        let mut local = self.local_conversation().await?.list_sandboxes().await?;
        for sandbox in &mut local {
            if let Some(remote_id) = local_to_remote.get(&sandbox.id) {
                sandbox.id = remote_id.clone();
            }
        }
        Ok(merge_sandbox_records(remote, local))
    }

    async fn create_sandbox(&self, request: CreateSandboxRequest) -> Result<SandboxId> {
        if !self.wants_local_sandbox(&request) {
            return self.remote.create_sandbox(request).await;
        }

        let remote_id = format!("sandbox-{}", Uuid7::now());
        let local = self.local_conversation().await?;
        let local_id = local.create_sandbox(request.clone()).await?;
        self.commit_local_sandbox(
            &local,
            remote_id.clone(),
            local_id,
            sandbox_created_events(&remote_id, request),
        )
        .await?;
        Ok(remote_id)
    }

    async fn fork_sandbox(&self, mut request: ForkSandboxRequest) -> Result<SandboxId> {
        let Some(local_source_id) = self.local_sandbox_id(&request.source_id).await? else {
            return self.remote.fork_sandbox(request).await;
        };
        let sandbox = request.sandbox.clone();
        request.source_id = local_source_id;
        let remote_id = format!("sandbox-{}", Uuid7::now());
        let local = self.local_conversation().await?;
        let local_id = local.fork_sandbox(request).await?;
        self.commit_local_sandbox(
            &local,
            remote_id.clone(),
            local_id,
            sandbox_created_events(&remote_id, sandbox),
        )
        .await?;
        Ok(remote_id)
    }

    async fn restore_sandbox(&self, request: RestoreSandboxRequest) -> Result<SandboxId> {
        if !self.wants_local_sandbox(&request.sandbox) {
            return self.remote.restore_sandbox(request).await;
        }

        let snapshot_id = request.snapshot_id;
        let sandbox = request.sandbox.clone();
        let remote_id = format!("sandbox-{}", Uuid7::now());
        let local = self.local_conversation().await?;
        let local_id = local.restore_sandbox(request).await?;
        let mut events = sandbox_created_events(&remote_id, sandbox);
        events.push(EventData::SandboxStarted {
            sandbox_id: remote_id.clone(),
            snapshot_id: Some(snapshot_id),
        });
        self.commit_local_sandbox(&local, remote_id.clone(), local_id, events)
            .await?;
        Ok(remote_id)
    }

    async fn terminate_sandbox(&self, id: SandboxId) -> Result<()> {
        let Some(local_id) = self.local_sandbox_id(&id).await? else {
            return self.remote.terminate_sandbox(id).await;
        };
        self.local_conversation()
            .await?
            .terminate_sandbox(local_id)
            .await?;
        forget_local_sandbox(&self.state, &id).await;
        self.append_remote_sandbox_events(vec![EventData::SandboxStopped { sandbox_id: id }])
            .await
    }

    async fn attach_sandbox(&self, request: AttachSandboxRequest) -> Result<SandboxId> {
        self.remote.attach_sandbox(request).await
    }

    async fn detach_sandbox(&self, id: SandboxId) -> Result<SandboxAttachment> {
        if let Some(attachment) = self.state.detached_sandboxes.lock().await.get(&id).cloned() {
            return Ok(attachment);
        }
        let Some(local_id) = self.local_sandbox_id(&id).await? else {
            return self.remote.detach_sandbox(id).await;
        };
        let attachment = self
            .local_conversation()
            .await?
            .detach_sandbox(local_id)
            .await?;
        self.append_remote_sandbox_events(vec![EventData::SandboxDetached {
            sandbox_id: id.clone(),
            attachment: attachment.clone(),
        }])
        .await?;
        self.state
            .detached_sandboxes
            .lock()
            .await
            .insert(id, attachment.clone());
        Ok(attachment)
    }

    async fn stop_sandbox(&self, id: SandboxId) -> Result<()> {
        let Some(local_id) = self.local_sandbox_id(&id).await? else {
            return self.remote.stop_sandbox(id).await;
        };
        self.local_conversation()
            .await?
            .stop_sandbox(local_id)
            .await?;
        self.append_remote_sandbox_events(vec![EventData::SandboxStopped { sandbox_id: id }])
            .await
    }

    async fn sandbox_supports_tcp(&self, id: SandboxId) -> Result<bool> {
        let Some(local_id) = self.local_sandbox_id(&id).await? else {
            return self.remote.sandbox_supports_tcp(id).await;
        };
        self.local_conversation()
            .await?
            .sandbox_supports_tcp(local_id)
            .await
    }

    async fn connect_sandbox_tcp(
        &self,
        id: SandboxId,
        port: u16,
    ) -> Result<Option<BoxSandboxTcpStream>> {
        let Some(local_id) = self.local_sandbox_id(&id).await? else {
            return self.remote.connect_sandbox_tcp(id, port).await;
        };
        self.local_conversation()
            .await?
            .connect_sandbox_tcp(local_id, port)
            .await
    }

    async fn start_sandbox_process(
        &self,
        request: StartSandboxProcessRequest,
    ) -> Result<SandboxProcessRecord> {
        let Some(local_id) = self.local_sandbox_id(&request.sandbox_id).await? else {
            return self.remote.start_sandbox_process(request).await;
        };
        start_mapped_sandbox_process(self.local_conversation().await?, local_id, request).await
    }

    async fn write_sandbox_process_input(
        &self,
        request: WriteSandboxProcessInputRequest,
    ) -> Result<()> {
        let Some(local_id) = self.local_sandbox_id(&request.sandbox_id).await? else {
            return self.remote.write_sandbox_process_input(request).await;
        };
        write_mapped_sandbox_process_input(self.local_conversation().await?, local_id, request)
            .await
    }

    async fn close_sandbox_process_input(
        &self,
        request: CloseSandboxProcessInputRequest,
    ) -> Result<()> {
        let Some(local_id) = self.local_sandbox_id(&request.sandbox_id).await? else {
            return self.remote.close_sandbox_process_input(request).await;
        };
        close_mapped_sandbox_process_input(self.local_conversation().await?, local_id, request)
            .await
    }

    async fn get_sandbox_process_events(
        &self,
        query: SandboxProcessEventQuery,
    ) -> Result<exoharness::GetSandboxProcessEventsResult> {
        let Some(local_id) = self.local_sandbox_id(&query.sandbox_id).await? else {
            return self.remote.get_sandbox_process_events(query).await;
        };
        get_mapped_sandbox_process_events(self.local_conversation().await?, local_id, query).await
    }

    async fn wait_sandbox_process(
        &self,
        request: WaitSandboxProcessRequest,
    ) -> Result<SandboxProcessStatus> {
        let Some(local_id) = self.local_sandbox_id(&request.sandbox_id).await? else {
            return self.remote.wait_sandbox_process(request).await;
        };
        wait_mapped_sandbox_process(self.local_conversation().await?, local_id, request).await
    }

    async fn cancel_sandbox_process(
        &self,
        request: CancelSandboxProcessRequest,
    ) -> Result<SandboxProcessStatus> {
        let Some(local_id) = self.local_sandbox_id(&request.sandbox_id).await? else {
            return self.remote.cancel_sandbox_process(request).await;
        };
        cancel_mapped_sandbox_process(self.local_conversation().await?, local_id, request).await
    }

    async fn run_in_sandbox(
        &self,
        request: RunInSandboxRequest,
    ) -> Result<Box<dyn SandboxProcess>> {
        let Some(local_id) = self.local_sandbox_id(&request.id).await? else {
            return self.remote.run_in_sandbox(request).await;
        };
        run_in_mapped_sandbox(self.local_conversation().await?, local_id, request).await
    }
}

fn sandbox_created_events(sandbox_id: &SandboxId, request: CreateSandboxRequest) -> Vec<EventData> {
    vec![
        EventData::SandboxCreated {
            sandbox_id: sandbox_id.clone(),
            name: request.name,
            provider: request.provider,
            image: request.image,
            default_workdir: request.default_workdir.unwrap_or_default(),
            file_system_mounts: request.file_system_mounts.unwrap_or_default(),
            durable_file_systems: request.durable_file_systems.unwrap_or_default(),
            enable_networking: request.egress_policy.as_ref().map_or(
                request.enable_networking.unwrap_or(true),
                exoharness::EgressPolicy::permits_unrestricted_egress,
            ),
            egress_policy: request.egress_policy,
            idle_seconds: request.idle_seconds.unwrap_or(60),
        },
        EventData::SandboxStarted {
            sandbox_id: sandbox_id.clone(),
            snapshot_id: None,
        },
    ]
}

struct LocalSandboxTurnHandle {
    state: Arc<LocalSandboxState>,
    conversation_id: ConversationId,
    remote: Arc<dyn TurnHandle>,
}

#[async_trait]
impl SnapshotHandle for LocalSandboxTurnHandle {
    async fn snapshot_sandbox(&self, id: SandboxId) -> Result<SnapshotId> {
        let Some(local_id) = local_sandbox_id_for(&self.state, self.conversation_id, &id).await?
        else {
            return self.remote.snapshot_sandbox(id).await;
        };
        let snapshot_id = local_conversation_for(
            &self.state,
            self.conversation_id,
            &self.conversation_id.to_string(),
        )
        .await?
        .snapshot_sandbox(local_id)
        .await?;
        self.remote
            .add_events(vec![EventData::SandboxSnapshotted {
                sandbox_id: id,
                snapshot_id,
            }])
            .await?;
        Ok(snapshot_id)
    }

    async fn start_sandbox(&self, request: StartSandboxRequest) -> Result<()> {
        let Some(local_id) =
            local_sandbox_id_for(&self.state, self.conversation_id, &request.id).await?
        else {
            return self.remote.start_sandbox(request).await;
        };
        local_conversation_for(
            &self.state,
            self.conversation_id,
            &self.conversation_id.to_string(),
        )
        .await?
        .start_sandbox(StartSandboxRequest {
            id: local_id,
            snapshot_id: request.snapshot_id,
            idle_seconds: request.idle_seconds,
            provider: request.provider,
        })
        .await?;
        self.remote
            .add_events(vec![EventData::SandboxStarted {
                sandbox_id: request.id,
                snapshot_id: Some(request.snapshot_id),
            }])
            .await?;
        Ok(())
    }
}

#[async_trait]
impl TurnHandle for LocalSandboxTurnHandle {
    fn record(&self) -> &TurnRecord {
        self.remote.record()
    }

    async fn add_events(&self, data: Vec<EventData>) -> Result<AddEventsResult> {
        self.remote.add_events(data).await
    }

    async fn write_artifact(&self, request: WriteArtifactRequest) -> Result<ArtifactVersion> {
        self.remote.write_artifact(request).await
    }

    async fn finish(&self) -> Result<EventId> {
        self.remote.finish().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::local_test_config;
    use exoharness::{BasicExoHarness, EventQuery, EventQueryDirection, SandboxProvider};
    use tempfile::TempDir;

    #[tokio::test(flavor = "current_thread")]
    async fn local_sandbox_creation_only_records_remote_events() {
        let tempdir = TempDir::new().expect("tempdir should exist");
        let remote = Arc::new(
            BasicExoHarness::new(local_test_config(tempdir.path().join("remote")))
                .await
                .expect("remote harness should initialize"),
        );
        let local = Arc::new(
            BasicExoHarness::new(local_test_config(tempdir.path().join("local")))
                .await
                .expect("local harness should initialize"),
        );
        let remote_harness: Arc<dyn ExoHarness> = remote.clone();
        let local_harness: Arc<dyn ExoHarness> = local;
        let wrapper = LocalSandboxExoHarness::new_with_local_providers(
            remote_harness,
            local_harness,
            [SandboxProvider::LocalProcess],
        );

        let agent = wrapper
            .new_agent(NewAgentRequest {
                slug: "demo".to_string(),
                name: "Demo".to_string(),
            })
            .await
            .expect("agent should be created");
        let conversation = agent
            .new_conversation(NewConversationRequest {
                slug: Some("session".to_string()),
                name: Some("Session".to_string()),
            })
            .await
            .expect("conversation should be created");
        let sandbox_id = conversation
            .create_sandbox(CreateSandboxRequest {
                name: None,
                provider: SandboxProvider::LocalProcess,
                image: "local-image".to_string(),
                default_workdir: Some("/workspace".to_string()),
                file_system_mounts: Some(Vec::new()),
                durable_file_systems: None,
                enable_networking: None,
                egress_policy: Some(exoharness::EgressPolicy {
                    default_deny: false,
                    ..Default::default()
                }),
                idle_seconds: Some(120),
            })
            .await
            .expect("sandbox should be created");

        let remote_events = conversation
            .get_events(Some(EventQuery {
                cursor: None,
                direction: Some(EventQueryDirection::Asc),
                limit: None,
                session_id: None,
                turn_id: None,
                types: Some(vec![EventKind::SANDBOX_CREATED, EventKind::SANDBOX_STARTED]),
            }))
            .await
            .expect("remote events should load")
            .events;
        assert_eq!(remote_events.len(), 2);
        assert!(matches!(
            &remote_events[0].data,
            EventData::SandboxCreated { enable_networking: true, egress_policy: Some(policy), .. }
                if policy.permits_unrestricted_egress()
        ));
        let sandboxes = conversation
            .list_sandboxes()
            .await
            .expect("local sandboxes should list through the wrapper");
        assert_eq!(sandboxes.len(), 1);
        assert_eq!(sandboxes[0].id, sandbox_id);

        let remote_agent = remote
            .get_agent(&agent.record().id)
            .await
            .expect("remote get agent should succeed")
            .expect("remote agent should exist");
        let remote_conversation = remote_agent
            .get_conversation(&conversation.record().id)
            .await
            .expect("remote get conversation should succeed")
            .expect("remote conversation should exist");
        let remote_process = remote_conversation
            .run_in_sandbox(RunInSandboxRequest {
                id: sandbox_id,
                command: vec!["true".to_string()],
                env: Default::default(),
            })
            .await;
        assert!(remote_process.is_err());
    }
}
