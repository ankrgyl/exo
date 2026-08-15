//! OCI image materialization for the Firecracker backend.
//!
//! Firecracker consumes block devices, not OCI layers. Registry images are
//! therefore resolved to an immutable platform manifest, unpacked without a
//! container daemon, converted to ext4, and cached by digest. The cache is
//! private to Exo's root-owned state directory and entries are published with
//! an atomic directory rename so a crash never exposes a partial filesystem.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs::{self, File, Permissions};
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use docker_credential::{CredentialRetrievalError, DockerCredential};
use flate2::read::MultiGzDecoder;
use oci_client::manifest::OciDescriptor;
use oci_client::secrets::RegistryAuth;
use oci_client::{Client, Reference};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::{Builder as TempBuilder, NamedTempFile};
use tokio::io::AsyncWriteExt;

const MATERIALIZER_VERSION: u32 = 4;
const EXT4_MAGIC_OFFSET: u64 = 1024 + 0x38;
const EXT4_MAGIC: [u8; 2] = [0x53, 0xef];
const GUEST_UID: u32 = 10_001;
const GUEST_GID: u32 = 10_001;

#[derive(Debug, Deserialize)]
struct OciImageConfiguration {
    architecture: String,
    os: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DockerCredentialConfiguration {
    #[serde(default)]
    cred_helpers: HashMap<String, String>,
    creds_store: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct CachedImageMetadata {
    materializer_version: u32,
    source: String,
    source_digest: String,
    manifest_digest: String,
    platform: String,
}

#[derive(Debug, Clone)]
struct CachedLayer {
    path: PathBuf,
    media_type: String,
}

#[derive(Debug, Default)]
struct PullStats {
    bytes: u64,
    cache_hits: usize,
    cache_misses: usize,
}

#[derive(Debug)]
enum Whiteout {
    Remove(PathBuf),
    Opaque(PathBuf),
}

pub(super) async fn resolve_image(
    state_root: &Path,
    source: &str,
    image_size_gib: u64,
) -> Result<PathBuf> {
    let total_started = Instant::now();
    if looks_like_local_image(source) {
        let lookup_started = Instant::now();
        let state_root = state_root.to_path_buf();
        let source_path = source.to_string();
        let (image, cache_hit) =
            tokio::task::spawn_blocking(move || cache_local_image(&state_root, &source_path))
                .await
                .context("joining local Firecracker image cache")??;
        record_step(
            source,
            None,
            "cache_lookup",
            lookup_started.elapsed(),
            cache_hit,
        );
        record_step(source, None, "total", total_started.elapsed(), cache_hit);
        return Ok(image);
    }

    let reference = source
        .parse::<Reference>()
        .with_context(|| format!("parsing Firecracker OCI image reference {source}"))?;
    let cache_root = state_root.join("images");
    prepare_private_dir(&cache_root)?;
    prepare_private_dir(&cache_root.join("blobs/sha256"))?;
    let platform = current_platform()?;

    // An OCI digest is immutable, so a completed platform cache entry can be
    // used without contacting the registry again. This keeps ECR authentication
    // and manifest lookup out of every warm VM launch.
    // https://github.com/opencontainers/distribution-spec/blob/main/spec.md#pulling-manifests
    if let Some(source_digest) = immutable_reference_digest(source)? {
        let cache_dir = cache_image_dir(&cache_root, &platform, source_digest)?;
        if cache_dir.try_exists()? {
            let lookup_started = Instant::now();
            let image = validate_cache_entry(&cache_dir, source_digest, None, &platform)?;
            record_step(
                source,
                Some(source_digest),
                "cache_lookup",
                lookup_started.elapsed(),
                true,
            );
            record_step(
                source,
                Some(source_digest),
                "total",
                total_started.elapsed(),
                true,
            );
            return Ok(image);
        }
    }

    let auth_started = Instant::now();
    let auth = registry_auth(&reference)?;
    record_step(source, None, "registry_auth", auth_started.elapsed(), false);

    let manifest_started = Instant::now();
    let client = Client::default();
    // Resolve tags through the registry API and retain the returned immutable
    // digest; tags remain only a user-facing input and never identify cache
    // entries.
    // https://github.com/opencontainers/distribution-spec/blob/main/spec.md#pulling-manifests
    let (manifest, manifest_digest, config_json, list_digest) = client
        .pull_manifest_and_config_and_list_digest(&reference, &auth)
        .await
        .with_context(|| format!("resolving OCI image manifest for {source}"))?;
    let source_digest = list_digest.unwrap_or_else(|| manifest_digest.clone());
    record_step(
        source,
        Some(&manifest_digest),
        "manifest_resolve",
        manifest_started.elapsed(),
        false,
    );

    let image_config: OciImageConfiguration = serde_json::from_str(&config_json)
        .with_context(|| format!("decoding OCI image configuration for {source}"))?;
    validate_platform(&image_config)?;
    let cache_dir = cache_image_dir(&cache_root, &platform, &source_digest)?;
    let cache_lookup_started = Instant::now();
    if cache_dir.try_exists()? {
        let image = validate_cache_entry(
            &cache_dir,
            &source_digest,
            Some(&manifest_digest),
            &platform,
        )?;
        record_step(
            source,
            Some(&manifest_digest),
            "cache_lookup",
            cache_lookup_started.elapsed(),
            true,
        );
        record_step(
            source,
            Some(&manifest_digest),
            "total",
            total_started.elapsed(),
            true,
        );
        return Ok(image);
    }
    record_step(
        source,
        Some(&manifest_digest),
        "cache_lookup",
        cache_lookup_started.elapsed(),
        false,
    );

    let pull_started = Instant::now();
    let mut pull_stats = PullStats::default();
    let mut layers = Vec::with_capacity(manifest.layers.len());
    for descriptor in &manifest.layers {
        let (path, cache_hit) = pull_blob(
            &client,
            &reference,
            descriptor,
            &cache_root.join("blobs/sha256"),
        )
        .await?;
        pull_stats.bytes = pull_stats
            .bytes
            .checked_add(u64::try_from(descriptor.size).context("negative OCI layer size")?)
            .context("OCI layer byte count overflow")?;
        if cache_hit {
            pull_stats.cache_hits += 1;
        } else {
            pull_stats.cache_misses += 1;
        }
        layers.push(CachedLayer {
            path,
            media_type: descriptor.media_type.clone(),
        });
    }
    tracing::info!(
        image = source,
        digest = manifest_digest,
        step = "blob_pull",
        duration_ms = pull_started.elapsed().as_secs_f64() * 1000.0,
        bytes = pull_stats.bytes,
        cache_hits = pull_stats.cache_hits,
        cache_misses = pull_stats.cache_misses,
        "Firecracker image materialization timing"
    );

    let metadata = CachedImageMetadata {
        materializer_version: MATERIALIZER_VERSION,
        source: source.to_string(),
        source_digest: source_digest.clone(),
        manifest_digest: manifest_digest.clone(),
        platform: platform.clone(),
    };
    let build_cache_root = cache_root.clone();
    let build_cache_dir = cache_dir.clone();
    let build_source = source.to_string();
    let build_digest = manifest_digest.clone();
    let image = tokio::task::spawn_blocking(move || {
        build_and_publish_image(
            &build_cache_root,
            &build_cache_dir,
            &build_source,
            &build_digest,
            layers,
            metadata,
            image_size_gib,
        )
    })
    .await
    .context("joining Firecracker image materialization")??;
    record_step(
        source,
        Some(&manifest_digest),
        "total",
        total_started.elapsed(),
        false,
    );
    Ok(image)
}

fn looks_like_local_image(source: &str) -> bool {
    source.starts_with('/')
        || source.starts_with("./")
        || source.starts_with("../")
        || source.ends_with(".ext4")
}

fn immutable_reference_digest(source: &str) -> Result<Option<&str>> {
    let Some((_, digest)) = source.rsplit_once('@') else {
        return Ok(None);
    };
    sha256_hex(digest)?;
    Ok(Some(digest))
}

fn cache_local_image(state_root: &Path, source: &str) -> Result<(PathBuf, bool)> {
    let source = fs::canonicalize(source)
        .with_context(|| format!("resolving Firecracker root filesystem image {source}"))?;
    validate_ext4_image(&source)?;
    let metadata = fs::metadata(&source)?;
    let identity = format!(
        "{}:{}:{}:{}:{}:{}:{}",
        metadata.dev(),
        metadata.ino(),
        metadata.len(),
        metadata.mtime(),
        metadata.mtime_nsec(),
        metadata.ctime(),
        metadata.ctime_nsec()
    );
    let digest = format!("{:x}", Sha256::digest(identity.as_bytes()));
    let local_root = state_root
        .join("images")
        .join(format!("v{MATERIALIZER_VERSION}"))
        .join("local");
    prepare_private_dir(&local_root)?;
    let cache_dir = local_root.join(digest);
    let cached = cache_dir.join("rootfs.ext4");
    if cached.try_exists()? {
        validate_ext4_image(&cached)?;
        return Ok((cached, true));
    }

    let temporary = TempBuilder::new()
        .prefix("local-image-")
        .tempdir_in(&local_root)?;
    let staged = temporary.path().join("rootfs.ext4");
    let copy = super::firecracker::trusted_host_command("cp")?;
    let status = Command::new(copy)
        .args(["--sparse=always", "--reflink=auto", "--"])
        .arg(&source)
        .arg(&staged)
        .status()
        .with_context(|| {
            format!(
                "staging local Firecracker image {} into its immutable cache",
                source.display()
            )
        })?;
    if !status.success() {
        bail!(
            "staging local Firecracker image {} into its immutable cache failed with {status}",
            source.display()
        );
    }
    fs::set_permissions(&staged, Permissions::from_mode(0o444))?;
    validate_ext4_image(&staged)?;
    let temporary_path = temporary.keep();
    match fs::rename(&temporary_path, &cache_dir) {
        Ok(()) => {}
        Err(error) if cache_dir.try_exists()? => {
            fs::remove_dir_all(&temporary_path)?;
            tracing::debug!(%error, path = %cache_dir.display(), "another process cached the local Firecracker image first");
        }
        Err(error) => {
            fs::remove_dir_all(&temporary_path)?;
            return Err(error).with_context(|| {
                format!(
                    "publishing local Firecracker image cache {}",
                    cache_dir.display()
                )
            });
        }
    }
    validate_ext4_image(&cached)?;
    Ok((cached, false))
}

fn registry_auth(reference: &Reference) -> Result<RegistryAuth> {
    let username = std::env::var("EXO_FIRECRACKER_REGISTRY_USERNAME").ok();
    let password = std::env::var("EXO_FIRECRACKER_REGISTRY_PASSWORD").ok();
    match (username, password) {
        (Some(username), Some(password)) if !username.is_empty() && !password.is_empty() => {
            return Ok(RegistryAuth::Basic(username, password));
        }
        (None, None) => {}
        _ => bail!(
            "EXO_FIRECRACKER_REGISTRY_USERNAME and EXO_FIRECRACKER_REGISTRY_PASSWORD must be set together"
        ),
    }

    let Some(config_path) = docker_config_path() else {
        return Ok(RegistryAuth::Anonymous);
    };
    match fs::metadata(&config_path) {
        Ok(metadata) if metadata.is_file() => {}
        Ok(_) => bail!(
            "Docker credential configuration is not a file: {}",
            config_path.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(RegistryAuth::Anonymous);
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "reading Docker credential configuration {}",
                    config_path.display()
                )
            });
        }
    }
    super::firecracker::validate_trusted_file("Docker credential configuration", &config_path)?;

    let server = reference.resolve_registry().trim_end_matches('/');
    validate_docker_credential_helper(&config_path, server)?;
    match docker_credential::get_credential(server) {
        Ok(DockerCredential::UsernamePassword(username, password)) => {
            Ok(RegistryAuth::Basic(username, password))
        }
        Ok(DockerCredential::IdentityToken(token)) => Ok(RegistryAuth::Bearer(token)),
        Err(CredentialRetrievalError::ConfigNotFound)
        | Err(CredentialRetrievalError::NoCredentialConfigured) => Ok(RegistryAuth::Anonymous),
        Err(error) => Err(anyhow!(error)).context("loading OCI registry credentials"),
    }
}

