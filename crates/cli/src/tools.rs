use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

const MANIFEST_FILE: &str = "exo-tool.json";
const SCHEMA_VERSION: u8 = 1;

#[derive(Debug, Clone, Subcommand)]
pub enum ToolCommands {
    /// List installed tool IDs and their sources.
    List,
    /// Inspect an installed tool and its manifest.
    Get { id: String },
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolLockfile {
    #[serde(rename = "schemaVersion")]
    schema_version: u8,
    tools: Vec<InstalledTool>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct InstalledTool {
    id: String,
    source: ToolSource,
    initialization: Map<String, Value>,
    #[serde(rename = "installPath")]
    install_path: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum ToolSource {
    Local(LocalSource),
    Git(GitSource),
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct LocalSource {
    path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    subdirectory: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct GitSource {
    repository: String,
    commit: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    subdirectory: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ToolManifest {
    #[serde(rename = "schemaVersion")]
    schema_version: u8,
    id: String,
    module: String,
}

#[derive(Debug, Serialize)]
struct ToolDetails<'a> {
    installed: &'a InstalledTool,
    manifest: ToolManifest,
    #[serde(rename = "modulePath")]
    module_path: PathBuf,
}

pub fn handle_tool_command(root: &Path, command: &ToolCommands) -> Result<()> {
    match command {
        ToolCommands::List => {
            for line in list_lines(root)? {
                println!("{line}");
            }
        }
        ToolCommands::Get { id } => {
            let lockfile = read_lockfile(root)?;
            let installed = lockfile
                .tools
                .iter()
                .find(|tool| tool.id == *id)
                .with_context(|| format!("installed tool not found: {id}"))?;
            let details = tool_details(installed)?;
            println!("{}", serde_json::to_string_pretty(&details)?);
        }
    }
    Ok(())
}

fn list_lines(root: &Path) -> Result<Vec<String>> {
    let lockfile = read_lockfile(root)?;
    Ok(lockfile
        .tools
        .iter()
        .map(|tool| format!("{}\t{}", tool.id, source_description(&tool.source)))
        .collect())
}

fn source_description(source: &ToolSource) -> String {
    match source {
        ToolSource::Local(source) => {
            describe_source("local", &source.path, source.subdirectory.as_deref())
        }
        ToolSource::Git(source) => {
            let location = format!("{}@{}", source.repository, source.commit);
            describe_source("git", &location, source.subdirectory.as_deref())
        }
    }
}

fn describe_source(kind: &str, location: &str, subdirectory: Option<&str>) -> String {
    match subdirectory {
        Some(subdirectory) => format!("{kind}:{location}#{subdirectory}"),
        None => format!("{kind}:{location}"),
    }
}

fn read_lockfile(root: &Path) -> Result<ToolLockfile> {
    let path = lockfile_path(root);
    if !path.exists() {
        return Ok(ToolLockfile {
            schema_version: SCHEMA_VERSION,
            tools: Vec::new(),
        });
    }

    let bytes = fs::read(&path)
        .with_context(|| format!("unable to read tool lockfile {}", path.display()))?;
    let lockfile: ToolLockfile = serde_json::from_slice(&bytes)
        .with_context(|| format!("invalid tool lockfile {}", path.display()))?;
    validate_lockfile(root, &lockfile)
        .with_context(|| format!("invalid tool lockfile {}", path.display()))?;
    Ok(lockfile)
}

fn validate_lockfile(root: &Path, lockfile: &ToolLockfile) -> Result<()> {
    if lockfile.schema_version != SCHEMA_VERSION {
        bail!("tool lockfile schemaVersion must be {SCHEMA_VERSION}");
    }
    for (index, tool) in lockfile.tools.iter().enumerate() {
        validate_installed_tool(root, tool)
            .with_context(|| format!("tool lockfile.tools[{index}]"))?;
    }
    Ok(())
}

fn validate_installed_tool(root: &Path, tool: &InstalledTool) -> Result<()> {
    validate_tool_id(&tool.id).context("id")?;
    if tool.install_path.is_empty() {
        bail!("installPath must be a non-empty string");
    }
    validate_managed_install_path(root, Path::new(&tool.install_path))?;
    match &tool.source {
        ToolSource::Local(source) => {
            validate_workspace_relative_path(&source.path)?;
            validate_optional_subdirectory(source.subdirectory.as_deref())?;
        }
        ToolSource::Git(source) => {
            require_non_empty(&source.repository, "source.repository")?;
            if !(40..=64).contains(&source.commit.len())
                || !source.commit.bytes().all(|byte| byte.is_ascii_hexdigit())
            {
                bail!("source.commit must be a pinned 40-64 digit hex hash");
            }
            validate_optional_subdirectory(source.subdirectory.as_deref())?;
        }
    }
    Ok(())
}

fn validate_optional_subdirectory(subdirectory: Option<&str>) -> Result<()> {
    if let Some(subdirectory) = subdirectory {
        validate_relative_path(subdirectory, "source.subdirectory")?;
    }
    Ok(())
}

fn tool_details(installed: &InstalledTool) -> Result<ToolDetails<'_>> {
    let install_path = Path::new(&installed.install_path);
    let manifest_path = install_path.join(MANIFEST_FILE);
    let bytes = fs::read(&manifest_path)
        .with_context(|| format!("unable to read tool manifest {}", manifest_path.display()))?;
    let manifest: ToolManifest = serde_json::from_slice(&bytes)
        .with_context(|| format!("invalid tool manifest {}", manifest_path.display()))?;
    validate_manifest(installed, &manifest)?;
    let module_path = contained_module_path(install_path, &manifest.module)?;
    Ok(ToolDetails {
        installed,
        manifest,
        module_path,
    })
}

fn validate_manifest(installed: &InstalledTool, manifest: &ToolManifest) -> Result<()> {
    if manifest.schema_version != SCHEMA_VERSION {
        bail!("tool manifest schemaVersion must be {SCHEMA_VERSION}");
    }
    validate_tool_id(&manifest.id).context("tool manifest id")?;
    if manifest.id != installed.id {
        bail!(
            "installed tool id {} does not match manifest id {}",
            installed.id,
            manifest.id
        );
    }
    validate_relative_path(&manifest.module, "tool manifest module")
}

fn contained_module_path(install_path: &Path, module: &str) -> Result<PathBuf> {
    let real_install_path = fs::canonicalize(install_path).with_context(|| {
        format!(
            "unable to resolve tool installPath {}",
            install_path.display()
        )
    })?;
    let candidate = install_path.join(module);
    let real_candidate = fs::canonicalize(&candidate)
        .with_context(|| format!("unable to resolve tool module {}", candidate.display()))?;
    if real_candidate == real_install_path || !real_candidate.starts_with(&real_install_path) {
        bail!("tool manifest module escapes its install directory");
    }
    if !real_candidate.is_file() {
        bail!("tool manifest module must resolve to a file");
    }
    Ok(real_candidate)
}

fn validate_tool_id(id: &str) -> Result<()> {
    let Some(suffix) = id.strip_prefix("tool:") else {
        bail!("must be a stable tool: namespace id");
    };
    if suffix.is_empty()
        || suffix.len() > 128
        || !suffix.as_bytes()[0].is_ascii_alphanumeric()
        || !suffix
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'/' | b'-'))
    {
        bail!("must be a stable tool: namespace id");
    }
    Ok(())
}

fn validate_relative_path(value: &str, label: &str) -> Result<()> {
    if value.is_empty()
        || Path::new(value).is_absolute()
        || value
            .split(['/', '\\'])
            .any(|component| component.is_empty() || component == "..")
    {
        bail!("{label} must be a contained relative path");
    }
    Ok(())
}

fn validate_workspace_relative_path(value: &str) -> Result<()> {
    validate_relative_path(value, "source.path")?;
    if value.split(['/', '\\']).any(|component| component == ".") {
        bail!("source.path must not contain '.' segments");
    }
    Ok(())
}

fn require_non_empty(value: &str, label: &str) -> Result<()> {
    if value.is_empty() {
        bail!("{label} must be a non-empty string");
    }
    Ok(())
}

fn validate_managed_install_path(root: &Path, install_path: &Path) -> Result<()> {
    let store_root = absolute_lexical(&tool_root(root).join("store"))?;
    let install_path = absolute_lexical(install_path)?;
    if install_path == store_root || !install_path.starts_with(&store_root) {
        bail!("installPath must be inside the managed tool store");
    }
    Ok(())
}

fn absolute_lexical(path: &Path) -> Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            component => normalized.push(component.as_os_str()),
        }
    }
    Ok(normalized)
}

