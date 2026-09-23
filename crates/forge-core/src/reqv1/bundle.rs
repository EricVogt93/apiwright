//! Lossless request/folder export and import.
//!
//! JSON bundles keep UTF-8 files readable and encode only binary files. A
//! cURL export is an executable shell script with the exact bundle embedded
//! in comments, so importing it restores assertions, hooks and properties.

use std::collections::{BTreeSet, VecDeque};
use std::io::Write;
use std::path::{Component, Path, PathBuf};

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use walkdir::{DirEntry, WalkDir};

use super::model::{Binding, BodySpec, BodyType, MockDef, ProjectConfig, RequestDocument};
use super::refs::{RefResolver, RefScheme};
use super::tickets::effective_ticket;
use super::{assertions_path, effective_environment, effective_openapi, hooks_path, load_project};

const BUNDLE_FORMAT: &str = "forge.bundle";
const BUNDLE_VERSION: u32 = 1;
const CURL_BUNDLE_BEGIN: &str = "# forge-bundle-v1: begin";
const CURL_BUNDLE_END: &str = "# forge-bundle-v1: end";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BundleFormat {
    Json,
    Curl,
}

impl BundleFormat {
    pub fn extension(self) -> &'static str {
        match self {
            Self::Json => "forge.json",
            Self::Curl => "forge.sh",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportSummary {
    pub files: usize,
    pub requests: usize,
    pub output: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportSummary {
    pub files: Vec<PathBuf>,
    /// An existing destination `project.json` was kept instead of replaced.
    pub preserved_project_config: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ForgeBundle {
    format: String,
    format_version: u32,
    kind: BundleKind,
    source: String,
    files: Vec<BundleFile>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum BundleKind {
    Request,
    Folder,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BundleFile {
    path: String,
    encoding: FileEncoding,
    content: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum FileEncoding {
    Utf8,
    Base64,
}

/// Export one `*.request.json` (including its sidecars) or a complete folder.
/// Existing output files are never overwritten.
pub fn export_bundle(
    project_root: &Path,
    source: &Path,
    format: BundleFormat,
    output: &Path,
) -> Result<ExportSummary, String> {
    ensure_no_symlink_components(output)?;
    if output.exists() {
        return Err(format!("{} already exists", output.display()));
    }
    let bundle = build_bundle(project_root, source)?;
    let requests = bundle
        .files
        .iter()
        .filter(|file| file.path.ends_with(".request.json"))
        .count();
    let rendered = match format {
        BundleFormat::Json => render_json(&bundle)?,
        BundleFormat::Curl => render_curl(&bundle)?,
    };
    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
    }
    ensure_no_symlink_components(output)?;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output)
        .map_err(|error| format!("cannot create {}: {error}", output.display()))?;
    file.write_all(rendered.as_bytes())
        .map_err(|error| format!("cannot write {}: {error}", output.display()))?;
    Ok(ExportSummary {
        files: bundle.files.len(),
        requests,
        output: output.to_path_buf(),
    })
}

/// Import a ApiWright JSON bundle or a cURL script exported by ApiWright. Bundle
/// paths are restored below `destination`; collisions abort before writing.
pub fn import_bundle(input: &Path, destination: &Path) -> Result<ImportSummary, String> {
    let text = std::fs::read_to_string(input)
        .map_err(|error| format!("cannot read {}: {error}", input.display()))?;
    let bundle = parse_bundle(&text)?;
    validate_bundle(&bundle)?;

    let decoded = bundle
        .files
        .iter()
        .map(|file| {
            let relative = safe_relative_path(&file.path)?;
            let bytes = match file.encoding {
                FileEncoding::Utf8 => file.content.as_bytes().to_vec(),
                FileEncoding::Base64 => BASE64
                    .decode(&file.content)
                    .map_err(|error| format!("invalid base64 in {}: {error}", file.path))?,
            };
            Ok((relative, bytes))
        })
        .collect::<Result<Vec<_>, String>>()?;

    ensure_no_symlink_components(destination)?;
    let project_target = destination.join("project.json");
    let preserve_project = match std::fs::symlink_metadata(&project_target) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(format!(
                "refusing symbolic link {}",
                project_target.display()
            ));
        }
        Ok(metadata) if !metadata.is_file() => {
            return Err(format!(
                "{} is not a regular file",
                project_target.display()
            ));
        }
        Ok(_) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => {
            return Err(format!(
                "cannot inspect {}: {error}",
                project_target.display()
            ))
        }
    };
    let mut paths = BTreeSet::new();
    for (relative, _) in &decoded {
        if preserve_project && relative == Path::new("project.json") {
            continue;
        }
        if !paths.insert(relative.clone()) {
            return Err(format!(
                "bundle contains duplicate path {}",
                relative.display()
            ));
        }
        let target = destination.join(relative);
        ensure_no_symlink_components(&target)?;
        if target.exists() {
            return Err(format!("{} already exists", target.display()));
        }
    }

    std::fs::create_dir_all(destination)
        .map_err(|error| format!("cannot create {}: {error}", destination.display()))?;
    ensure_no_symlink_components(destination)?;
    let mut written = Vec::new();
    for (relative, bytes) in decoded {
        if preserve_project && relative == Path::new("project.json") {
            continue;
        }
        let target = destination.join(relative);
        let result = (|| {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
                ensure_no_symlink_components(parent)?;
            }
            ensure_no_symlink_components(&target)?;
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&target)
                .map_err(|error| format!("cannot create {}: {error}", target.display()))?;
            file.write_all(&bytes)
                .map_err(|error| format!("cannot write {}: {error}", target.display()))
        })();
        if let Err(error) = result {
            for path in written.iter().rev() {
                let _ = std::fs::remove_file(path);
            }
            return Err(error);
        }
        written.push(target);
    }
    Ok(ImportSummary {
        files: written,
        preserved_project_config: preserve_project,
    })
}

fn build_bundle(project_root: &Path, source: &Path) -> Result<ForgeBundle, String> {
    let root = project_root
        .canonicalize()
        .map_err(|error| format!("cannot resolve {}: {error}", project_root.display()))?;
    ensure_no_symlink_components(source)?;
    let source = source
        .canonicalize()
        .map_err(|error| format!("cannot resolve {}: {error}", source.display()))?;
    if !source.starts_with(&root) {
        return Err(format!("{} is outside the project", source.display()));
    }

    let (kind, paths) = if source.is_dir() {
        let mut files = collect_folder_files(&source)?;
        let requests = files
            .iter()
            .filter(|path| is_request_file(path))
            .cloned()
            .collect::<Vec<_>>();
        if !requests.is_empty() {
            files.extend(collect_request_dependencies(&root, &requests)?);
        }
        (BundleKind::Folder, files)
    } else if is_request_file(&source) {
        (
            BundleKind::Request,
            collect_request_dependencies(&root, std::slice::from_ref(&source))?,
        )
    } else {
        return Err("export source must be a folder or *.request.json".to_string());
    };
    if paths.is_empty() {
        return Err("export scope contains no files".to_string());
    }

    let mut files = paths
        .into_iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .map(|path| {
            let relative = path
                .strip_prefix(&root)
                .map_err(|_| format!("{} is outside export scope", path.display()))?;
            let bytes = std::fs::read(&path)
                .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
            let (encoding, content) = match String::from_utf8(bytes) {
                Ok(text) => (FileEncoding::Utf8, text),
                Err(error) => (FileEncoding::Base64, BASE64.encode(error.into_bytes())),
            };
            Ok(BundleFile {
                path: portable_path(relative)?,
                encoding,
                content,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    files.sort_by(|left, right| left.path.cmp(&right.path));

    Ok(ForgeBundle {
        format: BUNDLE_FORMAT.to_string(),
        format_version: BUNDLE_VERSION,
        kind,
        source: portable_path(source.strip_prefix(&root).unwrap_or(Path::new(".")))?,
        files,
    })
}

fn collect_folder_files(source: &Path) -> Result<Vec<PathBuf>, String> {
    let mut files = Vec::new();
    for entry in WalkDir::new(source)
        .follow_links(false)
        .into_iter()
        .filter_entry(include_entry)
    {
        let entry = entry.map_err(|error| format!("cannot scan {}: {error}", source.display()))?;
        if entry.file_type().is_symlink() {
            return Err(format!("cannot export symlink {}", entry.path().display()));
        }
        if entry.file_type().is_file() && include_file(entry.path()) {
            files.push(entry.into_path());
        }
    }
    files.sort();
    Ok(files)
}

fn collect_request_files(request: &Path) -> Result<Vec<PathBuf>, String> {
    let mut candidates = vec![
        request.to_path_buf(),
        assertions_path(request),
        hooks_path(request),
    ];
    let name = request
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_default();
    for suffix in [".forge-jira", ".forge-environment", ".forge-openapi"] {
        candidates.push(request.with_file_name(format!(".{name}{suffix}")));
    }
    let mut files = Vec::new();
    for path in candidates {
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(format!("cannot inspect {}: {error}", path.display())),
        };
        if metadata.file_type().is_symlink() {
            return Err(format!("cannot export symlink {}", path.display()));
        }
        if !metadata.is_file() {
            return Err(format!("expected a file at {}", path.display()));
        }
        if include_file(&path) {
            files.push(path);
        }
    }
    files.sort();
    files.dedup();
    Ok(files)
}

/// Build a project-root-relative dependency closure for request exports.
/// Secret stores are never copied. Keeping original project paths lets a
/// restored request resolve its aliases and relative refs without rewriting
/// the saved documents.
fn collect_request_dependencies(root: &Path, requests: &[PathBuf]) -> Result<Vec<PathBuf>, String> {
    let project_path = root.join("project.json");
    match std::fs::symlink_metadata(&project_path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(format!("cannot export symlink {}", project_path.display()));
        }
        Ok(metadata) if !metadata.is_file() => {
            return Err(format!("expected a file at {}", project_path.display()));
        }
        _ => {}
    }
    let project = load_project(root).map_err(|diagnostic| diagnostic.message)?;
    let resolver =
        RefResolver::new(root, &project).map_err(|mut errors| errors.0.remove(0).message)?;

    let mut files = BTreeSet::new();
    let mut visited = BTreeSet::new();
    let mut pending = VecDeque::new();
    for request in requests {
        pending.push_back(request.clone());
    }
    if !requests.is_empty() {
        if project_path.is_file() {
            files.insert(project_path);
        }
        if let Some(auth) = &project.auth {
            let relative = Path::new(&auth.request);
            if relative.is_absolute()
                || relative
                    .components()
                    .any(|component| !matches!(component, Component::Normal(_)))
            {
                return Err("project auth request must be a project-relative path".to_string());
            }
            pending.push_back(root.join(relative));
        }
    }

    while let Some(document_path) = pending.pop_front() {
        ensure_no_symlink_components(&document_path)?;
        let canonical = document_path
            .canonicalize()
            .map_err(|error| format!("cannot resolve {}: {error}", document_path.display()))?;
        if !canonical.starts_with(root) {
            return Err(format!(
                "{} is outside the project",
                document_path.display()
            ));
        }
        if !visited.insert(canonical.clone()) {
            continue;
        }
        if !canonical.is_file() {
            return Err(format!("expected a file at {}", canonical.display()));
        }
        files.insert(canonical.clone());

        if is_request_file(&canonical) {
            for related in collect_request_files(&canonical)? {
                files.insert(related.clone());
                if related == assertions_path(&canonical) || related == hooks_path(&canonical) {
                    pending.push_back(related);
                }
            }
            if let Some(selection) = effective_environment(root, &canonical)? {
                add_existing_file(
                    root,
                    &selection_file(
                        &selection.source,
                        ".forge-environment",
                        ".forge-environment",
                    ),
                    &mut files,
                    &mut pending,
                )?;
                add_existing_file(
                    root,
                    &root
                        .join("environments")
                        .join(format!("{}.json", selection.value)),
                    &mut files,
                    &mut pending,
                )?;
            }
            if let Some(selection) = effective_openapi(root, &canonical)? {
                add_existing_file(
                    root,
                    &selection_file(&selection.source, ".forge-openapi", ".forge-openapi"),
                    &mut files,
                    &mut pending,
                )?;
                add_existing_file(root, &root.join(selection.value), &mut files, &mut pending)?;
            }
            if let Some(selection) = effective_ticket(root, &canonical)? {
                let sidecar = if selection.source.is_dir() {
                    selection.source.join(".forge-jira")
                } else {
                    let name = selection
                        .source
                        .file_name()
                        .map(|name| name.to_string_lossy())
                        .unwrap_or_default();
                    selection
                        .source
                        .with_file_name(format!(".{name}.forge-jira"))
                };
                add_optional_existing_file(root, &sidecar, &mut files, &mut pending)?;
            }
        }

        let text = match std::fs::read_to_string(&canonical) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::InvalidData => continue,
            Err(error) => return Err(format!("cannot read {}: {error}", canonical.display())),
        };
        for raw in references_in_document(&canonical, &text)? {
            let descriptor = resolver
                .resolve(&raw, canonical.parent().unwrap_or(root))
                .map_err(|diagnostic| {
                    format!(
                        "cannot export reference {raw:?} from {}: {}",
                        canonical.display(),
                        diagnostic.message
                    )
                })?;
            if descriptor.scheme == RefScheme::Builtin {
                continue;
            }
            let dependency = PathBuf::from(descriptor.address);
            if !dependency.starts_with(root) {
                return Err(format!("reference {raw:?} resolves outside the project"));
            }
            if !include_file(&dependency) {
                return Err(format!(
                    "reference {raw:?} points at a secret store; secret files are never included in bundles"
                ));
            }
            if !dependency.is_file() {
                return Err(format!(
                    "referenced file {} does not exist",
                    dependency.display()
                ));
            }
            let stem = dependency
                .file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
                .unwrap_or_default();
            let metadata = dependency.with_file_name(format!("{stem}.meta.json"));
            add_optional_existing_file(root, &metadata, &mut files, &mut pending)?;
            if dependency
                .extension()
                .is_some_and(|extension| extension == "json")
                && !dependency
                    .file_name()
                    .is_some_and(|name| name.to_string_lossy().ends_with(".schema.json"))
            {
                let schema = dependency.with_file_name(format!("{stem}.schema.json"));
                add_optional_existing_file(root, &schema, &mut files, &mut pending)?;
            }
            pending.push_back(dependency);
        }

        if matches!(
            canonical.extension().and_then(|value| value.to_str()),
            Some("js" | "ts")
        ) {
            pending.extend(module_dependencies(root, &canonical, &text)?);
        }
    }

    Ok(files.into_iter().collect())
}

fn add_optional_existing_file(
    root: &Path,
    path: &Path,
    files: &mut BTreeSet<PathBuf>,
    pending: &mut VecDeque<PathBuf>,
) -> Result<(), String> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => add_existing_file(root, path, files, pending),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("cannot inspect {}: {error}", path.display())),
    }
}

