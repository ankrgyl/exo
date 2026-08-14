//! macOS transport for the Linux Firecracker backend.
//!
//! Firecracker only runs on Linux/KVM. On Apple silicon that supports nested
//! virtualization, Lima can provide the Linux/KVM boundary while the Exo caller
//! remains a native macOS process:
//! https://developer.apple.com/documentation/virtualization/vzgenericplatformconfiguration/isnestedvirtualizationsupported
//! https://github.com/lima-vm/lima/blob/master/templates/default.yaml

use std::env;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;
use std::sync::Arc;
use std::task::{Context, Poll};

use anyhow::{Context as AnyhowContext, Result, anyhow, bail};
use async_trait::async_trait;
use serde::Deserialize;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use crate::sandbox::{
    BoxSandboxTcpStream, ManagedSandboxBackend, ManagedSandboxHandle, SandboxCommand,
    SandboxCommandOutput, SandboxRequest, SnapshotPayload,
};
use crate::{SandboxAttachment, SandboxProcessParts};

use super::firecracker::FirecrackerConfig;
use super::firecracker_bridge::{
    FirecrackerBridgeRequest, FirecrackerBridgeResponse, read_frame, write_frame,
};

const DEFAULT_LIMA_INSTANCE: &str = "exo-firecracker";
const DEFAULT_LIMA_TARGET_DIR: &str = "/var/tmp/exo-firecracker-bridge-target";

pub struct LimaFirecrackerSandboxBackend {
    config: FirecrackerConfig,
    limactl: PathBuf,
    instance: String,
    bridge_binary: PathBuf,
}

impl LimaFirecrackerSandboxBackend {
    pub async fn from_env(mut config: FirecrackerConfig) -> Result<Self> {
        if env::var_os("EXO_FIRECRACKER_MEMORY_MIB").is_none() {
            config.memory_mib = 1024;
        }
        let limactl = env_path("EXO_FIRECRACKER_LIMACTL", "limactl");
        let instance = env::var("EXO_FIRECRACKER_LIMA_INSTANCE")
            .unwrap_or_else(|_| DEFAULT_LIMA_INSTANCE.to_string());
        let target_dir = env_path("EXO_FIRECRACKER_LIMA_TARGET_DIR", DEFAULT_LIMA_TARGET_DIR);
        let bridge_binary = env::var_os("EXO_FIRECRACKER_LIMA_EXO_BINARY")
            .map(PathBuf::from)
            .unwrap_or_else(|| target_dir.join("debug/exo"));
        let backend = Self {
            config,
            limactl,
            instance,
            bridge_binary,
        };
        backend.prepare_bridge(&target_dir).await?;
        Ok(backend)
    }

    async fn prepare_bridge(&self, target_dir: &Path) -> Result<()> {
        self.run_checked(
            Command::new(&self.limactl).arg("start").arg(&self.instance),
            "starting the Firecracker Lima VM",
        )
        .await?;
        if env::var_os("EXO_FIRECRACKER_LIMA_EXO_BINARY").is_some() {
            return Ok(());
        }
        let source_root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .context("resolving the Exo source root")?;
        let mut command = Command::new(&self.limactl);
        // Keep the one-time Linux build viable in the 4 GiB development VM:
        // GNU ld retained multiple GiB while linking Exo, whereas LLVM lld is
        // designed for lower-memory parallel linking. Debug info and incremental
        // state are not useful for this executable-only bridge cache.
        // https://lld.llvm.org/
        command
            .arg("shell")
            .arg(&self.instance)
            .arg("--")
            .arg("env")
            .arg(format!("CARGO_TARGET_DIR={}", target_dir.display()))
            .arg("CARGO_BUILD_JOBS=2")
            .arg("CARGO_INCREMENTAL=0")
            .arg("CARGO_PROFILE_DEV_DEBUG=0")
            .arg("RUSTFLAGS=-C linker=clang -C link-arg=-fuse-ld=lld")
            .arg("cargo")
            .arg("build")
            .arg("--manifest-path")
            .arg(source_root.join("Cargo.toml"))
            .arg("--package")
            .arg("exo")
            .arg("--features")
            .arg("firecracker")
            .arg("--bin")
            .arg("exo");
        self.run_checked(&mut command, "building the Exo Firecracker bridge in Lima")
            .await
    }

