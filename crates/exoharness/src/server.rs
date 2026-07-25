use std::sync::Arc;

use anyhow::anyhow;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, BufWriter};

use crate::protocol::{
    ClientMessage, ConversationHandleInfo, Request, Response, SandboxScope, ServerMessage,
};
use crate::{
    AgentHandle, AgentId, CancelSandboxProcessRequest, CloseSandboxProcessInputRequest,
    ConversationHandle, ConversationId, CreateSandboxRequest, ExoHarness,
    GetSandboxProcessEventsResult, ListConversationsResult, Result, SandboxId,
    SandboxProcessEventQuery, SandboxProcessRecord, SandboxProcessStatus, SessionId, SnapshotId,
    StartSandboxProcessRequest, StartSandboxRequest, TurnHandle, TurnId, TurnRecord,
    WaitSandboxProcessRequest, WriteSandboxProcessInputRequest,
};

pub struct ExoHarnessServer {
    root: Arc<dyn ExoHarness>,
}

impl ExoHarnessServer {
    pub fn new(root: Arc<dyn ExoHarness>) -> Self {
        Self { root }
    }

    pub async fn handle_request(&self, request: Request) -> Result<Response> {
        match request {
            Request::PreflightSecretStorage => {
                self.root.preflight_secret_storage().await?;
                Ok(Response::Unit)
            }
            Request::ListAgents => Ok(Response::Agents {
                agents: self
                    .root
                    .list_agents()
                    .await?
                    .into_iter()
                    .map(|agent| agent.record().clone())
                    .collect(),
            }),
            Request::GetAgent { agent_id } => Ok(Response::Agent {
                agent: self
                    .root
                    .get_agent(&agent_id)
                    .await?
                    .map(|agent| agent.record().clone()),
            }),
            Request::NewAgent { request } => {
                let agent = self.root.new_agent(request).await?;
                Ok(Response::Agent {
                    agent: Some(agent.record().clone()),
                })
            }
            Request::DeleteAgent { agent_id } => Ok(Response::Bool {
                value: self.root.delete_agent(&agent_id).await?,
            }),
            Request::ListBindings => Ok(Response::Bindings {
                bindings: self.root.list_bindings().await?,
            }),
            Request::PutBinding { binding } => Ok(Response::BindingId {
                binding_id: self.root.put_binding(binding).await?,
            }),
            Request::GetBinding { binding_id } => Ok(Response::Binding {
                binding: self.root.get_binding(&binding_id).await?,
            }),
            Request::ListSecrets => Ok(Response::Secrets {
                secrets: self.root.list_secrets().await?,
            }),
            Request::PutSecret { request } => Ok(Response::SecretId {
                secret_id: self.root.put_secret(request).await?,
            }),
            Request::GetSecret { secret_id } => Ok(Response::Secret {
                secret: self.root.get_secret(&secret_id).await?,
            }),
            Request::LogoutOauthSecret { secret_id } => Ok(Response::LogoutOauth {
                result: self.root.logout_oauth_secret(&secret_id).await?,
            }),
            Request::ListConversations { agent_id, request } => {
                let agent = self.require_agent(&agent_id).await?;
                let result = agent.list_conversations(request).await?;
                Ok(Response::Conversations {
                    result: ListConversationsResult {
                        conversations: result
                            .conversations
                            .into_iter()
                            .map(|conversation| ConversationHandleInfo {
                                agent_id,
                                record: conversation.record().clone(),
                            })
                            .collect(),
                        next_cursor: result.next_cursor,
                    },
                })
            }
            Request::GetConversation {
                agent_id,
                conversation_id,
            } => {
                let agent = self.require_agent(&agent_id).await?;
                Ok(Response::Conversation {
                    conversation: agent.get_conversation(&conversation_id).await?.map(
                        |conversation| ConversationHandleInfo {
                            agent_id,
                            record: conversation.record().clone(),
                        },
                    ),
                })
            }
            Request::NewConversation { agent_id, request } => {
                let agent = self.require_agent(&agent_id).await?;
                let conversation = agent.new_conversation(request).await?;
                Ok(Response::Conversation {
                    conversation: Some(ConversationHandleInfo {
                        agent_id,
                        record: conversation.record().clone(),
                    }),
                })
            }
            Request::DeleteConversation {
                agent_id,
                conversation_id,
            } => {
                let agent = self.require_agent(&agent_id).await?;
                Ok(Response::Bool {
                    value: agent.delete_conversation(&conversation_id).await?,
                })
            }
            Request::AgentListArtifacts { agent_id } => {
                let agent = self.require_agent(&agent_id).await?;
                Ok(Response::ArtifactVersions {
                    artifacts: agent.list_artifacts().await?,
                })
            }
            Request::AgentReadArtifact { agent_id, request } => {
                let agent = self.require_agent(&agent_id).await?;
                Ok(Response::Artifact {
                    artifact: agent.read_artifact(request).await?,
                })
            }
            Request::AgentWriteArtifact { agent_id, request } => {
                let agent = self.require_agent(&agent_id).await?;
                Ok(Response::ArtifactVersion {
                    artifact: agent.write_artifact(request).await?,
                })
            }
            Request::CreateSandbox { scope, request } => Ok(Response::SandboxId {
                sandbox_id: self.create_sandbox(scope, request).await?,
            }),
            Request::SnapshotSandbox { scope, sandbox_id } => Ok(Response::SnapshotId {
                snapshot_id: self.snapshot_sandbox(scope, sandbox_id).await?,
            }),
            Request::StartSandbox { scope, request } => {
                self.start_sandbox(scope, request).await?;
                Ok(Response::Unit)
            }
            Request::StopSandbox { scope, sandbox_id } => {
                self.stop_sandbox(scope, sandbox_id).await?;
                Ok(Response::Unit)
            }
            Request::StartSandboxProcess { scope, request } => Ok(Response::SandboxProcess {
                process: self.start_sandbox_process(scope, request).await?,
            }),
            Request::WriteSandboxProcessInput { scope, request } => {
                self.write_sandbox_process_input(scope, request).await?;
                Ok(Response::Unit)
            }
            Request::CloseSandboxProcessInput { scope, request } => {
                self.close_sandbox_process_input(scope, request).await?;
                Ok(Response::Unit)
            }
            Request::GetSandboxProcessEvents { scope, query } => {
                Ok(Response::SandboxProcessEvents {
                    result: self.get_sandbox_process_events(scope, query).await?,
                })
            }
            Request::WaitSandboxProcess { scope, request } => Ok(Response::SandboxProcessStatus {
                status: self.wait_sandbox_process(scope, request).await?,
            }),
            Request::CancelSandboxProcess { scope, request } => {
                Ok(Response::SandboxProcessStatus {
                    status: self.cancel_sandbox_process(scope, request).await?,
                })
            }
            Request::AgentListBindings { agent_id } => {
                let agent = self.require_agent(&agent_id).await?;
                Ok(Response::Bindings {
                    bindings: agent.list_bindings().await?,
                })
            }
            Request::AgentPutBinding { agent_id, binding } => {
                let agent = self.require_agent(&agent_id).await?;
                Ok(Response::BindingId {
                    binding_id: agent.put_binding(binding).await?,
                })
            }
            Request::AgentGetBinding {
                agent_id,
                binding_id,
            } => {
                let agent = self.require_agent(&agent_id).await?;
                Ok(Response::Binding {
                    binding: agent.get_binding(&binding_id).await?,
                })
            }
            Request::AgentListSecrets { agent_id } => {
                let agent = self.require_agent(&agent_id).await?;
                Ok(Response::Secrets {
                    secrets: agent.list_secrets().await?,
                })
            }
            Request::AgentPutSecret { agent_id, request } => {
                let agent = self.require_agent(&agent_id).await?;
                Ok(Response::SecretId {
                    secret_id: agent.put_secret(request).await?,
                })
            }
            Request::AgentGetSecret {
                agent_id,
                secret_id,
            } => {
                let agent = self.require_agent(&agent_id).await?;
                Ok(Response::Secret {
                    secret: agent.get_secret(&secret_id).await?,
                })
            }
            Request::ConversationStartSession {
                agent_id,
                conversation_id,
            } => {
                let conversation = self.require_conversation(agent_id, conversation_id).await?;
                Ok(Response::SessionId {
                    session_id: conversation.start_session().await?,
                })
            }
            Request::ConversationEndSession {
                agent_id,
                conversation_id,
                session_id,
            } => {
                let conversation = self.require_conversation(agent_id, conversation_id).await?;
                conversation.end_session(session_id).await?;
                Ok(Response::Unit)
            }
            Request::ConversationBeginTurn {
                agent_id,
                conversation_id,
                request,
            } => {
                let conversation = self.require_conversation(agent_id, conversation_id).await?;
                let turn = conversation.begin_turn(request).await?;
                Ok(Response::Turn {
                    turn: crate::protocol::TurnHandleInfo {
                        conversation: ConversationHandleInfo {
                            agent_id,
                            record: conversation.record().clone(),
                        },
                        record: turn.record().clone(),
                    },
                })
            }
            Request::ConversationGetEvents {
                agent_id,
                conversation_id,
                query,
            } => {
                let conversation = self.require_conversation(agent_id, conversation_id).await?;
                Ok(Response::Events {
                    result: conversation.get_events(query).await?,
                })
            }
            Request::ConversationGetEvent {
                agent_id,
                conversation_id,
                event_id,
            } => {
                let conversation = self.require_conversation(agent_id, conversation_id).await?;
                Ok(Response::Event {
                    event: conversation.get_event(event_id).await?,
                })
            }
            Request::ConversationAddEvents {
                agent_id,
                conversation_id,
                request,
            } => {
                let conversation = self.require_conversation(agent_id, conversation_id).await?;
                Ok(Response::AddEvents {
                    result: conversation.add_events(request).await?,
                })
            }
            Request::ConversationFork {
                agent_id,
                conversation_id,
                request,
            } => {
                let conversation = self.require_conversation(agent_id, conversation_id).await?;
                let forked = conversation.fork(request).await?;
                Ok(Response::Conversation {
                    conversation: Some(ConversationHandleInfo {
                        agent_id,
                        record: forked.record().clone(),
                    }),
                })
            }
            Request::ConversationListArtifacts {
                agent_id,
                conversation_id,
            } => {
                let conversation = self.require_conversation(agent_id, conversation_id).await?;
                Ok(Response::ArtifactVersions {
                    artifacts: conversation.list_artifacts().await?,
                })
            }
            Request::ConversationReadArtifact {
                agent_id,
                conversation_id,
                request,
            } => {
                let conversation = self.require_conversation(agent_id, conversation_id).await?;
                Ok(Response::Artifact {
                    artifact: conversation.read_artifact(request).await?,
                })
            }
            Request::ConversationWriteArtifact {
                agent_id,
                conversation_id,
                request,
            } => {
                let conversation = self.require_conversation(agent_id, conversation_id).await?;
                Ok(Response::ArtifactVersion {
                    artifact: conversation.write_artifact(request).await?,
                })
            }
            Request::ConversationListBindings {
                agent_id,
                conversation_id,
            } => {
                let conversation = self.require_conversation(agent_id, conversation_id).await?;
                Ok(Response::Bindings {
                    bindings: conversation.list_bindings().await?,
                })
            }
            Request::ConversationPutBinding {
                agent_id,
                conversation_id,
                binding,
            } => {
                let conversation = self.require_conversation(agent_id, conversation_id).await?;
                Ok(Response::BindingId {
                    binding_id: conversation.put_binding(binding).await?,
                })
            }
            Request::ConversationGetBinding {
                agent_id,
                conversation_id,
                binding_id,
            } => {
                let conversation = self.require_conversation(agent_id, conversation_id).await?;
                Ok(Response::Binding {
                    binding: conversation.get_binding(&binding_id).await?,
                })
            }
            Request::ConversationListSecrets {
                agent_id,
                conversation_id,
            } => {
                let conversation = self.require_conversation(agent_id, conversation_id).await?;
                Ok(Response::Secrets {
                    secrets: conversation.list_secrets().await?,
                })
            }
            Request::ConversationPutSecret {
                agent_id,
                conversation_id,
                request,
            } => {
                let conversation = self.require_conversation(agent_id, conversation_id).await?;
                Ok(Response::SecretId {
                    secret_id: conversation.put_secret(request).await?,
                })
            }
            Request::ConversationGetSecret {
                agent_id,
                conversation_id,
                secret_id,
            } => {
                let conversation = self.require_conversation(agent_id, conversation_id).await?;
                Ok(Response::Secret {
                    secret: conversation.get_secret(&secret_id).await?,
                })
            }
            Request::TurnAddEvents {
                agent_id,
                conversation_id,
                session_id,
                turn_id,
                data,
            } => {
                let turn = self
                    .require_turn(agent_id, conversation_id, session_id, turn_id)
                    .await?;
                Ok(Response::AddEvents {
                    result: turn.add_events(data).await?,
                })
            }
            Request::TurnWriteArtifact {
                agent_id,
                conversation_id,
                session_id,
                turn_id,
                request,
            } => {
                let turn = self
                    .require_turn(agent_id, conversation_id, session_id, turn_id)
                    .await?;
                Ok(Response::ArtifactVersion {
                    artifact: turn.write_artifact(request).await?,
                })
            }
            Request::TurnFinish {
                agent_id,
                conversation_id,
                session_id,
                turn_id,
            } => {
                let turn = self
                    .require_turn(agent_id, conversation_id, session_id, turn_id)
                    .await?;
                Ok(Response::EventId {
                    event_id: turn.finish().await?,
                })
            }
        }
    }