fn add_existing_file(
    root: &Path,
    path: &Path,
    files: &mut BTreeSet<PathBuf>,
    pending: &mut VecDeque<PathBuf>,
) -> Result<(), String> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            Err(format!("cannot export symlink {}", path.display()))
        }
        Ok(metadata) if !metadata.is_file() => {
            Err(format!("expected a file at {}", path.display()))
        }
        Ok(_) => {
            ensure_no_symlink_components(path)?;
            let canonical = path
                .canonicalize()
                .map_err(|error| format!("cannot resolve {}: {error}", path.display()))?;
            if !canonical.starts_with(root) {
                return Err(format!("{} is outside the project", path.display()));
            }
            files.insert(canonical.clone());
            pending.push_back(canonical);
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Err(format!(
            "required project file {} does not exist",
            path.display()
        )),
        Err(error) => Err(format!("cannot inspect {}: {error}", path.display())),
    }
}

fn selection_file(source: &Path, folder_name: &str, file_suffix: &str) -> PathBuf {
    if source.is_dir() {
        source.join(folder_name)
    } else {
        let name = source
            .file_name()
            .map(|name| name.to_string_lossy())
            .unwrap_or_default();
        source.with_file_name(format!(".{name}{file_suffix}"))
    }
}

fn references_in_document(path: &Path, text: &str) -> Result<Vec<String>, String> {
    if is_request_file(path) {
        let document = RequestDocument::parse(text)
            .map_err(|error| format!("invalid request {}: {error}", path.display()))?;
        let mut references = Vec::new();
        for binding in document.bindings.values().chain(document.matrix.values()) {
            match binding {
                Binding::Ref(reference) => references.push(reference.reference.clone()),
                Binding::Use(use_binding) => references.push(use_binding.uses.clone()),
                Binding::Value(_) => {}
            }
        }
        if let Some(BodySpec::Ref(reference)) = &document.request.body {
            references.push(reference.reference.clone());
        }
        for entry in &document.pipeline {
            references.push(entry.uses.clone());
        }
        match &document.mock {
            Some(MockDef::Static(mock)) => {
                if let Some(BodySpec::Ref(reference)) = &mock.body {
                    references.push(reference.reference.clone());
                }
            }
            Some(MockDef::Dynamic(mock)) => references.push(mock.uses.clone()),
            None => {}
        }
        return Ok(references);
    }

    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("");
    if !name.ends_with(".assertions.json") && !name.ends_with(".hooks.json") {
        return Ok(Vec::new());
    }
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        // Preserve malformed sidecars byte-for-byte; their references cannot
        // be discovered, but bundling them should not mutate or discard them.
        return Ok(Vec::new());
    };
    let key = if name.ends_with(".assertions.json") {
        "assertions"
    } else {
        "hooks"
    };
    Ok(value
        .get(key)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.get("use").and_then(Value::as_str))
        .map(str::to_string)
        .collect())
}

