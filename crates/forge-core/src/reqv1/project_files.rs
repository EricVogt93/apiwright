//! Revision-checked access to the editable request-v1 project resources that
//! are not stored inside a request document.

use std::io::Write;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::{project_lock, ProjectIndex, SequenceDocument};

const MAX_PROJECT_FILE_BYTES: usize = 2 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectFileKind {
    Environment,
    Sequence,
    Asset,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectFileSnapshot {
    pub path: String,
    pub revision: String,
    pub content: Value,
}

/// Read one environment, sequence, or indexed project asset. `.env.local` and
/// other secret stores are outside this API and are never returned.
pub fn read_project_file(
    root: &Path,
    kind: ProjectFileKind,
    relative_path: &str,
) -> Result<ProjectFileSnapshot, String> {
    let root = validate_root(root)?;
    let _lock = project_lock(&root)?;
    let path = resource_path(&root, kind, relative_path, false)?;
    snapshot(kind, relative_path, &path)
}

/// Create or update an environment, sequence, or asset if the caller's
/// revision still matches. Use `new` when creating a file.
pub fn write_project_file(
    root: &Path,
    kind: ProjectFileKind,
    relative_path: &str,
    expected_revision: &str,
    content: Value,
) -> Result<ProjectFileSnapshot, String> {
    let root = validate_root(root)?;
    let _lock = project_lock(&root)?;
    let path = resource_path(&root, kind, relative_path, true)?;
    let current_revision = match std::fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(format!("refusing symbolic link {relative_path}"));
        }
        Ok(metadata) if !metadata.is_file() => {
            return Err(format!("{relative_path} is not a regular file"));
        }
        Ok(_) => Some(project_file_revision(kind, &path)?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(format!("cannot inspect {relative_path}: {error}")),
    };
    match (expected_revision, current_revision.as_deref()) {
        ("new", None) => {}
        ("new", Some(_)) => return Err(format!("{relative_path} already exists")),
        (expected, Some(current)) if expected == current => {}
        (_, current) => {
            return Err(format!(
                "revision_conflict: {relative_path} changed; read it again (current revision {})",
                current.unwrap_or("missing")
            ));
        }
    }

    let bytes = encode_content(kind, &root, &path, content)?;
    if bytes.len() > MAX_PROJECT_FILE_BYTES {
        return Err(format!(
            "{} exceeds the 2 MiB project-resource limit",
            relative_path
        ));
    }
    let parent = path
        .parent()
        .ok_or_else(|| format!("{relative_path} has no parent directory"))?;
    std::fs::create_dir_all(parent)
        .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
    reject_symlink_components(&root, relative_path)?;
    atomic_write(&path, &bytes, current_revision.is_none())?;

    snapshot(kind, relative_path, &path)
}

/// Delete one environment, sequence, or asset after a revision check.
/// Referenced assets and currently selected environments require the caller
/// to explicitly opt into leaving broken references.
pub fn delete_project_file(
    root: &Path,
    kind: ProjectFileKind,
    relative_path: &str,
    expected_revision: &str,
    allow_broken_references: bool,
) -> Result<(), String> {
    let root = validate_root(root)?;
    let _lock = project_lock(&root)?;
    let path = resource_path(&root, kind, relative_path, false)?;
    let current = project_file_revision(kind, &path)?;
    if expected_revision != current {
        return Err(format!(
            "revision_conflict: {relative_path} changed; read it again (current revision {current})"
        ));
    }

    match kind {
        ProjectFileKind::Environment if !allow_broken_references => {
            let name = path
                .file_stem()
                .and_then(|value| value.to_str())
                .unwrap_or_default();
            if environment_is_selected(&root, name)? {
                return Err(format!(
                    "environment {name:?} is selected by a project scope; set allowBrokenReferences after reviewing the affected requests"
                ));
            }
        }
        ProjectFileKind::Asset if !allow_broken_references => {
            let index = ProjectIndex::scan(&root).map_err(|diagnostic| diagnostic.message)?;
            if let Some(asset) = index
                .assets
                .iter()
                .find(|asset| asset.rel_path == relative_path)
            {
                if !asset.used_by.is_empty() {
                    let users = asset
                        .used_by
                        .iter()
                        .map(|usage| format!("{}{}", usage.request, usage.instance_path))
                        .collect::<Vec<_>>()
                        .join(", ");
                    return Err(format!(
                        "asset is referenced by {}; set allowBrokenReferences after reviewing the affected requests",
                        users
                    ));
                }
            }
        }
        _ => {}
    }

    std::fs::remove_file(&path)
        .map_err(|error| format!("cannot delete {relative_path}: {error}"))?;
    if kind == ProjectFileKind::Asset {
        let metadata = asset_metadata_path(&path);
        match std::fs::remove_file(&metadata) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!(
                    "deleted {relative_path}, but could not remove its metadata {}: {error}",
                    metadata.display()
                ));
            }
        }
    }
    Ok(())
}