fn validate_docker_credential_helper(config_path: &Path, server: &str) -> Result<()> {
    let config: DockerCredentialConfiguration = serde_json::from_slice(&fs::read(config_path)?)
        .with_context(|| {
            format!(
                "decoding Docker credential configuration {}",
                config_path.display()
            )
        })?;
    let helper = config
        .cred_helpers
        .get(server)
        .filter(|helper| !helper.is_empty())
        .or_else(|| {
            config
                .creds_store
                .as_ref()
                .filter(|helper| !helper.is_empty())
        });
    let Some(helper) = helper else {
        return Ok(());
    };
    if !helper
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        bail!("invalid Docker credential helper name: {helper}");
    }
    super::firecracker::trusted_host_command(&format!("docker-credential-{helper}"))
        .with_context(|| format!("untrusted Docker credential helper {helper}"))?;
    Ok(())
}

fn docker_config_path() -> Option<PathBuf> {
    std::env::var_os("DOCKER_CONFIG")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".docker")))
        .map(|directory| directory.join("config.json"))
}

async fn pull_blob(
    client: &Client,
    reference: &Reference,
    descriptor: &OciDescriptor,
    blob_root: &Path,
) -> Result<(PathBuf, bool)> {
    let digest = sha256_hex(&descriptor.digest)?;
    let destination = blob_root.join(digest);
    if destination.try_exists()? {
        validate_cached_blob(&destination, descriptor)?;
        return Ok((destination, true));
    }

    prepare_private_dir(blob_root)?;
    let temporary = NamedTempFile::new_in(blob_root).with_context(|| {
        format!(
            "creating OCI layer temporary file in {}",
            blob_root.display()
        )
    })?;
    let mut output = tokio::fs::File::from_std(temporary.reopen()?);
    client
        .pull_blob(reference, descriptor, &mut output)
        .await
        .with_context(|| format!("pulling OCI layer {}", descriptor.digest))?;
    output.flush().await?;
    output.sync_all().await?;
    drop(output);
    validate_blob(temporary.path(), descriptor)?;
    fs::set_permissions(temporary.path(), Permissions::from_mode(0o600))?;
    match fs::hard_link(temporary.path(), &destination) {
        Ok(()) => Ok((destination, false)),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            validate_cached_blob(&destination, descriptor)?;
            Ok((destination, true))
        }
        Err(error) => Err(error).with_context(|| {
            format!(
                "publishing OCI layer {} to {}",
                descriptor.digest,
                destination.display()
            )
        }),
    }
}

