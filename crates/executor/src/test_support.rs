use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use exoharness::{
    BasicExoHarnessConfig, SandboxBackendRegistration, SandboxProvider, SecretBackendChoice,
};

pub(crate) fn local_test_config(root: impl Into<PathBuf>) -> BasicExoHarnessConfig {
    BasicExoHarnessConfig {
        root: root.into(),
        secret_backend: SecretBackendChoice::Static([7u8; 32]),
        sandbox_default: SandboxProvider::LocalProcess,
        sandbox_backends: vec![SandboxBackendRegistration::local_process()],
    }
}

/// Sets a file's mtime `by` into the past, the way a clock would have left a
/// file written that long ago.
pub(crate) fn backdate_file(path: &Path, by: Duration) {
    std::fs::File::options()
        .write(true)
        .open(path)
        .expect("backdated file exists")
        .set_modified(SystemTime::now() - by)
        .expect("set mtime");
}