fn validate_root(root: &Path) -> Result<PathBuf, String> {
    let root = root
        .canonicalize()
        .map_err(|error| format!("cannot resolve project root: {error}"))?;
    let marker = root.join("project.json");
    let metadata = std::fs::symlink_metadata(&marker)
        .map_err(|error| format!("cannot inspect project.json: {error}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err("project.json must be a regular file".to_string());
    }
    Ok(root)
}

fn resource_path(
    root: &Path,
    kind: ProjectFileKind,
    relative_path: &str,
    allow_missing: bool,
) -> Result<PathBuf, String> {
    let relative = Path::new(relative_path);
    let components = relative.components().collect::<Vec<_>>();
    if relative_path.trim().is_empty()
        || relative.is_absolute()
        || components
            .iter()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err("path must be a non-empty project-relative path".to_string());
    }
    let file_name = relative
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    if file_name.ends_with(".secrets.json") || file_name == ".env.local" {
        return Err(
            "secret files cannot be read or modified through project resources".to_string(),
        );
    }
    let valid_scope = match kind {
        ProjectFileKind::Environment => {
            components.len() == 2
                && components[0].as_os_str() == "environments"
                && relative_path.ends_with(".json")
                && !relative_path.ends_with(".secrets.json")
        }
        ProjectFileKind::Sequence => {
            components.len() >= 2
                && components[0].as_os_str() == "sequences"
                && relative_path.ends_with(".sequence.json")
        }
        ProjectFileKind::Asset => {
            components.len() >= 3
                && components[0].as_os_str() == "assets"
                && matches!(
                    relative.extension().and_then(|value| value.to_str()),
                    Some("json" | "js" | "ts")
                )
                && !relative_path.ends_with(".meta.json")
                && !relative_path.ends_with(".schema.json")
        }
    };
    if !valid_scope {
        return Err(match kind {
            ProjectFileKind::Environment => {
                "environment path must be environments/<name>.json".to_string()
            }
            ProjectFileKind::Sequence => {
                "sequence path must be sequences/<name>.sequence.json".to_string()
            }
            ProjectFileKind::Asset => {
                "asset path must be assets/<kind>/<name>.json, .js, or .ts".to_string()
            }
        });
    }
    reject_symlink_components(root, relative_path)?;
    let path = root.join(relative);
    if kind == ProjectFileKind::Environment {
        let name = path
            .file_stem()
            .and_then(|value| value.to_str())
            .unwrap_or_default();
        if name.is_empty()
            || !name.chars().all(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '-' | '_')
            })
        {
            return Err(
                "environment file name may contain only letters, numbers, '-' and '_'".to_string(),
            );
        }
    }
    if kind == ProjectFileKind::Asset {
        let metadata = asset_metadata_path(&path);
        if std::fs::symlink_metadata(&metadata)
            .is_ok_and(|metadata| metadata.file_type().is_symlink())
        {
            return Err(format!("refusing symbolic link {}", metadata.display()));
        }
    }
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            Err(format!("refusing symbolic link {relative_path}"))
        }
        Ok(metadata) if !metadata.is_file() => Err(format!("{relative_path} is not a file")),
        Ok(_) => {
            let canonical = path
                .canonicalize()
                .map_err(|error| format!("cannot resolve {relative_path}: {error}"))?;
            if !canonical.starts_with(root) {
                return Err(format!("{relative_path} escapes the project"));
            }
            Ok(canonical)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && allow_missing => Ok(path),
        Err(error) => Err(format!("cannot resolve {relative_path}: {error}")),
    }
}

fn reject_symlink_components(root: &Path, relative_path: &str) -> Result<(), String> {
    let mut current = root.to_path_buf();
    for component in Path::new(relative_path).components() {
        let Component::Normal(name) = component else {
            return Err("path contains an unsafe component".to_string());
        };
        current.push(name);
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(format!("refusing symbolic link {}", current.display()));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("cannot inspect {}: {error}", current.display())),
        }
    }
    Ok(())
}