fn validate_cached_blob(path: &Path, descriptor: &OciDescriptor) -> Result<()> {
    let metadata = fs::metadata(path)
        .with_context(|| format!("reading cached OCI layer {}", path.display()))?;
    if !metadata.is_file()
        || metadata.len() != u64::try_from(descriptor.size).context("negative OCI layer size")?
    {
        bail!(
            "cached OCI layer does not match descriptor {}: {}",
            descriptor.digest,
            path.display()
        );
    }
    Ok(())
}

fn validate_blob(path: &Path, descriptor: &OciDescriptor) -> Result<()> {
    validate_cached_blob(path, descriptor)?;
    let expected = sha256_hex(&descriptor.digest)?;
    let mut input = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = input.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    let actual = format!("{:x}", hasher.finalize());
    if actual != expected {
        bail!("OCI layer digest mismatch: expected sha256:{expected}, got sha256:{actual}");
    }
    Ok(())
}

fn build_and_publish_image(
    cache_root: &Path,
    cache_dir: &Path,
    source: &str,
    manifest_digest: &str,
    layers: Vec<CachedLayer>,
    metadata: CachedImageMetadata,
    image_size_gib: u64,
) -> Result<PathBuf> {
    let temporary = TempBuilder::new()
        .prefix("image-build-")
        .tempdir_in(cache_root)
        .with_context(|| format!("creating image build directory in {}", cache_root.display()))?;
    let rootfs = temporary.path().join("rootfs");
    fs::create_dir(&rootfs)?;
    fs::set_permissions(&rootfs, Permissions::from_mode(0o700))?;

    // tar-rs documents concurrent mutation of its extraction target as outside
    // its threat model. This target is consequently inside Exo's root-owned
    // 0700 cache and is never exposed to guest or unprivileged host processes.
    // https://github.com/alexcrichton/tar-rs/blob/main/src/lib.rs#L12-L25
    let extract_started = Instant::now();
    for layer in &layers {
        apply_layer(&rootfs, layer)?;
    }
    record_step(
        source,
        Some(manifest_digest),
        "layer_extract",
        extract_started.elapsed(),
        false,
    );

    let guest_root_started = Instant::now();
    prepare_guest_rootfs(&rootfs)?;
    record_step(
        source,
        Some(manifest_digest),
        "guest_root_prepare",
        guest_root_started.elapsed(),
        false,
    );

    let filesystem_started = Instant::now();
    let image = temporary.path().join("rootfs.ext4");
    let image_bytes = image_size_gib
        .checked_mul(1024 * 1024 * 1024)
        .context("Firecracker image size overflow")?;
    let output = File::create(&image)?;
    output.set_len(image_bytes)?;
    drop(output);
    // Firecracker attaches the guest root as a block device; it does not consume
    // OCI layers directly. `mkfs.ext4 -d` populates that device without mounting
    // attacker-controlled image contents on the host.
    // https://github.com/firecracker-microvm/firecracker/blob/main/docs/getting-started.md#rootfs-and-kernel-image
    let mkfs = super::firecracker::trusted_host_command("mkfs.ext4")?;
    let status = Command::new(mkfs)
        .args([OsStr::new("-q"), OsStr::new("-F"), OsStr::new("-d")])
        .arg(&rootfs)
        .arg(&image)
        .status()
        .context("running mkfs.ext4 for Firecracker OCI image")?;
    if !status.success() {
        bail!("mkfs.ext4 failed while materializing {source}: {status}");
    }
    // The cache directories remain root-only. Making the image itself read-only
    // lets each jail hard-link the same immutable inode without changing its
    // ownership or copying its data.
    fs::set_permissions(&image, Permissions::from_mode(0o444))?;
    validate_ext4_image(&image)?;
    record_step(
        source,
        Some(manifest_digest),
        "filesystem_create",
        filesystem_started.elapsed(),
        false,
    );

    fs::write(
        temporary.path().join("metadata.json"),
        serde_json::to_vec_pretty(&metadata)?,
    )?;
    fs::set_permissions(
        temporary.path().join("metadata.json"),
        Permissions::from_mode(0o600),
    )?;
    fs::remove_dir_all(&rootfs)?;
    if let Some(parent) = cache_dir.parent() {
        prepare_private_dir(parent)?;
    }
    let temporary_path = temporary.keep();
    match fs::rename(&temporary_path, cache_dir) {
        Ok(()) => {}
        Err(error) if cache_dir.try_exists()? => {
            fs::remove_dir_all(&temporary_path).with_context(|| {
                format!(
                    "removing duplicate image build {}",
                    temporary_path.display()
                )
            })?;
            tracing::debug!(%error, path = %cache_dir.display(), "another process published the Firecracker image first");
        }
        Err(error) => {
            fs::remove_dir_all(&temporary_path).with_context(|| {
                format!("removing failed image build {}", temporary_path.display())
            })?;
            return Err(error)
                .with_context(|| format!("publishing Firecracker image {}", cache_dir.display()));
        }
    }
    validate_cache_entry(
        cache_dir,
        &metadata.source_digest,
        Some(&metadata.manifest_digest),
        &metadata.platform,
    )
}

