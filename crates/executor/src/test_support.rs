use std::path::PathBuf;

use exoharness::{
    BasicExoHarnessConfig, SandboxBackendRegistration, SandboxProvider, SecretBackendChoice,
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
