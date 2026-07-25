use std::collections::HashMap;
use std::fmt::{self, Display, Formatter};
use std::ops::Bound;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};

use anyhow::{Context, anyhow, bail};
use async_trait::async_trait;
use bytes::Bytes;
use chrono::{DateTime, TimeDelta, Utc};
use futures::StreamExt;
use futures::future::BoxFuture;
use futures::io::{AsyncReadExt, AsyncWriteExt};
use futures::stream::{self, BoxStream};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::{Mutex as AsyncMutex, Notify, mpsc};
use tokio::task::JoinHandle;
use tokio_stream::wrappers::UnboundedReceiverStream;

use crate::oauth::OAuthProviderRegistry;
use crate::sandbox::{
    CliContainerSandboxBackend, LocalProcessSandboxBackend, ManagedSandboxBackend,
    ManagedSandboxHandle, SANDBOX_MAIN_MOUNT_DIR, SandboxCommand, SandboxKey,
    SandboxLifecycleConfig, SandboxMount, SandboxMountAccess, SandboxNetworkPolicy, SandboxRequest,
    SandboxSpec, SnapshotKind, SnapshotPayload, sandbox_spec_hash,
};
#[cfg(feature = "apple-keychain")]
use crate::secrets::{AppleKeychainSecretKeyProvider, migrate_apple_keychain_master_key};
use crate::secrets::{
    EncryptedSecret, FileBackedSecretKeyProvider, SecretCipher, SecretKeyProvider,
    StaticSecretKeyProvider, default_master_key_path,
};
use crate::storage::BasicObjectStore;
use crate::{
    AddEventsRequest, AddEventsResult, AgentHandle, AgentId, AgentRecord, Artifact,
    ArtifactVersion, BeginTurnRequest, Binding, BindingId, BindingRecord, BindingType,
    BoxAsyncRead, BoxAsyncWrite, CancelSandboxProcessRequest, CloseSandboxProcessInputRequest,
    ConversationHandle, ConversationId, ConversationRecord, CreateSandboxRequest,
    DurableFileSystem, Event, EventData, EventId, EventKind, EventQuery, EventQueryDirection,
    EventStream, ExoHarness, FileSystemMount, ForkConversationRequest, GetEventsResult,
    GetSandboxProcessEventsResult, ListConversationsRequest, ListConversationsResult,
    LogoutOauthResult, NewAgentRequest, NewConversationRequest, PutSecretRequest,
    ReadArtifactRequest, Result, RunInSandboxRequest, SandboxHandle, SandboxId, SandboxProcess,
    SandboxProcessEvent, SandboxProcessEventQuery, SandboxProcessId, SandboxProcessMode,
    SandboxProcessParts, SandboxProcessRecord, SandboxProcessStatus, SandboxProcessStdin,
    SandboxProvider, SandboxProviderConfig, Secret, SecretId, SecretMetadata, SecretType,
    SessionId, SnapshotHandle, SnapshotId, StartSandboxProcessRequest, StartSandboxRequest,
    TurnHandle, TurnId, TurnRecord, Uuid7, WaitSandboxProcessRequest, WriteArtifactRequest,
    WriteSandboxProcessInputRequest,
};

const SANDBOX_PROVIDER_STATE_EVENT: &str = "sandbox_provider_state";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct SandboxProviderStatePayload {
    sandbox_id: SandboxId,
    provider: SandboxProvider,
    state_key: String,
    state: Value,
}

#[derive(Debug, Clone)]
pub enum SecretBackendChoice {
    #[cfg(feature = "apple-keychain")]
    AppleKeychain,
    File {
        path: Option<PathBuf>,
    },
    Static([u8; 32]),
}

type SandboxBackendFactory = Arc<
    dyn for<'a> Fn(
            &'a BasicExoHarnessInner,
        ) -> BoxFuture<'a, Result<Arc<dyn ManagedSandboxBackend>>>
        + Send
        + Sync,
>;

/// Registers a backend implementation for a [`SandboxProvider`].
///
/// Built-in registrations resolve remote-provider secrets lazily on first use.
/// Callers with their own provider implementation can register an already-built
/// trait object with [`SandboxBackendRegistration::from_backend`].
#[derive(Clone)]
pub struct SandboxBackendRegistration {
    provider: SandboxProvider,
    factory: SandboxBackendFactory,
}

impl SandboxBackendRegistration {
    pub fn from_backend(
        provider: SandboxProvider,
        backend: Arc<dyn ManagedSandboxBackend>,
    ) -> Self {
        Self::from_factory(provider, move |_| {
            let backend = Arc::clone(&backend);
            Box::pin(async move { Ok(backend) })
        })
    }

    pub fn apple_container() -> Self {
        Self::from_factory(SandboxProvider::AppleContainer, |_| {
            Box::pin(async {
                Ok(Arc::new(CliContainerSandboxBackend::apple_container())
                    as Arc<dyn ManagedSandboxBackend>)
            })
        })
    }

    pub fn docker() -> Self {
        Self::from_factory(SandboxProvider::Docker, |_| {
            Box::pin(async {
                Ok(Arc::new(CliContainerSandboxBackend::docker())
                    as Arc<dyn ManagedSandboxBackend>)
            })
        })
    }

    pub fn local_process() -> Self {
        Self::from_factory(SandboxProvider::LocalProcess, |_| {
            Box::pin(async {
                Ok(Arc::new(LocalProcessSandboxBackend::new()) as Arc<dyn ManagedSandboxBackend>)
            })
        })
    }

    pub fn daytona(spec: DaytonaBackendSpec) -> Self {
        Self::from_factory(SandboxProvider::Daytona, move |inner| {
            let spec = spec.clone();
            Box::pin(async move {
                let config = match inner.daytona_config_from_binding().await? {
                    Some(config) => config,
                    None => inner.daytona_config_from_spec(&spec).await?,
                };
                Ok(Arc::new(crate::DaytonaSandboxBackend::new(config)?)
                    as Arc<dyn ManagedSandboxBackend>)
            })
        })
    }

    pub fn e2b(spec: E2bBackendSpec) -> Self {
        Self::from_factory(SandboxProvider::E2b, move |inner| {
            let spec = spec.clone();
            Box::pin(async move {
                let config = match inner.e2b_config_from_binding().await? {
                    Some(config) => config,
                    None => inner.e2b_config_from_spec(&spec).await?,
                };
                Ok(Arc::new(crate::E2bSandboxBackend::new(config)?)
                    as Arc<dyn ManagedSandboxBackend>)
            })
        })
    }

    pub fn sprites(spec: SpritesBackendSpec) -> Self {
        Self::from_factory(SandboxProvider::Sprites, move |inner| {
            let spec = spec.clone();
            Box::pin(async move {
                let config = match inner.sprites_config_from_binding().await? {
                    Some(config) => config,
                    None => inner.sprites_config_from_spec(&spec).await?,
                };
                Ok(Arc::new(crate::SpritesSandboxBackend::new(config)?)
                    as Arc<dyn ManagedSandboxBackend>)
            })
        })
    }

    pub fn vercel(spec: VercelBackendSpec) -> Self {
        Self::from_factory(SandboxProvider::Vercel, move |inner| {
            let spec = spec.clone();
            Box::pin(async move {
                let config = match inner.vercel_config_from_binding().await? {
                    Some(config) => config,
                    None => inner.vercel_config_from_spec(&spec).await?,
                };
                Ok(Arc::new(crate::VercelSandboxBackend::new(config)?)
                    as Arc<dyn ManagedSandboxBackend>)
            })
        })
    }

    pub fn aws_agentcore() -> Self {
        Self::from_factory(SandboxProvider::AwsAgentCore, |_inner| {
            Box::pin(async move {
                #[cfg(feature = "aws-agentcore")]
                {
                    let config = _inner.aws_agentcore_config_from_binding().await?.ok_or_else(|| {
                        anyhow!(
                            "aws-agentcore sandbox requested but no sandbox provider binding is configured; run `exo provider configure --provider aws-agentcore --runtime-arn <arn>`"
                        )
                    })?;
                    Ok(
                        Arc::new(crate::AwsAgentCoreSandboxBackend::new(config).await?)
                            as Arc<dyn ManagedSandboxBackend>,
                    )
                }
                #[cfg(not(feature = "aws-agentcore"))]
                {
                    bail!(
                        "aws-agentcore sandbox backend requires building exoharness with the aws-agentcore feature"
                    );
                }
            })
        })
    }

    pub fn provider(&self) -> SandboxProvider {
        self.provider
    }

    fn from_factory<F>(provider: SandboxProvider, factory: F) -> Self
    where
        F: for<'a> Fn(
                &'a BasicExoHarnessInner,
            ) -> BoxFuture<'a, Result<Arc<dyn ManagedSandboxBackend>>>
            + Send
            + Sync
            + 'static,
    {
        Self {
            provider,
            factory: Arc::new(factory),
        }
    }
}

/// Daytona connection config plus the secret-store names for its credentials,
/// resolved lazily so the harness can advertise Daytona before any are set.
#[derive(Debug, Clone)]
pub struct DaytonaBackendSpec {
    pub api_url: String,
    pub toolbox_url: String,
    /// Secret holding the API key (required at first use).
    pub api_key_secret: String,
    pub organization_id_secret: Option<String>,
    pub target_secret: Option<String>,
}

impl Default for DaytonaBackendSpec {
    /// Official endpoints; credentials read from the conventional `DAYTONA_*`
    /// secret names.
    fn default() -> Self {
        Self {
            api_url: crate::DEFAULT_DAYTONA_API_URL.to_string(),
            toolbox_url: crate::DEFAULT_DAYTONA_TOOLBOX_URL.to_string(),
            api_key_secret: "DAYTONA_API_KEY".to_string(),
            organization_id_secret: Some("DAYTONA_ORGANIZATION_ID".to_string()),
            target_secret: Some("DAYTONA_TARGET".to_string()),
        }
    }
}

impl DaytonaBackendSpec {
    /// Official endpoints; credentials read from the conventional `DAYTONA_*`
    /// secret names.
    pub fn with_conventional_secrets() -> Self {
        Self::default()
    }
}

/// E2B connection config plus secret-store names for credentials, resolved
/// lazily on first use.
#[derive(Debug, Clone)]
pub struct E2bBackendSpec {
    pub api_url: String,
    pub api_key_secret: String,
    pub template_id: String,
}

impl Default for E2bBackendSpec {
    fn default() -> Self {
        Self {
            api_url: crate::DEFAULT_E2B_API_URL.to_string(),
            api_key_secret: "E2B_API_KEY".to_string(),
            template_id: crate::default_e2b_template(),
        }
    }
}

/// Sprites connection config plus secret-store name for the bearer token,
/// resolved lazily on first use.
#[derive(Debug, Clone)]
pub struct SpritesBackendSpec {
    pub api_url: String,
    pub token_secret: String,
    pub url_auth: Option<String>,
    pub organization: Option<String>,
    pub labels: Vec<String>,
}

impl Default for SpritesBackendSpec {
    fn default() -> Self {
        Self {
            api_url: crate::DEFAULT_SPRITES_API_URL.to_string(),
            token_secret: "SPRITES_TOKEN".to_string(),
            url_auth: None,
            organization: None,
            labels: Vec::new(),
        }
    }
}

/// Vercel connection config plus the secret-store names for its credentials,
/// resolved lazily so the harness can advertise Vercel before any are set.
#[derive(Debug, Clone)]
pub struct VercelBackendSpec {
    pub api_url: String,
    pub api_token_secret: String,
    pub team_id_secret: String,
    pub project_id_secret: String,
}

impl VercelBackendSpec {
    /// Official endpoint; credentials read from conventional `VERCEL_*` secret
    /// names.
    pub fn with_conventional_secrets() -> Self {
        Self {
            api_url: crate::DEFAULT_VERCEL_API_URL.to_string(),
            api_token_secret: "VERCEL_TOKEN".to_string(),
            team_id_secret: "VERCEL_TEAM_ID".to_string(),
            project_id_secret: "VERCEL_PROJECT_ID".to_string(),
        }
    }
}

// TODO: as more knobs land here, swap to a builder pattern.
#[derive(Clone)]
pub struct BasicExoHarnessConfig {
    pub root: PathBuf,
    pub secret_backend: SecretBackendChoice,
    /// Default when a caller doesn't request a provider. Must be in `sandbox_backends`.
    pub sandbox_default: SandboxProvider,
    /// Supported providers; anything not listed is rejected.
    pub sandbox_backends: Vec<SandboxBackendRegistration>,
}

#[derive(Clone)]
pub struct BasicExoHarness {
    inner: Arc<BasicExoHarnessInner>,
}

struct BasicExoHarnessInner {
    storage: BasicObjectStore,
    write_lock: AsyncMutex<()>,
    secret_locks: AsyncMutex<HashMap<SecretId, Weak<AsyncMutex<()>>>>,
    oauth_providers: OAuthProviderRegistry,
    subscribers: Mutex<HashMap<ConversationId, Vec<mpsc::UnboundedSender<Result<Event>>>>>,
    sandbox_registry: HashMap<SandboxProvider, SandboxBackendRegistration>,
    /// Backends built (and secrets read) lazily on first use, cached by provider.
    sandbox_backends: AsyncMutex<HashMap<SandboxProvider, Arc<dyn ManagedSandboxBackend>>>,
    running_sandboxes: AsyncMutex<HashMap<SandboxId, Arc<dyn ManagedSandboxHandle>>>,
    running_processes: AsyncMutex<HashMap<SandboxProcessId, Arc<RunningSandboxProcess>>>,
    secret_cipher: SecretCipher,
}

impl BasicExoHarnessInner {
    async fn sandbox_backend_for_provider(
        &self,
        provider: SandboxProvider,
    ) -> Result<Arc<dyn ManagedSandboxBackend>> {
        if let Some(backend) = self.sandbox_backends.lock().await.get(&provider) {
            return Ok(Arc::clone(backend));
        }
        let registration = self.sandbox_registry.get(&provider).ok_or_else(|| {
            anyhow!("sandbox provider {provider:?} is not supported by this harness")
        })?;
        // Build without the cache lock so a slow build (secret I/O) doesn't
        // serialize other providers; a concurrent build loses to `or_insert`.
        let backend = (registration.factory)(self).await?;
        Ok(Arc::clone(
            self.sandbox_backends
                .lock()
                .await
                .entry(provider)
                .or_insert(backend),
        ))
    }

    /// `DaytonaConfig` from the conventional `DAYTONA_*` secret-name spec.
    async fn daytona_config_from_spec(
        &self,
        spec: &DaytonaBackendSpec,
    ) -> Result<crate::DaytonaConfig> {
        let api_key = self
            .secret_key(&spec.api_key_secret)
            .await?
            .ok_or_else(|| {
                anyhow!(
                    "daytona sandbox requested but secret {:?} is not set",
                    spec.api_key_secret
                )
            })?;
        let organization_id = match &spec.organization_id_secret {
            Some(name) => self.secret_key(name).await?,
            None => None,
        };
        let target = match &spec.target_secret {
            Some(name) => self.secret_key(name).await?,
            None => None,
        };
        Ok(crate::DaytonaConfig {
            api_key,
            api_url: spec.api_url.clone(),
            toolbox_url: spec.toolbox_url.clone(),
            target,
            organization_id,
        })
    }

    async fn e2b_config_from_spec(&self, spec: &E2bBackendSpec) -> Result<crate::E2bConfig> {
        let api_key = self
            .secret_key(&spec.api_key_secret)
            .await?
            .ok_or_else(|| {
                anyhow!(
                    "e2b sandbox requested but secret {:?} is not set",
                    spec.api_key_secret
                )
            })?;
        Ok(crate::E2bConfig {
            api_key,
            api_url: spec.api_url.clone(),
            template_id: spec.template_id.clone(),
            envd_port: crate::DEFAULT_E2B_ENVD_PORT,
            envd_base_url: None,
            secure: false,
        })
    }

    async fn e2b_config_from_binding(&self) -> Result<Option<crate::E2bConfig>> {
        let bindings = list_binding_records(&self.storage, Path::new("bindings")).await?;
        let Some((api_key_secret_id, api_url, default_image)) = bindings
            .into_iter()
            .rev()
            .find_map(|record| match record.binding {
                Binding::Sandbox {
                    config:
                        SandboxProviderConfig::E2b {
                            api_key_secret_id,
                            api_url,
                            default_image,
                        },
                    ..
                } => Some((api_key_secret_id, api_url, default_image)),
                _ => None,
            })
        else {
            return Ok(None);
        };
        let api_key = self
            .secret_key_by_id(api_key_secret_id)
            .await?
            .ok_or_else(|| {
                anyhow!(
                    "e2b sandbox binding references secret id {api_key_secret_id}, which is not set"
                )
            })?;
        Ok(Some(crate::E2bConfig {
            api_key,
            api_url: api_url.unwrap_or_else(|| crate::DEFAULT_E2B_API_URL.to_string()),
            template_id: default_image,
            envd_port: crate::DEFAULT_E2B_ENVD_PORT,
            envd_base_url: None,
            secure: false,
        }))
    }

