//! Explicit, lossless migration from the legacy request model.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::model::{
    ApiKeyPlacement, AuthConfig, BodyDef, Check, ExtractScope, ExtractorSource, NumberOp,
    ParamKind, RequestDef, SecretValues, StringOp, ValueOp,
};

use super::model::{
    BodySpec, BodyType, ExecutionCondition, ExecutionPolicy, FormatVersion, HeaderSpec, InlineBody,
    LiteralCondition, PipelineEntry, PipelinePhase, RequestDocument, RequestKind, RequestMeta,
    RequestSpec, SkipGuard,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationError {
    pub unsupported: Vec<String>,
}

impl std::fmt::Display for MigrationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "request cannot be migrated without data loss: {}",
            self.unsupported.join("; ")
        )
    }
}

impl std::error::Error for MigrationError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MigrationStatus {
    Ready,
    Migrated,
    Blocked,
    Exists,
}

#[derive(Debug, Clone)]
pub struct MigrationItem {
    pub source: PathBuf,
    pub target: PathBuf,
    pub status: MigrationStatus,
    pub message: String,
}

/// One request-v1 request prepared from an imported Postman or Bruno tree.
#[derive(Debug, Clone)]
pub struct PlannedRequest {
    /// Project-relative target under `requests/`.
    pub path: PathBuf,
    pub document: RequestDocument,
}

#[derive(Debug, Clone)]
pub struct PlannedEnvironment {
    /// Project-relative target under `environments/`.
    pub path: PathBuf,
    /// Request-v1 environments are plain `${env.*}` maps, unlike the legacy
    /// `{name, variables}` workspace environment document.
    pub document: Value,
}

/// A read-only preview of importing a legacy collection into request-v1.
/// Unsupported requests remain in the source export and are listed rather
/// than being silently weakened during conversion.
#[derive(Debug, Clone, Default)]
pub struct CollectionMigrationPlan {
    pub requests: Vec<PlannedRequest>,
    pub blocked: Vec<String>,
    pub warnings: Vec<String>,
    pub environment: Option<PlannedEnvironment>,
    /// Values are never included in the committed environment document.
    pub secrets: SecretValues,
}

/// Prepare a collision-free request-v1 tree for a parsed Postman or Bruno
/// collection. No files are written. The caller can show this plan before
/// committing it, and the legacy export remains untouched.
pub fn plan_imported_collection(
    collection: &crate::convert::ImportedCollection,
    project_root: &Path,
    target_name: &str,
) -> CollectionMigrationPlan {
    let requests_root = project_root.join("requests");
    let mut collection_dir = safe_slug(target_name);
    let mut suffix = 2usize;
    while requests_root.join(&collection_dir).exists() {
        collection_dir = format!("{}-{suffix}", safe_slug(target_name));
        suffix += 1;
    }

    let collection_auth = match &collection.auth {
        AuthConfig::Inherit => AuthConfig::None,
        _ => collection.auth.clone(),
    };
    let mut plan = CollectionMigrationPlan {
        secrets: collection.secret_variables.clone(),
        ..CollectionMigrationPlan::default()
    };
    let mut relative_dir = PathBuf::from("requests").join(collection_dir);
    let mut names = HashSet::new();
    let has_collection_hooks = !collection.hooks.is_empty();
    plan_imported_items(
        &collection.items,
        &mut relative_dir,
        &collection_auth,
        has_collection_hooks,
        &mut names,
        project_root,
        &mut plan,
    );
    if !collection.description.trim().is_empty() {
        plan.warnings
            .push("collection description is not represented in request-v1 files".to_string());
    }
    if has_collection_hooks && collection.request_count() == 0 {
        plan.blocked.push(format!(
            "{}: collection lifecycle scripts cannot be represented by request-v1",
            collection.name
        ));
    }

    let secret_names = collection.secret_variables.keys().collect::<Vec<_>>();
    for request in &mut plan.requests {
        rewrite_secret_references(&mut request.document, &secret_names);
    }

    if !collection.variables.is_empty() {
        let environment_dir = PathBuf::from("environments");
        if target_has_symlink(project_root, &environment_dir) {
            plan.blocked.push(
                "collection environment: environments path contains a symbolic link".to_string(),
            );
        } else {
            let mut environment_name = safe_slug(target_name);
            let mut suffix = 2usize;
            let environment_path = loop {
                let relative = environment_dir.join(format!("{environment_name}.json"));
                if !project_root.join(&relative).exists()
                    && !target_has_symlink(project_root, &relative)
                {
                    break relative;
                }
                environment_name = format!("{}-{suffix}", safe_slug(target_name));
                suffix += 1;
            };
            let environment = Value::Object(Map::from_iter(
                collection
                    .variables
                    .iter()
                    .map(|(key, value)| (key.clone(), Value::String(value.clone()))),
            ));
            plan.environment = Some(PlannedEnvironment {
                path: environment_path,
                document: environment,
            });
        }
    }
    plan
}