fn apply_layer(rootfs: &Path, layer: &CachedLayer) -> Result<()> {
    // OCI whiteouts describe deletions from lower layers. Applying every
    // whiteout before unpacking the same layer preserves replacement entries and
    // opaque-directory semantics.
    // https://github.com/opencontainers/image-spec/blob/main/layer.md#whiteouts
    let whiteouts = collect_whiteouts(layer)?;
    for whiteout in whiteouts {
        match whiteout {
            Whiteout::Remove(path) => remove_whiteout_target(rootfs, &path)?,
            Whiteout::Opaque(path) => clear_opaque_directory(rootfs, &path)?,
        }
    }

    let reader = layer_reader(&layer.path, &layer.media_type)?;
    let mut archive = tar::Archive::new(reader);
    archive.set_preserve_permissions(true);
    archive.set_preserve_ownerships(true);
    archive.set_preserve_mtime(true);
    archive.set_unpack_xattrs(true);
    archive.set_overwrite(true);
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        validate_archive_path(&path)?;
        if whiteout_path(&path)?.is_some() {
            continue;
        }
        let entry_type = entry.header().entry_type();
        if entry_type.is_character_special()
            || entry_type.is_block_special()
            || entry_type.is_fifo()
        {
            bail!(
                "OCI layer {} contains unsupported special file {}",
                layer.path.display(),
                path.display()
            );
        }
        if !entry.unpack_in(rootfs)? {
            bail!(
                "OCI layer entry escapes the root filesystem: {}",
                path.display()
            );
        }
    }
    Ok(())
}