    pub async fn serve_jsonl<R, W>(&self, reader: R, writer: W) -> Result<()>
    where
        R: AsyncRead + Unpin,
        W: AsyncWrite + Unpin,
    {
        let mut lines = BufReader::new(reader).lines();
        let mut writer = BufWriter::new(writer);

        while let Some(line) = lines.next_line().await? {
            let message: ClientMessage = serde_json::from_str(&line)?;
            let ClientMessage::Request { id, request } = message;
            let response = match self.handle_request(request).await {
                Ok(response) => ServerMessage::Response {
                    id,
                    ok: true,
                    response: Some(response),
                    error: None,
                },
                Err(error) => ServerMessage::Response {
                    id,
                    ok: false,
                    response: None,
                    error: Some(error.to_string()),
                },
            };
            let encoded = serde_json::to_vec(&response)?;
            writer.write_all(&encoded).await?;
            writer.write_all(b"\n").await?;
            writer.flush().await?;
        }

        Ok(())
    }

    async fn create_sandbox(
        &self,
        scope: SandboxScope,
        request: CreateSandboxRequest,
    ) -> Result<SandboxId> {
        match scope {
            SandboxScope::Agent { agent_id } => {
                self.require_agent(&agent_id)
                    .await?
                    .create_sandbox(request)
                    .await
            }
            SandboxScope::Conversation {
                agent_id,
                conversation_id,
            } => {
                self.require_conversation(agent_id, conversation_id)
                    .await?
                    .create_sandbox(request)
                    .await
            }
            SandboxScope::Turn { .. } => {
                Err(anyhow!("create_sandbox is not supported on a turn scope"))
            }
        }
    }