fn rewrite_secret_references(document: &mut RequestDocument, names: &[&String]) {
    if names.is_empty() {
        return;
    }
    let Ok(mut value) = serde_json::to_value(&*document) else {
        return;
    };
    fn rewrite(value: &mut Value, names: &[&String]) {
        match value {
            Value::String(text) => {
                for name in names {
                    *text =
                        text.replace(&format!("${{env.{name}}}"), &format!("${{secret.{name}}}"));
                }
            }
            Value::Array(items) => items.iter_mut().for_each(|item| rewrite(item, names)),
            Value::Object(fields) => fields.values_mut().for_each(|item| rewrite(item, names)),
            _ => {}
        }
    }
    rewrite(&mut value, names);
    if let Ok(updated) = serde_json::from_value(value) {
        *document = updated;
    }
}

fn plan_imported_items(
    items: &[crate::convert::ImportedItem],
    relative_dir: &mut PathBuf,
    inherited_auth: &AuthConfig,
    inherited_hooks: bool,
    sibling_names: &mut HashSet<String>,
    project_root: &Path,
    plan: &mut CollectionMigrationPlan,
) {
    for item in items {
        match item {
            crate::convert::ImportedItem::Folder {
                name,
                auth,
                hooks,
                description,
                items,
            } => {
                if !description.trim().is_empty() {
                    plan.warnings.push(format!(
                        "folder {name:?} description is not represented in request-v1 files"
                    ));
                }
                let folder_auth = inherited_auth_if_needed(inherited_auth, auth);
                let folder_name = unique_slug(name, sibling_names);
                relative_dir.push(folder_name);
                let mut child_names = HashSet::new();
                plan_imported_items(
                    items,
                    relative_dir,
                    &folder_auth,
                    inherited_hooks || !hooks.is_empty(),
                    &mut child_names,
                    project_root,
                    plan,
                );
                relative_dir.pop();
            }
            crate::convert::ImportedItem::Request(def) => {
                let mut request = (**def).clone();
                if request.auth == AuthConfig::Inherit {
                    request.auth = inherited_auth.clone();
                }
                let file_name =
                    format!("{}.request.json", unique_slug(&request.name, sibling_names));
                let path = relative_dir.join(file_name);
                let display_path = path.to_string_lossy().into_owned();
                let mut reasons = Vec::new();
                if !request.scripts.is_empty() {
                    reasons.push("inline scripts cannot be represented by request-v1; confirm their removal before importing".to_string());
                }
                if inherited_hooks {
                    reasons.push(
                        "inherited collection/folder scripts cannot be represented by request-v1"
                            .to_string(),
                    );
                }
                if target_has_symlink(project_root, &path) {
                    reasons.push("target path contains a symbolic link".to_string());
                }
                if project_root.join(&path).exists() {
                    reasons.push("target already exists".to_string());
                }
                let id = path
                    .file_stem()
                    .and_then(|value| value.to_str())
                    .unwrap_or("request");
                let mut request = request;
                request.scripts = Default::default();
                match migrate_request(&request, id) {
                    Ok(document) if reasons.is_empty() => {
                        plan.requests.push(PlannedRequest { path, document })
                    }
                    Ok(_) => plan
                        .blocked
                        .push(format!("{display_path}: {}", reasons.join("; "))),
                    Err(error) => {
                        reasons.push(error.to_string());
                        plan.blocked
                            .push(format!("{display_path}: {}", reasons.join("; ")));
                    }
                }
            }
        }
    }
}