fn encode_content(
    kind: ProjectFileKind,
    root: &Path,
    path: &Path,
    content: Value,
) -> Result<Vec<u8>, String> {
    let bytes = match kind {
        ProjectFileKind::Asset => {
            let text = content
                .as_str()
                .ok_or_else(|| "asset content must be a UTF-8 string".to_string())?;
            if path
                .extension()
                .is_some_and(|extension| extension == "json")
            {
                serde_json::from_str::<Value>(text)
                    .map_err(|error| format!("asset JSON is invalid: {error}"))?;
            }
            text.as_bytes().to_vec()
        }
        ProjectFileKind::Environment => {
            if !content.is_object() {
                return Err("environment content must be a JSON object".to_string());
            }
            pretty_json(&content)?
        }
        ProjectFileKind::Sequence => {
            let document: SequenceDocument = serde_json::from_value(content.clone())
                .map_err(|error| format!("invalid sequence document: {error}"))?;
            for request in document.resolve_files(root)? {
                let relative = request
                    .strip_prefix(root)
                    .map_err(|_| "sequence request escapes the project root".to_string())?;
                reject_symlink_components(root, &relative.to_string_lossy())?;
                let canonical = request
                    .canonicalize()
                    .map_err(|error| format!("cannot resolve sequence request: {error}"))?;
                if !canonical.starts_with(root) {
                    return Err("sequence request escapes the project root".to_string());
                }
                let metadata = std::fs::symlink_metadata(&request)
                    .map_err(|error| format!("cannot inspect sequence request: {error}"))?;
                if metadata.file_type().is_symlink() || !metadata.is_file() {
                    return Err(format!(
                        "sequence request {} must be a regular project file",
                        request.display()
                    ));
                }
            }
            pretty_json(&content)?
        }
    };
    Ok(bytes)
}

fn decode_content(kind: ProjectFileKind, path: &Path, bytes: &[u8]) -> Result<Value, String> {
    match kind {
        ProjectFileKind::Asset => String::from_utf8(bytes.to_vec())
            .map(Value::String)
            .map_err(|error| format!("asset {} is not UTF-8 text: {error}", path.display())),
        ProjectFileKind::Environment | ProjectFileKind::Sequence => serde_json::from_slice(bytes)
            .map_err(|error| format!("invalid JSON in {}: {error}", path.display())),
    }
}

fn snapshot(
    kind: ProjectFileKind,
    relative_path: &str,
    path: &Path,
) -> Result<ProjectFileSnapshot, String> {
    let bytes =
        std::fs::read(path).map_err(|error| format!("cannot read {}: {error}", relative_path))?;
    if bytes.len() > MAX_PROJECT_FILE_BYTES {
        return Err(format!(
            "{} exceeds the 2 MiB project-resource limit",
            relative_path
        ));
    }
    Ok(ProjectFileSnapshot {
        path: relative_path.to_string(),
        revision: project_file_revision(kind, path)?,
        content: decode_content(kind, path, &bytes)?,
    })
}