    async fn snapshot_sandbox(
        &self,
        scope: SandboxScope,
        sandbox_id: SandboxId,
    ) -> Result<SnapshotId> {
        match scope {
            SandboxScope::Agent { agent_id } => {
                self.require_agent(&agent_id)
                    .await?
                    .snapshot_sandbox(sandbox_id)
                    .await
            }
            SandboxScope::Conversation {
                agent_id,
                conversation_id,
            } => {
                self.require_conversation(agent_id, conversation_id)
                    .await?
                    .snapshot_sandbox(sandbox_id)
                    .await
            }
            SandboxScope::Turn {
                agent_id,
                conversation_id,
                session_id,
                turn_id,
            } => {
                self.require_turn(agent_id, conversation_id, session_id, turn_id)
                    .await?
                    .snapshot_sandbox(sandbox_id)
                    .await
            }
        }
    }

    async fn start_sandbox(&self, scope: SandboxScope, request: StartSandboxRequest) -> Result<()> {
        match scope {
            SandboxScope::Agent { agent_id } => {
                self.require_agent(&agent_id)
                    .await?
                    .start_sandbox(request)
                    .await
            }
            SandboxScope::Conversation {
                agent_id,
                conversation_id,
            } => {
                self.require_conversation(agent_id, conversation_id)
                    .await?
                    .start_sandbox(request)
                    .await
            }
            SandboxScope::Turn {
                agent_id,
                conversation_id,
                session_id,
                turn_id,
            } => {
                self.require_turn(agent_id, conversation_id, session_id, turn_id)
                    .await?
                    .start_sandbox(request)
                    .await
            }
        }
    }