    async fn sprites_config_from_spec(
        &self,
        spec: &SpritesBackendSpec,
    ) -> Result<crate::SpritesConfig> {
        let token = self.secret_key(&spec.token_secret).await?.ok_or_else(|| {
            anyhow!(
                "sprites sandbox requested but secret {:?} is not set",
                spec.token_secret
            )
        })?;
        Ok(crate::SpritesConfig {
            token,
            api_url: spec.api_url.clone(),
            url_auth: spec.url_auth.clone(),
            organization: spec.organization.clone(),
            extra_labels: spec.labels.clone(),
        })
    }

    async fn sprites_config_from_binding(&self) -> Result<Option<crate::SpritesConfig>> {
        let bindings = list_binding_records(&self.storage, Path::new("bindings")).await?;
        let Some((token_secret_id, api_url, url_auth, organization, labels)) = bindings
            .into_iter()
            .rev()
            .find_map(|record| match record.binding {
                Binding::Sandbox {
                    config:
                        SandboxProviderConfig::Sprites {
                            token_secret_id,
                            api_url,
                            url_auth,
                            organization,
                            labels,
                        },
                    ..
                } => Some((token_secret_id, api_url, url_auth, organization, labels)),
                _ => None,
            })
        else {
            return Ok(None);
        };
        let token = self
            .secret_key_by_id(token_secret_id)
            .await?
            .ok_or_else(|| {
                anyhow!(
                    "sprites sandbox binding references secret id {token_secret_id}, which is not set"
                )
            })?;
        Ok(Some(crate::SpritesConfig {
            token,
            api_url: api_url.unwrap_or_else(|| crate::DEFAULT_SPRITES_API_URL.to_string()),
            url_auth,
            organization,
            extra_labels: labels,
        }))
    }

    /// `VercelConfig` from the conventional `VERCEL_*` secret-name spec.
    async fn vercel_config_from_spec(
        &self,
        spec: &VercelBackendSpec,
    ) -> Result<crate::VercelConfig> {
        let api_token = self
            .secret_key(&spec.api_token_secret)
            .await?
            .ok_or_else(|| {
                anyhow!(
                    "vercel sandbox requested but secret {:?} is not set",
                    spec.api_token_secret
                )
            })?;
        let team_id = self
            .secret_key(&spec.team_id_secret)
            .await?
            .ok_or_else(|| {
                anyhow!(
                    "vercel sandbox requested but secret {:?} is not set",
                    spec.team_id_secret
                )
            })?;
        let project_id = self
            .secret_key(&spec.project_id_secret)
            .await?
            .ok_or_else(|| {
                anyhow!(
                    "vercel sandbox requested but secret {:?} is not set",
                    spec.project_id_secret
                )
            })?;
        Ok(crate::VercelConfig {
            api_token,
            api_url: spec.api_url.clone(),
            team_id,
            project_id,
        })
    }

    /// Decrypt the first stored secret matching `predicate`, if any.
    async fn find_secret_by(
        &self,
        predicate: impl Fn(&StoredSecret) -> bool,
    ) -> Result<Option<Secret>> {
        let stored = self
            .storage
            .list_json_matching_suffix::<StoredSecret>(Path::new("secrets"), ".json")
            .await?;
        match stored.into_iter().find(|s| predicate(s)) {
            Some(record) => Ok(Some(self.secret_cipher.decrypt_secret(&record.secret)?)),
            None => Ok(None),
        }
    }

    /// Decrypt the `Key`-typed secret stored under `name`, if it exists.
    async fn secret_key(&self, name: &str) -> Result<Option<String>> {
        match self.find_secret_by(|s| s.metadata.name == name).await? {
            Some(Secret::Key { value }) => Ok(Some(value)),
            Some(Secret::Oauth { .. }) => {
                bail!("secret {name:?} is an OAuth secret; expected an API key")
            }
            None => Ok(None),
        }
    }

    /// Decrypt the `Key`-typed secret with the given id, if it exists.
    async fn secret_key_by_id(&self, id: SecretId) -> Result<Option<String>> {
        match self.find_secret_by(|s| s.metadata.id == id).await? {
            Some(Secret::Key { value }) => Ok(Some(value)),
            Some(Secret::Oauth { .. }) => {
                bail!("secret {id} is an OAuth secret; expected an API key")
            }
            None => Ok(None),
        }
    }

    /// `DaytonaConfig` from a root-scoped `Binding::Sandbox` (newest wins), or
    /// `None` if none is set so callers fall back to the secret-name spec.
    async fn daytona_config_from_binding(&self) -> Result<Option<crate::DaytonaConfig>> {
        let bindings = list_binding_records(&self.storage, Path::new("bindings")).await?;
        let Some((api_key_secret_id, region, organization_id, api_url)) = bindings
            .into_iter()
            .rev()
            .find_map(|record| match record.binding {
                Binding::Sandbox {
                    config:
                        SandboxProviderConfig::Daytona {
                            api_key_secret_id,
                            region,
                            organization_id,
                            api_url,
                            ..
                        },
                    ..
                } => Some((api_key_secret_id, region, organization_id, api_url)),
                _ => None,
            })
        else {
            return Ok(None);
        };
        let api_key = self
            .secret_key_by_id(api_key_secret_id)
            .await?
            .ok_or_else(|| {
                anyhow!(
                    "daytona sandbox binding references secret id {api_key_secret_id}, \
                 which is not set"
                )
            })?;
        Ok(Some(crate::DaytonaConfig {
            api_key,
            api_url: api_url.unwrap_or_else(|| crate::DEFAULT_DAYTONA_API_URL.to_string()),
            toolbox_url: crate::DEFAULT_DAYTONA_TOOLBOX_URL.to_string(),
            target: region,
            organization_id,
        }))
    }

    async fn vercel_config_from_binding(&self) -> Result<Option<crate::VercelConfig>> {
        let bindings = list_binding_records(&self.storage, Path::new("bindings")).await?;
        let Some((api_token_secret_id, team_id, project_id, api_url)) = bindings
            .into_iter()
            .rev()
            .find_map(|record| match record.binding {
                Binding::Sandbox {
                    config:
                        SandboxProviderConfig::Vercel {
                            api_token_secret_id,
                            team_id,
                            project_id,
                            api_url,
                            ..
                        },
                    ..
                } => Some((api_token_secret_id, team_id, project_id, api_url)),
                _ => None,
            })
        else {
            return Ok(None);
        };
        let api_token = self
            .secret_key_by_id(api_token_secret_id)
            .await?
            .ok_or_else(|| {
                anyhow!(
                    "vercel sandbox binding references secret id {api_token_secret_id}, \
                 which is not set"
                )
            })?;
        Ok(Some(crate::VercelConfig {
            api_token,
            api_url: api_url.unwrap_or_else(|| crate::DEFAULT_VERCEL_API_URL.to_string()),
            team_id,
            project_id,
        }))
    }

    #[cfg(feature = "aws-agentcore")]
    async fn aws_agentcore_config_from_binding(&self) -> Result<Option<crate::AwsAgentCoreConfig>> {
        let bindings = list_binding_records(&self.storage, Path::new("bindings")).await?;
        let Some((runtime_arn, region, qualifier, endpoint_url, session_storage_mount_path)) =
            bindings
                .into_iter()
                .rev()
                .find_map(|record| match record.binding {
                    Binding::Sandbox {
                        config:
                            SandboxProviderConfig::AwsAgentCore {
                                runtime_arn,
                                region,
                                qualifier,
                                endpoint_url,
                                session_storage_mount_path,
                                ..
                            },
                        ..
                    } => Some((
                        runtime_arn,
                        region,
                        qualifier,
                        endpoint_url,
                        session_storage_mount_path,
                    )),
                    _ => None,
                })
        else {
            return Ok(None);
        };
        let session_storage_mount_path = session_storage_mount_path
            .or_else(|| nonempty_env("AWS_AGENTCORE_SESSION_STORAGE_MOUNT_PATH"))
            .or_else(|| nonempty_env("AGENTCORE_SESSION_STORAGE_MOUNT_PATH"));
        Ok(Some(crate::AwsAgentCoreConfig {
            runtime_arn,
            region,
            qualifier,
            endpoint_url,
            credentials: aws_agentcore_credentials_from_env(),
            session_storage_mount_path,
        }))
    }

    /// The configured default base image for `provider`, from the newest
    /// `Binding::Sandbox` for it. `None` when no such binding exists, so the
    /// backend applies its own intrinsic default.
    async fn binding_default_image(&self, provider: SandboxProvider) -> Result<Option<String>> {
        let bindings = list_binding_records(&self.storage, Path::new("bindings")).await?;
        Ok(bindings.into_iter().rev().find_map(|record| {
            let Binding::Sandbox { config, .. } = record.binding else {
                return None;
            };
            (config.provider() == provider)
                .then(|| config.default_image().map(str::to_string))
                .flatten()
        }))
    }
}

#[cfg(feature = "aws-agentcore")]
fn aws_agentcore_credentials_from_env() -> Option<crate::AwsAgentCoreCredentials> {
    let access_key_id = std::env::var("AWS_AGENTCORE_ACCESS_KEY_ID")
        .ok()
        .filter(|value| !value.trim().is_empty())?;
    let secret_access_key = std::env::var("AWS_AGENTCORE_SECRET_ACCESS_KEY")
        .ok()
        .filter(|value| !value.trim().is_empty())?;
    let session_token = std::env::var("AWS_AGENTCORE_SESSION_TOKEN")
        .ok()
        .filter(|value| !value.trim().is_empty());

    Some(crate::AwsAgentCoreCredentials {
        access_key_id,
        secret_access_key,
        session_token,
    })
}

#[cfg(feature = "aws-agentcore")]
fn nonempty_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

impl BasicExoHarness {
    pub async fn new(config: BasicExoHarnessConfig) -> Result<Self> {
        Self::new_with_backend(config, None).await
    }

    #[cfg(test)]
    pub(crate) async fn new_with_sandbox_backend(
        config: BasicExoHarnessConfig,
        sandbox_backend: Arc<dyn ManagedSandboxBackend>,
    ) -> Result<Self> {
        Self::new_with_backend(config, Some(sandbox_backend)).await
    }

    #[cfg(test)]
    pub(crate) async fn daytona_config_from_binding_for_test(
        &self,
    ) -> Result<Option<crate::DaytonaConfig>> {
        self.inner.daytona_config_from_binding().await
    }

    /// `seed` pre-populates the cache for the default provider, letting tests
    /// inject a mock backend.
    async fn new_with_backend(
        config: BasicExoHarnessConfig,
        seed: Option<Arc<dyn ManagedSandboxBackend>>,
    ) -> Result<Self> {
        let BasicExoHarnessConfig {
            root,
            secret_backend,
            sandbox_default,
            sandbox_backends,
        } = config;

        let mut registry = HashMap::new();
        for choice in sandbox_backends {
            let provider = choice.provider();
            if registry.insert(provider, choice).is_some() {
                bail!("duplicate sandbox provider {provider:?} in sandbox_backends");
            }
        }
        if !registry.contains_key(&sandbox_default) {
            bail!("default sandbox provider {sandbox_default:?} is not in the supported set");
        }

        let mut cache = HashMap::new();
        if let Some(backend) = seed {
            cache.insert(sandbox_default, backend);
        }

        let keychain_account = root.to_string_lossy().to_string();
        let storage = BasicObjectStore::local_filesystem(&root).await?;
        #[cfg(feature = "apple-keychain")]
        let keychain_account = if matches!(&secret_backend, SecretBackendChoice::AppleKeychain) {
            keychain_migration::prepare_keychain_account(&storage, &keychain_account).await?
        } else {
            keychain_account
        };
        let secret_cipher = build_secret_cipher(secret_backend, keychain_account)?;
        Ok(Self {
            inner: Arc::new(BasicExoHarnessInner {
                storage,
                write_lock: AsyncMutex::new(()),
                secret_locks: AsyncMutex::new(HashMap::new()),
                oauth_providers: OAuthProviderRegistry::built_in(),
                subscribers: Mutex::new(HashMap::new()),
                sandbox_registry: registry,
                sandbox_backends: AsyncMutex::new(cache),
                running_sandboxes: AsyncMutex::new(HashMap::new()),
                running_processes: AsyncMutex::new(HashMap::new()),
                secret_cipher,
            }),
        })
    }

    fn agents_dir(&self) -> PathBuf {
        PathBuf::from("agents")
    }

    /// Slug-uniqueness index: one marker file per slug, so the per-create
    /// uniqueness check is O(1) instead of a full record scan.
    fn slug_index_dir(&self) -> PathBuf {
        self.agents_dir().join("by-slug")
    }

    fn slug_marker_path(&self, slug: &str) -> PathBuf {
        // Encode defensively so a hostile slug cannot escape the directory.
        let encoded: String = slug
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c.to_string()
                } else {
                    format!("%{:02x}", c as u32)
                }
            })
            .collect();
        // ".slug", never ".json": a slug named "record" must not match the
        // list_agent_records key filter.
        self.slug_index_dir().join(format!("{encoded}.slug"))
    }

    fn bindings_dir(&self) -> PathBuf {
        PathBuf::from("bindings")
    }

    fn secrets_dir(&self) -> PathBuf {
        PathBuf::from("secrets")
    }

    async fn list_agent_records(&self) -> Result<Vec<AgentRecord>> {
        let mut agents = Vec::new();
        for key in self.inner.storage.list_keys(self.agents_dir()).await? {
            if !key.ends_with("/record.json") || Path::new(&key).components().count() != 3 {
                continue;
            }
            agents.push(
                self.inner
                    .storage
                    .get_json::<AgentRecord>(Path::new(&key))
                    .await?,
            );
        }
        agents.sort_by_key(|record| record.id);
        Ok(agents)
    }
}

#[async_trait]
impl ExoHarness for BasicExoHarness {
    async fn preflight_secret_storage(&self) -> Result<()> {
        self.inner.secret_cipher.verify_key_access()
    }

    async fn list_agents(&self) -> Result<Vec<Arc<dyn AgentHandle>>> {
        let mut handles: Vec<Arc<dyn AgentHandle>> = Vec::new();
        for record in self.list_agent_records().await? {
            handles.push(Arc::new(BasicAgentHandle {
                harness: self.clone(),
                record,
            }));
        }
        Ok(handles)
    }

    async fn get_agent(&self, id: &AgentId) -> Result<Option<Arc<dyn AgentHandle>>> {
        let record_path = self.agents_dir().join(id.to_string()).join("record.json");
        let Some(record) = self
            .inner
            .storage
            .get_json_if_exists::<AgentRecord>(&record_path)
            .await?
        else {
            return Ok(None);
        };
        Ok(Some(Arc::new(BasicAgentHandle {
            harness: self.clone(),
            record,
        })))
    }

    async fn new_agent(&self, request: NewAgentRequest) -> Result<Arc<dyn AgentHandle>> {
        let _guard = self.inner.write_lock.lock().await;
        // TODO: claim the marker with a conditional put (put_json_if_absent,
        // arriving in PR #113) to close the cross-process create race.
        let marker = self.slug_marker_path(&request.slug);
        if self
            .inner
            .storage
            .get_json_if_exists::<Uuid7>(&marker)
            .await?
            .is_some()
        {
            bail!("agent slug already exists: {}", request.slug);
        }

        let record = AgentRecord {
            id: Uuid7::now(),
            slug: request.slug,
            name: request.name,
        };
        let agent_dir = self.agents_dir().join(record.id.to_string());
        self.inner
            .storage
            .put_json(agent_dir.join("record.json"), &record)
            .await?;
        self.inner.storage.put_json(marker, &record.id).await?;
        Ok(Arc::new(BasicAgentHandle {
            harness: self.clone(),
            record,
        }))
    }

    async fn delete_agent(&self, id: &AgentId) -> Result<bool> {
        let _guard = self.inner.write_lock.lock().await;
        let agent_dir = self.agents_dir().join(id.to_string());
        if self.inner.storage.list_keys(&agent_dir).await?.is_empty() {
            return Ok(false);
        }
        // Release the slug before the record (its source) disappears.
        if let Some(record) = self
            .inner
            .storage
            .get_json_if_exists::<AgentRecord>(&agent_dir.join("record.json"))
            .await?
        {
            self.inner
                .storage
                .delete_key_if_exists(self.slug_marker_path(&record.slug))
                .await?;
        } else {
            tracing::warn!(
                %id,
                "agent dir exists without record.json; cannot release slug marker, continuing with delete"
            );
        }
        self.inner.storage.delete_prefix(agent_dir).await?;
        Ok(true)
    }

    async fn list_bindings(&self) -> Result<Vec<BindingRecord>> {
        list_binding_records(&self.inner.storage, &self.bindings_dir()).await
    }

    async fn put_binding(&self, binding: Binding) -> Result<BindingId> {
        let _guard = self.inner.write_lock.lock().await;
        let id = Uuid7::now();
        let record = stored_binding(id, binding);
        self.inner
            .storage
            .put_json(self.bindings_dir().join(format!("{id}.json")), &record)
            .await?;
        Ok(id)
    }