fn module_dependencies(root: &Path, source: &Path, text: &str) -> Result<Vec<PathBuf>, String> {
    use std::sync::OnceLock;

    static IMPORT_RE: OnceLock<regex::Regex> = OnceLock::new();
    static REQUIRE_RE: OnceLock<regex::Regex> = OnceLock::new();
    let import_re = IMPORT_RE.get_or_init(|| {
        regex::Regex::new(r#"(?m)\b(?:import|export)\s+(?:[^;\n]*?\s+from\s*)?['\"]([^'\"]+)['\"]"#)
            .expect("static import regex is valid")
    });
    let require_re = REQUIRE_RE.get_or_init(|| {
        regex::Regex::new(r#"\brequire\s*\(\s*['\"]([^'\"]+)['\"]\s*\)"#)
            .expect("static require regex is valid")
    });
    let mut paths = BTreeSet::new();
    for captures in import_re
        .captures_iter(text)
        .chain(require_re.captures_iter(text))
    {
        let specifier = captures
            .get(1)
            .map(|capture| capture.as_str())
            .unwrap_or("");
        if !specifier.starts_with('.') {
            continue;
        }
        let unresolved = source.parent().unwrap_or(root).join(specifier);
        let candidates = if unresolved.extension().is_some() {
            vec![unresolved]
        } else {
            vec![
                unresolved.with_extension("js"),
                unresolved.with_extension("ts"),
                unresolved.with_extension("json"),
                unresolved.join("index.js"),
                unresolved.join("index.ts"),
            ]
        };
        let Some(candidate) = candidates.into_iter().find(|candidate| candidate.is_file()) else {
            return Err(format!(
                "relative module {specifier:?} imported by {} does not exist",
                source.display()
            ));
        };
        ensure_no_symlink_components(&candidate)?;
        let canonical = candidate
            .canonicalize()
            .map_err(|error| format!("cannot resolve {}: {error}", candidate.display()))?;
        if !canonical.starts_with(root) {
            return Err(format!("module import {specifier:?} escapes the project"));
        }
        paths.insert(canonical);
    }
    Ok(paths.into_iter().collect())
}

/// Reject symlinks in every existing component of a destination or source
/// path before bundle reads and writes can follow them.
fn ensure_no_symlink_components(path: &Path) -> Result<(), String> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|error| format!("cannot resolve current directory: {error}"))?
            .join(path)
    };
    let mut current = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::Prefix(prefix) => current.push(prefix.as_os_str()),
            Component::RootDir => current.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                current.pop();
            }
            Component::Normal(name) => {
                current.push(name);
                match std::fs::symlink_metadata(&current) {
                    Ok(metadata) if metadata.file_type().is_symlink() => {
                        return Err(format!("path contains symlink {}", current.display()));
                    }
                    Ok(_) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => {
                        return Err(format!("cannot inspect {}: {error}", current.display()));
                    }
                }
            }
        }
    }
    Ok(())
}

