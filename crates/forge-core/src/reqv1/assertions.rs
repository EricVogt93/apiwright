//! Request-adjacent assertion documents.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::catalog::{find_builtin, BuiltinIntent};
use super::hooks::HookDocument;
use super::model::{FormatVersion, PipelineEntry, PipelinePhase, RequestDocument};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AssertionDocument {
    #[serde(rename = "$schema", default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    pub format_version: FormatVersion,
    pub kind: AssertionKind,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub assertions: Vec<AssertionEntry>,
}

impl Default for AssertionDocument {
    fn default() -> Self {
        Self {
            schema: None,
            format_version: FormatVersion,
            kind: AssertionKind::Assertions,
            assertions: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AssertionKind {
    Assertions,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AssertionEntry {
    #[serde(rename = "use")]
    pub uses: String,
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub with: serde_json::Map<String, Value>,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

impl AssertionDocument {
    pub fn parse(text: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(text)
    }

    pub fn load_for_request(request: &Path) -> Result<Self, String> {
        let path = assertions_path(request);
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                Self::parse(&text).map_err(|error| format!("invalid {}: {error}", path.display()))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(format!("cannot read {}: {error}", path.display())),
        }
    }

    pub fn save_for_request(&self, request: &Path) -> Result<(), String> {
        let path = assertions_path(request);
        if self.assertions.is_empty() {
            return match std::fs::remove_file(&path) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(format!("cannot remove {}: {error}", path.display())),
            };
        }
        let mut text = serde_json::to_string_pretty(self).map_err(|error| error.to_string())?;
        text.push('\n');
        std::fs::write(&path, text)
            .map_err(|error| format!("cannot write {}: {error}", path.display()))
    }

    pub fn take_from_request(request: &mut RequestDocument) -> Self {
        let mut document = Self::default();
        request.pipeline.retain(|entry| {
            if is_assertion(entry) {
                document.push(entry.clone().into());
                false
            } else {
                true
            }
        });
        document
    }

    pub fn extend(&mut self, other: Self) {
        for assertion in other.assertions {
            self.push(assertion);
        }
    }

    pub fn apply_to(&self, request: &mut RequestDocument) {
        for assertion in &self.assertions {
            let entry = PipelineEntry {
                phase: PipelinePhase::AfterResponse,
                uses: assertion.uses.clone(),
                with: assertion.with.clone(),
                enabled: assertion.enabled,
            };
            request.pipeline.push(entry);
        }
    }

    pub fn push(&mut self, assertion: AssertionEntry) {
        self.assertions.push(assertion);
    }
}

impl From<PipelineEntry> for AssertionEntry {
    fn from(entry: PipelineEntry) -> Self {
        Self {
            uses: entry.uses,
            with: entry.with,
            enabled: entry.enabled,
        }
    }
}

pub fn assertions_path(request: &Path) -> PathBuf {
    let name = request
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("request.request.json");
    let stem = name.strip_suffix(".request.json").unwrap_or(name);
    request.with_file_name(format!("{stem}.assertions.json"))
}

pub fn load_request_document(path: &Path) -> Result<RequestDocument, String> {
    for candidate in [
        path.to_path_buf(),
        assertions_path(path),
        super::hooks::hooks_path(path),
    ] {
        if candidate.is_symlink() {
            return Err(format!(
                "refusing to load request data through symbolic link {}",
                candidate.display()
            ));
        }
    }
    let text = std::fs::read_to_string(path)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    let mut request = RequestDocument::parse(&text)
        .map_err(|error| format!("invalid {}: {error}", path.display()))?;
    super::hooks::HookDocument::load_for_request(path)?.apply_to(&mut request);
    AssertionDocument::load_for_request(path)?.apply_to(&mut request);
    Ok(request)
}

/// Save a request inside a request-v1 project with the same sidecar split used
/// by the IDE. An ancestor `project.json` is required for the shared lock.
pub fn save_request_document(
    path: &Path,
    request: RequestDocument,
    assertions: AssertionDocument,
    hooks: HookDocument,
    create_new: bool,
) -> Result<(RequestDocument, AssertionDocument, HookDocument, String), String> {
    let _lock = request_lock(path)?;
    let (request, assertions, hooks) =
        save_request_document_unlocked(path, request, assertions, hooks, create_new)?;
    let revision = request_revision(path)?;
    Ok((request, assertions, hooks, revision))
}

/// Save an existing request only if its main document and sidecars still match
/// the revision read by the caller. The revision check and three-file commit
/// share the same cross-process lock as IDE saves.
pub fn save_request_document_at_revision(
    path: &Path,
    request: RequestDocument,
    assertions: AssertionDocument,
    hooks: HookDocument,
    expected_revision: &str,
) -> Result<(RequestDocument, AssertionDocument, HookDocument, String), String> {
    let _lock = request_lock(path)?;
    let current = request_revision(path)?;
    if current != expected_revision {
        return Err(format!(
            "revision conflict: request changed before save (current revision {current})"
        ));
    }
    let (request, assertions, hooks) =
        save_request_document_unlocked(path, request, assertions, hooks, false)?;
    let revision = request_revision(path)?;
    Ok((request, assertions, hooks, revision))
}

/// Save an editor buffer inside a request-v1 project that is not currently
/// valid request JSON while still committing its assertion and hook sidecars
/// through the shared save lock.
pub fn save_request_text(
    path: &Path,
    text: &str,
    assertions: &AssertionDocument,
    hooks: &HookDocument,
    create_new: bool,
) -> Result<String, String> {
    let _lock = request_lock(path)?;
    commit_request_files_unlocked(
        path,
        text.as_bytes().to_vec(),
        assertions,
        hooks,
        create_new,
    )?;
    request_revision(path)
}

/// Preserve invalid editor text without overwriting a newer saved revision.
pub fn save_request_text_at_revision(
    path: &Path,
    text: &str,
    assertions: &AssertionDocument,
    hooks: &HookDocument,
    expected_revision: &str,
) -> Result<String, String> {
    let _lock = request_lock(path)?;
    let current = request_revision(path)?;
    if current != expected_revision {
        return Err(format!(
            "revision conflict: request changed before save (current revision {current})"
        ));
    }
    commit_request_files_unlocked(path, text.as_bytes().to_vec(), assertions, hooks, false)?;
    request_revision(path)
}

fn save_request_document_unlocked(
    path: &Path,
    mut request: RequestDocument,
    mut assertions: AssertionDocument,
    mut hooks: HookDocument,
    create_new: bool,
) -> Result<(RequestDocument, AssertionDocument, HookDocument), String> {
    assertions.extend(AssertionDocument::take_from_request(&mut request));
    hooks.extend(HookDocument::take_from_request(&mut request));

    commit_request_files_unlocked(
        path,
        pretty_json(&request)?,
        &assertions,
        &hooks,
        create_new,
    )?;
    Ok((request, assertions, hooks))
}

fn commit_request_files_unlocked(
    path: &Path,
    main: Vec<u8>,
    assertions: &AssertionDocument,
    hooks: &HookDocument,
    create_new: bool,
) -> Result<(), String> {
    let sidecars = [assertions_path(path), super::hooks::hooks_path(path)];
    if path.is_symlink() || sidecars.iter().any(|sidecar| sidecar.is_symlink()) {
        return Err("refusing to save a request or sidecar through a symbolic link".to_string());
    }
    if create_new && sidecars.iter().any(|sidecar| sidecar.exists()) {
        return Err("cannot create request because a sibling sidecar already exists".to_string());
    }
    let parent = path
        .parent()
        .ok_or_else(|| "request path must have a parent directory".to_string())?;
    std::fs::create_dir_all(parent)
        .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;

    let changes = [
        (path.to_path_buf(), Some(main)),
        (
            sidecars[0].clone(),
            (!assertions.assertions.is_empty())
                .then(|| pretty_json(&assertions))
                .transpose()?,
        ),
        (
            sidecars[1].clone(),
            (!hooks.hooks.is_empty())
                .then(|| pretty_json(&hooks))
                .transpose()?,
        ),
    ];
    commit_staged(changes, create_new)?;
    Ok(())
}

/// Hash the saved main request plus its assertion and hook sidecars.
pub fn request_revision(request: &Path) -> Result<String, String> {
    let mut hash = Sha256::new();
    for path in [
        request.to_path_buf(),
        assertions_path(request),
        super::hooks::hooks_path(request),
    ] {
        hash.update(path.file_name().unwrap_or_default().as_encoded_bytes());
        match std::fs::read(&path) {
            Ok(bytes) => {
                hash.update([1]);
                hash.update((bytes.len() as u64).to_le_bytes());
                hash.update(bytes);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => hash.update([0]),
            Err(error) => return Err(format!("cannot read {}: {error}", path.display())),
        }
    }
    Ok(hex_digest(hash.finalize()))
}

/// Cross-process project lock shared by request readers and writers.
pub struct RequestLock {
    _file: std::fs::File,
}

pub fn project_lock(root: &Path) -> Result<RequestLock, String> {
    lock_project_file(&root.join("project.json"))
}

pub fn request_lock(path: &Path) -> Result<RequestLock, String> {
    let project_file = path
        .parent()
        .and_then(|parent| {
            parent
                .ancestors()
                .map(|candidate| candidate.join("project.json"))
                .find(|candidate| candidate.is_file())
        })
        .ok_or_else(|| {
            format!(
                "cannot lock {} because no project.json was found",
                path.display()
            )
        })?;
    lock_project_file(&project_file)
}

fn lock_project_file(project_file: &Path) -> Result<RequestLock, String> {
    if project_file.is_symlink() {
        return Err(format!(
            "refusing to lock through symbolic link {}",
            project_file.display()
        ));
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .open(project_file)
        .map_err(|error| format!("cannot open {}: {error}", project_file.display()))?;
    file.lock()
        .map_err(|error| format!("cannot lock {}: {error}", project_file.display()))?;
    Ok(RequestLock { _file: file })
}

fn hex_digest(bytes: impl IntoIterator<Item = u8>) -> String {
    let mut value = String::with_capacity(64);
    for byte in bytes {
        use std::fmt::Write as _;
        write!(value, "{byte:02x}").expect("writing to a String cannot fail");
    }
    value
}

fn pretty_json(value: &impl Serialize) -> Result<Vec<u8>, String> {
    let mut text = serde_json::to_vec_pretty(value).map_err(|error| error.to_string())?;
    text.push(b'\n');
    Ok(text)
}

struct StagedChange {
    path: PathBuf,
    replacement: Option<tempfile::NamedTempFile>,
    backup: Option<tempfile::NamedTempFile>,
}

fn commit_staged(changes: [(PathBuf, Option<Vec<u8>>); 3], create_new: bool) -> Result<(), String> {
    let mut staged = changes
        .into_iter()
        .map(|(path, replacement)| stage_change(path, replacement))
        .collect::<Result<Vec<_>, _>>()?;

    for index in 0..staged.len() {
        let result = if let Some(replacement) = staged[index].replacement.take() {
            if create_new {
                replacement
                    .persist_noclobber(&staged[index].path)
                    .map(|_| ())
                    .map_err(|error| error.error)
            } else {
                replacement
                    .persist(&staged[index].path)
                    .map(|_| ())
                    .map_err(|error| error.error)
            }
        } else if create_new {
            if staged[index].path.exists() || staged[index].path.is_symlink() {
                Err(std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    "a sibling sidecar appeared during create",
                ))
            } else {
                Ok(())
            }
        } else {
            match std::fs::remove_file(&staged[index].path) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(error),
            }
        };
        if let Err(error) = result {
            let message = if create_new && index == 0 {
                format!("cannot create {}: {error}", staged[index].path.display())
            } else {
                format!("cannot replace {}: {error}", staged[index].path.display())
            };
            return Err(rollback_staged(message, &mut staged[..index]));
        }
    }
    #[cfg(unix)]
    {
        let parent = staged[0]
            .path
            .parent()
            .expect("request path has a parent directory");
        if let Err(error) = std::fs::File::open(parent).and_then(|directory| directory.sync_all()) {
            let message = format!("cannot sync {} after save: {error}", parent.display());
            return Err(rollback_staged(message, &mut staged));
        }
    }
    Ok(())
}

fn stage_change(path: PathBuf, replacement: Option<Vec<u8>>) -> Result<StagedChange, String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", path.display()))?;
    let permissions = match std::fs::metadata(&path) {
        Ok(metadata) if metadata.is_file() => Some(metadata.permissions()),
        Ok(_) => return Err(format!("{} is not a regular file", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(format!("cannot inspect {}: {error}", path.display())),
    };
    let backup = match std::fs::read(&path) {
        Ok(bytes) => Some(stage_bytes(parent, &bytes, permissions.clone())?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(format!("cannot snapshot {}: {error}", path.display())),
    };
    let replacement = replacement
        .map(|bytes| stage_bytes(parent, &bytes, permissions))
        .transpose()?;
    Ok(StagedChange {
        path,
        replacement,
        backup,
    })
}

fn stage_bytes(
    parent: &Path,
    bytes: &[u8],
    permissions: Option<std::fs::Permissions>,
) -> Result<tempfile::NamedTempFile, String> {
    use std::io::Write as _;

    let mut file = tempfile::Builder::new()
        .prefix(".apiwright-save-")
        .tempfile_in(parent)
        .map_err(|error| format!("cannot stage save in {}: {error}", parent.display()))?;
    file.write_all(bytes)
        .map_err(|error| format!("cannot stage save in {}: {error}", parent.display()))?;
    if let Some(permissions) = permissions {
        file.as_file()
            .set_permissions(permissions)
            .map_err(|error| format!("cannot stage permissions: {error}"))?;
    }
    file.as_file()
        .sync_all()
        .map_err(|error| format!("cannot stage save in {}: {error}", parent.display()))?;
    Ok(file)
}

fn rollback_staged(error: String, committed: &mut [StagedChange]) -> String {
    let mut failures = Vec::new();
    for change in committed.iter_mut().rev() {
        let result = match change.backup.take() {
            Some(backup) => backup
                .persist(&change.path)
                .map(|_| ())
                .map_err(|error| error.error),
            None => match std::fs::remove_file(&change.path) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(error),
            },
        };
        if let Err(restore_error) = result {
            failures.push(format!("{}: {restore_error}", change.path.display()));
        }
    }
    #[cfg(unix)]
    if let Some(parent) = committed.first().and_then(|change| change.path.parent()) {
        if let Err(sync_error) =
            std::fs::File::open(parent).and_then(|directory| directory.sync_all())
        {
            failures.push(format!("{}: {sync_error}", parent.display()));
        }
    }
    if failures.is_empty() {
        error
    } else {
        format!("{error}; rollback also failed for {}", failures.join(", "))
    }
}

fn is_assertion(entry: &PipelineEntry) -> bool {
    if entry.phase != PipelinePhase::AfterResponse {
        return false;
    }
    let builtin = entry
        .uses
        .strip_prefix("builtin:")
        .and_then(|reference| reference.split('@').next())
        .and_then(find_builtin)
        .is_some_and(|definition| definition.intent == BuiltinIntent::Validate);
    let normalized = entry.uses.replace('\\', "/");
    builtin
        || normalized.starts_with("assertions/")
        || normalized.contains("/assertions/")
        || normalized.contains(":assertions/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_assertions_can_be_configured_more_than_once() {
        let assertion = AssertionEntry {
            uses: "builtin:assert-status@1".to_string(),
            with: serde_json::Map::from_iter([("expected".to_string(), Value::from(200))]),
            enabled: true,
        };
        let mut assertions = AssertionDocument::default();
        assertions.push(assertion.clone());
        assertions.push(assertion);

        let mut request = RequestDocument::parse(
            r#"{"formatVersion":1,"kind":"request","meta":{"id":"test","name":"Test"},"request":{"method":"GET","url":"https://example.test"}}"#,
        )
        .expect("valid request document");
        assertions.apply_to(&mut request);

        assert_eq!(assertions.assertions.len(), 2);
        assert_eq!(request.pipeline.len(), 2);
    }

    #[test]
    fn derives_sidecar_path_and_moves_only_assertions() {
        let mut request = RequestDocument::parse(
            r#"{"formatVersion":1,"kind":"request","meta":{"id":"x","name":"x"},
                "request":{"method":"GET","url":"https://example.test"},
                "pipeline":[
                  {"phase":"afterResponse","use":"builtin:assert-status@1","with":{"expected":200}},
                  {"phase":"afterResponse","use":"builtin:extract-header@1","with":{"name":"X-Id","target":"id"}}
                ]}"#,
        )
        .unwrap();

        let assertions = AssertionDocument::take_from_request(&mut request);

        assert_eq!(
            assertions_path(Path::new("requests/users.get.request.json")),
            Path::new("requests/users.get.assertions.json")
        );
        assert_eq!(assertions.assertions.len(), 1);
        assert_eq!(request.pipeline.len(), 1);
    }

    #[test]
    fn saved_sidecar_is_loaded_into_the_effective_request() {
        let dir = tempfile::tempdir().unwrap();
        let request = dir.path().join("get.request.json");
        std::fs::write(
            &request,
            r#"{"formatVersion":1,"kind":"request","meta":{"id":"x","name":"x"},
                "request":{"method":"GET","url":"https://example.test"}}"#,
        )
        .unwrap();
        AssertionDocument {
            assertions: vec![AssertionEntry {
                uses: "builtin:assert-status@1".to_string(),
                with: serde_json::Map::from_iter([("expected".to_string(), Value::from(200))]),
                enabled: true,
            }],
            ..AssertionDocument::default()
        }
        .save_for_request(&request)
        .unwrap();

        let loaded = load_request_document(&request).unwrap();

        assert_eq!(loaded.pipeline.len(), 1);
        assert_eq!(loaded.pipeline[0].phase, PipelinePhase::AfterResponse);
    }

    #[test]
    fn saves_request_pipeline_in_sidecars_like_the_ide() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("project.json"), "{}").unwrap();
        let request_path = dir.path().join("saved.request.json");
        let request = RequestDocument::parse(
            r#"{
                "formatVersion": 1,
                "kind": "request",
                "meta": {"id": "saved", "name": "Saved"},
                "request": {"method": "GET", "url": "https://example.test"},
                "pipeline": [
                    {
                        "phase": "afterResponse",
                        "use": "builtin:assert-status@1",
                        "with": {"expected": 200}
                    },
                    {
                        "phase": "beforeRequest",
                        "use": "project:hooks/auth",
                        "with": {}
                    }
                ]
            }"#,
        )
        .unwrap();