    async fn stop_sandbox(&self, scope: SandboxScope, sandbox_id: SandboxId) -> Result<()> {
        match scope {
            SandboxScope::Agent { agent_id } => {
                self.require_agent(&agent_id)
                    .await?
                    .stop_sandbox(sandbox_id)
                    .await
            }
            SandboxScope::Conversation {
                agent_id,
                conversation_id,
            } => {
                self.require_conversation(agent_id, conversation_id)
                    .await?
                    .stop_sandbox(sandbox_id)
                    .await
            }
            SandboxScope::Turn { .. } => {
                Err(anyhow!("stop_sandbox is not supported on a turn scope"))
            }
        }
    }

    async fn start_sandbox_process(
        &self,
        scope: SandboxScope,
        request: StartSandboxProcessRequest,
    ) -> Result<SandboxProcessRecord> {
        match scope {
            SandboxScope::Agent { agent_id } => {
                self.require_agent(&agent_id)
                    .await?
                    .start_sandbox_process(request)
                    .await
            }
            SandboxScope::Conversation {
                agent_id,
                conversation_id,
            } => {
                self.require_conversation(agent_id, conversation_id)
                    .await?
                    .start_sandbox_process(request)
                    .await
            }
            SandboxScope::Turn { .. } => Err(anyhow!(
                "start_sandbox_process is not supported on a turn scope"
            )),
        }
    }