fn collect_whiteouts(layer: &CachedLayer) -> Result<Vec<Whiteout>> {
    let reader = layer_reader(&layer.path, &layer.media_type)?;
    let mut archive = tar::Archive::new(reader);
    let mut whiteouts = Vec::new();
    for entry in archive.entries()? {
        let entry = entry?;
        let path = entry.path()?.into_owned();
        validate_archive_path(&path)?;
        if let Some(whiteout) = whiteout_path(&path)? {
            whiteouts.push(whiteout);
        }
    }
    Ok(whiteouts)
}

fn whiteout_path(path: &Path) -> Result<Option<Whiteout>> {
    let Some(name) = path.file_name() else {
        return Ok(None);
    };
    let bytes = name.as_bytes();
    let parent = path.parent().unwrap_or_else(|| Path::new(""));
    if bytes == b".wh..wh..opq" {
        return Ok(Some(Whiteout::Opaque(parent.to_path_buf())));
    }
    let Some(removed) = bytes.strip_prefix(b".wh.") else {
        return Ok(None);
    };
    if removed.is_empty() || removed.contains(&b'/') || removed == b"." || removed == b".." {
        bail!("invalid OCI whiteout path {}", path.display());
    }
    Ok(Some(Whiteout::Remove(
        parent.join(OsStr::from_bytes(removed)),
    )))
}