fn tool_root(root: &Path) -> PathBuf {
    root.join("tools")
}

fn lockfile_path(root: &Path) -> PathBuf {
    tool_root(root).join("tools.lock.json")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn write_json(path: &Path, value: Value) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
    }

    fn local_entry(root: &Path, id: &str, directory: &str) -> Value {
        json!({
            "id": id,
            "source": {
                "type": "local",
                "path": ".exo/tool-sources/tool",
                "subdirectory": "package"
            },
            "initialization": {"enabled": true},
            "installPath": root.join("tools/store").join(directory)
        })
    }

    #[test]
    fn missing_lockfile_lists_no_tools() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().join(".exo");

        assert!(list_lines(&root).unwrap().is_empty());
    }

    #[test]
    fn lists_and_gets_valid_minimal_entries() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().join(".exo");
        let local_install = root.join("tools/store/local");
        let git_install = root.join("tools/store/git");
        for (install, id) in [
            (&local_install, "tool:test/local"),
            (&git_install, "tool:test/git"),
        ] {
            fs::create_dir_all(install).unwrap();
            fs::write(install.join("tool.ts"), "export default {};\n").unwrap();
            write_json(
                &install.join(MANIFEST_FILE),
                json!({"schemaVersion": 1, "id": id, "module": "tool.ts"}),
            );
        }
        write_json(
            &lockfile_path(&root),
            json!({
                "schemaVersion": 1,
                "tools": [
                    local_entry(&root, "tool:test/local", "local"),
                    {
                        "id": "tool:test/git",
                        "source": {
                            "type": "git",
                            "repository": "https://example.com/tools.git",
                            "commit": "0123456789abcdef0123456789abcdef01234567"
                        },
                        "initialization": {},
                        "installPath": git_install
                    }
                ]
            }),
        );

        assert_eq!(
            list_lines(&root).unwrap(),
            [
                "tool:test/local\tlocal:.exo/tool-sources/tool#package",
                "tool:test/git\tgit:https://example.com/tools.git@0123456789abcdef0123456789abcdef01234567"
            ]
        );
        let lockfile = read_lockfile(&root).unwrap();
        let details = tool_details(&lockfile.tools[0]).unwrap();
        assert_eq!(details.manifest.id, "tool:test/local");
        assert_eq!(
            details.module_path,
            fs::canonicalize(local_install.join("tool.ts")).unwrap()
        );
    }

    #[test]
    fn rejects_invalid_and_unknown_lockfile_fields() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().join(".exo");
        write_json(
            &lockfile_path(&root),
            json!({"schemaVersion": 2, "tools": []}),
        );
        assert!(
            read_lockfile(&root)
                .unwrap_err()
                .to_string()
                .contains("invalid tool lockfile")
        );

        let mut entry = local_entry(&root, "tool:test/local", "local");
        entry["status"] = json!("active");
        write_json(
            &lockfile_path(&root),
            json!({"schemaVersion": 1, "tools": [entry]}),
        );
        let error = read_lockfile(&root).unwrap_err();
        assert!(format!("{error:#}").contains("unknown field"));

        let mut entry = local_entry(&root, "tool:test/local", "local");
        entry["source"]["path"] = json!("/workspace/example-tool");
        write_json(
            &lockfile_path(&root),
            json!({"schemaVersion": 1, "tools": [entry]}),
        );
        let error = read_lockfile(&root).unwrap_err();
        assert!(format!("{error:#}").contains("contained relative path"));
    }

    #[test]
    fn rejects_unknown_manifest_fields_and_id_mismatch() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().join(".exo");
        let install = root.join("tools/store/local");
        fs::create_dir_all(&install).unwrap();
        fs::write(install.join("tool.ts"), "").unwrap();
        write_json(
            &lockfile_path(&root),
            json!({
                "schemaVersion": 1,
                "tools": [local_entry(&root, "tool:test/local", "local")]
            }),
        );
        let lockfile = read_lockfile(&root).unwrap();

        write_json(
            &install.join(MANIFEST_FILE),
            json!({
                "schemaVersion": 1,
                "id": "tool:test/local",
                "module": "tool.ts",
                "name": "stale"
            }),
        );
        let error = tool_details(&lockfile.tools[0]).unwrap_err();
        assert!(format!("{error:#}").contains("unknown field"));

        write_json(
            &install.join(MANIFEST_FILE),
            json!({
                "schemaVersion": 1,
                "id": "tool:test/other",
                "module": "tool.ts"
            }),
        );
        assert!(
            tool_details(&lockfile.tools[0])
                .unwrap_err()
                .to_string()
                .contains("does not match manifest id")
        );
    }

    #[test]
    fn rejects_manifest_module_path_escape() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().join(".exo");
        let install = root.join("tools/store/local");
        fs::create_dir_all(&install).unwrap();
        write_json(
            &install.join(MANIFEST_FILE),
            json!({
                "schemaVersion": 1,
                "id": "tool:test/local",
                "module": "../outside.ts"
            }),
        );
        write_json(
            &lockfile_path(&root),
            json!({
                "schemaVersion": 1,
                "tools": [local_entry(&root, "tool:test/local", "local")]
            }),
        );
        let lockfile = read_lockfile(&root).unwrap();

        assert!(
            tool_details(&lockfile.tools[0])
                .unwrap_err()
                .to_string()
                .contains("contained relative path")
        );
    }
}