fn include_entry(entry: &DirEntry) -> bool {
    if entry.depth() == 0 || !entry.file_type().is_dir() {
        return true;
    }
    !matches!(
        entry.file_name().to_str(),
        Some(".git" | ".forge" | ".forge-local" | "node_modules" | "target")
    )
}

fn include_file(path: &Path) -> bool {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    name != ".env.local" && !name.ends_with(".secrets.json")
}

fn is_request_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.ends_with(".request.json"))
}

fn portable_path(path: &Path) -> Result<String, String> {
    let components = path
        .components()
        .filter_map(|component| match component {
            Component::Normal(value) => Some(Ok(value.to_string_lossy().into_owned())),
            Component::CurDir => None,
            _ => Some(Err(format!("unsafe bundle path {}", path.display()))),
        })
        .collect::<Result<Vec<_>, String>>()?;
    if components.is_empty() {
        Ok(".".to_string())
    } else {
        Ok(components.join("/"))
    }
}

fn safe_relative_path(path: &str) -> Result<PathBuf, String> {
    let path = Path::new(path);
    let mut safe = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(value) => safe.push(value),
            _ => return Err(format!("unsafe bundle path {}", path.display())),
        }
    }
    if safe.as_os_str().is_empty() {
        return Err("bundle file path must not be empty".to_string());
    }
    Ok(safe)
}

fn render_json(bundle: &ForgeBundle) -> Result<String, String> {
    let mut json = serde_json::to_string_pretty(bundle)
        .map_err(|error| format!("cannot serialize bundle: {error}"))?;
    json.push('\n');
    Ok(json)
}