    async fn write_sandbox_process_input(
        &self,
        scope: SandboxScope,
        request: WriteSandboxProcessInputRequest,
    ) -> Result<()> {
        match scope {
            SandboxScope::Agent { agent_id } => {
                self.require_agent(&agent_id)
                    .await?
                    .write_sandbox_process_input(request)
                    .await
            }
            SandboxScope::Conversation {
                agent_id,
                conversation_id,
            } => {
                self.require_conversation(agent_id, conversation_id)
                    .await?
                    .write_sandbox_process_input(request)
                    .await
            }
            SandboxScope::Turn { .. } => Err(anyhow!(
                "write_sandbox_process_input is not supported on a turn scope"
            )),
        }
    }

    async fn close_sandbox_process_input(
        &self,
        scope: SandboxScope,
        request: CloseSandboxProcessInputRequest,
    ) -> Result<()> {
        match scope {
            SandboxScope::Agent { agent_id } => {
                self.require_agent(&agent_id)
                    .await?
                    .close_sandbox_process_input(request)
                    .await
            }
            SandboxScope::Conversation {
                agent_id,
                conversation_id,
            } => {
                self.require_conversation(agent_id, conversation_id)
                    .await?
                    .close_sandbox_process_input(request)
                    .await
            }
            SandboxScope::Turn { .. } => Err(anyhow!(
                "close_sandbox_process_input is not supported on a turn scope"
            )),
        }
    }

    async fn get_sandbox_process_events(
        &self,
        scope: SandboxScope,
        query: SandboxProcessEventQuery,
    ) -> Result<GetSandboxProcessEventsResult> {
        match scope {
            SandboxScope::Agent { agent_id } => {
                self.require_agent(&agent_id)
                    .await?
                    .get_sandbox_process_events(query)
                    .await
            }
            SandboxScope::Conversation {
                agent_id,
                conversation_id,
            } => {
                self.require_conversation(agent_id, conversation_id)
                    .await?
                    .get_sandbox_process_events(query)
                    .await
            }
            SandboxScope::Turn { .. } => Err(anyhow!(
                "get_sandbox_process_events is not supported on a turn scope"
            )),
        }
    }

    async fn wait_sandbox_process(
        &self,
        scope: SandboxScope,
        request: WaitSandboxProcessRequest,
    ) -> Result<SandboxProcessStatus> {
        match scope {
            SandboxScope::Agent { agent_id } => {
                self.require_agent(&agent_id)
                    .await?
                    .wait_sandbox_process(request)
                    .await
            }
            SandboxScope::Conversation {
                agent_id,
                conversation_id,
            } => {
                self.require_conversation(agent_id, conversation_id)
                    .await?
                    .wait_sandbox_process(request)
                    .await
            }
            SandboxScope::Turn { .. } => Err(anyhow!(
                "wait_sandbox_process is not supported on a turn scope"
            )),
        }
    }

    async fn cancel_sandbox_process(
        &self,
        scope: SandboxScope,
        request: CancelSandboxProcessRequest,
    ) -> Result<SandboxProcessStatus> {
        match scope {
            SandboxScope::Agent { agent_id } => {
                self.require_agent(&agent_id)
                    .await?
                    .cancel_sandbox_process(request)
                    .await
            }
            SandboxScope::Conversation {
                agent_id,
                conversation_id,
            } => {
                self.require_conversation(agent_id, conversation_id)
                    .await?
                    .cancel_sandbox_process(request)
                    .await
            }
            SandboxScope::Turn { .. } => Err(anyhow!(
                "cancel_sandbox_process is not supported on a turn scope"
            )),
        }
    }

    async fn require_agent(&self, agent_id: &AgentId) -> Result<Arc<dyn AgentHandle>> {
        self.root
            .get_agent(agent_id)
            .await?
            .ok_or_else(|| anyhow!("agent {agent_id} not found"))
    }

    async fn require_conversation(
        &self,
        agent_id: AgentId,
        conversation_id: ConversationId,
    ) -> Result<Arc<dyn ConversationHandle>> {
        self.require_agent(&agent_id)
            .await?
            .get_conversation(&conversation_id)
            .await?
            .ok_or_else(|| anyhow!("conversation {conversation_id} not found"))
    }

    async fn require_turn(
        &self,
        agent_id: AgentId,
        conversation_id: ConversationId,
        session_id: SessionId,
        turn_id: TurnId,
    ) -> Result<Arc<dyn TurnHandle>> {
        self.require_conversation(agent_id, conversation_id)
            .await?
            .turn_handle(TurnRecord {
                id: turn_id,
                session_id,
            })
            .await
    }
}