fn pretty_json(value: &Value) -> Result<Vec<u8>, String> {
    let mut bytes = serde_json::to_vec_pretty(value).map_err(|error| error.to_string())?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn project_file_revision(kind: ProjectFileKind, path: &Path) -> Result<String, String> {
    let mut hash = Sha256::new();
    let bytes =
        std::fs::read(path).map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    hash.update((bytes.len() as u64).to_le_bytes());
    hash.update(bytes);
    if kind == ProjectFileKind::Asset {
        let metadata = asset_metadata_path(path);
        if std::fs::symlink_metadata(&metadata)
            .is_ok_and(|metadata| metadata.file_type().is_symlink())
        {
            return Err(format!("refusing symbolic link {}", metadata.display()));
        }
        match std::fs::read(&metadata) {
            Ok(bytes) => {
                hash.update([1]);
                hash.update((bytes.len() as u64).to_le_bytes());
                hash.update(bytes);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => hash.update([0]),
            Err(error) => return Err(format!("cannot read {}: {error}", metadata.display())),
        }
    }
    let digest = hash.finalize();
    let encoded = digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    Ok(format!("sha256:{encoded}"))
}

fn asset_metadata_path(asset: &Path) -> PathBuf {
    let stem = asset
        .file_stem()
        .map(|value| value.to_string_lossy())
        .unwrap_or_default();
    asset.with_file_name(format!("{stem}.meta.json"))
}

fn atomic_write(path: &Path, bytes: &[u8], create_new: bool) -> Result<(), String> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut staged = tempfile::NamedTempFile::new_in(parent)
        .map_err(|error| format!("cannot stage {}: {error}", path.display()))?;
    if let Ok(metadata) = std::fs::metadata(path) {
        staged
            .as_file()
            .set_permissions(metadata.permissions())
            .map_err(|error| {
                format!(
                    "cannot preserve permissions for {}: {error}",
                    path.display()
                )
            })?;
    }
    staged
        .write_all(bytes)
        .map_err(|error| format!("cannot write {}: {error}", path.display()))?;
    staged
        .as_file()
        .sync_all()
        .map_err(|error| format!("cannot sync {}: {error}", path.display()))?;
    if create_new {
        staged
            .persist_noclobber(path)
            .map_err(|error| format!("cannot create {}: {}", path.display(), error.error))?;
    } else {
        staged
            .persist(path)
            .map_err(|error| format!("cannot replace {}: {}", path.display(), error.error))?;
    }
    Ok(())
}

fn environment_is_selected(root: &Path, name: &str) -> Result<bool, String> {
    for entry in walkdir::WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| {
            entry.depth() == 0
                || !entry.file_type().is_dir()
                || !matches!(
                    entry.file_name().to_str(),
                    Some(".git" | ".forge" | ".forge-local" | "target" | "node_modules")
                )
        })
    {
        let entry = entry.map_err(|error| format!("cannot scan project scopes: {error}"))?;
        if !entry.file_type().is_file()
            || !entry
                .file_name()
                .to_string_lossy()
                .ends_with(".forge-environment")
        {
            continue;
        }
        let selected = std::fs::read_to_string(entry.path())
            .map_err(|error| format!("cannot read {}: {error}", entry.path().display()))?;
        if selected.trim() == name {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn project() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("project.json"), r#"{"formatVersion":1}"#).unwrap();
        root
    }

    #[test]
    fn revision_checked_environment_create_update_and_delete() {
        let root = project();
        let created = write_project_file(
            root.path(),
            ProjectFileKind::Environment,
            "environments/test.json",
            "new",
            json!({"baseUrl":"https://old.example.test"}),
        )
        .unwrap();
        assert_eq!(created.content["baseUrl"], "https://old.example.test");
        assert!(created.revision.starts_with("sha256:"));

        let updated = write_project_file(
            root.path(),
            ProjectFileKind::Environment,
            "environments/test.json",
            &created.revision,
            json!({"baseUrl":"https://new.example.test"}),
        )
        .unwrap();
        assert_ne!(created.revision, updated.revision);
        assert!(write_project_file(
            root.path(),
            ProjectFileKind::Environment,
            "environments/test.json",
            &created.revision,
            json!({})
        )
        .is_err());
        delete_project_file(
            root.path(),
            ProjectFileKind::Environment,
            "environments/test.json",
            &updated.revision,
            false,
        )
        .unwrap();
        assert!(!root.path().join("environments/test.json").exists());
    }

    #[cfg(unix)]
    #[test]
    fn asset_writes_and_reads_reject_symlinked_directories() {
        use std::os::unix::fs::symlink;

        let root = project();
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("assets")).unwrap();
        symlink(outside.path(), root.path().join("assets/hooks")).unwrap();
        assert!(write_project_file(
            root.path(),
            ProjectFileKind::Asset,
            "assets/hooks/check.js",
            "new",
            Value::String("function run() { return true; }".to_string()),
        )
        .is_err());
        assert!(!outside.path().join("check.js").exists());
    }

    #[test]
    fn sequence_must_reference_existing_request_v1_files() {
        let root = project();
        let invalid = json!({
            "formatVersion":1,
            "kind":"sequence",
            "meta":{"id":"smoke","name":"Smoke"},
            "requests":["requests/missing.request.json"]
        });
        assert!(write_project_file(
            root.path(),
            ProjectFileKind::Sequence,
            "sequences/smoke.sequence.json",
            "new",
            invalid,
        )
        .is_err());
        assert!(!root.path().join("sequences/smoke.sequence.json").exists());
    }
}
