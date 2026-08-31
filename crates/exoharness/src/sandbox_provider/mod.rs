//! Per-provider [`crate::sandbox::ManagedSandboxBackend`] implementations,
//! selected via the harness's provider registry.
#[cfg(all(not(target_arch = "wasm32"), feature = "firecracker"))]
use std::{path::PathBuf, sync::Arc};
#[cfg(all(not(target_arch = "wasm32"), feature = "firecracker"))]
type FirecrackerBackend = Arc<dyn crate::ManagedSandboxBackend>;

const DEFAULT_FIRECRACKER_IMAGE: &str = "/var/lib/exo/firecracker/rootfs.ext4";

pub fn default_firecracker_image() -> String {
    DEFAULT_FIRECRACKER_IMAGE.to_string()
}

mod docker;

#[cfg(all(not(target_arch = "wasm32"), feature = "basic-backend"))]
mod daytona;
#[cfg(not(all(not(target_arch = "wasm32"), feature = "basic-backend")))]
mod daytona {
    pub fn default_daytona_image() -> String {
        "daytonaio/sandbox:0.8.0".to_string()
    }
}
#[cfg(all(
    not(target_arch = "wasm32"),
    feature = "basic-backend",
    feature = "aws-agentcore"
))]
mod aws_agentcore;
#[cfg(not(all(
    not(target_arch = "wasm32"),
    feature = "basic-backend",
    feature = "aws-agentcore"
)))]
mod aws_agentcore {
    pub fn default_aws_agentcore_image() -> String {
        String::new()
    }
}
#[cfg(all(not(target_arch = "wasm32"), feature = "basic-backend"))]
mod e2b;
#[cfg(all(not(target_arch = "wasm32"), feature = "firecracker"))]
mod firecracker;
#[cfg(all(not(target_arch = "wasm32"), feature = "firecracker"))]
mod firecracker_bridge;
#[cfg(all(not(target_arch = "wasm32"), feature = "firecracker"))]
mod firecracker_image;
#[cfg(all(
    target_os = "macos",
    not(target_arch = "wasm32"),
    feature = "firecracker"
))]
mod firecracker_lima;
#[cfg(all(not(target_arch = "wasm32"), feature = "basic-backend"))]
pub mod process_bridge;
#[cfg(all(not(target_arch = "wasm32"), feature = "basic-backend"))]
mod smolvm;
#[cfg(all(not(target_arch = "wasm32"), feature = "basic-backend"))]
mod sprites;
#[cfg(all(not(target_arch = "wasm32"), feature = "basic-backend"))]
mod vercel;
#[cfg(not(all(not(target_arch = "wasm32"), feature = "basic-backend")))]
mod vercel {
    pub fn default_vercel_image() -> String {
        "node24".to_string()
    }
}

pub use aws_agentcore::default_aws_agentcore_image;
#[cfg(all(
    not(target_arch = "wasm32"),
    feature = "basic-backend",
    feature = "aws-agentcore"
))]
pub use aws_agentcore::{AwsAgentCoreConfig, AwsAgentCoreCredentials, AwsAgentCoreSandboxBackend};
pub use daytona::default_daytona_image;
#[cfg(all(not(target_arch = "wasm32"), feature = "basic-backend"))]
pub use daytona::{
    DEFAULT_DAYTONA_API_URL, DEFAULT_DAYTONA_TOOLBOX_URL, DaytonaConfig, DaytonaSandboxBackend,
};
#[cfg(all(not(target_arch = "wasm32"), feature = "basic-backend"))]
pub(crate) use docker::DEFAULT_DOCKER_IMAGE;
pub use docker::default_docker_image;
#[cfg(all(not(target_arch = "wasm32"), feature = "basic-backend"))]
pub use e2b::{DEFAULT_E2B_API_URL, DEFAULT_E2B_ENVD_PORT, E2bConfig, E2bSandboxBackend};
#[cfg(all(not(target_arch = "wasm32"), feature = "firecracker"))]
pub use firecracker::{
    DEFAULT_FIRECRACKER_BINARY, DEFAULT_FIRECRACKER_INITRAMFS, DEFAULT_FIRECRACKER_JAILER,
    DEFAULT_FIRECRACKER_KERNEL, DEFAULT_FIRECRACKER_STATE_ROOT, DEFAULT_IMAGE_SIZE_GIB,
    DEFAULT_JAILER_UID_BASE, DEFAULT_MEMORY_MIB, DEFAULT_NETWORK_BYTES_PER_SECOND,
    DEFAULT_VCPU_COUNT, DEFAULT_WORKSPACE_SIZE_GIB, FirecrackerConfig,
    FirecrackerNetworkDevicePolicy, FirecrackerSandboxBackend,
};
#[cfg(all(not(target_arch = "wasm32"), feature = "firecracker"))]
pub use firecracker_bridge::run_firecracker_bridge;