    async fn get_binding(&self, id: &BindingId) -> Result<Option<Binding>> {
        let path = self.bindings_dir().join(format!("{id}.json"));
        Ok(self
            .inner
            .storage
            .get_json_if_exists::<StoredBinding>(&path)
            .await?
            .map(|record| record.record.binding))
    }

    async fn list_secrets(&self) -> Result<Vec<SecretMetadata>> {
        list_secret_metadata(&self.inner.storage, &self.secrets_dir()).await
    }

    async fn put_secret(&self, request: PutSecretRequest) -> Result<SecretId> {
        put_secret_in_dir(&self.inner, &self.secrets_dir(), request).await
    }

    async fn get_secret(&self, id: &SecretId) -> Result<Option<Secret>> {
        get_secret_in_scopes(&self.inner, &[self.secrets_dir()], id).await
    }

    async fn logout_oauth_secret(&self, id: &SecretId) -> Result<LogoutOauthResult> {
        logout_oauth_secret_in_dir(&self.inner, &self.secrets_dir(), id).await
    }
}

struct BasicAgentHandle {
    harness: BasicExoHarness,
    record: AgentRecord,
}

trait BasicSandboxScope {
    fn sandbox_handle(&self) -> BasicScopedSandboxHandle<'_>;
}

trait BasicFullSandboxScope: BasicSandboxScope {}

#[async_trait]
impl<T> SnapshotHandle for T
where
    T: BasicSandboxScope + Send + Sync,
{
    async fn snapshot_sandbox(&self, id: SandboxId) -> Result<SnapshotId> {
        self.sandbox_handle().snapshot_sandbox(id).await
    }

    async fn start_sandbox(&self, request: StartSandboxRequest) -> Result<()> {
        self.sandbox_handle().start_sandbox(request).await
    }
}

#[async_trait]
impl<T> SandboxHandle for T
where
    T: BasicFullSandboxScope + Send + Sync,
{
    async fn create_sandbox(&self, request: CreateSandboxRequest) -> Result<SandboxId> {
        self.sandbox_handle().create_sandbox(request).await
    }

    async fn stop_sandbox(&self, id: SandboxId) -> Result<()> {
        self.sandbox_handle().stop_sandbox(id).await
    }

    async fn start_sandbox_process(
        &self,
        request: StartSandboxProcessRequest,
    ) -> Result<SandboxProcessRecord> {
        self.sandbox_handle().start_sandbox_process(request).await
    }

    async fn write_sandbox_process_input(
        &self,
        request: WriteSandboxProcessInputRequest,
    ) -> Result<()> {
        self.sandbox_handle()
            .write_sandbox_process_input(request)
            .await
    }

    async fn close_sandbox_process_input(
        &self,
        request: CloseSandboxProcessInputRequest,
    ) -> Result<()> {
        self.sandbox_handle()
            .close_sandbox_process_input(request)
            .await
    }

    async fn get_sandbox_process_events(
        &self,
        query: SandboxProcessEventQuery,
    ) -> Result<GetSandboxProcessEventsResult> {
        self.sandbox_handle()
            .get_sandbox_process_events(query)
            .await
    }

    async fn wait_sandbox_process(
        &self,
        request: WaitSandboxProcessRequest,
    ) -> Result<SandboxProcessStatus> {
        self.sandbox_handle().wait_sandbox_process(request).await
    }

    async fn cancel_sandbox_process(
        &self,
        request: CancelSandboxProcessRequest,
    ) -> Result<SandboxProcessStatus> {
        self.sandbox_handle().cancel_sandbox_process(request).await
    }

    async fn run_in_sandbox(
        &self,
        request: RunInSandboxRequest,
    ) -> Result<Box<dyn SandboxProcess>> {
        self.sandbox_handle().run_in_sandbox(request).await
    }
}

#[async_trait]
impl AgentHandle for BasicAgentHandle {
    fn record(&self) -> &AgentRecord {
        &self.record
    }

    async fn list_conversations(
        &self,
        request: ListConversationsRequest,
    ) -> Result<ListConversationsResult<Arc<dyn ConversationHandle>>> {
        let mut handles: Vec<Arc<dyn ConversationHandle>> = Vec::new();
        let result = self.list_conversation_records(request).await?;
        for record in result.conversations {
            handles.push(Arc::new(BasicConversationHandle {
                harness: self.harness.clone(),
                agent_id: self.record.id,
                record,
            }));
        }
        Ok(ListConversationsResult {
            conversations: handles,
            next_cursor: result.next_cursor,
        })
    }

    async fn get_conversation(
        &self,
        id: &ConversationId,
    ) -> Result<Option<Arc<dyn ConversationHandle>>> {
        let record_path = self
            .conversations_dir()
            .join(id.to_string())
            .join("record.json");
        let Some(record) = self
            .harness
            .inner
            .storage
            .get_json_if_exists::<ConversationRecord>(&record_path)
            .await?
        else {
            return Ok(None);
        };
        Ok(Some(Arc::new(BasicConversationHandle {
            harness: self.harness.clone(),
            agent_id: self.record.id,
            record,
        })))
    }

    async fn new_conversation(
        &self,
        request: NewConversationRequest,
    ) -> Result<Arc<dyn ConversationHandle>> {
        let _guard = self.harness.inner.write_lock.lock().await;
        let existing = self
            .list_conversation_records(ListConversationsRequest::default())
            .await?
            .conversations;
        let slug = match request.slug {
            Some(slug) => {
                if existing
                    .iter()
                    .any(|conversation| conversation.slug == slug)
                {
                    bail!("conversation slug already exists for agent: {slug}");
                }
                slug
            }
            None => derive_unique_slug("conversation", &existing),
        };
        let record = ConversationRecord {
            id: Uuid7::now(),
            slug: slug.clone(),
            name: request.name.unwrap_or_else(|| slug_to_name(&slug)),
            latest_event_id: None,
        };
        let conversation_dir = self.conversations_dir().join(record.id.to_string());
        let mut record = record;
        append_events_to_conversation(
            &self.harness.inner,
            &conversation_dir,
            record.id,
            None,
            None,
            None,
            vec![EventData::ConversationCreated {
                slug: record.slug.clone(),
                name: record.name.clone(),
            }],
            &mut record,
        )
        .await?;
        self.harness
            .inner
            .storage
            .put_json(conversation_dir.join("record.json"), &record)
            .await?;
        Ok(Arc::new(BasicConversationHandle {
            harness: self.harness.clone(),
            agent_id: self.record.id,
            record,
        }))
    }

    async fn delete_conversation(&self, id: &ConversationId) -> Result<bool> {
        let _guard = self.harness.inner.write_lock.lock().await;
        let conversation_dir = self.conversations_dir().join(id.to_string());
        if self
            .harness
            .inner
            .storage
            .list_keys(&conversation_dir)
            .await?
            .is_empty()
        {
            return Ok(false);
        }
        if let Ok(mut record) = self
            .harness
            .inner
            .storage
            .get_json::<ConversationRecord>(conversation_dir.join("record.json"))
            .await
        {
            append_events_to_conversation(
                &self.harness.inner,
                &conversation_dir,
                record.id,
                None,
                None,
                record.latest_event_id,
                vec![EventData::ConversationDeleted],
                &mut record,
            )
            .await?;
        }
        self.harness
            .inner
            .storage
            .delete_prefix(conversation_dir)
            .await?;
        Ok(true)
    }

    async fn list_bindings(&self) -> Result<Vec<BindingRecord>> {
        Ok(merge_binding_records(vec![
            list_binding_records(&self.harness.inner.storage, &self.harness.bindings_dir()).await?,
            list_binding_records(&self.harness.inner.storage, &self.bindings_dir()).await?,
        ]))
    }

    async fn put_binding(&self, binding: Binding) -> Result<BindingId> {
        let _guard = self.harness.inner.write_lock.lock().await;
        let id = Uuid7::now();
        let record = stored_binding(id, binding);
        self.harness
            .inner
            .storage
            .put_json(self.bindings_dir().join(format!("{id}.json")), &record)
            .await?;
        Ok(id)
    }

    async fn get_binding(&self, id: &BindingId) -> Result<Option<Binding>> {
        let path = self.bindings_dir().join(format!("{id}.json"));
        if let Some(record) = self
            .harness
            .inner
            .storage
            .get_json_if_exists::<StoredBinding>(&path)
            .await?
        {
            return Ok(Some(record.record.binding));
        }
        self.harness.get_binding(id).await
    }

    async fn list_secrets(&self) -> Result<Vec<SecretMetadata>> {
        Ok(merge_secret_metadata(vec![
            list_secret_metadata(&self.harness.inner.storage, &self.harness.secrets_dir()).await?,
            list_secret_metadata(&self.harness.inner.storage, &self.secrets_dir()).await?,
        ]))
    }

    async fn put_secret(&self, request: PutSecretRequest) -> Result<SecretId> {
        put_secret_in_dir(&self.harness.inner, &self.secrets_dir(), request).await
    }

    async fn get_secret(&self, id: &SecretId) -> Result<Option<Secret>> {
        get_secret_in_scopes(
            &self.harness.inner,
            &[self.secrets_dir(), self.harness.secrets_dir()],
            id,
        )
        .await
    }

    async fn write_artifact(&self, request: WriteArtifactRequest) -> Result<ArtifactVersion> {
        let _guard = self.harness.inner.write_lock.lock().await;
        write_artifact_version(&self.harness.inner, &self.artifacts_dir(), request).await
    }

    async fn read_artifact(&self, request: ReadArtifactRequest) -> Result<Option<Artifact>> {
        let versions =
            load_artifact_versions(&self.harness.inner.storage, &self.artifacts_dir()).await?;
        let selected = versions
            .into_iter()
            .filter(|artifact| artifact.artifact_id == request.artifact_id)
            .filter(|artifact| {
                request
                    .version
                    .is_none_or(|version| artifact.version == version)
            })
            .max_by_key(|artifact| artifact.version);
        let Some(selected) = selected else {
            return Ok(None);
        };
        let artifact_dir = self.artifacts_dir().join(selected.artifact_id.to_string());
        let contents =
            load_artifact_contents(&self.harness.inner.storage, &artifact_dir, selected.version)
                .await?;
        Ok(Some(Artifact {
            version: selected,
            contents,
        }))
    }

    async fn list_artifacts(&self) -> Result<Vec<ArtifactVersion>> {
        load_artifact_versions(&self.harness.inner.storage, &self.artifacts_dir()).await
    }
}

impl BasicSandboxScope for BasicAgentHandle {
    fn sandbox_handle(&self) -> BasicScopedSandboxHandle<'_> {
        BasicScopedSandboxHandle::agent(&self.harness, self.record.id)
    }
}

impl BasicFullSandboxScope for BasicAgentHandle {}

impl BasicAgentHandle {
    fn agent_dir(&self) -> PathBuf {
        self.harness.agents_dir().join(self.record.id.to_string())
    }

    fn conversations_dir(&self) -> PathBuf {
        self.agent_dir().join("conversations")
    }

    fn bindings_dir(&self) -> PathBuf {
        self.agent_dir().join("bindings")
    }

    fn secrets_dir(&self) -> PathBuf {
        self.agent_dir().join("secrets")
    }

    fn artifacts_dir(&self) -> PathBuf {
        self.agent_dir().join("artifacts")
    }

    async fn list_conversation_records(
        &self,
        request: ListConversationsRequest,
    ) -> Result<ListConversationsResult<ConversationRecord>> {
        let mut conversations = Vec::new();
        for key in self
            .harness
            .inner
            .storage
            .list_keys(self.conversations_dir())
            .await?
        {
            if !key.ends_with("/record.json") || Path::new(&key).components().count() != 5 {
                continue;
            }
            conversations.push(
                self.harness
                    .inner
                    .storage
                    .get_json::<ConversationRecord>(Path::new(&key))
                    .await?,
            );
        }
        conversations.sort_by_key(conversation_recency_key);
        conversations.reverse();
        paginate_conversation_records(conversations, request)
    }
}

fn conversation_recency_key(record: &ConversationRecord) -> Uuid7 {
    record.latest_event_id.unwrap_or(record.id)
}

fn paginate_conversation_records(
    conversations: Vec<ConversationRecord>,
    request: ListConversationsRequest,
) -> Result<ListConversationsResult<ConversationRecord>> {
    let start = match request.cursor {
        Some(cursor) => conversations
            .iter()
            .position(|conversation| conversation_recency_key(conversation) == cursor)
            .map(|index| index + 1)
            .ok_or_else(|| anyhow!("conversation cursor not found: {cursor}"))?,
        None => 0,
    };
    let remaining = conversations.len().saturating_sub(start);
    let Some(limit) = request.limit.filter(|limit| *limit > 0) else {
        return Ok(ListConversationsResult {
            conversations: conversations.into_iter().skip(start).collect(),
            next_cursor: None,
        });
    };
    let has_more = remaining > limit;
    let page: Vec<_> = conversations.into_iter().skip(start).take(limit).collect();
    let next_cursor = if has_more {
        page.last().map(conversation_recency_key)
    } else {
        None
    };
    Ok(ListConversationsResult {
        conversations: page,
        next_cursor,
    })
}

#[derive(Debug, Clone, Copy)]
enum SandboxOwner {
    Agent(AgentId),
    Conversation(ConversationId),
}

struct BasicScopedSandboxHandle<'a> {
    harness: &'a BasicExoHarness,
    owner_dir: PathBuf,
    owner: SandboxOwner,
    event_sink: BasicSandboxEventSink<'a>,
}

enum BasicSandboxEventSink<'a> {
    None,
    Conversation {
        conversation_id: ConversationId,
    },
    Turn {
        conversation_id: ConversationId,
        session_id: SessionId,
        turn_id: TurnId,
        state: &'a Mutex<BasicTurnState>,
    },
}

impl<'a> BasicScopedSandboxHandle<'a> {
    fn agent(harness: &'a BasicExoHarness, agent_id: AgentId) -> Self {
        Self {
            harness,
            owner_dir: harness.agents_dir().join(agent_id.to_string()),
            owner: SandboxOwner::Agent(agent_id),
            event_sink: BasicSandboxEventSink::None,
        }
    }

    fn conversation(
        harness: &'a BasicExoHarness,
        conversation_id: ConversationId,
        conversation_dir: PathBuf,
    ) -> Self {
        Self {
            harness,
            owner_dir: conversation_dir,
            owner: SandboxOwner::Conversation(conversation_id),
            event_sink: BasicSandboxEventSink::Conversation { conversation_id },
        }
    }

    fn turn(
        harness: &'a BasicExoHarness,
        conversation_id: ConversationId,
        conversation_dir: PathBuf,
        session_id: SessionId,
        turn_id: TurnId,
        state: &'a Mutex<BasicTurnState>,
    ) -> Self {
        Self {
            harness,
            owner_dir: conversation_dir,
            owner: SandboxOwner::Conversation(conversation_id),
            event_sink: BasicSandboxEventSink::Turn {
                conversation_id,
                session_id,
                turn_id,
                state,
            },
        }
    }

    fn sandboxes_dir(&self) -> PathBuf {
        self.owner_dir.join("sandboxes")
    }

    async fn create_sandbox(&self, request: CreateSandboxRequest) -> Result<SandboxId> {
        self.ensure_full_sandbox_scope("create_sandbox")?;
        if request.name.is_none() {
            return self.create_new_sandbox(request).await;
        }
        let prepared = prepare_sandbox_request(self.harness, request).await?;
        let _guard = self.harness.inner.write_lock.lock().await;
        if let Some((sandbox_id, sandbox)) = self.find_matching_sandbox(&prepared).await? {
            let (_handle, provider_state_event) = active_sandbox_handle(
                self.harness,
                &self.owner_dir,
                self.owner,
                &sandbox_id,
                &sandbox,
            )
            .await?;
            if let Some(event) = provider_state_event {
                self.append_events_locked(vec![event]).await?;
            }
            return Ok(sandbox_id);
        }
        self.create_new_sandbox_locked(prepared).await
    }

    async fn snapshot_sandbox(&self, id: SandboxId) -> Result<SnapshotId> {
        let (snapshot_id, event) =
            snapshot_sandbox_side_effect(self.harness, &self.owner_dir, id).await?;
        self.append_events(vec![event]).await?;
        Ok(snapshot_id)
    }

    async fn start_sandbox(&self, request: StartSandboxRequest) -> Result<()> {
        let event =
            start_sandbox_side_effect(self.harness, &self.owner_dir, self.owner, request).await?;
        self.append_events(vec![event]).await?;
        Ok(())
    }

    async fn stop_sandbox(&self, id: SandboxId) -> Result<()> {
        self.ensure_full_sandbox_scope("stop_sandbox")?;
        let sandbox_handle = self
            .harness
            .inner
            .running_sandboxes
            .lock()
            .await
            .remove(&id);
        if let Some(sandbox_handle) = sandbox_handle {
            sandbox_handle.stop().await?;
        }

        let _guard = self.harness.inner.write_lock.lock().await;
        let mut sandbox = self.load_sandbox(&id).await?;
        if !sandbox.running {
            return Ok(());
        }
        sandbox.running = false;
        self.harness
            .inner
            .storage
            .put_json(self.sandboxes_dir().join(format!("{id}.json")), &sandbox)
            .await?;
        self.append_events_locked(vec![EventData::SandboxStopped { sandbox_id: id }])
            .await?;
        Ok(())
    }

