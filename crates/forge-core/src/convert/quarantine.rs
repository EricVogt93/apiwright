use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::store::{load_json, save_json, StoreError, StoreResult};

use super::common::encode_lower_hex;

pub const IMPORT_QUARANTINE_PATH: &str = "imports/quarantine/manifest.json";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportQuarantineManifest {
    pub format_version: u32,
    pub kind: String,
    pub entries: Vec<ImportQuarantineEntry>,
}

impl ImportQuarantineManifest {
    pub fn new(entries: Vec<ImportQuarantineEntry>) -> Self {
        Self {
            format_version: 1,
            kind: "import-quarantine".to_string(),
            entries,
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.format_version != 1 {
            return Err(format!(
                "unsupported quarantine format version {}",
                self.format_version
            ));
        }
        if self.kind != "import-quarantine" {
            return Err("invalid quarantine manifest kind".to_string());
        }
        if self.entries.iter().any(|entry| entry.id.is_empty()) {
            return Err("quarantine entries require stable IDs".to_string());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportQuarantineEntry {
    pub id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub import_key: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub source_identity: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub source_fingerprint: String,
    pub source_format: ImportSourceFormat,
    pub category: QuarantineCategory,
    pub disposition: QuarantineDisposition,
    pub source_path: String,
    pub source_index: usize,
    pub script: String,
    pub reason: String,
    #[serde(default)]
    pub comment: String,
}

impl ImportQuarantineEntry {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        source_format: ImportSourceFormat,
        category: QuarantineCategory,
        disposition: QuarantineDisposition,
        source_path: impl Into<String>,
        source_index: usize,
        script: impl Into<String>,
        reason: impl Into<String>,
    ) -> Self {
        let source_path = source_path.into();
        let script = script.into();
        let mut entry = Self {
            id: String::new(),
            import_key: String::new(),
            source_identity: format!("{source_path}#script-{source_index}"),
            source_fingerprint: content_fingerprint(&script),
            source_format,
            category,
            disposition,
            source_path,
            source_index,
            script,
            reason: reason.into(),
            comment: String::new(),
        };
        entry.refresh_id();
        entry
    }

    pub fn with_import_key(mut self, import_key: impl Into<String>) -> Self {
        self.import_key = import_key.into();
        self.refresh_id();
        self
    }

    pub fn with_source_identity(mut self, source_identity: impl Into<String>) -> Self {
        self.source_identity = source_identity.into();
        self.refresh_id();
        self
    }

    fn refresh_id(&mut self) {
        let mut hash = Sha256::new();
        hash.update(b"apiwright-import-quarantine-v1\0");
        hash.update(self.import_key.as_bytes());
        hash.update(b"\0");
        hash.update(self.source_format.as_str().as_bytes());
        hash.update(b"\0");
        hash.update(self.source_identity.as_bytes());
        hash.update(b"\0");
        hash.update(self.category.as_str().as_bytes());
        self.id = format!("q-{}", encode_lower_hex(&hash.finalize()));
    }
}

fn content_fingerprint(script: &str) -> String {
    format!(
        "sha256:{}",
        encode_lower_hex(&Sha256::digest(script.as_bytes()))
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ImportSourceFormat {
    Bruno,
    BrunoOpenCollection,
    Postman,
}

impl ImportSourceFormat {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Bruno => "bruno",
            Self::BrunoOpenCollection => "bruno-open-collection",
            Self::Postman => "postman",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Bruno => "Bruno",
            Self::BrunoOpenCollection => "Bruno OpenCollection",
            Self::Postman => "Postman",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum QuarantineCategory {
    Assertion,
    BeforeRequest,
    AfterResponse,
    BeforeEach,
    AfterEach,
}

impl QuarantineCategory {
    pub const ALL: [Self; 5] = [
        Self::Assertion,
        Self::BeforeRequest,
        Self::AfterResponse,
        Self::BeforeEach,
        Self::AfterEach,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Assertion => "assertion",
            Self::BeforeRequest => "before-request",
            Self::AfterResponse => "after-response",
            Self::BeforeEach => "before-each",
            Self::AfterEach => "after-each",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Assertion => "Assertions",
            Self::BeforeRequest => "Before request",
            Self::AfterResponse => "After response",
            Self::BeforeEach => "Before each",
            Self::AfterEach => "After each",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum QuarantineDisposition {
    Blocked,
    ReviewRequired,
}

impl QuarantineDisposition {
    pub fn label(self) -> &'static str {
        match self {
            Self::Blocked => "Blocked",
            Self::ReviewRequired => "Review required",
        }
    }
}

pub fn quarantine_path(root: &Path) -> PathBuf {
    root.join(IMPORT_QUARANTINE_PATH)
}

pub fn load_import_quarantine(root: &Path) -> StoreResult<Option<ImportQuarantineManifest>> {
    let path = quarantine_path(root);
    if !path.exists() {
        return Ok(None);
    }
    let manifest: ImportQuarantineManifest = load_json(&path)?;
    manifest.validate().map_err(|message| StoreError::Io {
        path,
        source: std::io::Error::new(std::io::ErrorKind::InvalidData, message),
    })?;
    Ok(Some(manifest))
}

pub fn save_import_quarantine(root: &Path, manifest: &ImportQuarantineManifest) -> StoreResult<()> {
    manifest.validate().map_err(|message| StoreError::Io {
        path: quarantine_path(root),
        source: std::io::Error::new(std::io::ErrorKind::InvalidData, message),
    })?;
    save_json(&quarantine_path(root), manifest)
}

pub fn merge_import_quarantine(
    root: &Path,
    entries: impl IntoIterator<Item = ImportQuarantineEntry>,
) -> StoreResult<ImportQuarantineManifest> {
    let mut merged =
        load_import_quarantine(root)?.unwrap_or_else(|| ImportQuarantineManifest::new(Vec::new()));
    let existing = merged
        .entries
        .drain(..)
        .map(|entry| (entry.id.clone(), entry))
        .collect::<BTreeMap<_, _>>();
    let mut by_id = existing;
    for mut entry in entries {
        if let Some(previous) = by_id.remove(&entry.id) {
            if previous.source_fingerprint == entry.source_fingerprint {
                entry.script = previous.script;
            }
            entry.comment = previous.comment;
        }
        by_id.insert(entry.id.clone(), entry);
    }
    merged.entries = by_id.into_values().collect();
    save_import_quarantine(root, &merged)?;
    Ok(merged)
}

pub fn sync_import_quarantine(
    root: &Path,
    import_key: &str,
    entries: impl IntoIterator<Item = ImportQuarantineEntry>,
) -> StoreResult<ImportQuarantineManifest> {
    let entries = entries
        .into_iter()
        .map(|entry| entry.with_import_key(import_key))
        .collect::<Vec<_>>();
    let mut manifest =
        load_import_quarantine(root)?.unwrap_or_else(|| ImportQuarantineManifest::new(Vec::new()));
    let previous = manifest
        .entries
        .iter()
        .filter(|entry| entry.import_key == import_key)
        .map(|entry| (entry.id.clone(), entry.clone()))
        .collect::<BTreeMap<_, _>>();
    let mut existing = manifest
        .entries
        .drain(..)
        .filter(|entry| entry.import_key != import_key)
        .map(|entry| (entry.id.clone(), entry))
        .collect::<BTreeMap<_, _>>();
    for mut entry in entries {
        if let Some(previous) = previous.get(&entry.id) {
            if previous.source_fingerprint == entry.source_fingerprint {
                entry.script.clone_from(&previous.script);
            }
            entry.comment.clone_from(&previous.comment);
        }
        existing.insert(entry.id.clone(), entry);
    }
    manifest.entries = existing.into_values().collect();
    let path = quarantine_path(root);
    if manifest.entries.is_empty() {
        if path.exists() {
            std::fs::remove_file(&path).map_err(|source| StoreError::Io { path, source })?;
        }
        return Ok(manifest);
    }
    save_import_quarantine(root, &manifest)?;
    Ok(manifest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_preserves_review_edits_and_stable_ids() {
        let root = tempfile::tempdir().unwrap();
        let entry = ImportQuarantineEntry::new(
            ImportSourceFormat::Postman,
            QuarantineCategory::BeforeRequest,
            QuarantineDisposition::ReviewRequired,
            "Users/Create",
            1,
            "pm.variables.set('id', 1);",
            "Postman scripts require review",
        );
        merge_import_quarantine(root.path(), [entry.clone()]).unwrap();
        let mut manifest = load_import_quarantine(root.path()).unwrap().unwrap();
        manifest.entries[0].script = "// reviewed".to_string();
        manifest.entries[0].comment = "Approved by API team".to_string();
        save_import_quarantine(root.path(), &manifest).unwrap();

        merge_import_quarantine(root.path(), [entry.clone()]).unwrap();
        let merged = load_import_quarantine(root.path()).unwrap().unwrap();
        assert_eq!(merged.entries[0].id, entry.id);
        assert_eq!(merged.entries[0].script, "// reviewed");
        assert_eq!(merged.entries[0].comment, "Approved by API team");
    }

    #[test]
    fn merge_refreshes_changed_source_but_preserves_review_comment() {
        let root = tempfile::tempdir().unwrap();
        let entry = ImportQuarantineEntry::new(
            ImportSourceFormat::Postman,
            QuarantineCategory::BeforeRequest,
            QuarantineDisposition::ReviewRequired,
            "Users/Create",
            1,
            "pm.variables.set('id', 1);",
            "Postman scripts require review",
        )
        .with_import_key("payments");
        merge_import_quarantine(root.path(), [entry.clone()]).unwrap();
        let mut manifest = load_import_quarantine(root.path()).unwrap().unwrap();
        manifest.entries[0].script = "// local review edit".to_string();
        manifest.entries[0].comment = "Check this change".to_string();
        save_import_quarantine(root.path(), &manifest).unwrap();

        let changed = ImportQuarantineEntry::new(
            ImportSourceFormat::Postman,
            QuarantineCategory::BeforeRequest,
            QuarantineDisposition::ReviewRequired,
            "Users/Create",
            1,
            "pm.variables.set('id', 2);",
            "Updated reason",
        )
        .with_import_key("payments");
        assert_eq!(entry.id, changed.id);
        merge_import_quarantine(root.path(), [changed]).unwrap();

        let merged = load_import_quarantine(root.path()).unwrap().unwrap();
        assert_eq!(merged.entries[0].script, "pm.variables.set('id', 2);");
        assert_eq!(merged.entries[0].comment, "Check this change");
        assert_eq!(merged.entries[0].reason, "Updated reason");
    }

    #[test]
    fn import_keys_namespace_identical_source_locations() {
        let first = ImportQuarantineEntry::new(
            ImportSourceFormat::Postman,
            QuarantineCategory::Assertion,
            QuarantineDisposition::ReviewRequired,
            "Users/Get",
            1,
            "pm.test('ok', () => {});",
            "review",
        )
        .with_import_key("first");
        let second = first.clone().with_import_key("second");

        assert_ne!(first.id, second.id);
    }

    #[test]
    fn sync_removes_stale_entries_only_for_the_selected_import() {
        let root = tempfile::tempdir().unwrap();
        let entry = |path: &str, script: &str| {
            ImportQuarantineEntry::new(
                ImportSourceFormat::Postman,
                QuarantineCategory::Assertion,
                QuarantineDisposition::ReviewRequired,
                path,
                1,
                script,
                "review",
            )
        };
        sync_import_quarantine(root.path(), "first", [entry("old", "old")]).unwrap();
        sync_import_quarantine(root.path(), "second", [entry("other", "other")]).unwrap();

        sync_import_quarantine(root.path(), "first", [entry("new", "new")]).unwrap();
        let manifest = load_import_quarantine(root.path()).unwrap().unwrap();
        assert_eq!(manifest.entries.len(), 2);
        assert!(!manifest
            .entries
            .iter()
            .any(|entry| entry.source_path == "old"));
        assert!(manifest
            .entries
            .iter()
            .any(|entry| entry.source_path == "new"));
        assert!(manifest
            .entries
            .iter()
            .any(|entry| entry.source_path == "other"));
    }

    #[test]
    fn load_rejects_files_that_are_not_import_quarantine_manifests() {
        let root = tempfile::tempdir().unwrap();
        let path = quarantine_path(root.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, r#"{"formatVersion":1,"kind":"other","entries":[]}"#).unwrap();

        assert!(load_import_quarantine(root.path()).is_err());
    }
}