    async fn run_checked(&self, command: &mut Command, description: &str) -> Result<()> {
        let output = command.output().await?;
        if output.status.success() {
            return Ok(());
        }
        bail!(
            "{description} failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )
    }

    fn bridge_command(&self) -> Command {
        let mut command = Command::new(&self.limactl);
        // The direct backend deliberately uses Firecracker's jailer and host
        // network setup, both of which require root inside the Lima VM. `-n`
        // prevents an invisible password prompt from hanging the host process.
        // https://github.com/firecracker-microvm/firecracker/blob/main/docs/prod-host-setup.md
        command
            .arg("shell")
            .arg(&self.instance)
            .arg("--")
            .arg("sudo")
            .arg("-n")
            .arg(&self.bridge_binary)
            .arg("firecracker-bridge")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    async fn request(
        &self,
        request: &FirecrackerBridgeRequest,
    ) -> Result<FirecrackerBridgeResponse> {
        let mut child = self.bridge_command().spawn()?;
        let mut stdin = child
            .stdin
            .take()
            .context("opening Firecracker bridge stdin")?;
        write_frame(&mut stdin, request).await?;
        stdin.shutdown().await?;
        let output = child.wait_with_output().await?;
        if !output.status.success() {
            bail!(
                "Firecracker Lima bridge failed ({}): {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        let mut stdout = output.stdout.as_slice();
        read_frame(&mut stdout).await
    }

    async fn start_process(
        &self,
        request: FirecrackerBridgeRequest,
    ) -> Result<SandboxProcessParts> {
        let mut child = self.bridge_command().spawn()?;
        let mut stdin = child
            .stdin
            .take()
            .context("opening Firecracker bridge stdin")?;
        write_frame(&mut stdin, &request).await?;
        let stdout = child
            .stdout
            .take()
            .context("opening Firecracker bridge stdout")?;
        let stderr = child
            .stderr
            .take()
            .context("opening Firecracker bridge stderr")?;
        let wait = Box::pin(async move {
            let status = child.wait().await?;
            status
                .code()
                .ok_or_else(|| anyhow!("Firecracker Lima bridge exited without a status code"))
        });
        Ok(SandboxProcessParts {
            stdout: Box::pin(stdout.compat()),
            stderr: Box::pin(stderr.compat()),
            stdin: Box::pin(stdin.compat_write()),
            wait,
        })
    }
}

#[async_trait]
impl ManagedSandboxBackend for LimaFirecrackerSandboxBackend {
    fn is_local(&self) -> bool {
        true
    }

    async fn acquire(&self, request: SandboxRequest) -> Result<Arc<dyn ManagedSandboxHandle>> {
        let response = self
            .request(&FirecrackerBridgeRequest::Acquire {
                config: self.config.clone(),
                request: request.clone(),
            })
            .await?;
        let FirecrackerBridgeResponse::Acquired {
            id,
            provider_state,
            effective_image,
        } = response
        else {
            bail!("Firecracker Lima bridge returned the wrong response to acquire");
        };
        Ok(Arc::new(LimaFirecrackerSandboxHandle {
            id,
            provider_state,
            effective_image,
            request,
            backend: Arc::new(Self {
                config: self.config.clone(),
                limactl: self.limactl.clone(),
                instance: self.instance.clone(),
                bridge_binary: self.bridge_binary.clone(),
            }),
        }))
    }

    async fn attach(
        &self,
        _request: SandboxRequest,
        _attachment: SandboxAttachment,
    ) -> Result<Arc<dyn ManagedSandboxHandle>> {
        bail!("Firecracker sandboxes do not support external attachments")
    }

    async fn acquire_from_snapshot(
        &self,
        _request: SandboxRequest,
        _payload: SnapshotPayload,
    ) -> Result<Arc<dyn ManagedSandboxHandle>> {
        bail!("Firecracker sandboxes do not support restoring Exo snapshots")
    }
}

struct LimaFirecrackerSandboxHandle {
    id: String,
    provider_state: Option<serde_json::Value>,
    effective_image: Option<String>,
    request: SandboxRequest,
    backend: Arc<LimaFirecrackerSandboxBackend>,
}

#[async_trait]
impl ManagedSandboxHandle for LimaFirecrackerSandboxHandle {
    fn id(&self) -> &str {
        &self.id
    }

    fn provider_state(&self) -> Option<serde_json::Value> {
        self.provider_state.clone()
    }

    fn effective_image(&self) -> Option<String> {
        self.effective_image.clone()
    }

    async fn exec(&self, command: &SandboxCommand) -> Result<SandboxCommandOutput> {
        let response = self
            .backend
            .request(&FirecrackerBridgeRequest::Exec {
                config: self.backend.config.clone(),
                request: self.request.clone(),
                command: command.clone(),
            })
            .await?;
        let FirecrackerBridgeResponse::Exec { output } = response else {
            bail!("Firecracker Lima bridge returned the wrong response to exec");
        };
        Ok(output)
    }

    async fn start_process(&self, command: &SandboxCommand) -> Result<SandboxProcessParts> {
        self.backend
            .start_process(FirecrackerBridgeRequest::StartProcess {
                config: self.backend.config.clone(),
                request: self.request.clone(),
                command: command.clone(),
            })
            .await
    }

    async fn connect_tcp(&self, port: u16) -> Result<Option<BoxSandboxTcpStream>> {
        #[derive(Deserialize)]
        struct ProviderState {
            guest_ip: Option<Ipv4Addr>,
        }
        let state = self
            .provider_state
            .as_ref()
            .context("missing Firecracker provider state")?;
        let state: ProviderState = serde_json::from_value(state.clone())?;
        let guest_ip = state
            .guest_ip
            .context("Firecracker sandbox does not have networking enabled")?;
        let octets = guest_ip.octets();
        // Never let provider state turn this tunnel into an arbitrary connection
        // from inside Lima. This is the subnet allocated by the direct backend.
        // https://github.com/firecracker-microvm/firecracker/blob/main/docs/network-setup.md#host-network-setup
        if octets[0] != 10 || octets[1] & 0xfc != 240 {
            bail!("Firecracker bridge rejected guest address {guest_ip}");
        }
        let mut command = Command::new(&self.backend.limactl);
        command
            .arg("shell")
            .arg(&self.backend.instance)
            .arg("--")
            .arg("nc")
            .arg(guest_ip.to_string())
            .arg(port.to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn()?;
        let stdin = child
            .stdin
            .take()
            .context("opening Lima TCP bridge stdin")?;
        let stdout = child
            .stdout
            .take()
            .context("opening Lima TCP bridge stdout")?;
        if let Some(mut stderr) = child.stderr.take() {
            tokio::spawn(async move {
                let mut bytes = Vec::new();
                if let Err(error) =
                    tokio::io::AsyncReadExt::read_to_end(&mut stderr, &mut bytes).await
                {
                    tracing::debug!(%error, "failed to drain Lima TCP bridge stderr");
                }
                if !bytes.is_empty() {
                    tracing::debug!(
                        message = %String::from_utf8_lossy(&bytes).trim(),
                        "Firecracker Lima TCP bridge stderr"
                    );
                }
            });
        }
        Ok(Some(Box::pin(LimaTcpStream {
            stdin,
            stdout,
            child,
        })))
    }

    async fn stop(&self) -> Result<()> {
        match self
            .backend
            .request(&FirecrackerBridgeRequest::Stop {
                config: self.backend.config.clone(),
                request: self.request.clone(),
            })
            .await?
        {
            FirecrackerBridgeResponse::Stopped => Ok(()),
            _ => bail!("Firecracker Lima bridge returned the wrong response to stop"),
        }
    }

    async fn detach(&self) -> Result<SandboxAttachment> {
        bail!("Firecracker sandboxes cannot be detached")
    }

    async fn snapshot(&self) -> Result<SnapshotPayload> {
        bail!("Firecracker sandbox snapshots are not implemented")
    }
}

struct LimaTcpStream {
    stdin: ChildStdin,
    stdout: ChildStdout,
    child: Child,
}

impl AsyncRead for LimaTcpStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.stdout).poll_read(context, buffer)
    }
}

impl AsyncWrite for LimaTcpStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.stdin).poll_write(context, buffer)
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.stdin).poll_flush(context)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.stdin).poll_shutdown(context)
    }
}

impl Drop for LimaTcpStream {
    fn drop(&mut self) {
        if let Err(error) = self.child.start_kill() {
            tracing::debug!(%error, "failed to stop Firecracker Lima TCP bridge");
        }
    }
}

fn env_path(name: &str, default: &str) -> PathBuf {
    env::var_os(name)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(default))
}