fn layer_reader(path: &Path, media_type: &str) -> Result<Box<dyn Read>> {
    let mut file =
        File::open(path).with_context(|| format!("opening cached OCI layer {}", path.display()))?;
    let mut magic = [0_u8; 4];
    let count = file.read(&mut magic)?;
    file.seek(SeekFrom::Start(0))?;
    if count >= 2 && magic[..2] == [0x1f, 0x8b] {
        return Ok(Box::new(MultiGzDecoder::new(file)));
    }
    if count == 4 && magic == [0x28, 0xb5, 0x2f, 0xfd] {
        return Ok(Box::new(zstd::stream::read::Decoder::new(file)?));
    }
    if media_type.ends_with("+gzip") || media_type.ends_with(".gzip") {
        bail!(
            "OCI layer claims gzip compression but has invalid magic: {}",
            path.display()
        );
    }
    if media_type.ends_with("+zstd") {
        bail!(
            "OCI layer claims zstd compression but has invalid magic: {}",
            path.display()
        );
    }
    Ok(Box::new(BufReader::new(file)))
}

fn validate_archive_path(path: &Path) -> Result<()> {
    if path.as_os_str().is_empty() {
        bail!("OCI layer contains an empty path");
    }
    for component in path.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                bail!(
                    "OCI layer path escapes its root filesystem: {}",
                    path.display()
                )
            }
        }
    }
    Ok(())
}

fn remove_whiteout_target(rootfs: &Path, relative: &Path) -> Result<()> {
    let target = jailed_path(rootfs, relative, true)?;
    let metadata = match fs::symlink_metadata(&target) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if metadata.is_dir() {
        fs::remove_dir_all(target)?;
    } else {
        fs::remove_file(target)?;
    }
    Ok(())
}