fn render_curl(bundle: &ForgeBundle) -> Result<String, String> {
    let compact = serde_json::to_vec(bundle)
        .map_err(|error| format!("cannot serialize embedded bundle: {error}"))?;
    let encoded = BASE64.encode(compact);
    let mut output = String::from(
        "#!/usr/bin/env sh\n# Generated by ApiWright. Import this script to restore its full request metadata.\n",
    );
    output.push_str(CURL_BUNDLE_BEGIN);
    output.push('\n');
    for chunk in encoded.as_bytes().chunks(76) {
        output.push_str("# ");
        output.push_str(std::str::from_utf8(chunk).expect("base64 is ASCII"));
        output.push('\n');
    }
    output.push_str(CURL_BUNDLE_END);
    output.push_str("\nset -eu\n");

    let has_project_auth = bundle
        .files
        .iter()
        .find(|file| file.path == "project.json" && matches!(file.encoding, FileEncoding::Utf8))
        .and_then(|file| serde_json::from_str::<ProjectConfig>(&file.content).ok())
        .is_some_and(|project| project.auth.is_some());

    let mut request_count = 0;
    for file in &bundle.files {
        if !file.path.ends_with(".request.json") {
            continue;
        }
        let text = match file.encoding {
            FileEncoding::Utf8 => &file.content,
            FileEncoding::Base64 => {
                return Err(format!("request {} is not UTF-8", file.path));
            }
        };
        let request = RequestDocument::parse(text)
            .map_err(|error| format!("invalid request {}: {error}", file.path))?;
        output.push_str("\n# ");
        output.push_str(&file.path);
        output.push('\n');
        let command = request_curl(&request);
        if let Some(reason) = curl_blocker(&request, has_project_auth) {
            output.push_str("# Not run: ");
            output.push_str(reason);
            output.push('\n');
            for line in command.replace(" \\\n  ", " ").lines() {
                output.push_str("# ");
                output.push_str(line);
                output.push('\n');
            }
        } else {
            output.push_str(&command);
        }
        output.push('\n');
        request_count += 1;
    }
    if request_count == 0 {
        return Err("cURL export contains no request documents".to_string());
    }
    Ok(output)
}

fn request_curl(request: &RequestDocument) -> String {
    let (mut url, fragment) = match request.request.url.split_once('#') {
        Some((url, fragment)) => (url.to_string(), Some(fragment)),
        None => (request.request.url.clone(), None),
    };
    let query = request
        .request
        .query
        .iter()
        .filter(|item| item.enabled)
        .collect::<Vec<_>>();
    if !query.is_empty() {
        let mut first = !url.contains('?');
        for item in query {
            if first {
                url.push('?');
            } else if !url.ends_with('?') && !url.ends_with('&') {
                url.push('&');
            }
            first = false;
            url.push_str(
                &url::form_urlencoded::byte_serialize(item.name.as_bytes()).collect::<String>(),
            );
            url.push('=');
            url.push_str(
                &url::form_urlencoded::byte_serialize(item.value.as_bytes()).collect::<String>(),
            );
        }
    }
    if let Some(fragment) = fragment {
        url.push('#');
        url.push_str(fragment);
    }

    let mut headers = request
        .request
        .headers
        .iter()
        .filter(|header| header.enabled)
        .map(|header| (header.name.clone(), header.value.clone()))
        .collect::<Vec<_>>();
    let mut body = Vec::new();
    match request.request.body.as_ref() {
        None
        | Some(BodySpec::Inline(super::model::InlineBody {
            body_type: BodyType::None,
            ..
        })) => {}
        Some(BodySpec::Inline(inline)) => match inline.body_type {
            BodyType::Json => {
                if !headers
                    .iter()
                    .any(|(name, _)| name.eq_ignore_ascii_case("content-type"))
                {
                    headers.push(("Content-Type".to_string(), "application/json".to_string()));
                }
                if let Some(value) = &inline.value {
                    body.push(format!("--data-raw {}", shell_quote(&value.to_string())));
                }
            }
            BodyType::Text => {
                if let Some(value) = &inline.value {
                    let value = value
                        .as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| value.to_string());
                    body.push(format!("--data-raw {}", shell_quote(&value)));
                }
            }
            BodyType::Form => add_form_body(&mut body, inline.value.as_ref(), "--data-urlencode"),
            BodyType::None => {}
        },
        Some(BodySpec::Multipart(multipart)) => {
            for part in &multipart.parts {
                match part {
                    super::model::MultipartPart::Text {
                        name,
                        value,
                        filename,
                        content_type,
                        enabled,
                    } if *enabled => {
                        let mut value = format!("{name}={value}");
                        if let Some(filename) = filename {
                            value.push_str(&format!(";filename={filename}"));
                        }
                        if let Some(content_type) = content_type {
                            value.push_str(&format!(";type={content_type}"));
                        }
                        body.push(format!("--form {}", shell_quote(&value)));
                    }
                    super::model::MultipartPart::File {
                        name,
                        file,
                        filename,
                        content_type,
                        enabled,
                    } if *enabled => {
                        let mut value = format!("{name}=@{file}");
                        if let Some(filename) = filename {
                            value.push_str(&format!(";filename={filename}"));
                        }
                        if let Some(content_type) = content_type {
                            value.push_str(&format!(";type={content_type}"));
                        }
                        body.push(format!("--form {}", shell_quote(&value)));
                    }
                    _ => {}
                }
            }
        }
        Some(BodySpec::Binary(binary)) => body.push(format!(
            "--data-binary {}",
            shell_quote(&format!("@{}", binary.file))
        )),
        Some(BodySpec::Ref(reference)) => body.push(format!(
            "--data-binary {}",
            shell_quote(&format!("@{}", reference.reference))
        )),
    }

    let mut chunks = vec![
        "curl".to_string(),
        format!("--request {}", request.request.method.as_str()),
        shell_quote(&url),
    ];
    chunks.extend(
        headers
            .into_iter()
            .map(|(name, value)| format!("--header {}", shell_quote(&format!("{name}: {value}")))),
    );
    chunks.extend(body);
    chunks.join(" \\\n  ")
}

