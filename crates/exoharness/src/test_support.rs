use std::path::PathBuf;

use crate::{
    BasicExoHarnessConfig, DaytonaBackendSpec, SandboxBackendRegistration, SandboxProvider,
    SecretBackendChoice,
};

pub(crate) fn local_test_config(root: impl Into<PathBuf>) -> BasicExoHarnessConfig {
    BasicExoHarnessConfig {
        root: root.into(),
        secret_backend: SecretBackendChoice::Static([7u8; 32]),
        sandbox_default: SandboxProvider::LocalProcess,
        sandbox_backends: vec![SandboxBackendRegistration::local_process()],
        groundhog: None,
    }
}

/// Like [`local_test_config`] but also advertises Daytona, so tests can exercise
/// lazy secret resolution. Daytona credentials are still read from the secret
/// store on first use.
pub(crate) fn local_test_config_with_daytona(root: impl Into<PathBuf>) -> BasicExoHarnessConfig {
    BasicExoHarnessConfig {
        root: root.into(),
        secret_backend: SecretBackendChoice::Static([7u8; 32]),
        sandbox_default: SandboxProvider::LocalProcess,
        sandbox_backends: vec![
            SandboxBackendRegistration::local_process(),
            SandboxBackendRegistration::daytona(DaytonaBackendSpec::default()),
        ],
        groundhog: None,
    }
}