#[cfg(all(not(target_arch = "wasm32"), feature = "firecracker"))]
#[derive(Debug, Clone)]
pub struct FirecrackerLimaConfig {
    pub limactl: PathBuf,
    pub instance: String,
    pub target_dir: PathBuf,
    /// Prebuilt bridge binary inside the Lima VM. When absent, Exo builds and
    /// installs the bridge from the current checkout.
    pub bridge_binary: Option<PathBuf>,
}

#[cfg(all(not(target_arch = "wasm32"), feature = "firecracker"))]
impl Default for FirecrackerLimaConfig {
    fn default() -> Self {
        Self {
            limactl: PathBuf::from("limactl"),
            instance: "exo-firecracker".to_string(),
            target_dir: PathBuf::from("/var/tmp/exo-firecracker-bridge-target"),
            bridge_binary: None,
        }
    }
}

#[cfg(all(not(target_arch = "wasm32"), feature = "firecracker"))]
pub async fn firecracker_backend(
    config: FirecrackerConfig,
    lima: FirecrackerLimaConfig,
) -> anyhow::Result<FirecrackerBackend> {
    #[cfg(target_os = "linux")]
    {
        drop(lima);
        Ok(Arc::new(FirecrackerSandboxBackend::new(config).await?))
    }
    #[cfg(target_os = "macos")]
    {
        Ok(Arc::new(
            firecracker_lima::LimaFirecrackerSandboxBackend::new(config, lima).await?,
        ))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        drop(config);
        drop(lima);
        anyhow::bail!("Firecracker sandbox execution is only supported on Linux or macOS with Lima")
    }
}

#[cfg(all(test, not(target_arch = "wasm32"), feature = "firecracker"))]
pub(crate) async fn firecracker_backend_for_test(
    config: FirecrackerConfig,
) -> anyhow::Result<std::sync::Arc<dyn crate::ManagedSandboxBackend>> {
    firecracker_backend(config, FirecrackerLimaConfig::default()).await
}
#[cfg(all(not(target_arch = "wasm32"), feature = "basic-backend"))]
// `default_smolvm_image` deliberately lives in `types`, not here: it must be
// callable without the `basic-backend` feature this module is gated on, and
// re-exporting a second copy made `exoharness::default_smolvm_image` ambiguous.
pub use smolvm::{SmolvmBackendConfig, SmolvmExecutionMode, SmolvmSandboxBackend};
#[cfg(all(not(target_arch = "wasm32"), feature = "basic-backend"))]
pub use sprites::{DEFAULT_SPRITES_API_URL, SpritesConfig, SpritesSandboxBackend};
pub use vercel::default_vercel_image;
#[cfg(all(not(target_arch = "wasm32"), feature = "basic-backend"))]
pub use vercel::{DEFAULT_VERCEL_API_URL, VercelConfig, VercelSandboxBackend};

#[cfg(all(not(target_arch = "wasm32"), feature = "basic-backend"))]
fn shell_quote(arg: &str) -> String {
    if !arg.is_empty()
        && arg.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | ':' | '=' | ',')
        })
    {
        return arg.to_string();
    }
    let mut quoted = String::with_capacity(arg.len() + 2);
    quoted.push('\'');
    for c in arg.chars() {
        if c == '\'' {
            quoted.push_str("'\\''");
        } else {
            quoted.push(c);
        }
    }
    quoted.push('\'');
    quoted
}