#[cfg(test)]
#[test]
fn request_curl_encodes_query_components_and_keeps_fragments_last() {
    let mut request = RequestDocument::parse(
        r#"{"formatVersion":1,"kind":"request","meta":{"id":"get","name":"Get"},"request":{"method":"GET","url":"https://example.test/items#section"}}"#,
    )
    .unwrap();
    request.request.query.push(super::model::HeaderSpec {
        name: "search term".to_string(),
        value: "red blue&green".to_string(),
        enabled: true,
    });

    let command = request_curl(&request);

    assert!(command.contains("https://example.test/items?search+term=red+blue%26green#section"));
}

fn curl_blocker(request: &RequestDocument, has_project_auth: bool) -> Option<&'static str> {
    if has_project_auth {
        return Some("project authentication is not resolved by a cURL export");
    }
    if !request.bindings.is_empty() || !request.matrix.is_empty() {
        return Some("request bindings or matrix values are not resolved by a cURL export");
    }
    if !request.pipeline.is_empty() {
        return Some("request pipeline steps are not executed by a cURL export");
    }
    if contains_template(&request.request.url)
        || request
            .request
            .headers
            .iter()
            .chain(request.request.query.iter())
            .any(|entry| contains_template(&entry.value))
    {
        return Some("project variables or secrets are not resolved by a cURL export");
    }
    match request.request.body.as_ref() {
        Some(BodySpec::Ref(_)) => {
            return Some("referenced request bodies are not resolved by a cURL export");
        }
        Some(BodySpec::Binary(_)) => {
            return Some("binary body paths are not made portable by a cURL export");
        }
        Some(BodySpec::Multipart(multipart)) => {
            if multipart.parts.iter().any(|part| {
                matches!(
                    part,
                    super::model::MultipartPart::File { enabled: true, .. }
                )
            }) {
                return Some("multipart file paths are not made portable by a cURL export");
            }
            if multipart.parts.iter().any(|part| match part {
                super::model::MultipartPart::Text { value, enabled, .. } => {
                    *enabled && contains_template(value)
                }
                super::model::MultipartPart::File { file, enabled, .. } => {
                    *enabled && contains_template(file)
                }
            }) {
                return Some("project variables or secrets are not resolved by a cURL export");
            }
        }
        Some(BodySpec::Inline(inline))
            if inline.value.as_ref().is_some_and(value_contains_template) =>
        {
            return Some("project variables or secrets are not resolved by a cURL export");
        }
        Some(BodySpec::Inline(_)) => {}
        None => {}
    }
    None
}

fn contains_template(value: &str) -> bool {
    value.contains("${") || value.contains("{{")
}

fn value_contains_template(value: &Value) -> bool {
    match value {
        Value::String(value) => contains_template(value),
        Value::Array(values) => values.iter().any(value_contains_template),
        Value::Object(values) => values.values().any(value_contains_template),
        Value::Null | Value::Bool(_) | Value::Number(_) => false,
    }
}

fn add_form_body(chunks: &mut Vec<String>, value: Option<&Value>, flag: &str) {
    let Some(Value::Object(fields)) = value else {
        return;
    };
    for (name, value) in fields {
        let value = value
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| value.to_string());
        chunks.push(format!(
            "{flag} {}",
            shell_quote(&format!("{name}={value}"))
        ));
    }
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r#"'"'"'"#))
}

fn parse_bundle(text: &str) -> Result<ForgeBundle, String> {
    if text.trim_start().starts_with('{') {
        return serde_json::from_str(text)
            .map_err(|error| format!("invalid ApiWright bundle: {error}"));
    }
    let mut inside = false;
    let mut encoded = String::new();
    for line in text.lines() {
        if line.trim() == CURL_BUNDLE_BEGIN {
            inside = true;
            continue;
        }
        if line.trim() == CURL_BUNDLE_END {
            inside = false;
            break;
        }
        if inside {
            let chunk = line
                .trim()
                .strip_prefix('#')
                .map(str::trim)
                .ok_or_else(|| "invalid embedded ApiWright bundle".to_string())?;
            encoded.push_str(chunk);
        }
    }
    if inside || encoded.is_empty() {
        return Err(
            "file is neither a ApiWright JSON bundle nor a ApiWright cURL export".to_string(),
        );
    }
    let json = BASE64
        .decode(encoded)
        .map_err(|error| format!("invalid embedded ApiWright bundle: {error}"))?;
    serde_json::from_slice(&json)
        .map_err(|error| format!("invalid embedded ApiWright bundle: {error}"))
}