    async fn start_sandbox_process(
        &self,
        request: StartSandboxProcessRequest,
    ) -> Result<SandboxProcessRecord> {
        self.ensure_full_sandbox_scope("start_sandbox_process")?;
        let pending = prepare_sandbox_process(
            self.harness,
            &self.owner_dir,
            self.owner,
            self.process_event_log(),
            request,
        )
        .await?;
        let mut events = Vec::new();
        if let Some(event) = pending.provider_state_event.clone() {
            events.push(event);
        }
        events.push(pending.started_event.clone());
        self.append_events(events).await?;
        spawn_pending_sandbox_process(self.harness, pending).await
    }

    async fn write_sandbox_process_input(
        &self,
        request: WriteSandboxProcessInputRequest,
    ) -> Result<()> {
        self.ensure_full_sandbox_scope("write_sandbox_process_input")?;
        let process = self
            .require_sandbox_process(&request.sandbox_id, &request.process_id)
            .await?;
        if !sandbox_process_status(&process).await.is_running() {
            bail!("sandbox process is not running: {}", request.process_id);
        }
        let mut stdin = process.stdin.lock().await;
        let stdin = stdin
            .as_mut()
            .ok_or_else(|| anyhow!("sandbox process stdin is closed: {}", request.process_id))?;
        stdin.write_all(&request.data).await?;
        stdin.flush().await?;
        Ok(())
    }

    async fn close_sandbox_process_input(
        &self,
        request: CloseSandboxProcessInputRequest,
    ) -> Result<()> {
        self.ensure_full_sandbox_scope("close_sandbox_process_input")?;
        let process = self
            .require_sandbox_process(&request.sandbox_id, &request.process_id)
            .await?;
        process.stdin.lock().await.take();
        Ok(())
    }

    async fn get_sandbox_process_events(
        &self,
        query: SandboxProcessEventQuery,
    ) -> Result<GetSandboxProcessEventsResult> {
        self.ensure_full_sandbox_scope("get_sandbox_process_events")?;
        let process = self
            .require_sandbox_process(&query.sandbox_id, &query.process_id)
            .await?;
        let after = query.after.unwrap_or_default();
        let limit = query.limit.unwrap_or(u32::MAX) as usize;
        let events = process
            .events
            .lock()
            .await
            .iter()
            .filter(|event| event.cursor() > after)
            .take(limit)
            .cloned()
            .collect::<Vec<_>>();
        let cursor = events
            .last()
            .map(SandboxProcessEvent::cursor)
            .or(query.after);
        Ok(GetSandboxProcessEventsResult {
            events,
            cursor,
            status: sandbox_process_status(&process).await,
        })
    }

    async fn wait_sandbox_process(
        &self,
        request: WaitSandboxProcessRequest,
    ) -> Result<SandboxProcessStatus> {
        self.ensure_full_sandbox_scope("wait_sandbox_process")?;
        let process = self
            .require_sandbox_process(&request.sandbox_id, &request.process_id)
            .await?;
        Ok(wait_for_sandbox_process_terminal_status(&process).await)
    }

    async fn cancel_sandbox_process(
        &self,
        request: CancelSandboxProcessRequest,
    ) -> Result<SandboxProcessStatus> {
        self.ensure_full_sandbox_scope("cancel_sandbox_process")?;
        let process = self
            .require_sandbox_process(&request.sandbox_id, &request.process_id)
            .await?;
        process.stdin.lock().await.take();
        if let Some(tasks) = process.tasks.lock().await.take() {
            tasks.stdout.abort();
            tasks.stderr.abort();
            tasks.wait.abort();
        }
        push_sandbox_process_event(&process, SandboxProcessEventPayload::Cancelled).await;
        set_sandbox_process_status(&process, SandboxProcessStatus::Cancelled).await;
        Ok(SandboxProcessStatus::Cancelled)
    }

    async fn run_in_sandbox(
        &self,
        request: RunInSandboxRequest,
    ) -> Result<Box<dyn SandboxProcess>> {
        self.ensure_full_sandbox_scope("run_in_sandbox")?;
        let sandbox = self.load_sandbox(&request.id).await?;
        if !sandbox.running {
            bail!("sandbox is not running: {}", request.id);
        }
        if request.command.is_empty() {
            bail!("sandbox command must not be empty");
        }
        let (sandbox_handle, provider_state_event) = active_sandbox_handle(
            self.harness,
            &self.owner_dir,
            self.owner,
            &request.id,
            &sandbox,
        )
        .await?;
        if let Some(event) = provider_state_event {
            self.append_events(vec![event]).await?;
        }
        let parts = sandbox_handle
            .start_process(&SandboxCommand {
                argv: request.command.clone(),
                env: request.env.clone(),
                display_argv: Some(request.command),
                cwd: None,
                timeout: None,
            })
            .await
            .with_context(|| format!("failed to run command in sandbox {}", request.id))?;
        Ok(Box::new(LiveSandboxProcess::new(parts)))
    }

    fn ensure_full_sandbox_scope(&self, operation: &str) -> Result<()> {
        if matches!(self.event_sink, BasicSandboxEventSink::Turn { .. }) {
            bail!("{operation} is not supported on a turn scope");
        }
        Ok(())
    }

    async fn create_new_sandbox(&self, request: CreateSandboxRequest) -> Result<SandboxId> {
        let prepared = prepare_sandbox_request(self.harness, request).await?;
        let sandbox_id = format!("sandbox-{}", Uuid7::now());
        let sandbox = prepared.stored_sandbox(sandbox_id.clone());
        let (sandbox_handle, provider_state_event) = create_sandbox_handle(
            self.harness,
            &self.owner_dir,
            self.owner,
            &sandbox_id,
            &sandbox,
        )
        .await?;
        let _guard = self.harness.inner.write_lock.lock().await;
        self.persist_created_sandbox_locked(sandbox, sandbox_handle, provider_state_event)
            .await
    }

    async fn create_new_sandbox_locked(
        &self,
        request: PreparedSandboxRequest,
    ) -> Result<SandboxId> {
        let sandbox_id = format!("sandbox-{}", Uuid7::now());
        let sandbox = request.stored_sandbox(sandbox_id.clone());
        let (sandbox_handle, provider_state_event) = create_sandbox_handle(
            self.harness,
            &self.owner_dir,
            self.owner,
            &sandbox_id,
            &sandbox,
        )
        .await?;
        self.persist_created_sandbox_locked(sandbox, sandbox_handle, provider_state_event)
            .await
    }

    async fn persist_created_sandbox_locked(
        &self,
        sandbox: StoredSandbox,
        sandbox_handle: Arc<dyn ManagedSandboxHandle>,
        provider_state_event: Option<EventData>,
    ) -> Result<SandboxId> {
        let sandbox_id = sandbox.id.clone();
        self.harness
            .inner
            .storage
            .put_json(
                self.sandboxes_dir().join(format!("{sandbox_id}.json")),
                &sandbox,
            )
            .await?;
        self.harness
            .inner
            .running_sandboxes
            .lock()
            .await
            .insert(sandbox_id.clone(), sandbox_handle);
        let mut events = vec![
            EventData::SandboxCreated {
                sandbox_id: sandbox_id.clone(),
                name: sandbox.name,
                provider: sandbox.provider,
                image: sandbox.image,
                default_workdir: sandbox.default_workdir.unwrap_or_default(),
                file_system_mounts: sandbox.file_system_mounts,
                durable_file_systems: sandbox.durable_file_systems,
                enable_networking: sandbox.enable_networking,
                idle_seconds: sandbox.idle_seconds,
            },
            EventData::SandboxStarted {
                sandbox_id: sandbox_id.clone(),
                snapshot_id: None,
            },
        ];
        if let Some(event) = provider_state_event {
            events.push(event);
        }
        self.append_events_locked(events).await?;
        Ok(sandbox_id)
    }

    async fn find_matching_sandbox(
        &self,
        request: &PreparedSandboxRequest,
    ) -> Result<Option<(SandboxId, StoredSandbox)>> {
        match self.event_sink {
            BasicSandboxEventSink::None => {
                find_matching_stored_sandbox(
                    &self.harness.inner.storage,
                    &self.sandboxes_dir(),
                    request,
                )
                .await
            }
            BasicSandboxEventSink::Conversation { .. } => {
                self.find_matching_conversation_sandbox(request).await
            }
            BasicSandboxEventSink::Turn { .. } => {
                bail!("create_sandbox is not supported on a turn scope")
            }
        }
    }

    async fn find_matching_conversation_sandbox(
        &self,
        request: &PreparedSandboxRequest,
    ) -> Result<Option<(SandboxId, StoredSandbox)>> {
        let Some(name) = &request.name else {
            return Ok(None);
        };
        let mut events = load_events(&self.harness.inner.storage, &self.owner_dir.join("events"))
            .await?
            .into_iter()
            .filter(|event| event.data.kind() == EventKind::SANDBOX_CREATED)
            .collect::<Vec<_>>();
        events.sort_by_key(|event| event.id);

        for event in events.into_iter().rev() {
            let EventData::SandboxCreated {
                sandbox_id,
                name: event_name,
                provider,
                image,
                default_workdir,
                file_system_mounts,
                durable_file_systems,
                enable_networking,
                idle_seconds,
                ..
            } = event.data
            else {
                continue;
            };
            if event_name.as_ref() != Some(name) {
                continue;
            }
            let sandbox = self.load_sandbox(&sandbox_id).await?;
            if !sandbox.running {
                continue;
            }
            if provider != request.provider
                || image != request.image
                || default_workdir != request.default_workdir.clone().unwrap_or_default()
                || file_system_mounts != request.file_system_mounts
                || durable_file_systems != request.durable_file_systems
                || enable_networking != request.enable_networking
                || idle_seconds != request.idle_seconds
            {
                bail!("sandbox name {name:?} already exists with a different configuration");
            }
            return Ok(Some((sandbox_id, sandbox)));
        }
        Ok(None)
    }

    async fn load_sandbox(&self, id: &str) -> Result<StoredSandbox> {
        load_stored_sandbox(self.harness, &self.owner_dir, id).await
    }

    async fn require_sandbox_process(
        &self,
        sandbox_id: &str,
        process_id: &str,
    ) -> Result<Arc<RunningSandboxProcess>> {
        require_running_sandbox_process(self.harness, sandbox_id, process_id).await
    }

    fn process_event_log(&self) -> Option<SandboxProcessEventLog> {
        match self.event_sink {
            BasicSandboxEventSink::None | BasicSandboxEventSink::Turn { .. } => None,
            BasicSandboxEventSink::Conversation { conversation_id } => {
                Some(SandboxProcessEventLog {
                    inner: Arc::clone(&self.harness.inner),
                    conversation_id,
                    conversation_dir: self.owner_dir.clone(),
                })
            }
        }
    }

    async fn append_events(&self, data: Vec<EventData>) -> Result<()> {
        if matches!(self.event_sink, BasicSandboxEventSink::None) {
            return Ok(());
        }
        let _guard = self.harness.inner.write_lock.lock().await;
        self.append_events_locked(data).await
    }

    async fn append_events_locked(&self, data: Vec<EventData>) -> Result<()> {
        match self.event_sink {
            BasicSandboxEventSink::None => Ok(()),
            BasicSandboxEventSink::Conversation { conversation_id } => {
                let mut record = self
                    .harness
                    .inner
                    .storage
                    .get_json::<ConversationRecord>(self.owner_dir.join("record.json"))
                    .await?;
                append_events_to_conversation(
                    &self.harness.inner,
                    &self.owner_dir,
                    conversation_id,
                    None,
                    None,
                    record.latest_event_id,
                    data,
                    &mut record,
                )
                .await?;
                self.harness
                    .inner
                    .storage
                    .put_json(self.owner_dir.join("record.json"), &record)
                    .await?;
                Ok(())
            }
            BasicSandboxEventSink::Turn {
                conversation_id,
                session_id,
                turn_id,
                state,
            } => {
                let expected_head = state.lock().expect("turn state poisoned").latest_event_id;
                let mut record = self
                    .harness
                    .inner
                    .storage
                    .get_json::<ConversationRecord>(self.owner_dir.join("record.json"))
                    .await?;
                let add_result = append_events_to_conversation(
                    &self.harness.inner,
                    &self.owner_dir,
                    conversation_id,
                    Some(session_id),
                    Some(turn_id),
                    expected_head,
                    data,
                    &mut record,
                )
                .await?;
                self.harness
                    .inner
                    .storage
                    .put_json(self.owner_dir.join("record.json"), &record)
                    .await?;
                state.lock().expect("turn state poisoned").latest_event_id =
                    Some(add_result.latest_event_id);
                Ok(())
            }
        }
    }
}

struct BasicConversationHandle {
    harness: BasicExoHarness,
    agent_id: AgentId,
    record: ConversationRecord,
}

#[async_trait]
impl ConversationHandle for BasicConversationHandle {
    fn record(&self) -> &ConversationRecord {
        &self.record
    }

    async fn start_session(&self) -> Result<SessionId> {
        let session_id = Uuid7::now();
        self.append_events_internal(
            Some(session_id),
            None,
            None,
            vec![EventData::SessionStarted],
        )
        .await?;
        Ok(session_id)
    }

    async fn end_session(&self, id: SessionId) -> Result<()> {
        self.append_events_internal(Some(id), None, None, vec![EventData::SessionEnded])
            .await?;
        Ok(())
    }

    async fn begin_turn(&self, request: BeginTurnRequest) -> Result<Arc<dyn TurnHandle>> {
        let _guard = self.harness.inner.write_lock.lock().await;
        let mut record = self.load_record().await?;
        let conversation_dir = self.conversation_dir();

        let session_id = request.session_id.unwrap_or_else(Uuid7::now);
        let turn_record = TurnRecord {
            id: Uuid7::now(),
            session_id,
        };
        let mut events_to_append = Vec::new();

        if request.session_id.is_none() {
            events_to_append.push(EventData::SessionStarted);
        }
        events_to_append.push(EventData::TurnStarted);
        if !request.input.is_empty() {
            events_to_append.push(EventData::Messages {
                messages: request.input,
                response_id: None,
                usage: None,
            });
        }

        let add_result = append_events_to_conversation(
            &self.harness.inner,
            &conversation_dir,
            self.record.id,
            Some(session_id),
            Some(turn_record.id),
            record.latest_event_id,
            events_to_append,
            &mut record,
        )
        .await?;
        self.harness
            .inner
            .storage
            .put_json(conversation_dir.join("record.json"), &record)
            .await?;

        Ok(Arc::new(BasicTurnHandle {
            harness: self.harness.clone(),
            conversation_dir,
            conversation_id: self.record.id,
            record: turn_record,
            state: Mutex::new(BasicTurnState {
                latest_event_id: Some(add_result.latest_event_id),
                finished: false,
            }),
        }))
    }

    async fn turn_handle(&self, record: TurnRecord) -> Result<Arc<dyn TurnHandle>> {
        let events = load_events(&self.harness.inner.storage, &self.events_dir()).await?;
        let mut latest_event_id = None;
        let mut finished = false;
        for event in events
            .into_iter()
            .filter(|event| event.session_id == Some(record.session_id))
            .filter(|event| event.turn_id == Some(record.id))
        {
            latest_event_id = Some(event.id);
            finished = matches!(event.data, EventData::TurnEnded);
        }
        if latest_event_id.is_none() {
            bail!(
                "turn {} in session {} was not found",
                record.id,
                record.session_id
            );
        }
        Ok(Arc::new(BasicTurnHandle {
            harness: self.harness.clone(),
            conversation_dir: self.conversation_dir(),
            conversation_id: self.record.id,
            record,
            state: Mutex::new(BasicTurnState {
                latest_event_id,
                finished,
            }),
        }))
    }

    async fn get_events(&self, query: Option<EventQuery>) -> Result<GetEventsResult> {
        let mut events = load_events(&self.harness.inner.storage, &self.events_dir()).await?;
        if let Some(query) = query {
            if let Some(session_id) = query.session_id {
                events.retain(|event| event.session_id == Some(session_id));
            }
            if let Some(turn_id) = query.turn_id {
                events.retain(|event| event.turn_id == Some(turn_id));
            }
            if let Some(types) = query.types {
                events.retain(|event| types.contains(&event.data.kind()));
            }
            match query.direction.unwrap_or(EventQueryDirection::Asc) {
                EventQueryDirection::Asc => {
                    if let Some(cursor) = query.cursor {
                        events.retain(|event| event.id > cursor);
                    }
                }
                EventQueryDirection::Desc => {
                    events.reverse();
                    if let Some(cursor) = query.cursor {
                        events.retain(|event| event.id < cursor);
                    }
                }
            }
            if let Some(limit) = query.limit {
                events.truncate(limit as usize);
            }
        }
        let cursor = events.last().map(|event| event.id);
        Ok(GetEventsResult { events, cursor })
    }

