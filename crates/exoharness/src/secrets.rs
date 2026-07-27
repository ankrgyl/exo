use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use anyhow::{Context, anyhow, bail};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{Result, Secret};

const MASTER_KEY_LEN: usize = 32;
const NONCE_LEN: usize = 12;
#[cfg(feature = "apple-keychain")]
const KEYCHAIN_SERVICE: &str = "exo-exoharness-master-key";
const MASTER_KEY_FILE_PERMS: u32 = 0o600;
const MASTER_KEY_DIR_PERMS: u32 = 0o700;

#[derive(Clone)]
pub(crate) struct SecretCipher {
    key_provider: Arc<dyn SecretKeyProvider>,
}

impl SecretCipher {
    pub(crate) fn new(key_provider: Arc<dyn SecretKeyProvider>) -> Self {
        Self { key_provider }
    }

    pub(crate) fn encrypt_secret(&self, secret: &Secret) -> Result<EncryptedSecret> {
        let key = self.key_provider.get_or_create_key()?;
        let payload = serde_json::to_vec(secret)?;
        let cipher = Aes256Gcm::new_from_slice(&key)
            .map_err(|_| anyhow!("invalid secret encryption key length"))?;
        let nonce = random_nonce();
        let nonce = Nonce::from(nonce);
        let ciphertext = cipher
            .encrypt(&nonce, payload.as_slice())
            .context("failed to encrypt secret payload")?;
        Ok(EncryptedSecret {
            algorithm: SecretEncryptionAlgorithm::Aes256Gcm,
            nonce: nonce.to_vec(),
            ciphertext,
        })
    }

    pub(crate) fn decrypt_secret(&self, encrypted: &EncryptedSecret) -> Result<Secret> {
        match encrypted.algorithm {
            SecretEncryptionAlgorithm::Aes256Gcm => {}
        }
        if encrypted.nonce.len() != NONCE_LEN {
            bail!("invalid secret nonce length");
        }
        let nonce: [u8; NONCE_LEN] = encrypted
            .nonce
            .clone()
            .try_into()
            .map_err(|_| anyhow!("invalid secret nonce length"))?;
        let key = self.key_provider.get_or_create_key()?;
        let cipher = Aes256Gcm::new_from_slice(&key)
            .map_err(|_| anyhow!("invalid secret encryption key length"))?;
        let plaintext = cipher
            .decrypt(&Nonce::from(nonce), encrypted.ciphertext.as_slice())
            .context("failed to decrypt secret payload")?;
        serde_json::from_slice(&plaintext).map_err(Into::into)
    }

    pub(crate) fn verify_key_access(&self) -> Result<()> {
        if self.key_provider.get_key_if_exists()?.is_none() {
            bail!("secret master key does not exist");
        }
        Ok(())
    }
}

pub(crate) trait SecretKeyProvider: Send + Sync {
    fn get_key_if_exists(&self) -> Result<Option<[u8; MASTER_KEY_LEN]>>;
    fn get_or_create_key(&self) -> Result<[u8; MASTER_KEY_LEN]>;
}

pub(crate) struct StaticSecretKeyProvider {
    key: [u8; MASTER_KEY_LEN],
}

impl StaticSecretKeyProvider {
    pub(crate) fn new(key: [u8; MASTER_KEY_LEN]) -> Self {
        Self { key }
    }
}

impl SecretKeyProvider for StaticSecretKeyProvider {
    fn get_key_if_exists(&self) -> Result<Option<[u8; MASTER_KEY_LEN]>> {
        Ok(Some(self.key))
    }

    fn get_or_create_key(&self) -> Result<[u8; MASTER_KEY_LEN]> {
        Ok(self.key)
    }
}

#[cfg(feature = "apple-keychain")]
pub(crate) struct AppleKeychainSecretKeyProvider {
    account: String,
    key: OnceLock<[u8; MASTER_KEY_LEN]>,
}

#[cfg(feature = "apple-keychain")]
impl AppleKeychainSecretKeyProvider {
    pub(crate) fn new(account: String) -> Self {
        Self {
            account,
            key: OnceLock::new(),
        }
    }
}