fn validate_bundle(bundle: &ForgeBundle) -> Result<(), String> {
    if bundle.format != BUNDLE_FORMAT {
        return Err(format!("unsupported bundle format {}", bundle.format));
    }
    if bundle.format_version != BUNDLE_VERSION {
        return Err(format!(
            "unsupported bundle version {} (this build supports {BUNDLE_VERSION})",
            bundle.format_version
        ));
    }
    if bundle.files.is_empty() {
        return Err("bundle contains no files".to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const REQUEST: &str = r#"{
  "formatVersion": 1,
  "kind": "request",
  "meta": { "id": "users.get", "name": "Get user" },
  "request": {
    "method": "GET",
    "url": "https://api.example.com/users/${env.userId}",
    "headers": [{ "name": "Accept", "value": "application/json", "enabled": true }]
  }
}
"#;

    #[test]
    fn json_folder_roundtrip_preserves_nested_text_binary_and_sidecars() {
        let project = tempfile::tempdir().unwrap();
        let story = project.path().join("requests/SHOP-42");
        let nested = story.join("happy-path");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("get.request.json"), REQUEST).unwrap();
        std::fs::write(nested.join("get.assertions.json"), "{\"assertions\":[]}").unwrap();
        std::fs::write(story.join(".forge-jira"), "SHOP-42\n").unwrap();
        std::fs::write(story.join("payload.bin"), [0, 159, 146, 150]).unwrap();
        std::fs::write(story.join(".env.local"), "TOKEN=secret\n").unwrap();

        let output = project.path().join("story.forge.json");
        let summary = export_bundle(project.path(), &story, BundleFormat::Json, &output).unwrap();
        assert_eq!(summary.requests, 1);
        assert_eq!(summary.files, 4);

        let destination = tempfile::tempdir().unwrap();
        let imported = import_bundle(&output, destination.path()).unwrap();
        assert_eq!(imported.files.len(), 4);
        let restored = destination.path().join("requests/SHOP-42");
        assert_eq!(
            std::fs::read_to_string(restored.join("happy-path/get.request.json")).unwrap(),
            REQUEST
        );
        assert_eq!(
            std::fs::read(restored.join("payload.bin")).unwrap(),
            [0, 159, 146, 150]
        );
        assert!(!restored.join(".env.local").exists());
    }

    #[test]
    fn curl_request_roundtrip_restores_assertions_hooks_and_properties() {
        let project = tempfile::tempdir().unwrap();
        let request = project.path().join("get.request.json");
        std::fs::write(&request, REQUEST).unwrap();
        std::fs::write(project.path().join("get.assertions.json"), "assertions").unwrap();
        std::fs::write(project.path().join("get.hooks.json"), "hooks").unwrap();
        std::fs::write(
            project.path().join(".get.request.json.forge-openapi"),
            "openapi.yaml\n",
        )
        .unwrap();
        std::fs::write(project.path().join("openapi.yaml"), "openapi: 3.0.0\n").unwrap();

        let output = project.path().join("get.forge.sh");
        let summary = export_bundle(project.path(), &request, BundleFormat::Curl, &output).unwrap();
        assert_eq!(summary.files, 5);
        let script = std::fs::read_to_string(&output).unwrap();
        assert!(script.contains("# Not run: project variables or secrets are not resolved"));
        assert!(script.contains("# curl --request GET"));
        assert!(!script.contains("\ncurl --request GET"));
        assert!(script.contains(CURL_BUNDLE_BEGIN));

        let destination = tempfile::tempdir().unwrap();
        import_bundle(&output, destination.path()).unwrap();
        assert_eq!(
            std::fs::read_to_string(destination.path().join("get.assertions.json")).unwrap(),
            "assertions"
        );
        assert_eq!(
            std::fs::read_to_string(destination.path().join("get.hooks.json")).unwrap(),
            "hooks"
        );
        assert_eq!(
            std::fs::read_to_string(destination.path().join(".get.request.json.forge-openapi"))
                .unwrap(),
            "openapi.yaml\n"
        );
    }

    #[test]
    fn request_bundle_includes_linked_files_and_keeps_secret_store_out() {
        let project = tempfile::tempdir().unwrap();
        let request_dir = project.path().join("requests/orders");
        let data_dir = project.path().join("assets/fixtures");
        let hooks_dir = project.path().join("assets/hooks");
        let env_dir = project.path().join("environments");
        let contracts_dir = project.path().join("contracts");
        std::fs::create_dir_all(&request_dir).unwrap();
        std::fs::create_dir_all(&data_dir).unwrap();
        std::fs::create_dir_all(&hooks_dir).unwrap();
        std::fs::create_dir_all(&env_dir).unwrap();
        std::fs::create_dir_all(&contracts_dir).unwrap();
        std::fs::write(
            project.path().join("project.json"),
            r#"{"formatVersion":1,"aliases":{"@fixtures":"assets/fixtures/","@hooks":"assets/hooks/"}}"#,
        )
        .unwrap();
        let request = request_dir.join("get.request.json");
        std::fs::write(
            &request,
            r#"{"formatVersion":1,"kind":"request","meta":{"id":"get-order","name":"Get order"},"request":{"method":"POST","url":"https://example.test/orders","body":{"ref":"@fixtures/payload.json#/data","type":"json"}},"pipeline":[{"phase":"afterResponse","use":"@hooks/verify.js"}]}"#,
        )
        .unwrap();
        std::fs::write(data_dir.join("payload.json"), r#"{"data":{"id":42}}"#).unwrap();
        std::fs::write(
            data_dir.join("payload.schema.json"),
            r#"{"type":"object","required":["data"]}"#,
        )
        .unwrap();
        std::fs::write(
            data_dir.join("payload.meta.json"),
            r#"{"formatVersion":1,"title":"Order payload","kind":"data","parameters":[]}"#,
        )
        .unwrap();
        std::fs::write(
            hooks_dir.join("verify.js"),
            "import { check } from './shared.js';\nexport function run(ctx) { check(ctx); }\n",
        )
        .unwrap();
        std::fs::write(
            hooks_dir.join("shared.js"),
            "export const check = () => true;\n",
        )
        .unwrap();
        std::fs::write(request_dir.join(".forge-environment"), "dev\n").unwrap();
        std::fs::write(request_dir.join(".forge-jira"), "API-42\n").unwrap();
        std::fs::write(
            env_dir.join("dev.json"),
            r#"{"host":"https://example.test"}"#,
        )
        .unwrap();
        std::fs::write(
            request_dir.join(".forge-openapi"),
            "contracts/orders.yaml\n",
        )
        .unwrap();
        std::fs::write(contracts_dir.join("orders.yaml"), "openapi: 3.0.0\n").unwrap();
        std::fs::write(project.path().join(".env.local"), "TOKEN=must-not-export\n").unwrap();

        let output = project.path().join("get.forge.json");
        let summary = export_bundle(project.path(), &request, BundleFormat::Json, &output).unwrap();
        let bundle = parse_bundle(&std::fs::read_to_string(output).unwrap()).unwrap();
        let paths = bundle
            .files
            .iter()
            .map(|file| file.path.as_str())
            .collect::<BTreeSet<_>>();

        assert_eq!(summary.requests, 1);
        for path in [
            "project.json",
            "requests/orders/get.request.json",
            "requests/orders/.forge-environment",
            "requests/orders/.forge-jira",
            "requests/orders/.forge-openapi",
            "assets/fixtures/payload.json",
            "assets/fixtures/payload.schema.json",
            "assets/fixtures/payload.meta.json",
            "assets/hooks/verify.js",
            "assets/hooks/shared.js",
            "environments/dev.json",
            "contracts/orders.yaml",
        ] {
            assert!(paths.contains(path), "bundle omitted {path}");
        }
        assert!(!paths.contains(".env.local"));

        let destination = tempfile::tempdir().unwrap();
        let imported =
            import_bundle(&project.path().join("get.forge.json"), destination.path()).unwrap();
        assert!(!imported.preserved_project_config);
        assert_eq!(imported.files.len(), summary.files);
        assert!(!destination.path().join(".env.local").exists());
        assert!(destination.path().join("assets/hooks/shared.js").is_file());
        assert!(destination
            .path()
            .join("assets/fixtures/payload.schema.json")
            .is_file());
        assert!(destination
            .path()
            .join("assets/fixtures/payload.meta.json")
            .is_file());
        assert!(destination
            .path()
            .join("requests/orders/.forge-jira")
            .is_file());

        let existing_project = tempfile::tempdir().unwrap();
        let local_config = r#"{"formatVersion":1,"name":"destination project"}"#;
        std::fs::write(existing_project.path().join("project.json"), local_config).unwrap();
        let merged = import_bundle(
            &project.path().join("get.forge.json"),
            existing_project.path(),
        )
        .unwrap();
        assert!(merged.preserved_project_config);
        assert_eq!(
            std::fs::read_to_string(existing_project.path().join("project.json")).unwrap(),
            local_config,
            "bundle imports keep the destination project's own configuration"
        );
        assert!(existing_project
            .path()
            .join("requests/orders/get.request.json")
            .is_file());
        assert_eq!(merged.files.len() + 1, summary.files);
    }

    #[test]
    fn import_rejects_traversal_and_existing_files_before_writing() {
        let destination = tempfile::tempdir().unwrap();
        std::fs::write(destination.path().join("safe.json"), "original").unwrap();
        let bundle = ForgeBundle {
            format: BUNDLE_FORMAT.to_string(),
            format_version: BUNDLE_VERSION,
            kind: BundleKind::Folder,
            source: "requests".to_string(),
            files: vec![
                BundleFile {
                    path: "safe.json".to_string(),
                    encoding: FileEncoding::Utf8,
                    content: "replacement".to_string(),
                },
                BundleFile {
                    path: "../escape.json".to_string(),
                    encoding: FileEncoding::Utf8,
                    content: "escape".to_string(),
                },
            ],
        };
        let input = destination.path().join("bad.forge.json");
        std::fs::write(&input, render_json(&bundle).unwrap()).unwrap();

        assert!(import_bundle(&input, destination.path()).is_err());
        assert_eq!(
            std::fs::read_to_string(destination.path().join("safe.json")).unwrap(),
            "original"
        );
        assert!(!destination
            .path()
            .parent()
            .unwrap()
            .join("escape.json")
            .exists());
    }

    #[cfg(unix)]
    #[test]
    fn export_rejects_symlinked_output_parent() {
        use std::os::unix::fs::symlink;

        let project = tempfile::tempdir().unwrap();
        let request = project.path().join("get.request.json");
        std::fs::write(&request, REQUEST).unwrap();
        let target = tempfile::tempdir().unwrap();
        let linked_parent = project.path().join("linked-output");
        symlink(target.path(), &linked_parent).unwrap();

        let result = export_bundle(
            project.path(),
            &request,
            BundleFormat::Json,
            &linked_parent.join("export.forge.json"),
        );

        assert!(result.is_err());
        assert!(!target.path().join("export.forge.json").exists());
    }

    #[cfg(unix)]
    #[test]
    fn import_rejects_symlinked_parent_without_writing_outside_destination() {
        use std::os::unix::fs::symlink;

        let destination = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        symlink(outside.path(), destination.path().join("redirect")).unwrap();
        let bundle = ForgeBundle {
            format: BUNDLE_FORMAT.to_string(),
            format_version: BUNDLE_VERSION,
            kind: BundleKind::Folder,
            source: "requests".to_string(),
            files: vec![BundleFile {
                path: "redirect/escaped.json".to_string(),
                encoding: FileEncoding::Utf8,
                content: "must not be written".to_string(),
            }],
        };
        let input = destination.path().join("symlink.forge.json");
        std::fs::write(&input, render_json(&bundle).unwrap()).unwrap();

        let error = import_bundle(&input, destination.path()).unwrap_err();

        assert!(error.contains("symlink"), "{error}");
        assert!(!outside.path().join("escaped.json").exists());
    }

    #[cfg(unix)]
    #[test]
    fn request_export_rejects_symlinked_sidecars() {
        use std::os::unix::fs::symlink;

        let project = tempfile::tempdir().unwrap();
        let request = project.path().join("get.request.json");
        std::fs::write(
            &request,
            r#"{"formatVersion":1,"kind":"request","meta":{"id":"get","name":"Get"},"request":{"method":"GET","url":"https://example.test"}}"#,
        )
        .unwrap();
        let outside = tempfile::tempdir().unwrap();
        let secret_sidecar = outside.path().join("outside.json");
        std::fs::write(&secret_sidecar, "private sidecar contents").unwrap();
        symlink(&secret_sidecar, project.path().join("get.hooks.json")).unwrap();

        let error = build_bundle(project.path(), &request).unwrap_err();

        assert!(error.contains("symlink"), "{error}");
        assert!(!error.contains("private sidecar contents"));
    }
}