    async fn watch_events(&self, after_exclusive: Bound<EventId>) -> Result<EventStream> {
        let _guard = self.harness.inner.write_lock.lock().await;
        let existing = match after_exclusive {
            Bound::Unbounded => Vec::new(),
            _ => {
                let events = load_events(&self.harness.inner.storage, &self.events_dir()).await?;
                events
                    .into_iter()
                    .filter(|event| matches_bound(event.id, &after_exclusive))
                    .collect::<Vec<_>>()
            }
        };
        let (tx, rx) = mpsc::unbounded_channel();
        self.harness
            .inner
            .subscribers
            .lock()
            .expect("subscribers poisoned")
            .entry(self.record.id)
            .or_default()
            .push(tx);
        let existing_stream: BoxStream<'static, Result<Event>> =
            stream::iter(existing.into_iter().map(Ok)).boxed();
        let live_stream = UnboundedReceiverStream::new(rx);
        Ok(Box::pin(existing_stream.chain(live_stream)))
    }

    async fn get_event(&self, id: EventId) -> Result<Option<Event>> {
        let path = self.events_dir().join(format!("{id}.json"));
        self.harness.inner.storage.get_json_if_exists(&path).await
    }

    async fn add_events(&self, request: AddEventsRequest) -> Result<AddEventsResult> {
        self.append_events_internal(request.session_id, request.turn_id, None, request.data)
            .await
    }

    async fn fork(&self, request: ForkConversationRequest) -> Result<Arc<dyn ConversationHandle>> {
        let _guard = self.harness.inner.write_lock.lock().await;
        let agent = BasicAgentHandle {
            harness: self.harness.clone(),
            record: self
                .harness
                .inner
                .storage
                .get_json::<AgentRecord>(self.agent_dir().join("record.json"))
                .await?,
        };
        let existing = agent
            .list_conversation_records(ListConversationsRequest::default())
            .await?
            .conversations;
        let slug = match request.slug {
            Some(slug) => {
                if existing
                    .iter()
                    .any(|conversation| conversation.slug == slug)
                {
                    bail!("conversation slug already exists for agent: {slug}");
                }
                slug
            }
            None => derive_unique_slug("fork", &existing),
        };
        let mut events = load_events(&self.harness.inner.storage, &self.events_dir()).await?;
        if let Some(limit) = request.up_to_inclusive {
            events.retain(|event| event.id <= limit);
        }
        let record = ConversationRecord {
            id: Uuid7::now(),
            slug: slug.clone(),
            name: request.name.unwrap_or_else(|| slug_to_name(&slug)),
            latest_event_id: None,
        };
        let conversation_dir = agent.conversations_dir().join(record.id.to_string());
        self.harness
            .inner
            .storage
            .copy_prefix(self.bindings_dir(), conversation_dir.join("bindings"))
            .await?;
        self.harness
            .inner
            .storage
            .copy_prefix(self.secrets_dir(), conversation_dir.join("secrets"))
            .await?;
        self.harness
            .inner
            .storage
            .copy_prefix(self.artifacts_dir(), conversation_dir.join("artifacts"))
            .await?;
        self.harness
            .inner
            .storage
            .copy_prefix(self.sandboxes_dir(), conversation_dir.join("sandboxes"))
            .await?;

        let mut latest_event_id = None;
        for mut event in events {
            let new_event_id = Uuid7::now();
            event.id = new_event_id;
            event.conversation_id = record.id;
            event.created_at = new_event_id.timestamp().expect("uuid7 timestamp");
            latest_event_id = Some(new_event_id);
            self.harness
                .inner
                .storage
                .put_json(
                    conversation_dir
                        .join("events")
                        .join(format!("{}.json", event.id)),
                    &event,
                )
                .await?;
        }

        let mut fork_record = record.clone();
        fork_record.latest_event_id = latest_event_id;
        append_events_to_conversation(
            &self.harness.inner,
            &conversation_dir,
            record.id,
            None,
            None,
            fork_record.latest_event_id,
            vec![EventData::ConversationForked {
                source_conversation_id: self.record.id,
                up_to_inclusive: request.up_to_inclusive,
            }],
            &mut fork_record,
        )
        .await?;
        self.harness
            .inner
            .storage
            .put_json(conversation_dir.join("record.json"), &fork_record)
            .await?;
        Ok(Arc::new(BasicConversationHandle {
            harness: self.harness.clone(),
            agent_id: self.agent_id,
            record: fork_record,
        }))
    }

    async fn write_artifact(&self, request: WriteArtifactRequest) -> Result<ArtifactVersion> {
        let _guard = self.harness.inner.write_lock.lock().await;
        let artifact_version =
            write_artifact_version(&self.harness.inner, &self.artifacts_dir(), request).await?;
        let conversation_dir = self.conversation_dir();
        let mut record = self.load_record().await?;
        append_events_to_conversation(
            &self.harness.inner,
            &conversation_dir,
            self.record.id,
            None,
            None,
            record.latest_event_id,
            vec![EventData::ArtifactWritten {
                artifact_id: artifact_version.artifact_id,
                path: artifact_version.path.clone(),
                version: artifact_version.version,
            }],
            &mut record,
        )
        .await?;
        self.harness
            .inner
            .storage
            .put_json(conversation_dir.join("record.json"), &record)
            .await?;
        Ok(artifact_version)
    }

    async fn read_artifact(&self, request: ReadArtifactRequest) -> Result<Option<Artifact>> {
        let versions =
            load_artifact_versions(&self.harness.inner.storage, &self.artifacts_dir()).await?;
        let selected = versions
            .into_iter()
            .filter(|artifact| artifact.artifact_id == request.artifact_id)
            .filter(|artifact| {
                request
                    .version
                    .is_none_or(|version| artifact.version == version)
            })
            .max_by_key(|artifact| artifact.version);
        let Some(selected) = selected else {
            return Ok(None);
        };
        let artifact_dir = self.artifacts_dir().join(selected.artifact_id.to_string());
        let contents =
            load_artifact_contents(&self.harness.inner.storage, &artifact_dir, selected.version)
                .await?;
        Ok(Some(Artifact {
            version: selected,
            contents,
        }))
    }

    async fn list_artifacts(&self) -> Result<Vec<ArtifactVersion>> {
        load_artifact_versions(&self.harness.inner.storage, &self.artifacts_dir()).await
    }

    async fn list_bindings(&self) -> Result<Vec<BindingRecord>> {
        Ok(merge_binding_records(vec![
            list_binding_records(&self.harness.inner.storage, &self.harness.bindings_dir()).await?,
            list_binding_records(
                &self.harness.inner.storage,
                &agent_bindings_dir(&self.harness, self.agent_id),
            )
            .await?,
            list_binding_records(&self.harness.inner.storage, &self.bindings_dir()).await?,
        ]))
    }

    async fn put_binding(&self, binding: Binding) -> Result<BindingId> {
        let _guard = self.harness.inner.write_lock.lock().await;
        let id = Uuid7::now();
        let record = stored_binding(id, binding);
        self.harness
            .inner
            .storage
            .put_json(self.bindings_dir().join(format!("{id}.json")), &record)
            .await?;
        Ok(id)
    }

    async fn get_binding(&self, id: &BindingId) -> Result<Option<Binding>> {
        let local_path = self.bindings_dir().join(format!("{id}.json"));
        if let Some(record) = self
            .harness
            .inner
            .storage
            .get_json_if_exists::<StoredBinding>(&local_path)
            .await?
        {
            return Ok(Some(record.record.binding));
        }
        let agent_path =
            agent_bindings_dir(&self.harness, self.agent_id).join(format!("{id}.json"));
        if let Some(record) = self
            .harness
            .inner
            .storage
            .get_json_if_exists::<StoredBinding>(&agent_path)
            .await?
        {
            return Ok(Some(record.record.binding));
        }
        self.harness.get_binding(id).await
    }

    async fn list_secrets(&self) -> Result<Vec<SecretMetadata>> {
        Ok(merge_secret_metadata(vec![
            list_secret_metadata(&self.harness.inner.storage, &self.harness.secrets_dir()).await?,
            list_secret_metadata(
                &self.harness.inner.storage,
                &agent_secrets_dir(&self.harness, self.agent_id),
            )
            .await?,
            list_secret_metadata(&self.harness.inner.storage, &self.secrets_dir()).await?,
        ]))
    }

    async fn put_secret(&self, request: PutSecretRequest) -> Result<SecretId> {
        put_secret_in_dir(&self.harness.inner, &self.secrets_dir(), request).await
    }

    async fn get_secret(&self, id: &SecretId) -> Result<Option<Secret>> {
        get_secret_in_scopes(
            &self.harness.inner,
            &[
                self.secrets_dir(),
                agent_secrets_dir(&self.harness, self.agent_id),
                self.harness.secrets_dir(),
            ],
            id,
        )
        .await
    }
}

impl BasicSandboxScope for BasicConversationHandle {
    fn sandbox_handle(&self) -> BasicScopedSandboxHandle<'_> {
        BasicScopedSandboxHandle::conversation(
            &self.harness,
            self.record.id,
            self.conversation_dir(),
        )
    }
}

impl BasicFullSandboxScope for BasicConversationHandle {}

impl BasicConversationHandle {
    fn agent_dir(&self) -> PathBuf {
        self.harness.agents_dir().join(self.agent_id.to_string())
    }

    fn conversation_dir(&self) -> PathBuf {
        self.agent_dir()
            .join("conversations")
            .join(self.record.id.to_string())
    }

    fn events_dir(&self) -> PathBuf {
        self.conversation_dir().join("events")
    }

    fn bindings_dir(&self) -> PathBuf {
        self.conversation_dir().join("bindings")
    }

    fn secrets_dir(&self) -> PathBuf {
        self.conversation_dir().join("secrets")
    }

    fn artifacts_dir(&self) -> PathBuf {
        self.conversation_dir().join("artifacts")
    }

    fn sandboxes_dir(&self) -> PathBuf {
        self.conversation_dir().join("sandboxes")
    }

    async fn load_record(&self) -> Result<ConversationRecord> {
        self.harness
            .inner
            .storage
            .get_json(self.conversation_dir().join("record.json"))
            .await
    }

    async fn append_events_internal(
        &self,
        session_id: Option<SessionId>,
        turn_id: Option<TurnId>,
        expected_head: Option<EventId>,
        data: Vec<EventData>,
    ) -> Result<AddEventsResult> {
        let _guard = self.harness.inner.write_lock.lock().await;
        let conversation_dir = self.conversation_dir();
        let mut record = self.load_record().await?;
        let add_result = append_events_to_conversation(
            &self.harness.inner,
            &conversation_dir,
            self.record.id,
            session_id,
            turn_id,
            expected_head,
            data,
            &mut record,
        )
        .await?;
        self.harness
            .inner
            .storage
            .put_json(conversation_dir.join("record.json"), &record)
            .await?;
        Ok(add_result)
    }
}

async fn snapshot_sandbox_side_effect(
    harness: &BasicExoHarness,
    owner_dir: &Path,
    id: SandboxId,
) -> Result<(SnapshotId, EventData)> {
    // Capture the payload before taking the write lock. Backends may need to
    // talk to docker or pause the container, which can be slow.
    let handle = harness
        .inner
        .running_sandboxes
        .lock()
        .await
        .get(&id)
        .cloned()
        .ok_or_else(|| anyhow!("sandbox {id} is not running; start it before snapshotting"))?;
    let payload = handle.snapshot().await?;

    let _guard = harness.inner.write_lock.lock().await;
    let mut sandbox = load_stored_sandbox(harness, owner_dir, &id).await?;
    let snapshot_id = Uuid7::now();

    let manifest = StoredSnapshotManifest {
        snapshot_id,
        sandbox_id: id.clone(),
        kind: payload.kind,
        created_at: Utc::now(),
        payload_size_bytes: payload.bytes.len() as u64,
    };
    let snapshot_dir = owner_dir.join("snapshots").join(snapshot_id.to_string());
    let storage = &harness.inner.storage;
    tokio::try_join!(
        storage.put_bytes(snapshot_dir.join("payload.bin"), payload.bytes.to_vec()),
        storage.put_json(snapshot_dir.join("manifest.json"), &manifest),
    )?;

    sandbox.latest_snapshot_id = Some(snapshot_id);
    harness
        .inner
        .storage
        .put_json(
            owner_dir.join("sandboxes").join(format!("{id}.json")),
            &sandbox,
        )
        .await?;
    Ok((
        snapshot_id,
        EventData::SandboxSnapshotted {
            sandbox_id: id,
            snapshot_id,
        },
    ))
}

async fn start_sandbox_side_effect(
    harness: &BasicExoHarness,
    owner_dir: &Path,
    owner: SandboxOwner,
    request: StartSandboxRequest,
) -> Result<EventData> {
    // Load the snapshot payload before acquiring the write lock. It can be
    // large, and we don't want to block writers while we read.
    let snapshot_dir = owner_dir
        .join("snapshots")
        .join(request.snapshot_id.to_string());
    let storage = &harness.inner.storage;
    let (manifest_result, payload_result) = tokio::join!(
        storage.get_json::<StoredSnapshotManifest>(snapshot_dir.join("manifest.json")),
        storage.get_bytes(snapshot_dir.join("payload.bin")),
    );
    let manifest = manifest_result.with_context(|| {
        format!(
            "loading snapshot manifest for {} (have you taken a snapshot?)",
            request.snapshot_id
        )
    })?;
    let payload_bytes = payload_result
        .with_context(|| format!("loading snapshot payload for {}", request.snapshot_id))?;
    let payload = SnapshotPayload {
        kind: manifest.kind,
        bytes: Bytes::from(payload_bytes),
    };

    let mut sandbox = load_stored_sandbox(harness, owner_dir, &request.id).await?;
    sandbox.running = true;
    sandbox.latest_snapshot_id = Some(request.snapshot_id);
    if let Some(idle_seconds) = request.idle_seconds {
        sandbox.idle_seconds = idle_seconds;
    }
    // Optional provider override: restore under a different backend (e.g.
    // teleport a Docker snapshot up to Daytona). Set before routing so the
    // restore targets the new backend and the new provider is persisted;
    // unsupported providers / snapshot kinds error in the calls below.
    let previous_provider = sandbox.provider;
    if let Some(provider) = request.provider {
        sandbox.provider = provider;
    }
    let cross_provider = sandbox.provider != previous_provider;

    // Remote work before the write lock. Two orders, chosen by whether the
    // restore changes providers:
    //   - Cross-provider (teleport): make-before-break. Boot the new sandbox
    //     first and stop the old one only once it's up, so a failed restore
    //     leaves the source sandbox running and serving. Safe because the two
    //     handles live on different backends and can't collide.
    //   - Same provider: stop-then-boot. The backend replaces the warm
    //     container for this key itself during restore, and stopping the old
    //     handle after the new one exists would tear down the new container's
    //     warm-cache entry (both handles share the same SandboxKey).
    let sandbox_handle = if cross_provider {
        let sandbox_handle = harness
            .inner
            .sandbox_backend_for_provider(sandbox.provider)
            .await?
            .acquire_from_snapshot(sandbox_request(owner, &request.id, &sandbox, None), payload)
            .await?;
        let previous_handle = harness
            .inner
            .running_sandboxes
            .lock()
            .await
            .remove(&request.id);
        if let Some(previous_handle) = previous_handle
            && let Err(error) = previous_handle.stop().await
        {
            // The new sandbox is authoritative at this point; a failure to
            // reap the source shouldn't fail the restore.
            tracing::warn!(
                sandbox_id = %request.id,
                previous_provider = %previous_provider,
                error = format!("{error:#}"),
                "failed to stop the source sandbox after a cross-provider restore"
            );
        }
        sandbox_handle
    } else {
        let previous_handle = harness
            .inner
            .running_sandboxes
            .lock()
            .await
            .remove(&request.id);
        if let Some(previous_handle) = previous_handle {
            previous_handle.stop().await?;
        }
        harness
            .inner
            .sandbox_backend_for_provider(sandbox.provider)
            .await?
            .acquire_from_snapshot(sandbox_request(owner, &request.id, &sandbox, None), payload)
            .await?
    };

    // A restore can boot the sandbox from a different image than the one the
    // sandbox was created with (e.g. the docker backend loads the snapshot as
    // a new tag). Persist the effective image so every process — including
    // scheduler and adapter runners in other processes — derives the same
    // warm-sandbox spec and reattaches to this container instead of creating
    // a second one. Keep the originally requested image for named matching.
    if let Some(effective_image) = sandbox_handle.effective_image()
        && effective_image != sandbox.image
    {
        sandbox
            .requested_image
            .get_or_insert_with(|| sandbox.image.clone());
        sandbox.image = effective_image;
    }

    let _guard = harness.inner.write_lock.lock().await;
    harness
        .inner
        .storage
        .put_json(
            owner_dir
                .join("sandboxes")
                .join(format!("{}.json", request.id)),
            &sandbox,
        )
        .await?;
    harness
        .inner
        .running_sandboxes
        .lock()
        .await
        .insert(request.id.clone(), sandbox_handle);
    Ok(EventData::SandboxStarted {
        sandbox_id: request.id,
        snapshot_id: Some(request.snapshot_id),
    })
}

async fn load_stored_sandbox(
    harness: &BasicExoHarness,
    owner_dir: &Path,
    id: &str,
) -> Result<StoredSandbox> {
    harness
        .inner
        .storage
        .get_json(owner_dir.join("sandboxes").join(format!("{id}.json")))
        .await
}

