//! Per-provider [`crate::sandbox::ManagedSandboxBackend`] implementations,
//! selected via the harness's provider registry.
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
#[cfg(all(not(target_arch = "wasm32"), feature = "basic-backend"))]
pub mod process_bridge;
#[cfg(all(not(target_arch = "wasm32"), feature = "basic-backend"))]
mod sprites;
#[cfg(all(not(target_arch = "wasm32"), feature = "basic-backend"))]
mod tensorlake;
#[cfg(not(all(not(target_arch = "wasm32"), feature = "basic-backend")))]
mod tensorlake {
    pub fn default_tensorlake_image() -> String {
        "tensorlake/ubuntu-minimal".to_string()
    }
}
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
#[cfg(all(not(target_arch = "wasm32"), feature = "basic-backend"))]
pub use sprites::{DEFAULT_SPRITES_API_URL, SpritesConfig, SpritesSandboxBackend};
pub use tensorlake::default_tensorlake_image;
#[cfg(all(not(target_arch = "wasm32"), feature = "basic-backend"))]
pub use tensorlake::{
    DEFAULT_TENSORLAKE_API_URL, DEFAULT_TENSORLAKE_IMAGE, TensorlakeConfig,
    TensorlakeSandboxBackend,
};
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