#[cfg(feature = "apple-keychain")]
impl SecretKeyProvider for AppleKeychainSecretKeyProvider {
    fn get_key_if_exists(&self) -> Result<Option<[u8; MASTER_KEY_LEN]>> {
        use keyring_core::{Entry, Error as KeyringError};

        if let Some(key) = self.key.get() {
            return Ok(Some(*key));
        }
        ensure_apple_keychain_store()?;
        let entry = Entry::new(KEYCHAIN_SERVICE, &self.account)?;
        match entry.get_password() {
            Ok(serialized) => Ok(Some(cache_key(&self.key, deserialize_key(&serialized)?))),
            Err(KeyringError::NoEntry) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    fn get_or_create_key(&self) -> Result<[u8; MASTER_KEY_LEN]> {
        use keyring_core::Entry;

        if let Some(key) = self.get_key_if_exists()? {
            return Ok(key);
        }
        let key = random_master_key();
        let entry = Entry::new(KEYCHAIN_SERVICE, &self.account)?;
        entry
            .set_password(&serde_json::to_string(&key.to_vec())?)
            .context("failed to persist namespaced exoharness master key in keychain")?;
        Ok(cache_key(&self.key, key))
    }
}

#[cfg(feature = "apple-keychain")]
pub(crate) fn migrate_apple_keychain_master_key(account: &str, legacy_account: &str) -> Result<()> {
    use keyring_core::Entry;

    ensure_apple_keychain_store()?;
    let entry = Entry::new(KEYCHAIN_SERVICE, account)?;
    let legacy_entry = Entry::new(KEYCHAIN_SERVICE, legacy_account)?;
    migrate_keychain_master_key_entries(&entry, &legacy_entry, |serialized| {
        entry.set_password(serialized).map_err(Into::into)
    })
}

#[cfg(feature = "apple-keychain")]
fn migrate_keychain_master_key_entries(
    entry: &keyring_core::Entry,
    legacy_entry: &keyring_core::Entry,
    set_password: impl FnOnce(&str) -> Result<()>,
) -> Result<()> {
    use keyring_core::Error as KeyringError;

    match entry.get_password() {
        Ok(_) => return Ok(()),
        Err(KeyringError::NoEntry) => {}
        Err(error) => return Err(error.into()),
    }

    let serialized = match legacy_entry.get_password() {
        Ok(serialized) => serialized,
        Err(KeyringError::NoEntry) => bail!(
            "existing encrypted secrets require the legacy keychain master key, but it was not found"
        ),
        Err(error) => return Err(error.into()),
    };
    deserialize_key(&serialized)?;
    set_password(&serialized)
        .context("failed to migrate exoharness master key to its namespaced keychain account")
}

#[cfg(all(test, feature = "apple-keychain"))]
mod keychain_migration_tests {
    use keyring_core::api::CredentialStoreApi;
    use keyring_core::mock;

    use super::{KEYCHAIN_SERVICE, migrate_keychain_master_key_entries};

    #[test]
    fn migration_copies_the_legacy_master_key() {
        let store = mock::Store::new().expect("mock keychain");
        let current = store
            .build(KEYCHAIN_SERVICE, "current", None)
            .expect("current entry");
        let legacy = store
            .build(KEYCHAIN_SERVICE, "legacy", None)
            .expect("legacy entry");
        let serialized = serde_json::to_string(&vec![7u8; 32]).expect("serialize key");
        legacy.set_password(&serialized).expect("legacy key");

        migrate_keychain_master_key_entries(&current, &legacy, |serialized| {
            current.set_password(serialized).map_err(Into::into)
        })
        .expect("migrate key");

        assert_eq!(current.get_password().expect("current key"), serialized);
    }

    #[test]
    fn migration_does_not_replace_an_existing_namespaced_key() {
        let store = mock::Store::new().expect("mock keychain");
        let current = store
            .build(KEYCHAIN_SERVICE, "current", None)
            .expect("current entry");
        let legacy = store
            .build(KEYCHAIN_SERVICE, "legacy", None)
            .expect("legacy entry");
        let current_serialized = serde_json::to_string(&vec![3u8; 32]).expect("serialize current");
        let legacy_serialized = serde_json::to_string(&vec![7u8; 32]).expect("serialize legacy");
        current
            .set_password(&current_serialized)
            .expect("current key");
        legacy.set_password(&legacy_serialized).expect("legacy key");

        migrate_keychain_master_key_entries(&current, &legacy, |serialized| {
            current.set_password(serialized).map_err(Into::into)
        })
        .expect("check migration");

        assert_eq!(
            current.get_password().expect("preserved current key"),
            current_serialized
        );
    }
}

pub(crate) struct FileBackedSecretKeyProvider {
    path: PathBuf,
    key: OnceLock<[u8; MASTER_KEY_LEN]>,
}

impl FileBackedSecretKeyProvider {
    pub(crate) fn new(path: PathBuf) -> Self {
        Self {
            path,
            key: OnceLock::new(),
        }
    }
}

impl SecretKeyProvider for FileBackedSecretKeyProvider {
    fn get_key_if_exists(&self) -> Result<Option<[u8; MASTER_KEY_LEN]>> {
        if let Some(key) = self.key.get() {
            return Ok(Some(*key));
        }
        match std::fs::read(&self.path) {
            Ok(bytes) => Ok(Some(cache_key(
                &self.key,
                parse_master_key_bytes(&bytes)
                    .with_context(|| format!("reading master key at {}", self.path.display()))?,
            ))),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(anyhow::Error::from(error)
                .context(format!("reading master key at {}", self.path.display()))),
        }
    }

    fn get_or_create_key(&self) -> Result<[u8; MASTER_KEY_LEN]> {
        if let Some(key) = self.get_key_if_exists()? {
            return Ok(key);
        }
        let key = random_master_key();
        write_master_key_file(&self.path, &key)?;
        Ok(cache_key(&self.key, key))
    }
}

#[cfg(test)]
mod key_provider_tests {
    use std::sync::Arc;

    use tempfile::TempDir;

    use super::{FileBackedSecretKeyProvider, SecretCipher};

    #[test]
    fn verifying_key_access_does_not_create_a_missing_key() {
        let tempdir = TempDir::new().expect("tempdir");
        let key_path = tempdir.path().join("master.key");
        let cipher =
            SecretCipher::new(Arc::new(FileBackedSecretKeyProvider::new(key_path.clone())));

        let error = cipher
            .verify_key_access()
            .expect_err("missing key should fail verification");

        assert!(error.to_string().contains("master key does not exist"));
        assert!(!key_path.exists());
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct EncryptedSecret {
    pub(crate) algorithm: SecretEncryptionAlgorithm,
    pub(crate) nonce: Vec<u8>,
    pub(crate) ciphertext: Vec<u8>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SecretEncryptionAlgorithm {
    Aes256Gcm,
}

#[cfg(feature = "apple-keychain")]
fn deserialize_key(serialized: &str) -> Result<[u8; MASTER_KEY_LEN]> {
    let bytes: Vec<u8> = serde_json::from_str(serialized)?;
    if bytes.len() != MASTER_KEY_LEN {
        bail!("invalid secret master key length");
    }
    let mut key = [0u8; MASTER_KEY_LEN];
    key.copy_from_slice(&bytes);
    Ok(key)
}

fn random_master_key() -> [u8; MASTER_KEY_LEN] {
    let mut key = [0u8; MASTER_KEY_LEN];
    key[..16].copy_from_slice(Uuid::new_v4().as_bytes());
    key[16..].copy_from_slice(Uuid::new_v4().as_bytes());
    key
}

fn cache_key(
    cache: &OnceLock<[u8; MASTER_KEY_LEN]>,
    key: [u8; MASTER_KEY_LEN],
) -> [u8; MASTER_KEY_LEN] {
    match cache.set(key) {
        Ok(()) => key,
        Err(key) => cache.get().copied().unwrap_or(key),
    }
}

fn random_nonce() -> [u8; NONCE_LEN] {
    let mut nonce = [0u8; NONCE_LEN];
    nonce.copy_from_slice(&Uuid::new_v4().as_bytes()[..NONCE_LEN]);
    nonce
}

#[cfg(feature = "apple-keychain")]
fn ensure_apple_keychain_store() -> Result<()> {
    use std::collections::HashMap;

    static INIT: OnceLock<std::result::Result<(), String>> = OnceLock::new();
    let result = INIT.get_or_init(|| {
        keyring::use_apple_keychain_store(&HashMap::new())
            .map_err(|error| format!("failed to initialize macOS keychain store: {error}"))
    });
    match result {
        Ok(()) => Ok(()),
        Err(message) => Err(anyhow!(message.clone())),
    }
}

fn parse_master_key_bytes(bytes: &[u8]) -> Result<[u8; MASTER_KEY_LEN]> {
    if bytes.len() != MASTER_KEY_LEN {
        bail!(
            "invalid master key length: expected {MASTER_KEY_LEN}, got {}",
            bytes.len()
        );
    }
    let mut key = [0u8; MASTER_KEY_LEN];
    key.copy_from_slice(bytes);
    Ok(key)
}

fn write_master_key_file(path: &Path, key: &[u8; MASTER_KEY_LEN]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating master key directory {}", parent.display()))?;
        std::fs::set_permissions(
            parent,
            std::fs::Permissions::from_mode(MASTER_KEY_DIR_PERMS),
        )
        .with_context(|| format!("setting permissions on {}", parent.display()))?;
    }

    let tmp_path = path.with_extension("tmp");
    let _ = std::fs::remove_file(&tmp_path);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(MASTER_KEY_FILE_PERMS)
        .open(&tmp_path)
        .with_context(|| format!("creating master key file {}", tmp_path.display()))?;
    file.write_all(key)
        .with_context(|| format!("writing master key file {}", tmp_path.display()))?;
    file.sync_all()
        .with_context(|| format!("syncing master key file {}", tmp_path.display()))?;
    drop(file);
    std::fs::rename(&tmp_path, path)
        .with_context(|| format!("renaming master key into place at {}", path.display()))?;
    Ok(())
}

pub(crate) fn default_master_key_path() -> Result<PathBuf> {
    if let Some(value) = std::env::var_os("XDG_CONFIG_HOME") {
        let path = PathBuf::from(value);
        if !path.as_os_str().is_empty() {
            return Ok(path.join("exo").join("master.key"));
        }
    }
    if let Some(value) = std::env::var_os("HOME") {
        let path = PathBuf::from(value);
        if !path.as_os_str().is_empty() {
            return Ok(path.join(".config").join("exo").join("master.key"));
        }
    }
    bail!("could not determine config directory: set XDG_CONFIG_HOME or HOME")
}