async fn prepare_sandbox_request(
    harness: &BasicExoHarness,
    request: CreateSandboxRequest,
) -> Result<PreparedSandboxRequest> {
    let image = if !request.image.trim().is_empty() {
        request.image.clone()
    } else if let Some(default) = harness
        .inner
        .binding_default_image(request.provider)
        .await?
    {
        default
    } else {
        request.image.clone()
    };

    Ok(PreparedSandboxRequest {
        name: request.name,
        provider: request.provider,
        image,
        default_workdir: request.default_workdir,
        file_system_mounts: request.file_system_mounts.unwrap_or_default(),
        durable_file_systems: request.durable_file_systems.unwrap_or_default(),
        enable_networking: request.enable_networking.unwrap_or(true),
        idle_seconds: request.idle_seconds.unwrap_or(60),
    })
}

async fn find_matching_stored_sandbox(
    storage: &BasicObjectStore,
    sandboxes_dir: &Path,
    request: &PreparedSandboxRequest,
) -> Result<Option<(SandboxId, StoredSandbox)>> {
    let Some(name) = &request.name else {
        return Ok(None);
    };
    let mut sandboxes = storage
        .list_json_matching_suffix::<StoredSandbox>(sandboxes_dir, ".json")
        .await?;
    sandboxes.sort_by_key(|sandbox| sandbox.id.clone());
    for sandbox in sandboxes.into_iter().rev() {
        if sandbox.name.as_ref() != Some(name) {
            continue;
        }
        if !sandbox.running {
            continue;
        }
        // After a snapshot restore, `image` holds the restored tag; the
        // caller still asks for the image it originally configured, so match
        // against that.
        let comparable_image = sandbox.requested_image.as_ref().unwrap_or(&sandbox.image);
        if sandbox.provider != request.provider
            || comparable_image != &request.image
            || sandbox.default_workdir != request.default_workdir
            || sandbox.file_system_mounts != request.file_system_mounts
            || sandbox.durable_file_systems != request.durable_file_systems
            || sandbox.enable_networking != request.enable_networking
            || sandbox.idle_seconds != request.idle_seconds
        {
            bail!("sandbox name {name:?} already exists with a different configuration");
        }
        return Ok(Some((sandbox.id.clone(), sandbox)));
    }
    Ok(None)
}

async fn active_sandbox_handle(
    harness: &BasicExoHarness,
    owner_dir: &Path,
    owner: SandboxOwner,
    sandbox_id: &SandboxId,
    sandbox: &StoredSandbox,
) -> Result<(Arc<dyn ManagedSandboxHandle>, Option<EventData>)> {
    if let Some(handle) = harness
        .inner
        .running_sandboxes
        .lock()
        .await
        .get(sandbox_id)
        .cloned()
    {
        return Ok((handle, None));
    }

    let (handle, provider_state_event) =
        create_sandbox_handle(harness, owner_dir, owner, sandbox_id, sandbox).await?;
    harness
        .inner
        .running_sandboxes
        .lock()
        .await
        .insert(sandbox_id.clone(), Arc::clone(&handle));
    Ok((handle, provider_state_event))
}

async fn create_sandbox_handle(
    harness: &BasicExoHarness,
    owner_dir: &Path,
    owner: SandboxOwner,
    sandbox_id: &SandboxId,
    sandbox: &StoredSandbox,
) -> Result<(Arc<dyn ManagedSandboxHandle>, Option<EventData>)> {
    let state_key = sandbox_provider_state_key(owner, sandbox_id, sandbox);
    let previous_state = load_sandbox_provider_state(
        harness,
        owner_dir,
        owner,
        sandbox_id,
        sandbox.provider,
        &state_key,
    )
    .await?;
    let handle = harness
        .inner
        .sandbox_backend_for_provider(sandbox.provider)
        .await?
        .acquire(sandbox_request(
            owner,
            sandbox_id,
            sandbox,
            previous_state.clone(),
        ))
        .await?;
    let provider_state_event = sandbox_provider_state_event(
        sandbox_id,
        sandbox.provider,
        state_key,
        previous_state,
        &handle,
    )?;
    Ok((handle, provider_state_event))
}

fn sandbox_provider_state_key(
    owner: SandboxOwner,
    sandbox_id: &SandboxId,
    sandbox: &StoredSandbox,
) -> String {
    let request = sandbox_request(owner, sandbox_id, sandbox, None);
    format!("{}\n{}", request.key, sandbox_spec_hash(&request.spec))
}

async fn load_sandbox_provider_state(
    harness: &BasicExoHarness,
    owner_dir: &Path,
    owner: SandboxOwner,
    sandbox_id: &SandboxId,
    provider: SandboxProvider,
    state_key: &str,
) -> Result<Option<Value>> {
    let SandboxOwner::Conversation(_) = owner else {
        return Ok(None);
    };
    let mut events = load_events(&harness.inner.storage, &owner_dir.join("events"))
        .await?
        .into_iter()
        .filter(|event| event.data.kind() == EventKind::custom(SANDBOX_PROVIDER_STATE_EVENT))
        .collect::<Vec<_>>();
    events.sort_by_key(|event| event.id);
    for event in events.into_iter().rev() {
        let EventData::Custom {
            event_type,
            payload,
        } = event.data
        else {
            continue;
        };
        if event_type != SANDBOX_PROVIDER_STATE_EVENT {
            continue;
        }
        let Ok(payload) = serde_json::from_value::<SandboxProviderStatePayload>(payload) else {
            continue;
        };
        if payload.sandbox_id == *sandbox_id
            && payload.provider == provider
            && payload.state_key == state_key
        {
            return Ok(Some(payload.state));
        }
    }
    Ok(None)
}

fn sandbox_provider_state_event(
    sandbox_id: &SandboxId,
    provider: SandboxProvider,
    state_key: String,
    previous_state: Option<Value>,
    handle: &Arc<dyn ManagedSandboxHandle>,
) -> Result<Option<EventData>> {
    let Some(state) = handle.provider_state() else {
        return Ok(None);
    };
    if previous_state.as_ref() == Some(&state) {
        return Ok(None);
    }
    Ok(Some(EventData::Custom {
        event_type: SANDBOX_PROVIDER_STATE_EVENT.to_string(),
        payload: serde_json::to_value(SandboxProviderStatePayload {
            sandbox_id: sandbox_id.clone(),
            provider,
            state_key,
            state,
        })?,
    }))
}

async fn require_running_sandbox_process(
    harness: &BasicExoHarness,
    sandbox_id: &str,
    process_id: &str,
) -> Result<Arc<RunningSandboxProcess>> {
    let process = harness
        .inner
        .running_processes
        .lock()
        .await
        .get(process_id)
        .cloned()
        .ok_or_else(|| anyhow!("sandbox process not found: {process_id}"))?;
    if process.sandbox_id != sandbox_id {
        bail!("sandbox process {process_id} does not belong to sandbox {sandbox_id}");
    }
    Ok(process)
}

struct BasicTurnHandle {
    harness: BasicExoHarness,
    conversation_dir: PathBuf,
    conversation_id: ConversationId,
    record: TurnRecord,
    state: Mutex<BasicTurnState>,
}

struct BasicTurnState {
    latest_event_id: Option<EventId>,
    finished: bool,
}

impl BasicSandboxScope for BasicTurnHandle {
    fn sandbox_handle(&self) -> BasicScopedSandboxHandle<'_> {
        BasicScopedSandboxHandle::turn(
            &self.harness,
            self.conversation_id,
            self.conversation_dir.clone(),
            self.record.session_id,
            self.record.id,
            &self.state,
        )
    }
}

#[async_trait]
impl TurnHandle for BasicTurnHandle {
    fn record(&self) -> &TurnRecord {
        &self.record
    }

    async fn add_events(&self, data: Vec<EventData>) -> Result<AddEventsResult> {
        let _guard = self.harness.inner.write_lock.lock().await;
        let mut record = self
            .harness
            .inner
            .storage
            .get_json::<ConversationRecord>(self.conversation_dir.join("record.json"))
            .await?;
        let expected_head = record.latest_event_id;
        let add_result = append_events_to_conversation(
            &self.harness.inner,
            &self.conversation_dir,
            self.conversation_id,
            Some(self.record.session_id),
            Some(self.record.id),
            expected_head,
            data,
            &mut record,
        )
        .await?;
        self.harness
            .inner
            .storage
            .put_json(self.conversation_dir.join("record.json"), &record)
            .await?;
        self.state
            .lock()
            .expect("turn state poisoned")
            .latest_event_id = Some(add_result.latest_event_id);
        Ok(add_result)
    }

    async fn write_artifact(&self, request: WriteArtifactRequest) -> Result<ArtifactVersion> {
        let _guard = self.harness.inner.write_lock.lock().await;
        let mut record = self
            .harness
            .inner
            .storage
            .get_json::<ConversationRecord>(self.conversation_dir.join("record.json"))
            .await?;
        let expected_head = record.latest_event_id;
        let artifact_version = write_artifact_version(
            &self.harness.inner,
            &self.conversation_dir.join("artifacts"),
            request,
        )
        .await?;
        let add_result = append_events_to_conversation(
            &self.harness.inner,
            &self.conversation_dir,
            self.conversation_id,
            Some(self.record.session_id),
            Some(self.record.id),
            expected_head,
            vec![EventData::ArtifactWritten {
                artifact_id: artifact_version.artifact_id,
                path: artifact_version.path.clone(),
                version: artifact_version.version,
            }],
            &mut record,
        )
        .await?;
        self.harness
            .inner
            .storage
            .put_json(self.conversation_dir.join("record.json"), &record)
            .await?;
        self.state
            .lock()
            .expect("turn state poisoned")
            .latest_event_id = Some(add_result.latest_event_id);
        Ok(artifact_version)
    }