fn clear_opaque_directory(rootfs: &Path, relative: &Path) -> Result<()> {
    let directory = jailed_path(rootfs, relative, false)?;
    if !directory.try_exists()? {
        return Ok(());
    }
    if !fs::symlink_metadata(&directory)?.is_dir() {
        bail!(
            "OCI opaque whiteout target is not a directory: {}",
            directory.display()
        );
    }
    for entry in fs::read_dir(&directory)? {
        let entry = entry?;
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.is_dir() {
            fs::remove_dir_all(entry.path())?;
        } else {
            fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}

fn jailed_path(root: &Path, relative: &Path, allow_final_symlink: bool) -> Result<PathBuf> {
    validate_archive_path(relative)?;
    let components = relative
        .components()
        .filter_map(|component| match component {
            Component::Normal(part) => Some(part.to_os_string()),
            Component::CurDir => None,
            _ => unreachable!("validated above"),
        })
        .collect::<Vec<_>>();
    let mut path = root.to_path_buf();
    for (index, component) in components.iter().enumerate() {
        path.push(component);
        let is_final = index + 1 == components.len();
        if is_final && allow_final_symlink {
            continue;
        }
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                bail!("OCI layer path traverses a symlink: {}", path.display())
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(path)
}

fn prepare_guest_rootfs(rootfs: &Path) -> Result<()> {
    let home = create_jailed_dir(rootfs, Path::new("home/exo"))?;
    let workspace = create_jailed_dir(rootfs, Path::new("home/exo/workspace"))?;
    std::os::unix::fs::chown(&home, Some(GUEST_UID), Some(GUEST_GID))?;
    std::os::unix::fs::chown(&workspace, Some(GUEST_UID), Some(GUEST_GID))?;
    Ok(())
}

fn create_jailed_dir(rootfs: &Path, relative: &Path) -> Result<PathBuf> {
    validate_archive_path(relative)?;
    let mut path = rootfs.to_path_buf();
    for component in relative.components() {
        let Component::Normal(component) = component else {
            continue;
        };
        path.push(component);
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                bail!(
                    "OCI image runtime path traverses a symlink: {}",
                    path.display()
                )
            }
            Ok(metadata) if !metadata.is_dir() => {
                bail!(
                    "OCI image runtime path is not a directory: {}",
                    path.display()
                )
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir(&path)?;
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(path)
}

fn validate_platform(config: &OciImageConfiguration) -> Result<()> {
    if config.os != "linux" {
        bail!("Firecracker OCI image must target Linux, got {}", config.os);
    }
    let expected = current_architecture()?;
    if config.architecture != expected {
        bail!(
            "Firecracker OCI image architecture mismatch: host requires {expected}, image is {}",
            config.architecture
        );
    }
    Ok(())
}

fn current_platform() -> Result<String> {
    Ok(format!("linux-{}", current_architecture()?))
}

fn current_architecture() -> Result<&'static str> {
    match std::env::consts::ARCH {
        "x86_64" => Ok("amd64"),
        "aarch64" => Ok("arm64"),
        architecture => bail!("Firecracker does not support host architecture {architecture}"),
    }
}

fn cache_image_dir(cache_root: &Path, platform: &str, digest: &str) -> Result<PathBuf> {
    let digest = sha256_hex(digest)?;
    Ok(cache_root
        .join(format!("v{MATERIALIZER_VERSION}"))
        .join(platform)
        .join(digest))
}

fn sha256_hex(digest: &str) -> Result<&str> {
    let Some(hex) = digest.strip_prefix("sha256:") else {
        bail!("only sha256 OCI digests are supported: {digest}");
    };
    if hex.len() != 64 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("invalid sha256 OCI digest: {digest}");
    }
    Ok(hex)
}

fn validate_cache_entry(
    cache_dir: &Path,
    source_digest: &str,
    manifest_digest: Option<&str>,
    platform: &str,
) -> Result<PathBuf> {
    let metadata_path = cache_dir.join("metadata.json");
    let metadata: CachedImageMetadata =
        serde_json::from_slice(&fs::read(&metadata_path).with_context(|| {
            format!("reading image cache metadata {}", metadata_path.display())
        })?)
        .with_context(|| format!("decoding image cache metadata {}", metadata_path.display()))?;
    if metadata.materializer_version != MATERIALIZER_VERSION
        || metadata.source_digest != source_digest
        || metadata.platform != platform
        || manifest_digest.is_some_and(|digest| metadata.manifest_digest != digest)
    {
        bail!(
            "Firecracker image cache metadata mismatch: {}",
            cache_dir.display()
        );
    }
    let image = cache_dir.join("rootfs.ext4");
    validate_ext4_image(&image)?;
    Ok(image)
}

pub(super) fn validate_ext4_image(path: &Path) -> Result<()> {
    let metadata = fs::metadata(path)
        .with_context(|| format!("opening Firecracker root filesystem {}", path.display()))?;
    if !metadata.is_file() {
        bail!(
            "Firecracker root filesystem is not a file: {}",
            path.display()
        );
    }
    let mut image = File::open(path)?;
    image.seek(SeekFrom::Start(EXT4_MAGIC_OFFSET))?;
    let mut magic = [0_u8; 2];
    image.read_exact(&mut magic)?;
    if magic != EXT4_MAGIC {
        bail!(
            "Firecracker image is not an ext4 filesystem: {}",
            path.display()
        );
    }
    Ok(())
}

fn prepare_private_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    fs::set_permissions(path, Permissions::from_mode(0o700))?;
    let metadata = fs::metadata(path)?;
    if metadata.uid() != 0 || metadata.mode() & 0o077 != 0 {
        bail!(
            "Firecracker image cache must be root-owned with mode 0700: {}",
            path.display()
        );
    }
    Ok(())
}

fn record_step(image: &str, digest: Option<&str>, step: &str, duration: Duration, cache_hit: bool) {
    tracing::info!(
        image,
        digest,
        step,
        duration_ms = duration.as_secs_f64() * 1000.0,
        cache_hit,
        "Firecracker image materialization timing"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use tar::{Builder, EntryType, Header};

    fn append_file(
        builder: &mut Builder<Vec<u8>>,
        path: &str,
        contents: &[u8],
        uid: u64,
        gid: u64,
    ) {
        let mut header = Header::new_gnu();
        header.set_path(path).unwrap();
        header.set_size(contents.len() as u64);
        header.set_mode(0o644);
        header.set_uid(uid);
        header.set_gid(gid);
        header.set_mtime(0);
        header.set_entry_type(EntryType::Regular);
        header.set_cksum();
        builder.append(&header, contents).unwrap();
    }

    fn write_layer(directory: &Path, name: &str, entries: &[(&str, &[u8])]) -> CachedLayer {
        let mut builder = Builder::new(Vec::new());
        let metadata = fs::metadata(directory).unwrap();
        for (path, contents) in entries {
            append_file(
                &mut builder,
                path,
                contents,
                u64::from(metadata.uid()),
                u64::from(metadata.gid()),
            );
        }
        let bytes = builder.into_inner().unwrap();
        let path = directory.join(name);
        fs::write(&path, bytes).unwrap();
        CachedLayer {
            path,
            media_type: "application/vnd.oci.image.layer.v1.tar".to_string(),
        }
    }

    #[test]
    fn layers_apply_whiteouts_before_extracting_new_entries() {
        let directory = tempfile::tempdir().unwrap();
        let rootfs = directory.path().join("rootfs");
        fs::create_dir(&rootfs).unwrap();
        let base = write_layer(
            directory.path(),
            "base.tar",
            &[
                ("etc/old", b"old"),
                ("etc/keep", b"keep"),
                ("opaque/lower", b"lower"),
            ],
        );
        let upper = write_layer(
            directory.path(),
            "upper.tar",
            &[
                ("etc/.wh.old", b""),
                ("opaque/.wh..wh..opq", b""),
                ("opaque/new", b"new"),
            ],
        );

        apply_layer(&rootfs, &base).unwrap();
        apply_layer(&rootfs, &upper).unwrap();

        assert!(!rootfs.join("etc/old").exists());
        assert_eq!(fs::read(rootfs.join("etc/keep")).unwrap(), b"keep");
        assert!(!rootfs.join("opaque/lower").exists());
        assert_eq!(fs::read(rootfs.join("opaque/new")).unwrap(), b"new");
    }

    #[test]
    fn whiteout_cannot_escape_through_a_symlink() {
        let directory = tempfile::tempdir().unwrap();
        let rootfs = directory.path().join("rootfs");
        let victim = directory.path().join("victim");
        fs::create_dir(&rootfs).unwrap();
        fs::create_dir(&victim).unwrap();
        fs::write(victim.join("precious"), b"safe").unwrap();
        std::os::unix::fs::symlink(&victim, rootfs.join("escape")).unwrap();
        let layer = write_layer(
            directory.path(),
            "hostile.tar",
            &[("escape/.wh.precious", b"")],
        );

        assert!(apply_layer(&rootfs, &layer).is_err());
        assert_eq!(fs::read(victim.join("precious")).unwrap(), b"safe");
    }

    #[test]
    fn cache_path_is_versioned_platform_and_digest_addressed() {
        let digest = format!("sha256:{}", "a".repeat(64));
        assert_eq!(
            cache_image_dir(Path::new("/cache"), "linux-arm64", &digest).unwrap(),
            Path::new("/cache/v4/linux-arm64").join("a".repeat(64))
        );
        assert!(cache_image_dir(Path::new("/cache"), "linux-arm64", "latest").is_err());
    }

    #[test]
    fn immutable_reference_digest_requires_sha256() {
        let digest = format!("sha256:{}", "a".repeat(64));
        assert_eq!(
            immutable_reference_digest(&format!("registry.example/repo@{digest}")).unwrap(),
            Some(digest.as_str())
        );
        assert_eq!(
            immutable_reference_digest("registry.example/repo:latest").unwrap(),
            None
        );
        assert!(immutable_reference_digest("registry.example/repo@latest").is_err());
    }
}
