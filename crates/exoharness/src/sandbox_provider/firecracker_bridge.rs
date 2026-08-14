use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::compat::{FuturesAsyncReadCompatExt, FuturesAsyncWriteCompatExt};

use crate::{
    FirecrackerConfig, FirecrackerSandboxBackend, ManagedSandboxBackend, ManagedSandboxHandle,
    SandboxCommand, SandboxCommandOutput, SandboxProcessParts, SandboxRequest,
};

const MAX_BRIDGE_FRAME_BYTES: usize = 16 * 1024 * 1024;
const OUTPUT_DRAIN_GRACE: Duration = Duration::from_secs(2);

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum FirecrackerBridgeRequest {
    Acquire {
        config: FirecrackerConfig,
        request: SandboxRequest,
    },
    Exec {
        config: FirecrackerConfig,
        request: SandboxRequest,
        command: SandboxCommand,
    },
    StartProcess {
        config: FirecrackerConfig,
        request: SandboxRequest,
        command: SandboxCommand,
    },
    Stop {
        config: FirecrackerConfig,
        request: SandboxRequest,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum FirecrackerBridgeResponse {
    Acquired {
        id: String,
        provider_state: Option<Value>,
        effective_image: Option<String>,
    },
    Exec {
        output: SandboxCommandOutput,
    },
    Stopped,
}

pub async fn run_firecracker_bridge() -> Result<Option<i32>> {
    let request = read_frame::<FirecrackerBridgeRequest>(&mut tokio::io::stdin()).await?;
    match request {
        FirecrackerBridgeRequest::Acquire { config, request } => {
            let handle = acquire(config, request).await?;
            write_frame(
                &mut tokio::io::stdout(),
                &FirecrackerBridgeResponse::Acquired {
                    id: handle.id().to_string(),
                    provider_state: handle.provider_state(),
                    effective_image: handle.effective_image(),
                },
            )
            .await?;
            Ok(None)
        }
        FirecrackerBridgeRequest::Exec {
            config,
            request,
            command,
        } => {
            let output = acquire(config, request).await?.exec(&command).await?;
            write_frame(
                &mut tokio::io::stdout(),
                &FirecrackerBridgeResponse::Exec { output },
            )
            .await?;
            Ok(None)
        }
        FirecrackerBridgeRequest::StartProcess {
            config,
            request,
            command,
        } => {
            let parts = acquire(config, request)
                .await?
                .start_process(&command)
                .await?;
            Ok(Some(proxy_process(parts).await?))
        }
        FirecrackerBridgeRequest::Stop { config, request } => {
            acquire(config, request).await?.stop().await?;
            write_frame(
                &mut tokio::io::stdout(),
                &FirecrackerBridgeResponse::Stopped,
            )
            .await?;
            Ok(None)
        }
    }
}

async fn acquire(
    config: FirecrackerConfig,
    request: SandboxRequest,
) -> Result<Arc<dyn ManagedSandboxHandle>> {
    FirecrackerSandboxBackend::new(config)?
        .acquire(request)
        .await
}

async fn proxy_process(parts: SandboxProcessParts) -> Result<i32> {
    let SandboxProcessParts {
        stdout,
        stderr,
        stdin,
        wait,
    } = parts;
    let input = tokio::spawn(async move {
        let mut source = tokio::io::stdin();
        let mut destination = stdin.compat_write();
        tokio::io::copy(&mut source, &mut destination).await?;
        destination.shutdown().await
    });
    let stdout = tokio::spawn(async move {
        let mut source = stdout.compat();
        let mut destination = tokio::io::stdout();
        tokio::io::copy(&mut source, &mut destination).await?;
        destination.flush().await
    });
    let stderr = tokio::spawn(async move {
        let mut source = stderr.compat();
        let mut destination = tokio::io::stderr();
        tokio::io::copy(&mut source, &mut destination).await?;
        destination.flush().await
    });
    let exit_code = wait.await?;
    input.abort();
    match tokio::time::timeout(OUTPUT_DRAIN_GRACE, async {
        stdout
            .await
            .context("joining Firecracker stdout bridge")??;
        stderr
            .await
            .context("joining Firecracker stderr bridge")??;
        Result::<()>::Ok(())
    })
    .await
    {
        Ok(result) => result?,
        Err(_) => {
            // A background guest child may retain inherited descriptors after the
            // requested process exits. The caller already has the terminal status.
        }
    }
    Ok(exit_code)
}

pub async fn write_frame<T: Serialize>(
    writer: &mut (impl tokio::io::AsyncWrite + Unpin),
    value: &T,
) -> Result<()> {
    let payload = serde_json::to_vec(value)?;
    if payload.len() > MAX_BRIDGE_FRAME_BYTES {
        bail!("Firecracker bridge request exceeds {MAX_BRIDGE_FRAME_BYTES} bytes");
    }
    writer.write_u64(payload.len() as u64).await?;
    writer.write_all(&payload).await?;
    writer.flush().await?;
    Ok(())
}

pub async fn read_frame<T: DeserializeOwned>(
    reader: &mut (impl tokio::io::AsyncRead + Unpin),
) -> Result<T> {
    let length = reader.read_u64().await?;
    let length = usize::try_from(length).context("Firecracker bridge frame length overflows")?;
    if length > MAX_BRIDGE_FRAME_BYTES {
        bail!("Firecracker bridge response exceeds {MAX_BRIDGE_FRAME_BYTES} bytes");
    }
    let mut payload = vec![0; length];
    reader.read_exact(&mut payload).await?;
    serde_json::from_slice(&payload).map_err(|error| anyhow!(error))
}