        let (saved, assertions, hooks, revision) = save_request_document(
            &request_path,
            request,
            AssertionDocument::default(),
            HookDocument::default(),
            true,
        )
        .unwrap();

        assert!(saved.pipeline.is_empty());
        assert_eq!(revision, request_revision(&request_path).unwrap());
        assert_eq!(assertions.assertions.len(), 1);
        assert_eq!(hooks.hooks.len(), 1);
        assert_eq!(
            load_request_document(&request_path).unwrap().pipeline.len(),
            2
        );
    }

    #[test]
    fn invalid_editor_text_uses_the_shared_request_commit() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("project.json"), "{}").unwrap();
        let request = dir.path().join("invalid.request.json");
        let assertions = AssertionDocument {
            assertions: vec![AssertionEntry {
                uses: "builtin:assert-status@1".to_string(),
                with: serde_json::Map::from_iter([("expected".to_string(), Value::from(200))]),
                enabled: true,
            }],
            ..AssertionDocument::default()
        };

        let revision = save_request_text(
            &request,
            "{ invalid",
            &assertions,
            &HookDocument::default(),
            true,
        )
        .unwrap();

        assert_eq!(std::fs::read_to_string(&request).unwrap(), "{ invalid");
        assert!(assertions_path(&request).is_file());
        assert_eq!(revision, request_revision(&request).unwrap());
    }

    #[test]
    fn revision_checked_saves_are_serialized_across_writers() {
        use std::sync::{Arc, Barrier};

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("project.json"), "{}").unwrap();
        let request_path = dir.path().join("serialized.request.json");
        let base = RequestDocument::parse(
            r#"{
                "formatVersion": 1,
                "kind": "request",
                "meta": {"id": "serialized", "name": "Base"},
                "request": {"method": "GET", "url": "https://example.test"}
            }"#,
        )
        .unwrap();
        save_request_document(
            &request_path,
            base.clone(),
            AssertionDocument::default(),
            HookDocument::default(),
            true,
        )
        .unwrap();
        let revision = request_revision(&request_path).unwrap();
        let barrier = Arc::new(Barrier::new(3));

        let writers = ["Writer A", "Writer B"].map(|name| {
            let barrier = Arc::clone(&barrier);
            let path = request_path.clone();
            let revision = revision.clone();
            let mut document = base.clone();
            document.meta.name = name.to_string();
            std::thread::spawn(move || {
                barrier.wait();
                save_request_document_at_revision(
                    &path,
                    document,
                    AssertionDocument::default(),
                    HookDocument::default(),
                    &revision,
                )
            })
        });
        barrier.wait();
        let results = writers.map(|writer| writer.join().unwrap());

        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert!(results
            .iter()
            .filter_map(|result| result.as_ref().err())
            .any(|error| error.contains("revision conflict")));
    }

    #[test]
    fn request_lock_does_not_write_into_the_project() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project.json");
        let request = dir.path().join("read-only.request.json");
        std::fs::write(&project, "{}").unwrap();
        std::fs::write(&request, "{}").unwrap();
        let mut permissions = std::fs::metadata(&project).unwrap().permissions();
        permissions.set_readonly(true);
        std::fs::set_permissions(&project, permissions).unwrap();

        let lock = request_lock(&request).unwrap();

        assert!(!dir.path().join(".forge-local").exists());
        drop(lock);
    }

    #[cfg(unix)]
    #[test]
    fn request_loader_rejects_symlinked_sidecars() {
        use std::os::unix::fs::symlink;

        let project = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let request = project.path().join("safe.request.json");
        std::fs::write(
            &request,
            r#"{
                "formatVersion": 1,
                "kind": "request",
                "meta": {"id": "safe", "name": "Safe"},
                "request": {"method": "GET", "url": "https://example.test"}
            }"#,
        )
        .unwrap();
        let outside_assertions = outside.path().join("outside.assertions.json");
        std::fs::write(&outside_assertions, "{}").unwrap();
        symlink(outside_assertions, assertions_path(&request)).unwrap();

        assert!(load_request_document(&request)
            .unwrap_err()
            .contains("symbolic link"));
    }

    #[test]
    fn shipped_schema_accepts_an_assertion_document() {
        let schema: Value = serde_json::from_str(
            &std::fs::read_to_string(
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../../schemas/assertions-v1.schema.json"),
            )
            .unwrap(),
        )
        .unwrap();
        let validator = jsonschema::validator_for(&schema).unwrap();
        let document = serde_json::json!({
            "formatVersion": 1,
            "kind": "assertions",
            "assertions": [{
                "use": "builtin:assert-status@1",
                "with": {"expected": 200}
            }]
        });

        assert!(validator.is_valid(&document));
    }
}
