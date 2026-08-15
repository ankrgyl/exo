//! Per-provider [`crate::sandbox::ManagedSandboxBackend`] implementations,
//! selected via the harness's provider registry.
#[cfg(all(not(target_arch = "wasm32"), feature = "firecracker"))]
use std::sync::Arc;
#[cfg(all(not(target_arch = "wasm32"), feature = "firecracker"))]
type FirecrackerBackend = Arc<dyn crate::ManagedSandboxBackend>;

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
#[cfg(not(all(not(target_arch = "wasm32"), feature = "firecracker")))]
mod firecracker {
    pub fn default_firecracker_image() -> String {
        "/var/lib/exo/firecracker/rootfs.ext4".to_string()
    }
}
#[cfg(all(not(target_arch = "wasm32"), feature = "basic-backend"))]
pub mod process_bridge;
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
pub use firecracker::default_firecracker_image;
#[cfg(all(not(target_arch = "wasm32"), feature = "firecracker"))]
pub use firecracker::{FirecrackerConfig, FirecrackerSandboxBackend};
#[cfg(all(not(target_arch = "wasm32"), feature = "firecracker"))]
pub use firecracker_bridge::run_firecracker_bridge;

#[cfg(all(not(target_arch = "wasm32"), feature = "firecracker"))]
pub async fn firecracker_backend_from_env() -> anyhow::Result<FirecrackerBackend> {
    firecracker_backend_from_config(FirecrackerConfig::from_env()?).await
}

#[cfg(all(not(target_arch = "wasm32"), feature = "firecracker"))]
async fn firecracker_backend_from_config(
    config: FirecrackerConfig,
) -> anyhow::Result<FirecrackerBackend> {
    #[cfg(target_os = "linux")]
    {
        Ok(Arc::new(FirecrackerSandboxBackend::new(config)?))
    }
    #[cfg(target_os = "macos")]
    {
        Ok(Arc::new(
            firecracker_lima::LimaFirecrackerSandboxBackend::from_env(config).await?,
        ))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        drop(config);
        anyhow::bail!("Firecracker sandbox execution is only supported on Linux or macOS with Lima")
    }
}

#[cfg(all(test, not(target_arch = "wasm32"), feature = "firecracker"))]
pub(crate) async fn firecracker_backend_for_test(
    config: FirecrackerConfig,
) -> anyhow::Result<std::sync::Arc<dyn crate::ManagedSandboxBackend>> {
    firecracker_backend_from_config(config).await
}
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