    async fn finish(&self) -> Result<EventId> {
        let _guard = self.harness.inner.write_lock.lock().await;
        {
            let state = self.state.lock().expect("turn state poisoned");
            if state.finished {
                return state
                    .latest_event_id
                    .ok_or_else(|| anyhow!("turn has no latest event id"));
            }
        }
        let mut record = self
            .harness
            .inner
            .storage
            .get_json::<ConversationRecord>(self.conversation_dir.join("record.json"))
            .await?;
        let expected_head = record.latest_event_id;
        let add_result = append_events_to_conversation(
            &self.harness.inner,
            &self.conversation_dir,
            self.conversation_id,
            Some(self.record.session_id),
            Some(self.record.id),
            expected_head,
            vec![EventData::TurnEnded],
            &mut record,
        )
        .await?;
        self.harness
            .inner
            .storage
            .put_json(self.conversation_dir.join("record.json"), &record)
            .await?;
        let latest = add_result.latest_event_id;
        let mut state = self.state.lock().expect("turn state poisoned");
        state.latest_event_id = Some(latest);
        state.finished = true;
        Ok(latest)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredBinding {
    record: BindingRecord,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredSecret {
    metadata: SecretMetadata,
    secret: EncryptedSecret,
}

impl BasicExoHarnessInner {
    async fn secret_lock(&self, id: SecretId) -> Arc<AsyncMutex<()>> {
        let mut locks = self.secret_locks.lock().await;
        locks.retain(|_, lock| lock.strong_count() > 0);
        if let Some(lock) = locks.get(&id).and_then(Weak::upgrade) {
            return lock;
        }
        let lock = Arc::new(AsyncMutex::new(()));
        locks.insert(id, Arc::downgrade(&lock));
        lock
    }
}

async fn put_secret_in_dir(
    inner: &BasicExoHarnessInner,
    secrets_dir: &Path,
    request: PutSecretRequest,
) -> Result<SecretId> {
    if let Secret::Oauth {
        provider: requested_provider,
        ..
    } = &request.secret
        && let Some(existing) = inner
            .storage
            .list_json_matching_suffix::<StoredSecret>(secrets_dir, ".json")
            .await?
            .into_iter()
            .filter(|record| record.metadata.name == request.name)
            .max_by_key(|record| record.metadata.id)
    {
        let secret_lock = inner.secret_lock(existing.metadata.id).await;
        let _secret_guard = secret_lock.lock().await;
        let _write_guard = inner.write_lock.lock().await;
        let path = secrets_dir.join(format!("{}.json", existing.metadata.id));
        let Some(current) = inner
            .storage
            .get_json_if_exists::<StoredSecret>(&path)
            .await?
        else {
            bail!("OAuth credential changed while login was being saved; retry login");
        };
        match inner.secret_cipher.decrypt_secret(&current.secret)? {
            Secret::Oauth {
                provider,
                access_token: None,
                ..
            } if provider == *requested_provider => {
                let record = StoredSecret {
                    metadata: current.metadata,
                    secret: inner.secret_cipher.encrypt_secret(&request.secret)?,
                };
                inner.storage.put_json(path, &record).await?;
                return Ok(record.metadata.id);
            }
            Secret::Oauth {
                provider,
                access_token: None,
                ..
            } => {
                bail!(
                    "credential `{}` belongs to OAuth provider {:?}, not {:?}",
                    request.name,
                    provider,
                    requested_provider
                );
            }
            Secret::Oauth { .. } => {
                bail!(
                    "OAuth credential `{}` is already logged in; log out before replacing it",
                    request.name
                );
            }
            Secret::Key { .. } => {
                bail!(
                    "credential `{}` is an API key, not an OAuth credential",
                    request.name
                );
            }
        }
    }

    let _write_guard = inner.write_lock.lock().await;
    if matches!(&request.secret, Secret::Oauth { .. })
        && inner
            .storage
            .list_json_matching_suffix::<StoredSecret>(secrets_dir, ".json")
            .await?
            .into_iter()
            .any(|record| record.metadata.name == request.name)
    {
        bail!(
            "OAuth credential `{}` changed while login was being saved; retry login",
            request.name
        );
    }
    let id = Uuid7::now();
    let record = StoredSecret {
        metadata: SecretMetadata {
            id,
            r#type: secret_type(&request.secret),
            name: request.name,
            created_at: id.timestamp().expect("uuid7 timestamp"),
        },
        secret: inner.secret_cipher.encrypt_secret(&request.secret)?,
    };
    inner
        .storage
        .put_json(secrets_dir.join(format!("{id}.json")), &record)
        .await?;
    Ok(id)
}

async fn find_secret_record(
    inner: &BasicExoHarnessInner,
    scopes: &[PathBuf],
    id: &SecretId,
) -> Result<Option<(PathBuf, StoredSecret)>> {
    for scope in scopes {
        let path = scope.join(format!("{id}.json"));
        if let Some(record) = inner
            .storage
            .get_json_if_exists::<StoredSecret>(&path)
            .await?
        {
            return Ok(Some((path, record)));
        }
    }
    Ok(None)
}

async fn get_secret_in_scopes(
    inner: &BasicExoHarnessInner,
    scopes: &[PathBuf],
    id: &SecretId,
) -> Result<Option<Secret>> {
    let Some((_, record)) = find_secret_record(inner, scopes, id).await? else {
        return Ok(None);
    };
    let secret = inner.secret_cipher.decrypt_secret(&record.secret)?;
    let Secret::Oauth {
        provider: Some(_),
        refresh_token: Some(_),
        expires_at: Some(expires_at),
        ..
    } = &secret
    else {
        return Ok(Some(secret));
    };
    if *expires_at > Utc::now() + TimeDelta::minutes(5) {
        return Ok(Some(secret));
    }

    let secret_lock = inner.secret_lock(*id).await;
    let _secret_guard = secret_lock.lock().await;
    let Some((path, record)) = find_secret_record(inner, scopes, id).await? else {
        return Ok(None);
    };
    let secret = inner.secret_cipher.decrypt_secret(&record.secret)?;
    let Secret::Oauth {
        provider: Some(provider),
        account_id,
        refresh_token: Some(refresh_token),
        expires_at: Some(expires_at),
        ..
    } = &secret
    else {
        return Ok(Some(secret));
    };
    if *expires_at > Utc::now() + TimeDelta::minutes(5) {
        return Ok(Some(secret));
    }

    let refreshed = inner
        .oauth_providers
        .get(provider)?
        .refresh(refresh_token)
        .await?;
    let refreshed_secret = Secret::Oauth {
        provider: Some(provider.clone()),
        account_id: refreshed.account_id.or_else(|| account_id.clone()),
        access_token: Some(refreshed.access_token),
        refresh_token: refreshed.refresh_token,
        expires_at: Some(refreshed.expires_at),
    };
    let updated = StoredSecret {
        metadata: record.metadata,
        secret: inner.secret_cipher.encrypt_secret(&refreshed_secret)?,
    };
    let _write_guard = inner.write_lock.lock().await;
    inner.storage.put_json(path, &updated).await?;
    Ok(Some(refreshed_secret))
}

async fn logout_oauth_secret_in_dir(
    inner: &BasicExoHarnessInner,
    secrets_dir: &Path,
    id: &SecretId,
) -> Result<LogoutOauthResult> {
    let secret_lock = inner.secret_lock(*id).await;
    let _secret_guard = secret_lock.lock().await;
    let path = secrets_dir.join(format!("{id}.json"));
    let Some(record) = inner
        .storage
        .get_json_if_exists::<StoredSecret>(&path)
        .await?
    else {
        bail!("OAuth credential `{id}` does not exist");
    };
    let secret = inner.secret_cipher.decrypt_secret(&record.secret)?;
    let Secret::Oauth {
        provider,
        access_token,
        refresh_token,
        ..
    } = secret
    else {
        bail!("secret `{id}` is an API key, not an OAuth credential");
    };

    let was_logged_in = access_token.is_some() || refresh_token.is_some();
    let remote_revocation_confirmed = if !was_logged_in {
        false
    } else if let Some(provider_id) = provider.as_deref() {
        match inner.oauth_providers.get(provider_id) {
            Ok(oauth_provider) => oauth_provider
                .revoke(access_token.as_deref(), refresh_token.as_deref())
                .await
                .is_ok(),
            Err(_) => false,
        }
    } else {
        false
    };

    let logged_out = Secret::Oauth {
        provider,
        account_id: None,
        access_token: None,
        refresh_token: None,
        expires_at: None,
    };
    let updated = StoredSecret {
        metadata: record.metadata,
        secret: inner.secret_cipher.encrypt_secret(&logged_out)?,
    };
    let _write_guard = inner.write_lock.lock().await;
    inner.storage.put_json(path, &updated).await?;
    Ok(LogoutOauthResult {
        was_logged_in,
        remote_revocation_confirmed,
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredArtifactMetadata {
    #[serde(flatten)]
    version: ArtifactVersion,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredSandbox {
    id: SandboxId,
    #[serde(default)]
    name: Option<String>,
    provider: SandboxProvider,
    image: String,
    /// The image originally requested at creation, kept when a snapshot
    /// restore rewrites `image` to the restored tag. Named-sandbox matching
    /// compares against this so config-derived requests still resolve to the
    /// restored sandbox instead of erroring or spawning a duplicate.
    #[serde(default)]
    requested_image: Option<String>,
    default_workdir: Option<String>,
    file_system_mounts: Vec<FileSystemMount>,
    #[serde(default)]
    durable_file_systems: Vec<DurableFileSystem>,
    enable_networking: bool,
    idle_seconds: u64,
    running: bool,
    latest_snapshot_id: Option<SnapshotId>,
}

#[derive(Debug, Clone)]
struct PreparedSandboxRequest {
    name: Option<String>,
    provider: SandboxProvider,
    image: String,
    default_workdir: Option<String>,
    file_system_mounts: Vec<FileSystemMount>,
    durable_file_systems: Vec<DurableFileSystem>,
    enable_networking: bool,
    idle_seconds: u64,
}

impl PreparedSandboxRequest {
    fn stored_sandbox(&self, id: SandboxId) -> StoredSandbox {
        StoredSandbox {
            id,
            name: self.name.clone(),
            provider: self.provider,
            image: self.image.clone(),
            requested_image: None,
            default_workdir: self.default_workdir.clone(),
            file_system_mounts: self.file_system_mounts.clone(),
            durable_file_systems: self.durable_file_systems.clone(),
            enable_networking: self.enable_networking,
            idle_seconds: self.idle_seconds,
            running: true,
            latest_snapshot_id: None,
        }
    }
}

struct PendingSandboxProcess {
    record: SandboxProcessRecord,
    process: Arc<RunningSandboxProcess>,
    stdout: BoxAsyncRead,
    stderr: BoxAsyncRead,
    wait: BoxFuture<'static, Result<i32>>,
    started_event: EventData,
    provider_state_event: Option<EventData>,
}

async fn prepare_sandbox_process(
    harness: &BasicExoHarness,
    owner_dir: &Path,
    owner: SandboxOwner,
    event_log: Option<SandboxProcessEventLog>,
    request: StartSandboxProcessRequest,
) -> Result<PendingSandboxProcess> {
    let sandbox = load_stored_sandbox(harness, owner_dir, &request.sandbox_id).await?;
    if !sandbox.running {
        bail!("sandbox is not running: {}", request.sandbox_id);
    }
    if request.command.is_empty() {
        bail!("sandbox command must not be empty");
    }
    if request.mode != SandboxProcessMode::Exec {
        bail!("basic sandbox backend only supports exec-mode processes");
    }
    let (sandbox_handle, provider_state_event) =
        active_sandbox_handle(harness, owner_dir, owner, &request.sandbox_id, &sandbox).await?;
    let process_id = format!("process-{}", Uuid7::now());
    let sandbox_id = request.sandbox_id.clone();
    let command = request.command.clone();
    let cwd = request.cwd.clone();
    let mode = request.mode;
    let stdin_mode = request.stdin;
    let output = request.output;
    let lifecycle = request.lifecycle;
    let name = request.name.clone();
    let parts = sandbox_handle
        .start_process(&SandboxCommand {
            argv: command.clone(),
            env: request.env,
            display_argv: Some(command.clone()),
            cwd: cwd.clone(),
            timeout: None,
        })
        .await
        .with_context(|| format!("failed to start process in sandbox {}", request.sandbox_id))?;
    let SandboxProcessParts {
        stdout,
        stderr,
        stdin,
        wait,
    } = parts;
    let stdin = match stdin_mode {
        SandboxProcessStdin::Open => Some(stdin),
        SandboxProcessStdin::None => None,
    };
    let process = Arc::new(RunningSandboxProcess {
        event_log,
        sandbox_id: sandbox_id.clone(),
        process_id: process_id.clone(),
        stdin: AsyncMutex::new(stdin),
        events: AsyncMutex::new(Vec::new()),
        status: AsyncMutex::new(SandboxProcessStatus::Running),
        open_output_streams: AsyncMutex::new(2),
        output_drained: Notify::new(),
        tasks: AsyncMutex::new(None),
        notify: Notify::new(),
    });
    Ok(PendingSandboxProcess {
        record: SandboxProcessRecord {
            id: process_id.clone(),
            sandbox_id: sandbox_id.clone(),
            name: name.clone(),
            status: SandboxProcessStatus::Running,
        },
        process,
        stdout,
        stderr,
        wait,
        started_event: EventData::SandboxProcessStarted {
            sandbox_id,
            process_id,
            name,
            command,
            cwd,
            mode,
            stdin: stdin_mode,
            output,
            lifecycle,
            status: SandboxProcessStatus::Running,
            provider_state: None,
        },
        provider_state_event,
    })
}

async fn spawn_pending_sandbox_process(
    harness: &BasicExoHarness,
    pending: PendingSandboxProcess,
) -> Result<SandboxProcessRecord> {
    let PendingSandboxProcess {
        record,
        process,
        stdout,
        stderr,
        wait,
        ..
    } = pending;
    let process_id = record.id.clone();
    let stdout_task = tokio::spawn(record_sandbox_process_output(
        Arc::clone(&process),
        SandboxProcessOutputStream::Stdout,
        stdout,
    ));
    let stderr_task = tokio::spawn(record_sandbox_process_output(
        Arc::clone(&process),
        SandboxProcessOutputStream::Stderr,
        stderr,
    ));
    let wait_task = tokio::spawn(record_sandbox_process_exit(Arc::clone(&process), wait));
    *process.tasks.lock().await = Some(RunningSandboxProcessTasks {
        stdout: stdout_task,
        stderr: stderr_task,
        wait: wait_task,
    });
    harness
        .inner
        .running_processes
        .lock()
        .await
        .insert(process_id, process);
    Ok(record)
}

struct RunningSandboxProcess {
    event_log: Option<SandboxProcessEventLog>,
    sandbox_id: SandboxId,
    process_id: SandboxProcessId,
    stdin: AsyncMutex<Option<BoxAsyncWrite>>,
    events: AsyncMutex<Vec<SandboxProcessEvent>>,
    status: AsyncMutex<SandboxProcessStatus>,
    open_output_streams: AsyncMutex<u8>,
    output_drained: Notify,
    tasks: AsyncMutex<Option<RunningSandboxProcessTasks>>,
    notify: Notify,
}

struct SandboxProcessEventLog {
    inner: Arc<BasicExoHarnessInner>,
    conversation_id: ConversationId,
    conversation_dir: PathBuf,
}

struct RunningSandboxProcessTasks {
    stdout: JoinHandle<()>,
    stderr: JoinHandle<()>,
    wait: JoinHandle<()>,
}

enum SandboxProcessOutputStream {
    Stdout,
    Stderr,
}

async fn record_sandbox_process_output(
    process: Arc<RunningSandboxProcess>,
    stream: SandboxProcessOutputStream,
    mut reader: BoxAsyncRead,
) {
    let mut buffer = vec![0; 8192];
    loop {
        match reader.read(&mut buffer).await {
            Ok(0) => {
                mark_sandbox_process_output_closed(&process).await;
                return;
            }
            Ok(length) => {
                let data = buffer[..length].to_vec();
                match stream {
                    SandboxProcessOutputStream::Stdout => {
                        push_sandbox_process_event(
                            &process,
                            SandboxProcessEventPayload::Stdout(data),
                        )
                        .await;
                    }
                    SandboxProcessOutputStream::Stderr => {
                        push_sandbox_process_event(
                            &process,
                            SandboxProcessEventPayload::Stderr(data),
                        )
                        .await;
                    }
                }
            }
            Err(error) => {
                let message = error.to_string();
                push_sandbox_process_event(
                    &process,
                    SandboxProcessEventPayload::Error(message.clone()),
                )
                .await;
                set_sandbox_process_status(&process, SandboxProcessStatus::Failed { message })
                    .await;
                mark_sandbox_process_output_closed(&process).await;
                return;
            }
        }
    }
}

async fn record_sandbox_process_exit(
    process: Arc<RunningSandboxProcess>,
    wait: BoxFuture<'static, Result<i32>>,
) {
    let terminal = wait.await;
    wait_for_sandbox_process_output_drained(&process).await;
    if !sandbox_process_status(&process).await.is_running() {
        return;
    }
    match terminal {
        Ok(exit_code) => {
            push_sandbox_process_event(&process, SandboxProcessEventPayload::Exit(exit_code)).await;
            set_sandbox_process_status(&process, SandboxProcessStatus::Exited { exit_code }).await;
        }
        Err(error) => {
            let message = error.to_string();
            push_sandbox_process_event(
                &process,
                SandboxProcessEventPayload::Error(message.clone()),
            )
            .await;
            set_sandbox_process_status(
                &process,
                SandboxProcessStatus::Failed {
                    message: message.clone(),
                },
            )
            .await;
        }
    }
}

async fn mark_sandbox_process_output_closed(process: &Arc<RunningSandboxProcess>) {
    let mut open_output_streams = process.open_output_streams.lock().await;
    if *open_output_streams > 0 {
        *open_output_streams -= 1;
    }
    let drained = *open_output_streams == 0;
    drop(open_output_streams);
    if drained {
        process.output_drained.notify_waiters();
    }
}

async fn wait_for_sandbox_process_output_drained(process: &Arc<RunningSandboxProcess>) {
    loop {
        let notified = process.output_drained.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if *process.open_output_streams.lock().await == 0 {
            return;
        }
        notified.await;
    }
}

enum SandboxProcessEventPayload {
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
    Exit(i32),
    Error(String),
    Cancelled,
}

async fn push_sandbox_process_event(
    process: &Arc<RunningSandboxProcess>,
    payload: SandboxProcessEventPayload,
) {
    let event = push_sandbox_process_event_memory_only(process, payload).await;
    if let Err(error) = append_sandbox_process_data(
        process,
        vec![EventData::SandboxProcessEvent {
            sandbox_id: process.sandbox_id.clone(),
            process_id: process.process_id.clone(),
            event,
        }],
    )
    .await
    {
        push_sandbox_process_event_memory_only(
            process,
            SandboxProcessEventPayload::Error(format!(
                "failed to persist sandbox process event: {error}"
            )),
        )
        .await;
    }
}

async fn push_sandbox_process_event_memory_only(
    process: &Arc<RunningSandboxProcess>,
    payload: SandboxProcessEventPayload,
) -> SandboxProcessEvent {
    let mut events = process.events.lock().await;
    let cursor = events.len() as u64 + 1;
    let event = match payload {
        SandboxProcessEventPayload::Stdout(data) => SandboxProcessEvent::Stdout { cursor, data },
        SandboxProcessEventPayload::Stderr(data) => SandboxProcessEvent::Stderr { cursor, data },
        SandboxProcessEventPayload::Exit(exit_code) => {
            SandboxProcessEvent::Exit { cursor, exit_code }
        }
        SandboxProcessEventPayload::Error(message) => {
            SandboxProcessEvent::Error { cursor, message }
        }
        SandboxProcessEventPayload::Cancelled => SandboxProcessEvent::Cancelled { cursor },
    };
    events.push(event.clone());
    drop(events);
    process.notify.notify_waiters();
    event
}

async fn set_sandbox_process_status(
    process: &Arc<RunningSandboxProcess>,
    status: SandboxProcessStatus,
) {
    let append_result = append_sandbox_process_data(
        process,
        vec![EventData::SandboxProcessStateUpdated {
            sandbox_id: process.sandbox_id.clone(),
            process_id: process.process_id.clone(),
            status: status.clone(),
            provider_state: None,
        }],
    )
    .await;
    let mut current = process.status.lock().await;
    *current = status;
    drop(current);
    process.notify.notify_waiters();
    if let Err(error) = append_result {
        push_sandbox_process_event_memory_only(
            process,
            SandboxProcessEventPayload::Error(format!(
                "failed to persist sandbox process status: {error}"
            )),
        )
        .await;
    }
}

async fn append_sandbox_process_data(
    process: &Arc<RunningSandboxProcess>,
    data: Vec<EventData>,
) -> Result<()> {
    let Some(event_log) = &process.event_log else {
        return Ok(());
    };
    let _guard = event_log.inner.write_lock.lock().await;
    let mut record = event_log
        .inner
        .storage
        .get_json::<ConversationRecord>(event_log.conversation_dir.join("record.json"))
        .await?;
    append_events_to_conversation(
        &event_log.inner,
        &event_log.conversation_dir,
        event_log.conversation_id,
        None,
        None,
        record.latest_event_id,
        data,
        &mut record,
    )
    .await?;
    event_log
        .inner
        .storage
        .put_json(event_log.conversation_dir.join("record.json"), &record)
        .await?;
    Ok(())
}

async fn sandbox_process_status(process: &Arc<RunningSandboxProcess>) -> SandboxProcessStatus {
    process.status.lock().await.clone()
}

async fn wait_for_sandbox_process_terminal_status(
    process: &Arc<RunningSandboxProcess>,
) -> SandboxProcessStatus {
    loop {
        let notified = process.notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        let status = sandbox_process_status(process).await;
        if !status.is_running() {
            return status;
        }
        notified.await;
    }
}

/// Sidecar JSON describing a snapshot payload.
///
/// Lives at `{conversation_dir}/snapshots/{snapshot_id}/manifest.json` alongside
/// the payload blob at `payload.bin`. The `kind` controls how the payload is
/// interpreted on restore — only a backend that recognises that kind can
/// reconstruct a sandbox from it.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredSnapshotManifest {
    snapshot_id: SnapshotId,
    sandbox_id: SandboxId,
    kind: SnapshotKind,
    created_at: DateTime<Utc>,
    payload_size_bytes: u64,
}

struct LiveSandboxProcess {
    parts: Option<SandboxProcessParts>,
}

impl LiveSandboxProcess {
    fn new(parts: SandboxProcessParts) -> Self {
        Self { parts: Some(parts) }
    }
}

impl SandboxProcess for LiveSandboxProcess {
    fn into_parts(mut self: Box<Self>) -> SandboxProcessParts {
        self.parts
            .take()
            .expect("live sandbox process parts already consumed")
    }
}

fn sandbox_request(
    owner: SandboxOwner,
    sandbox_id: &str,
    sandbox: &StoredSandbox,
    provider_state: Option<Value>,
) -> SandboxRequest {
    SandboxRequest {
        key: match owner {
            SandboxOwner::Agent(agent_id) => SandboxKey::AgentSandbox {
                agent_id: agent_id.to_string(),
                sandbox_id: sandbox_id.to_string(),
            },
            SandboxOwner::Conversation(conversation_id) => SandboxKey::ConversationSandbox {
                conversation_id: conversation_id.to_string(),
                sandbox_id: sandbox_id.to_string(),
            },
        },
        spec: SandboxSpec {
            image: sandbox.image.clone(),
            mounts: sandbox
                .file_system_mounts
                .iter()
                .map(|mount| SandboxMount {
                    host_path: PathBuf::from(&mount.host_path),
                    guest_path: mount.mount_path.clone(),
                    access: match mount.mode {
                        crate::FileSystemMountMode::ReadOnly => SandboxMountAccess::ReadOnly,
                        crate::FileSystemMountMode::ReadWrite => SandboxMountAccess::ReadWrite,
                    },
                    internal: mount.internal.unwrap_or(false),
                })
                .collect(),
            durable_file_systems: sandbox.durable_file_systems.clone(),
            network: if sandbox.enable_networking {
                SandboxNetworkPolicy::Enabled
            } else {
                SandboxNetworkPolicy::Disabled
            },
            default_workdir: sandbox
                .default_workdir
                .clone()
                .unwrap_or_else(|| SANDBOX_MAIN_MOUNT_DIR.to_string()),
        },
        lifecycle: SandboxLifecycleConfig {
            idle_ttl: Some(std::time::Duration::from_secs(sandbox.idle_seconds)),
        },
        provider_state,
    }
}

async fn append_events_to_conversation(
    inner: &BasicExoHarnessInner,
    conversation_dir: &Path,
    conversation_id: ConversationId,
    session_id: Option<SessionId>,
    turn_id: Option<TurnId>,
    expected_head: Option<EventId>,
    data: Vec<EventData>,
    record: &mut ConversationRecord,
) -> Result<AddEventsResult> {
    if data.is_empty() {
        bail!("cannot append zero events");
    }
    if let Some(expected_head) = expected_head {
        ensure_conversation_head(
            record.latest_event_id,
            Some(expected_head),
            session_id,
            turn_id,
        )?;
    }
    let mut event_ids = Vec::new();
    let mut latest_event_id = None;
    for data in data {
        let id = Uuid7::now();
        let event = Event {
            id,
            conversation_id,
            session_id,
            turn_id,
            created_at: id.timestamp().expect("uuid7 timestamp"),
            data,
        };
        inner
            .storage
            .put_json(
                conversation_dir
                    .join("events")
                    .join(format!("{}.json", event.id)),
                &event,
            )
            .await?;
        notify_subscribers(inner, conversation_id, event.clone());
        latest_event_id = Some(event.id);
        event_ids.push(event.id);
    }
    let latest_event_id = latest_event_id.expect("at least one event");
    record.latest_event_id = Some(latest_event_id);
    Ok(AddEventsResult {
        event_ids,
        latest_event_id,
    })
}

fn ensure_conversation_head(
    current_head: Option<EventId>,
    expected_head: Option<EventId>,
    session_id: Option<SessionId>,
    turn_id: Option<TurnId>,
) -> Result<()> {
    if current_head == expected_head {
        return Ok(());
    }
    Err(ConversationHeadMismatch {
        current_head,
        expected_head,
        session_id,
        turn_id,
    }
    .into())
}

#[derive(Debug, Clone)]
struct ConversationHeadMismatch {
    current_head: Option<EventId>,
    expected_head: Option<EventId>,
    session_id: Option<SessionId>,
    turn_id: Option<TurnId>,
}

impl Display for ConversationHeadMismatch {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let expected = format_event_head_timestamp(self.expected_head);
        let current = format_event_head_timestamp(self.current_head);
        if let Some(turn_id) = self.turn_id {
            let session = self
                .session_id
                .map(|session_id| session_id.to_string())
                .unwrap_or_else(|| "none".to_string());
            return write!(
                f,
                "turn is stale and cannot be resumed: conversation head advanced outside this turn \
                 (turn_id: {turn_id}, session_id: {session}, expected_head_at: {expected}, \
                 current_head_at: {current})"
            );
        }
        write!(
            f,
            "conversation head mismatch: expected_head_at: {expected}, current_head_at: {current}"
        )
    }
}

impl std::error::Error for ConversationHeadMismatch {}

fn format_event_head_timestamp(head: Option<EventId>) -> String {
    let Some(id) = head else {
        return "none".to_string();
    };
    id.timestamp()
        .map(|timestamp| timestamp.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
        .unwrap_or_else(|| "unknown".to_string())
}

fn notify_subscribers(inner: &BasicExoHarnessInner, conversation_id: ConversationId, event: Event) {
    let mut subscribers = inner.subscribers.lock().expect("subscribers poisoned");
    let Some(entries) = subscribers.get_mut(&conversation_id) else {
        return;
    };
    entries.retain(|sender| sender.send(Ok(event.clone())).is_ok());
}

fn matches_bound(event_id: EventId, bound: &Bound<EventId>) -> bool {
    match bound {
        Bound::Unbounded => false,
        Bound::Included(id) => event_id >= *id,
        Bound::Excluded(id) => event_id > *id,
    }
}

async fn load_events(storage: &BasicObjectStore, events_dir: &Path) -> Result<Vec<Event>> {
    let mut events = storage
        .list_json_matching_suffix::<Event>(events_dir, ".json")
        .await?;
    events.sort_by_key(|event| event.id);
    Ok(events)
}

async fn load_artifact_versions(
    storage: &BasicObjectStore,
    artifacts_dir: &Path,
) -> Result<Vec<ArtifactVersion>> {
    let mut versions = storage
        .list_json_matching_suffix::<StoredArtifactMetadata>(artifacts_dir, ".json")
        .await?
        .into_iter()
        .map(|artifact| artifact.version)
        .collect::<Vec<_>>();
    versions.sort_by_key(|artifact| (artifact.artifact_id, artifact.version));
    Ok(versions)
}

async fn write_artifact_version(
    inner: &BasicExoHarnessInner,
    artifacts_dir: &Path,
    request: WriteArtifactRequest,
) -> Result<ArtifactVersion> {
    let versions = load_artifact_versions(&inner.storage, artifacts_dir).await?;
    let existing = versions
        .iter()
        .filter(|artifact| artifact.path == request.path)
        .max_by_key(|artifact| artifact.version);
    let artifact_id = existing
        .map(|artifact| artifact.artifact_id)
        .unwrap_or_else(Uuid7::now);
    let version = existing.map(|artifact| artifact.version + 1).unwrap_or(1);
    let created_at = Uuid7::now().timestamp().expect("uuid7 timestamp");
    let artifact_version = ArtifactVersion {
        artifact_id,
        path: request.path,
        version,
        created_at,
        size_bytes: request.contents.len() as u64,
    };
    let artifact_dir = artifacts_dir.join(artifact_id.to_string());
    inner
        .storage
        .put_json(
            artifact_dir.join(format!("{version}.json")),
            &StoredArtifactMetadata {
                version: artifact_version.clone(),
            },
        )
        .await?;
    inner
        .storage
        .put_bytes(
            artifact_dir.join(format!("{version}.bin")),
            request.contents,
        )
        .await?;
    Ok(artifact_version)
}

async fn load_artifact_contents(
    storage: &BasicObjectStore,
    artifact_dir: &Path,
    version: u64,
) -> Result<Vec<u8>> {
    let contents_path = artifact_dir.join(format!("{version}.bin"));
    if let Some(contents) = storage.get_bytes_if_exists(&contents_path).await? {
        return Ok(contents);
    }

    let metadata_path = artifact_dir.join(format!("{version}.json"));
    let legacy_artifact = storage
        .get_json_if_exists::<Artifact>(&metadata_path)
        .await?;
    let Some(legacy_artifact) = legacy_artifact else {
        bail!("missing artifact contents for {}", metadata_path.display());
    };
    Ok(legacy_artifact.contents)
}

async fn list_binding_records(
    storage: &BasicObjectStore,
    bindings_dir: &Path,
) -> Result<Vec<BindingRecord>> {
    let mut bindings = storage
        .list_json_matching_suffix::<StoredBinding>(bindings_dir, ".json")
        .await?
        .into_iter()
        .map(|stored| stored.record)
        .collect::<Vec<_>>();
    bindings.sort_by_key(|metadata| metadata.id);
    Ok(bindings)
}

fn stored_binding(id: BindingId, binding: Binding) -> StoredBinding {
    StoredBinding {
        record: BindingRecord {
            id,
            r#type: binding_type(&binding),
            name: binding_name(&binding).to_string(),
            created_at: id.timestamp().expect("uuid7 timestamp"),
            binding,
        },
    }
}

async fn list_secret_metadata(
    storage: &BasicObjectStore,
    secrets_dir: &Path,
) -> Result<Vec<SecretMetadata>> {
    let mut secrets = storage
        .list_json_matching_suffix::<StoredSecret>(secrets_dir, ".json")
        .await?
        .into_iter()
        .map(|secret| secret.metadata)
        .collect::<Vec<_>>();
    secrets.sort_by_key(|metadata| metadata.id);
    Ok(secrets)
}

fn merge_binding_records(scopes: Vec<Vec<BindingRecord>>) -> Vec<BindingRecord> {
    let mut effective = HashMap::<String, BindingRecord>::new();
    for bindings in scopes {
        for binding in bindings {
            effective.insert(binding.name.clone(), binding);
        }
    }
    let mut bindings = effective.into_values().collect::<Vec<_>>();
    bindings.sort_by_key(|metadata| metadata.id);
    bindings
}

fn merge_secret_metadata(scopes: Vec<Vec<SecretMetadata>>) -> Vec<SecretMetadata> {
    let mut effective = HashMap::<String, SecretMetadata>::new();
    for secrets in scopes {
        for secret in secrets {
            effective.insert(secret.name.clone(), secret);
        }
    }
    let mut secrets = effective.into_values().collect::<Vec<_>>();
    secrets.sort_by_key(|metadata| metadata.id);
    secrets
}

fn binding_type(binding: &Binding) -> BindingType {
    match binding {
        Binding::Env { .. } => BindingType::Env,
        Binding::Mcp { .. } => BindingType::Mcp,
        Binding::Llm { .. } => BindingType::Llm,
        Binding::Sandbox { .. } => BindingType::Sandbox,
    }
}

fn binding_name(binding: &Binding) -> &str {
    match binding {
        Binding::Env { name, .. }
        | Binding::Mcp { name, .. }
        | Binding::Llm { name, .. }
        | Binding::Sandbox { name, .. } => name,
    }
}

fn secret_type(secret: &Secret) -> SecretType {
    match secret {
        Secret::Key { .. } => SecretType::Key,
        Secret::Oauth { .. } => SecretType::Oauth,
    }
}

fn derive_unique_slug(prefix: &str, existing: &[ConversationRecord]) -> String {
    let mut counter = 1usize;
    loop {
        let candidate = format!("{prefix}-{counter}");
        if existing
            .iter()
            .all(|conversation| conversation.slug != candidate)
        {
            return candidate;
        }
        counter += 1;
    }
}

fn slug_to_name(slug: &str) -> String {
    slug.replace('-', " ")
}

fn agent_bindings_dir(harness: &BasicExoHarness, agent_id: AgentId) -> PathBuf {
    harness
        .agents_dir()
        .join(agent_id.to_string())
        .join("bindings")
}

fn agent_secrets_dir(harness: &BasicExoHarness, agent_id: AgentId) -> PathBuf {
    harness
        .agents_dir()
        .join(agent_id.to_string())
        .join("secrets")
}

fn build_secret_cipher(
    choice: SecretBackendChoice,
    keychain_account: String,
) -> Result<SecretCipher> {
    #[cfg(not(feature = "apple-keychain"))]
    drop(keychain_account);

    let provider: Arc<dyn SecretKeyProvider> = match choice {
        #[cfg(feature = "apple-keychain")]
        SecretBackendChoice::AppleKeychain => {
            Arc::new(AppleKeychainSecretKeyProvider::new(keychain_account))
        }
        SecretBackendChoice::File { path } => {
            let path = match path {
                Some(path) => path,
                None => default_master_key_path()?,
            };
            Arc::new(FileBackedSecretKeyProvider::new(path))
        }
        SecretBackendChoice::Static(key) => Arc::new(StaticSecretKeyProvider::new(key)),
    };
    Ok(SecretCipher::new(provider))
}

#[cfg(feature = "apple-keychain")]
mod keychain_migration {
    use super::*;

    const SECRET_STORE_ID_PATH: &str = "metadata/secret-store.json";
    const KEYCHAIN_MIGRATION_PATH: &str = "metadata/keychain-migration-v1.json";

    #[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
    struct SecretStoreIdentity {
        id: uuid::Uuid,
    }

    pub(super) async fn prepare_keychain_account(
        storage: &BasicObjectStore,
        legacy_account: &str,
    ) -> Result<String> {
        let account = keychain_account_for_store(storage).await?;
        migrate_keychain_account_if_needed(storage, &account, legacy_account).await?;
        Ok(account)
    }

    async fn keychain_account_for_store(storage: &BasicObjectStore) -> Result<String> {
        let identity = secret_store_identity(storage).await?;
        Ok(format!("exo-secret-store:{}", identity.id))
    }

    async fn migrate_keychain_account_if_needed(
        storage: &BasicObjectStore,
        account: &str,
        legacy_account: &str,
    ) -> Result<()> {
        let marker = Path::new(KEYCHAIN_MIGRATION_PATH);
        if storage
            .get_json_if_exists::<bool>(marker)
            .await?
            .unwrap_or(false)
        {
            return Ok(());
        }
        if has_stored_secret_records(storage).await? {
            migrate_apple_keychain_master_key(account, legacy_account)?;
        }
        storage.put_json(marker, &true).await
    }

    async fn has_stored_secret_records(storage: &BasicObjectStore) -> Result<bool> {
        Ok(storage.list_keys("").await?.iter().any(|key| {
            let components = key.split('/').collect::<Vec<_>>();
            matches!(
                components.as_slice(),
                ["secrets", file]
                    | ["agents", _, "secrets", file]
                    | ["agents", _, "conversations", _, "secrets", file]
                    if file.ends_with(".json")
            )
        }))
    }

    async fn secret_store_identity(storage: &BasicObjectStore) -> Result<SecretStoreIdentity> {
        let path = Path::new(SECRET_STORE_ID_PATH);
        if let Some(identity) = storage
            .get_json_if_exists::<SecretStoreIdentity>(path)
            .await?
        {
            return Ok(identity);
        }
        let candidate = SecretStoreIdentity {
            id: uuid::Uuid::new_v4(),
        };
        storage.put_json(path, &candidate).await?;
        Ok(candidate)
    }

    #[cfg(test)]
    mod keychain_account_tests {
        use tempfile::TempDir;

        use super::{
            BasicObjectStore, KEYCHAIN_MIGRATION_PATH, has_stored_secret_records,
            keychain_account_for_store, migrate_keychain_account_if_needed,
        };

        #[tokio::test]
        async fn fresh_store_identity_is_stable_across_paths() {
            let parent = TempDir::new().expect("parent tempdir");
            let original_path = parent.path().join("original");
            let moved_path = parent.path().join("moved");
            tokio::fs::create_dir(&original_path)
                .await
                .expect("create original store");
            let storage = BasicObjectStore::local_filesystem(&original_path)
                .await
                .expect("object store");
            let first = keychain_account_for_store(&storage)
                .await
                .expect("first account");
            drop(storage);
            tokio::fs::rename(&original_path, &moved_path)
                .await
                .expect("move store");
            let moved_storage = BasicObjectStore::local_filesystem(&moved_path)
                .await
                .expect("moved object store");
            let moved = keychain_account_for_store(&moved_storage)
                .await
                .expect("moved account");

            assert_eq!(first, moved);
            assert!(first.starts_with("exo-secret-store:"));
        }

        #[tokio::test]
        async fn distinct_stores_have_distinct_identities() {
            let first_dir = TempDir::new().expect("first tempdir");
            let second_dir = TempDir::new().expect("second tempdir");
            let first_storage = BasicObjectStore::local_filesystem(first_dir.path())
                .await
                .expect("first object store");
            let second_storage = BasicObjectStore::local_filesystem(second_dir.path())
                .await
                .expect("second object store");

            let first = keychain_account_for_store(&first_storage)
                .await
                .expect("first account");
            let second = keychain_account_for_store(&second_storage)
                .await
                .expect("second account");

            assert_ne!(first, second);
        }

        #[tokio::test]
        async fn fresh_stores_complete_migration_without_keychain_access() {
            let tempdir = TempDir::new().expect("tempdir");
            let storage = BasicObjectStore::local_filesystem(tempdir.path())
                .await
                .expect("object store");

            migrate_keychain_account_if_needed(
                &storage,
                "unused-current-account",
                "unused-legacy-account",
            )
            .await
            .expect("complete migration");

            assert!(
                storage
                    .get_json::<bool>(KEYCHAIN_MIGRATION_PATH)
                    .await
                    .expect("migration marker")
            );
        }

        #[tokio::test]
        async fn stored_secret_detection_checks_each_supported_scope() {
            for secret_path in [
                "secrets/root.json",
                "agents/agent-id/secrets/agent.json",
                "agents/agent-id/conversations/conversation-id/secrets/conversation.json",
            ] {
                let tempdir = TempDir::new().expect("tempdir");
                let storage = BasicObjectStore::local_filesystem(tempdir.path())
                    .await
                    .expect("object store");
                storage
                    .put_json(secret_path, &"encrypted fixture")
                    .await
                    .expect("encrypted secret marker");

                assert!(
                    has_stored_secret_records(&storage)
                        .await
                        .expect("detect stored secret"),
                    "failed to detect {secret_path}"
                );
            }

            let tempdir = TempDir::new().expect("tempdir");
            let storage = BasicObjectStore::local_filesystem(tempdir.path())
                .await
                .expect("object store");
            storage
                .put_json(
                    "agents/agent-id/artifacts/secrets/not-a-secret.json",
                    &"fixture",
                )
                .await
                .expect("unrelated fixture");
            assert!(
                !has_stored_secret_records(&storage)
                    .await
                    .expect("check unrelated json")
            );
        }
    }
}