fn inherited_auth_if_needed(parent: &AuthConfig, own: &AuthConfig) -> AuthConfig {
    if *own == AuthConfig::Inherit {
        parent.clone()
    } else {
        own.clone()
    }
}

fn unique_slug(value: &str, assigned: &mut HashSet<String>) -> String {
    let base = safe_slug(value);
    let mut candidate = base.clone();
    let mut suffix = 2usize;
    while !assigned.insert(candidate.clone()) {
        candidate = format!("{base}-{suffix}");
        suffix += 1;
    }
    candidate
}

fn safe_slug(value: &str) -> String {
    let mut slug = String::new();
    let mut separator = false;
    for character in value.chars().flat_map(char::to_lowercase) {
        if character.is_ascii_alphanumeric() {
            slug.push(character);
            separator = false;
        } else if !slug.is_empty() && !separator {
            slug.push('-');
            separator = true;
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    if slug.is_empty() {
        "request".to_string()
    } else {
        slug
    }
}

fn target_has_symlink(root: &Path, relative: &Path) -> bool {
    let mut current = root.to_path_buf();
    for component in relative.components() {
        current.push(component.as_os_str());
        if std::fs::symlink_metadata(&current)
            .is_ok_and(|metadata| metadata.file_type().is_symlink())
        {
            return true;
        }
    }
    false
}

/// Plan or execute a whole-tree migration while preserving relative paths.
/// Existing targets and unsupported legacy features are reported per file
/// and never overwritten.
pub fn migrate_tree(
    source_root: &Path,
    target_root: &Path,
    dry_run: bool,
) -> Result<Vec<MigrationItem>, std::io::Error> {
    let mut sources = walkdir::WalkDir::new(source_root)
        .into_iter()
        .filter_entry(|entry| {
            entry.depth() == 0
                || !entry.file_type().is_dir()
                || !crate::is_ignored_dir(&entry.file_name().to_string_lossy())
        })
        .collect::<Result<Vec<_>, _>>()
        .map_err(std::io::Error::other)?
        .into_iter()
        .filter(|entry| {
            entry.file_type().is_file() && entry.path().to_string_lossy().ends_with(".request.json")
        })
        .map(|entry| entry.into_path())
        .collect::<Vec<_>>();
    sources.sort();

    let mut report = Vec::with_capacity(sources.len());
    for source in sources {
        let relative = source.strip_prefix(source_root).unwrap_or(&source);
        let target = target_root.join(relative);
        if target.exists() {
            report.push(MigrationItem {
                source,
                target,
                status: MigrationStatus::Exists,
                message: "target already exists".to_string(),
            });
            continue;
        }
        let result = std::fs::read_to_string(&source)
            .map_err(|error| error.to_string())
            .and_then(|text| {
                serde_json::from_str::<RequestDef>(&text).map_err(|error| error.to_string())
            })
            .and_then(|legacy| {
                let stem = source
                    .file_stem()
                    .and_then(|name| name.to_str())
                    .unwrap_or("request");
                let id = stem.strip_suffix(".request").unwrap_or(stem);
                migrate_request(&legacy, id).map_err(|error| error.to_string())
            });
        match result {
            Err(message) => report.push(MigrationItem {
                source,
                target,
                status: MigrationStatus::Blocked,
                message,
            }),
            Ok(_) if dry_run => report.push(MigrationItem {
                source,
                target,
                status: MigrationStatus::Ready,
                message: "ready to migrate".to_string(),
            }),
            Ok(document) => {
                let write_result = target
                    .parent()
                    .map(std::fs::create_dir_all)
                    .transpose()
                    .and_then(|_| {
                        serde_json::to_vec_pretty(&document)
                            .map_err(std::io::Error::other)
                            .and_then(|mut json| {
                                json.push(b'\n');
                                std::fs::write(&target, json)
                            })
                    });
                match write_result {
                    Ok(()) => report.push(MigrationItem {
                        source,
                        target,
                        status: MigrationStatus::Migrated,
                        message: "migrated".to_string(),
                    }),
                    Err(error) => report.push(MigrationItem {
                        source,
                        target,
                        status: MigrationStatus::Blocked,
                        message: error.to_string(),
                    }),
                }
            }
        }
    }
    Ok(report)
}

/// Convert one legacy request. Anything v1 cannot represent is reported
/// explicitly; no field is silently dropped.
pub fn migrate_request(
    def: &RequestDef,
    id: impl Into<String>,
) -> Result<RequestDocument, MigrationError> {
    let mut unsupported = Vec::new();
    let mut headers = def
        .headers
        .iter()
        .map(|header| HeaderSpec {
            name: convert_vars(&header.key),
            value: convert_vars(&header.value),
            enabled: header.enabled,
        })
        .collect::<Vec<_>>();
    let mut query = Vec::new();
    let mut url = convert_vars(&def.url);
    for parameter in &def.params {
        match parameter.kind {
            ParamKind::Query => query.push(HeaderSpec {
                name: convert_vars(&parameter.kv.key),
                value: convert_vars(&parameter.kv.value),
                enabled: parameter.kv.enabled,
            }),
            ParamKind::Path if parameter.kv.enabled => {
                url = url.replace(
                    &format!(":{}", parameter.kv.key),
                    &convert_vars(&parameter.kv.value),
                );
            }
            ParamKind::Path => {}
        }
    }

    let mut pipeline = Vec::new();
    migrate_auth(
        &def.auth,
        &mut headers,
        &mut query,
        &mut pipeline,
        &mut unsupported,
    );
    let body = migrate_body(&def.body, &mut headers, &mut unsupported);
    migrate_assertions(def, &mut pipeline, &mut unsupported);
    migrate_extractors(def, &mut pipeline, &mut unsupported);

    if !def.scripts.is_empty() {
        unsupported.push("inline request scripts require project .js assets".to_string());
    }
    if def.settings.verify_tls.is_some() {
        unsupported.push("per-request TLS verification has no v1 equivalent".to_string());
    }
    if !unsupported.is_empty() {
        unsupported.sort();
        unsupported.dedup();
        return Err(MigrationError { unsupported });
    }

    Ok(RequestDocument {
        schema: None,
        format_version: FormatVersion,
        kind: RequestKind::Request,
        meta: RequestMeta {
            id: id.into(),
            name: def.name.clone(),
            description: (!def.description.is_empty()).then(|| def.description.clone()),
            tags: Vec::new(),
        },
        bindings: BTreeMap::new(),
        matrix: BTreeMap::new(),
        execution: def.settings.skip_in_runs.then(|| ExecutionPolicy {
            delay_before_ms: None,
            skip: Some(SkipGuard {
                when: ExecutionCondition::Literal(LiteralCondition { literal: true }),
                reason: "Skipped by migrated request setting".to_string(),
            }),
        }),
        auth: None,
        request: RequestSpec {
            method: def.method,
            url,
            headers,
            query,
            body,
            settings: super::model::RequestTransportSettings {
                timeout_ms: def.settings.timeout_ms,
                follow_redirects: def.settings.follow_redirects,
                max_redirects: def.settings.max_redirects,
                encode_url: None,
            },
        },
        pipeline,
        mock: None,
    })
}

fn migrate_auth(
    auth: &AuthConfig,
    headers: &mut Vec<HeaderSpec>,
    query: &mut Vec<HeaderSpec>,
    pipeline: &mut Vec<PipelineEntry>,
    unsupported: &mut Vec<String>,
) {
    let before = |uses: &str, with: Map<String, Value>| PipelineEntry {
        phase: PipelinePhase::BeforeRequest,
        uses: uses.to_string(),
        with,
        enabled: true,
    };
    match auth {
        AuthConfig::None => {}
        AuthConfig::Inherit => unsupported.push(
            "inherited auth must be resolved from its collection before migration".to_string(),
        ),
        AuthConfig::Basic { username, password } => pipeline.push(before(
            "builtin:basic@1",
            Map::from_iter([
                ("username".into(), Value::String(convert_vars(username))),
                ("password".into(), Value::String(convert_vars(password))),
            ]),
        )),
        AuthConfig::Bearer { token, prefix } => {
            let mut with = Map::from_iter([("token".into(), Value::String(convert_vars(token)))]);
            if let Some(prefix) = prefix {
                with.insert("prefix".into(), Value::String(prefix.clone()));
            }
            pipeline.push(before("builtin:bearer@1", with));
        }
        AuthConfig::ApiKey {
            key,
            value,
            placement,
        } => {
            let entry = HeaderSpec {
                name: convert_vars(key),
                value: convert_vars(value),
                enabled: true,
            };
            match placement {
                ApiKeyPlacement::Header => headers.push(entry),
                ApiKeyPlacement::Query => query.push(entry),
            }
        }
        other => unsupported.push(format!(
            "{} auth is not yet represented by request format v1",
            auth_name(other)
        )),
    }
}

fn auth_name(auth: &AuthConfig) -> &'static str {
    match auth {
        AuthConfig::OAuth2ClientCredentials { .. } => "OAuth2 client credentials",
        AuthConfig::OAuth2AuthCode { .. } => "OAuth2 authorization code",
        AuthConfig::Digest { .. } => "Digest",
        AuthConfig::Ntlm { .. } => "NTLM",
        AuthConfig::AwsSigV4 { .. } => "AWS SigV4",
        _ => "request",
    }
}

fn migrate_body(
    body: &BodyDef,
    headers: &mut Vec<HeaderSpec>,
    unsupported: &mut Vec<String>,
) -> Option<BodySpec> {
    let inline = |body_type, value| {
        Some(BodySpec::Inline(InlineBody {
            body_type,
            value: Some(value),
        }))
    };
    match body {
        BodyDef::None => None,
        BodyDef::Raw { text, .. } | BodyDef::Xml { text } => {
            inline(BodyType::Text, Value::String(convert_vars(text)))
        }
        BodyDef::Json { text } => match serde_json::from_str(&convert_vars(text)) {
            Ok(value) => inline(BodyType::Json, value),
            Err(error) => {
                unsupported.push(format!(
                    "JSON body is invalid after variable migration: {error}"
                ));
                None
            }
        },
        BodyDef::FormUrlencoded { fields } => {
            let mut object = Map::new();
            for field in fields.iter().filter(|field| field.enabled) {
                if object
                    .insert(
                        convert_vars(&field.key),
                        Value::String(convert_vars(&field.value)),
                    )
                    .is_some()
                {
                    unsupported.push(
                        "form body contains duplicate keys, which v1 cannot preserve".to_string(),
                    );
                }
            }
            inline(BodyType::Form, Value::Object(object))
        }
        BodyDef::GraphQl {
            query,
            variables,
            operation_name,
        } => {
            let variables = if variables.trim().is_empty() {
                Value::Object(Map::new())
            } else {
                match serde_json::from_str(&convert_vars(variables)) {
                    Ok(value) => value,
                    Err(error) => {
                        unsupported.push(format!("GraphQL variables are invalid JSON: {error}"));
                        Value::Null
                    }
                }
            };
            add_content_type(headers, "application/json");
            inline(
                BodyType::Json,
                serde_json::json!({
                    "query": convert_vars(query),
                    "variables": variables,
                    "operationName": operation_name,
                }),
            )
        }
        BodyDef::Multipart { .. } => {
            unsupported.push("multipart bodies are not supported by v1".to_string());
            None
        }
        BodyDef::Binary { .. } => {
            unsupported.push("binary bodies are not supported by v1".to_string());
            None
        }
    }
}

fn add_content_type(headers: &mut Vec<HeaderSpec>, value: &str) {
    if !headers
        .iter()
        .any(|header| header.name.eq_ignore_ascii_case("content-type"))
    {
        headers.push(HeaderSpec {
            name: "Content-Type".to_string(),
            value: value.to_string(),
            enabled: true,
        });
    }
}

fn migrate_assertions(
    def: &RequestDef,
    pipeline: &mut Vec<PipelineEntry>,
    unsupported: &mut Vec<String>,
) {
    for assertion in &def.assertions {
        if !assertion.note.is_empty() {
            unsupported.push("assertion notes have no v1 field".to_string());
            continue;
        }
        let mapped = match &assertion.check {
            Check::StatusCode {
                op: NumberOp::Eq,
                value,
            } => builtin("assert-status", serde_json::json!({"expected": value})),
            Check::Header {
                name,
                op: StringOp::Equals,
                value,
            } => builtin(
                "assert-header",
                serde_json::json!({"name": convert_vars(name), "value": convert_vars(value)}),
            ),
            Check::Header {
                name,
                op: StringOp::Exists,
                ..
            } => builtin(
                "assert-header",
                serde_json::json!({"name": convert_vars(name)}),
            ),
            Check::ContentType { value } => builtin(
                "assert-header",
                serde_json::json!({"name": "Content-Type", "value": convert_vars(value)}),
            ),
            Check::JsonPath { path, op, value } => {
                let operator = match op {
                    ValueOp::Equals => Some("equals"),
                    ValueOp::Contains => Some("contains"),
                    ValueOp::Exists => Some("exists"),
                    ValueOp::NotExists => Some("notExists"),
                    _ => None,
                };
                if let Some(operator) = operator {
                    let mut with = serde_json::json!({
                        "path": convert_vars(path),
                        "operator": operator,
                    });
                    if !matches!(op, ValueOp::Exists | ValueOp::NotExists) {
                        with["value"] = convert_value_vars(value);
                    }
                    builtin("assert-json-path", with)
                } else {
                    unsupported.push(format!("JSONPath operator {op:?} is not supported by v1"));
                    continue;
                }
            }
            Check::BodyContains { value } => builtin(
                "assert-body-text",
                serde_json::json!({"text": convert_vars(value)}),
            ),
            Check::BodyMatches { regex } => builtin(
                "assert-body-regex",
                serde_json::json!({"pattern": convert_vars(regex)}),
            ),
            Check::ResponseTimeBelow { max_ms } => {
                builtin("assert-response-time", serde_json::json!({"maxMs": max_ms}))
            }
            Check::JsonSchema { schema } => {
                builtin("assert-schema", serde_json::json!({"schema": schema}))
            }
            other => {
                unsupported.push(format!("assertion {:?} is not supported by v1", other));
                continue;
            }
        };
        pipeline.push(PipelineEntry {
            enabled: assertion.enabled,
            ..mapped
        });
    }
}

fn migrate_extractors(
    def: &RequestDef,
    pipeline: &mut Vec<PipelineEntry>,
    unsupported: &mut Vec<String>,
) {
    for extractor in &def.extractors {
        if extractor.scope != ExtractScope::Runtime {
            unsupported.push("environment-scoped extractors are not supported by v1".to_string());
            continue;
        }
        let mapped = match &extractor.source {
            ExtractorSource::JsonPath { expr } => builtin(
                "extract-json-path",
                serde_json::json!({"path": convert_vars(expr), "target": extractor.var}),
            ),
            ExtractorSource::Header { name } => builtin(
                "extract-header",
                serde_json::json!({"name": convert_vars(name), "target": extractor.var}),
            ),
            ExtractorSource::Regex { .. } => {
                unsupported.push("regex extractors are not supported by v1".to_string());
                continue;
            }
        };
        pipeline.push(PipelineEntry {
            enabled: extractor.enabled,
            ..mapped
        });
    }
}

fn builtin(name: &str, with: Value) -> PipelineEntry {
    PipelineEntry {
        phase: PipelinePhase::AfterResponse,
        uses: format!("builtin:{name}@1"),
        with: with.as_object().cloned().unwrap_or_default(),
        enabled: true,
    }
}

fn convert_value_vars(value: &Value) -> Value {
    match value {
        Value::String(value) => Value::String(convert_vars(value)),
        Value::Array(values) => Value::Array(values.iter().map(convert_value_vars).collect()),
        Value::Object(values) => Value::Object(
            values
                .iter()
                .map(|(key, value)| (key.clone(), convert_value_vars(value)))
                .collect(),
        ),
        other => other.clone(),
    }
}

fn convert_vars(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else {
            out.push_str(&rest[start..]);
            return out;
        };
        let name = after[..end].trim();
        if name.is_empty() {
            out.push_str("{{}}");
        } else {
            out.push_str("${env.");
            out.push_str(name);
            out.push('}');
        }
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AssertionDef, KeyValue, Method};

    #[test]
    fn migrates_common_request_without_losing_variable_mapping() {
        let mut def = RequestDef::new("Create", Method::Post, "{{baseUrl}}/users/:id");
        def.params.push(crate::model::Param {
            kv: KeyValue::new("id", "{{userId}}"),
            kind: ParamKind::Path,
        });
        def.auth = AuthConfig::Bearer {
            token: "{{token}}".to_string(),
            prefix: None,
        };
        def.body = BodyDef::Json {
            text: r#"{"name":"{{name}}"}"#.to_string(),
        };
        def.assertions.push(AssertionDef::from(Check::StatusCode {
            op: NumberOp::Eq,
            value: 201,
        }));

        let migrated = migrate_request(&def, "users.create").unwrap();

        assert_eq!(migrated.request.url, "${env.baseUrl}/users/${env.userId}");
        assert_eq!(
            migrated.pipeline[0].with["token"],
            Value::String("${env.token}".to_string())
        );
        assert_eq!(migrated.pipeline[1].uses, "builtin:assert-status@1");
    }

    #[test]
    fn refuses_features_that_would_be_dropped() {
        let mut def = RequestDef::new("Binary", Method::Post, "https://example.test");
        def.body = BodyDef::Binary {
            path: "payload.bin".to_string(),
        };

        let error = migrate_request(&def, "binary").unwrap_err();

        assert!(error
            .unsupported
            .iter()
            .any(|item| item.contains("binary bodies")));
    }

    #[test]
    fn migrates_supported_transport_settings() {
        let mut def = RequestDef::new("Settings", Method::Get, "https://example.test");
        def.auth = AuthConfig::None;
        def.settings.timeout_ms = Some(0);
        def.settings.follow_redirects = Some(false);
        def.settings.max_redirects = Some(2);

        let migrated = migrate_request(&def, "settings").unwrap();
        assert_eq!(migrated.request.settings.timeout_ms, Some(0));
        assert_eq!(migrated.request.settings.follow_redirects, Some(false));
        assert_eq!(migrated.request.settings.max_redirects, Some(2));
    }

    #[test]
    fn migrates_legacy_run_skip_to_native_execution_policy() {
        let mut def = RequestDef::new("Optional", Method::Get, "https://example.test");
        def.auth = AuthConfig::None;
        def.settings.skip_in_runs = true;

        let migrated = migrate_request(&def, "optional").unwrap();
        assert!(matches!(
            migrated.execution.unwrap().skip.unwrap().when,
            ExecutionCondition::Literal(LiteralCondition { literal: true })
        ));
    }

    #[test]
    fn collection_plan_resolves_inherited_auth_and_keeps_secrets_out_of_environment() {
        let import = crate::convert::parse_postman(
            r#"{
              "info":{"name":"Source"},
              "auth":{"type":"bearer","bearer":[{"key":"token","value":"{{apiToken}}"}]},
              "variable":[
                {"key":"baseUrl","value":"https://example.test"},
                {"key":"apiToken","value":"very-secret","type":"secret"}
              ],
              "item":[{"name":"Accounts","item":[{"name":"List","request":{"method":"GET","url":"{{baseUrl}}/accounts"}}]}]
            }"#,
        )
        .unwrap();
        let root = tempfile::tempdir().unwrap();
        let plan = plan_imported_collection(&import, root.path(), "Payments API");

        assert!(plan.blocked.is_empty(), "{:?}", plan.blocked);
        assert_eq!(plan.requests.len(), 1);
        assert_eq!(
            plan.requests[0].path,
            PathBuf::from("requests/payments-api/accounts/list.request.json")
        );
        assert_eq!(
            plan.requests[0].document.pipeline[0].with["token"],
            Value::String("${secret.apiToken}".to_string())
        );
        assert_eq!(
            plan.requests[0].document.request.url,
            "${env.baseUrl}/accounts"
        );
        let environment = plan.environment.unwrap();
        assert_eq!(environment.document["baseUrl"], "https://example.test");
        assert!(environment.document.get("apiToken").is_none());
        assert_eq!(
            plan.secrets.get("apiToken").map(String::as_str),
            Some("very-secret")
        );
        let serialized = serde_json::to_string(&environment.document).unwrap();
        assert!(!serialized.contains("very-secret"));
    }

    #[test]
    fn collection_plan_blocks_scripts_and_unsupported_auth_without_dropping_them() {
        let import = crate::convert::parse_postman(
            r#"{
              "info":{"name":"Source"},
              "event":[{"listen":"prerequest","script":{"exec":["pm.variables.set('x','y')"]}}],
              "item":[{"name":"Unsafe","request":{"method":"GET","url":"https://example.test","auth":{"type":"ntlm","ntlm":[]}}}]
            }"#,
        )
        .unwrap();
        let root = tempfile::tempdir().unwrap();
        let plan = plan_imported_collection(&import, root.path(), "Source");

        assert!(plan.requests.is_empty());
        assert!(!import.quarantine.is_empty());
        assert!(plan.blocked.iter().any(|note| note.contains("NTLM")));
        assert!(!root.path().join("requests/source").exists());
    }

    #[test]
    fn bulk_migration_has_a_non_writing_dry_run_and_preserves_paths() {
        let source = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(source.path().join("users")).unwrap();
        let mut request = RequestDef::new("List", Method::Get, "https://example.test/users");
        request.auth = AuthConfig::None;
        std::fs::write(
            source.path().join("users/list.request.json"),
            serde_json::to_vec(&request).unwrap(),
        )
        .unwrap();
        let output = target.path().join("migrated");

        let dry_run = migrate_tree(source.path(), &output, true).unwrap();
        assert_eq!(
            dry_run[0].status,
            MigrationStatus::Ready,
            "{}",
            dry_run[0].message
        );
        assert!(!output.exists());

        let migrated = migrate_tree(source.path(), &output, false).unwrap();
        assert_eq!(migrated[0].status, MigrationStatus::Migrated);
        assert!(output.join("users/list.request.json").is_file());

        let second = migrate_tree(source.path(), &output, false).unwrap();
        assert_eq!(second[0].status, MigrationStatus::Exists);
    }
}
