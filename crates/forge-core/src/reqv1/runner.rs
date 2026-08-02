//! Runner: document → IR → beforeRequest hooks → HTTP send (or mock) →
//! afterResponse assertions/extractors → [`RunResult`]. See §9, §12, §17.

use std::collections::{BTreeMap, HashMap};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::exec::{
    HttpEngine, PartData as ExecPartData, ResolvedBody as ExecBody, ResolvedPart as ExecPart,
    ResolvedRequest as ExecRequest,
};

use super::build::{build_ir, BuildInputs};
use super::diag::{Code, Diagnostic};
use super::ir::{
    ResolvedBody, ResolvedHeader, ResolvedMock, ResolvedMultipartData, ResolvedPipelineEntry,
    ResolvedRequest,
};
use super::model::{
    ExecutionCondition, ExecutionVariableScope, PipelinePhase, ProjectAuthConfig, ProjectConfig,
    RequestDocument,
};
use super::pipeline::{
    is_project_asset, run_after_response, run_before_request, AssertionResult, ResponseView,
};
use super::refs::RefResolver;
use super::resolve::DataStore;
use super::vars::sensitive_name;

type ReactionWithLogs = (Vec<AssertionResult>, BTreeMap<String, Value>, Vec<String>);

/// Outcome of one request run (or one matrix case). See §17.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RunResult {
    pub request_id: String,
    pub status: RunStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skip_reason: Option<String>,
    pub http: Option<HttpResultView>,
    pub assertions: Vec<AssertionResult>,
    pub runtime: BTreeMap<String, Value>,
    pub diagnostics: Vec<Diagnostic>,
    pub duration_ms: u64,
}

struct RunExecution {
    result: RunResult,
    /// Masked response exposed by public run APIs.
    response: Option<ResponseView>,
    /// Original response retained only for internal auth/extractor flows.
    response_unmasked: Option<ResponseView>,
    /// Extractor output before public secret masking, used only to feed the
    /// next request in a sequence.
    runtime_unmasked: BTreeMap<String, Value>,
}

/// In-memory project-auth tokens and observed request durations. A GUI bridge
/// keeps one session for its lifetime; CLI runs keep one per command.
#[derive(Default)]
pub struct AuthSession {
    state: tokio::sync::Mutex<AuthSessionState>,
}

#[derive(Default)]
struct AuthSessionState {
    tokens: HashMap<AuthCacheKey, CachedAuth>,
    durations_ms: HashMap<(PathBuf, PathBuf), u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct AuthCacheKey {
    root: PathBuf,
    provider: PathBuf,
    environment: String,
    settings: String,
    mode: RunMode,
}

#[derive(Clone)]
struct CachedAuth {
    value: String,
    expires_at: Instant,
}

struct ProjectAuthHeader {
    token: String,
    refreshed: bool,
    provider_name: String,
}

struct SelectedProjectAuth {
    name: String,
    config: ProjectAuthConfig,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RunStatus {
    Passed,
    Failed,
    Error,
    Skipped,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HttpResultView {
    pub status: u16,
    pub time_ms: u64,
    pub bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogRequestView {
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Value,
}

#[derive(Debug, Clone)]
pub struct CatalogPreview {
    pub request_before: Option<CatalogRequestView>,
    pub request_after: Option<CatalogRequestView>,
    pub assertions: Vec<AssertionResult>,
    pub runtime_writes: BTreeMap<String, Value>,
    pub logs: Vec<String>,
    pub diagnostics: Vec<Diagnostic>,
}

/// How to source the response.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RunMode {
    /// Send the request over HTTP.
    Http,
    /// Serve the document's `mock` instead of sending (§10).
    Mock,
}

/// Load `<root>/environments/<name>.json`, or an empty object if `name` is
/// None. Errors if a named environment is missing.
pub fn load_environment(root: &Path, name: Option<&str>) -> Result<Value, Diagnostic> {
    match name {
        None => Ok(Value::Object(Default::default())),
        Some(name) => {
            let name = super::environment_scope::validate_environment_name(name)
                .map_err(|message| Diagnostic::new(Code::InvalidAssetInput, message))?;
            let path = root.join("environments").join(format!("{name}.json"));
            let text = std::fs::read_to_string(&path).map_err(|e| {
                Diagnostic::new(Code::AssetNotFound, format!("environment {name}: {e}"))
            })?;
            serde_json::from_str(&text).map_err(|e| {
                Diagnostic::new(Code::InvalidAssetInput, format!("environment {name}: {e}"))
            })
        }
    }
}

/// Load an explicitly selected environment, or the closest default attached
/// to the request or one of its parent folders.
pub fn load_request_environment(
    root: &Path,
    request_file: &Path,
    explicit_name: Option<&str>,
) -> Result<Value, Diagnostic> {
    let inherited_name = if explicit_name.is_none() {
        super::environment_scope::effective_environment(root, request_file)
            .map_err(|message| Diagnostic::new(Code::InvalidAssetInput, message))?
            .map(|selection| selection.value)
    } else {
        None
    };
    load_environment(root, explicit_name.or(inherited_name.as_deref()))
}

/// Load `<root>/project.json` (empty config if absent).
pub fn load_project(root: &Path) -> Result<ProjectConfig, Diagnostic> {
    let path = root.join("project.json");
    match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text)
            .map_err(|e| Diagnostic::new(Code::InvalidAssetInput, format!("project.json: {e}"))),
        Err(_) => Ok(ProjectConfig::default()),
    }
}

/// Load the configured auth-fetch request while keeping it inside `root`.
pub fn load_project_auth_document(
    root: &Path,
) -> Result<Option<(PathBuf, RequestDocument)>, Diagnostic> {
    let Some(auth) = load_project(root)?.auth else {
        return Ok(None);
    };
    let path = checked_project_path(root, &auth.request, "auth request")?;
    let document = super::assertions::load_request_document(&path)
        .map_err(|message| Diagnostic::new(Code::InvalidAssetInput, message))?;
    Ok(Some((path, document)))
}

/// Load every legacy and named auth request. This is primarily used by trust
/// checks, which must account for project code in any selectable provider.
pub fn load_project_auth_documents(
    root: &Path,
) -> Result<Vec<(PathBuf, RequestDocument)>, Diagnostic> {
    let project = load_project(root)?;
    let mut configs = Vec::new();
    if let Some(config) = project.auth {
        configs.push(config);
    }
    configs.extend(project.auth_providers.into_values());
    let mut documents = Vec::with_capacity(configs.len());
    for config in configs {
        config
            .validate()
            .map_err(|message| Diagnostic::new(Code::InvalidAssetInput, message))?;
        let path = checked_project_path(root, &config.request, "auth request")?;
        let document = super::assertions::load_request_document(&path)
            .map_err(|message| Diagnostic::new(Code::InvalidAssetInput, message))?;
        documents.push((path, document));
    }
    Ok(documents)
}

fn checked_project_path(root: &Path, value: &str, label: &str) -> Result<PathBuf, Diagnostic> {
    let relative = Path::new(value);
    if value.trim().is_empty()
        || relative.is_absolute()
        || relative
            .components()
            .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(Diagnostic::new(
            Code::InvalidAssetInput,
            format!("{label} must be a project-relative path"),
        ));
    }
    let path = root.join(relative);
    let canonical_root = root.canonicalize().map_err(|error| {
        Diagnostic::new(
            Code::InvalidAssetInput,
            format!("cannot resolve project root {}: {error}", root.display()),
        )
    })?;
    let canonical_path = path.canonicalize().map_err(|error| {
        Diagnostic::new(
            Code::AssetNotFound,
            format!("cannot resolve {label} {}: {error}", path.display()),
        )
    })?;
    if !canonical_path.starts_with(&canonical_root) {
        return Err(Diagnostic::new(
            Code::InvalidAssetInput,
            format!("{label} must stay inside the project"),
        ));
    }
    Ok(canonical_path)
}

/// Validate a request document down to the canonical IR without touching the
/// network (§12 stages 1–5). Returns the IR or the collected diagnostics.
pub fn validate(
    doc: &RequestDocument,
    root: &Path,
    request_file: &Path,
    env: Value,
    secret: &(dyn Fn(&str) -> Option<String> + Sync),
) -> Result<ResolvedRequest, Vec<Diagnostic>> {
    validate_case(
        doc,
        root,
        request_file,
        env,
        secret,
        Value::Null,
        empty_object(),
    )
}

fn empty_object() -> Value {
    Value::Object(Default::default())
}

/// [`validate`] for one specific matrix case (`matrix` = the case object)
/// and incoming `runtime` (from earlier requests in a sequence).
pub fn validate_case(
    doc: &RequestDocument,
    root: &Path,
    request_file: &Path,
    env: Value,
    secret: &(dyn Fn(&str) -> Option<String> + Sync),
    matrix: Value,
    runtime: Value,
) -> Result<ResolvedRequest, Vec<Diagnostic>> {
    let project = load_project(root).map_err(|d| vec![d])?;
    let resolver = RefResolver::new(root, &project).map_err(|e| e.0)?;
    let store = DataStore::new(&resolver);
    let base_dir = request_file.parent().unwrap_or(root);
    let inp = BuildInputs {
        resolver: &resolver,
        store: &store,
        base_dir,
        env,
        matrix,
        runtime,
        secret,
    };
    build_ir(doc, &inp).map_err(|e| e.0)
}

/// Full run: validate + execute. `secret` is the secret provider.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    doc: &RequestDocument,
    root: &Path,
    request_file: &Path,
    env: Value,
    secret: &(dyn Fn(&str) -> Option<String> + Sync),
    engine: &HttpEngine,
    mode: RunMode,
    cancel: CancellationToken,
    matrix: Value,
) -> RunResult {
    run_with_runtime(
        doc,
        root,
        request_file,
        env,
        secret,
        engine,
        mode,
        cancel,
        matrix,
        empty_object(),
    )
    .await
}

/// [`run`] plus incoming runtime (`${runtime.*}` from earlier requests in a
/// sequence). Runs all four pipeline phases: beforeRequest → send/mock →
/// afterResponse, then onError (only if the run errored) and finally
/// (always). See §9.
#[allow(clippy::too_many_arguments)]
pub async fn run_with_runtime(
    doc: &RequestDocument,
    root: &Path,
    request_file: &Path,
    env: Value,
    secret: &(dyn Fn(&str) -> Option<String> + Sync),
    engine: &HttpEngine,
    mode: RunMode,
    cancel: CancellationToken,
    matrix: Value,
    runtime_in: Value,
) -> RunResult {
    let auth = AuthSession::default();
    execute(
        doc,
        root,
        request_file,
        env,
        secret,
        engine,
        mode,
        cancel,
        matrix,
        runtime_in,
        &auth,
    )
    .await
    .result
}

/// Run one standalone request and retain its full response for an immediate
/// in-process preview.
#[allow(clippy::too_many_arguments)]
pub async fn run_with_response(
    doc: &RequestDocument,
    root: &Path,
    request_file: &Path,
    env: Value,
    secret: &(dyn Fn(&str) -> Option<String> + Sync),
    engine: &HttpEngine,
    mode: RunMode,
    cancel: CancellationToken,
    matrix: Value,
) -> (RunResult, Option<ResponseView>) {
    let auth = AuthSession::default();
    run_with_response_in_session(
        doc,
        root,
        request_file,
        env,
        secret,
        engine,
        mode,
        cancel,
        matrix,
        &auth,
    )
    .await
}

/// [`run_with_response`] using a caller-owned auth cache shared across runs.
#[allow(clippy::too_many_arguments)]
pub async fn run_with_response_in_session(
    doc: &RequestDocument,
    root: &Path,
    request_file: &Path,
    env: Value,
    secret: &(dyn Fn(&str) -> Option<String> + Sync),
    engine: &HttpEngine,
    mode: RunMode,
    cancel: CancellationToken,
    matrix: Value,
    auth: &AuthSession,
) -> (RunResult, Option<ResponseView>) {
    let execution = execute(
        doc,
        root,
        request_file,
        env,
        secret,
        engine,
        mode,
        cancel,
        matrix,
        empty_object(),
        auth,
    )
    .await;
    (execution.result, execution.response)
}

#[allow(clippy::too_many_arguments)]
async fn project_auth_header(
    doc: &RequestDocument,
    root: &Path,
    request_file: &Path,
    env: &Value,
    secret: &(dyn Fn(&str) -> Option<String> + Sync),
    engine: &HttpEngine,
    mode: RunMode,
    cancel: CancellationToken,
    auth: &AuthSession,
) -> Result<Option<ProjectAuthHeader>, Diagnostic> {
    let project = load_project(root)?;
    let Some(selected) = select_project_auth(doc, &project, root, request_file)? else {
        return Ok(None);
    };
    let config = selected.config;
    let provider = checked_project_path(root, &config.request, "auth request")?;
    let provider_doc = super::assertions::load_request_document(&provider)
        .map_err(|message| Diagnostic::new(Code::InvalidAssetInput, message))?;
    let canonical_root = root.canonicalize().map_err(|error| {
        Diagnostic::new(
            Code::InvalidAssetInput,
            format!("cannot resolve project root {}: {error}", root.display()),
        )
    })?;
    let target = request_file
        .strip_prefix(root)
        .unwrap_or(request_file)
        .to_path_buf();
    let key = AuthCacheKey {
        root: canonical_root.clone(),
        provider: provider.clone(),
        environment: serde_json::to_string(env).map_err(|error| {
            Diagnostic::new(
                Code::InvalidAssetInput,
                format!("cannot identify auth environment: {error}"),
            )
        })?,
        settings: serde_json::to_string(&config).map_err(|error| {
            Diagnostic::new(
                Code::InvalidAssetInput,
                format!("cannot identify auth settings: {error}"),
            )
        })?,
        mode,
    };

    // ponytail: one async lock serializes auth refreshes; split per provider
    // only if concurrent multi-project throughput becomes measurable.
    let mut state = auth.state.lock().await;
    let estimated = state
        .durations_ms
        .get(&(canonical_root, target))
        .copied()
        .unwrap_or_default();
    let required = Duration::from_secs(config.refresh_before_seconds)
        .saturating_add(Duration::from_millis(estimated));
    if let Some(cached) = state.tokens.get(&key) {
        if cached.expires_at.saturating_duration_since(Instant::now()) > required {
            return Ok(Some(ProjectAuthHeader {
                token: cached.value.clone(),
                refreshed: false,
                provider_name: selected.name,
            }));
        }
    }

    let execution = execute_without_project_auth(
        &provider_doc,
        root,
        &provider,
        env.clone(),
        secret,
        engine,
        mode,
        cancel,
        Value::Null,
        empty_object(),
        None,
    )
    .await;
    if execution.result.status != RunStatus::Passed {
        let detail = execution
            .result
            .diagnostics
            .first()
            .map(|diagnostic| format!(": {}", diagnostic.message))
            .unwrap_or_default();
        return Err(Diagnostic::new(
            Code::HttpError,
            format!(
                "auth provider '{}' request {} failed{detail}",
                selected.name, config.request
            ),
        ));
    }
    let response = execution.response_unmasked.ok_or_else(|| {
        Diagnostic::new(
            Code::HttpError,
            format!(
                "auth provider '{}' request {} returned no response",
                selected.name, config.request
            ),
        )
    })?;
    if !(200..300).contains(&response.status) {
        return Err(Diagnostic::new(
            Code::HttpError,
            format!(
                "auth provider '{}' request {} returned HTTP {}",
                selected.name, config.request, response.status
            ),
        ));
    }
    let token = super::pipeline::query_one(&response, &config.token_path)?
        .as_str()
        .filter(|token| !token.is_empty())
        .map(str::to_string)
        .ok_or_else(|| {
            Diagnostic::new(
                Code::InvalidAssetInput,
                format!(
                    "auth token at {} must be a non-empty string",
                    config.token_path
                ),
            )
        })?;
    state.tokens.insert(
        key,
        CachedAuth {
            value: token.clone(),
            expires_at: Instant::now() + Duration::from_secs(config.lifetime_seconds),
        },
    );
    Ok(Some(ProjectAuthHeader {
        token,
        refreshed: true,
        provider_name: selected.name,
    }))
}

fn select_project_auth(
    doc: &RequestDocument,
    project: &ProjectConfig,
    root: &Path,
    request_file: &Path,
) -> Result<Option<SelectedProjectAuth>, Diagnostic> {
    let target = request_file.strip_prefix(root).unwrap_or(request_file);
    if project
        .auth
        .iter()
        .chain(project.auth_providers.values())
        .any(|config| target == Path::new(&config.request))
    {
        return Ok(None);
    }
    if let Some(selection) = &doc.auth {
        selection
            .validate()
            .map_err(|message| Diagnostic::new(Code::InvalidAssetInput, message))?;
        if selection.is_none() {
            return Ok(None);
        }
        let config = project
            .auth_providers
            .get(selection.as_str())
            .ok_or_else(|| {
                Diagnostic::new(
                    Code::InvalidAssetInput,
                    format!(
                        "request selects unknown auth provider '{}'",
                        selection.as_str()
                    ),
                )
            })?
            .clone();
        config
            .validate()
            .map_err(|message| Diagnostic::new(Code::InvalidAssetInput, message))?;
        if has_explicit_auth(doc) {
            return Ok(None);
        }
        return Ok(Some(SelectedProjectAuth {
            name: selection.as_str().to_string(),
            config,
        }));
    }
    if has_explicit_auth(doc) {
        return Ok(None);
    }

    let mut matching = Vec::new();
    for (name, config) in &project.auth_providers {
        if auth_applies(config, root, request_file)? {
            let scope = Path::new(&config.apply_to);
            matching.push((
                scope.components().count(),
                config.apply_to.len(),
                name,
                config,
            ));
        }
    }
    matching.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| right.1.cmp(&left.1)));
    if let Some((components, length, name, config)) = matching.first() {
        let ambiguous = matching
            .iter()
            .skip(1)
            .take_while(|candidate| candidate.0 == *components && candidate.1 == *length)
            .map(|candidate| candidate.2.as_str())
            .collect::<Vec<_>>();
        if !ambiguous.is_empty() {
            let mut names = vec![name.as_str()];
            names.extend(ambiguous);
            return Err(Diagnostic::new(
                Code::InvalidAssetInput,
                format!(
                    "ambiguous auth providers for {} at equal specificity: {}",
                    target.display(),
                    names.join(", ")
                ),
            ));
        }
        return Ok(Some(SelectedProjectAuth {
            name: name.as_str().to_string(),
            config: (**config).clone(),
        }));
    }
    if let Some(config) = &project.auth {
        if auth_applies(config, root, request_file)? {
            return Ok(Some(SelectedProjectAuth {
                name: "legacy".to_string(),
                config: config.clone(),
            }));
        }
    }
    Ok(None)
}

fn auth_applies(
    config: &ProjectAuthConfig,
    root: &Path,
    request_file: &Path,
) -> Result<bool, Diagnostic> {
    config
        .validate()
        .map_err(|message| Diagnostic::new(Code::InvalidAssetInput, message))?;
    let scope = Path::new(&config.apply_to);
    let target = request_file.strip_prefix(root).unwrap_or(request_file);
    Ok(target == scope || target.starts_with(scope))
}

fn has_explicit_auth(doc: &RequestDocument) -> bool {
    doc.request
        .headers
        .iter()
        .any(|header| header.enabled && header.name.eq_ignore_ascii_case("authorization"))
        || doc.pipeline.iter().any(|entry| {
            entry.enabled
                && entry.phase == PipelinePhase::BeforeRequest
                && (entry.uses.starts_with("builtin:bearer@")
                    || entry.uses.starts_with("builtin:basic@")
                    || (entry.uses.starts_with("builtin:header@")
                        && entry
                            .with
                            .get("name")
                            .and_then(Value::as_str)
                            .is_some_and(|name| name.eq_ignore_ascii_case("authorization"))))
        })
}

async fn record_duration(auth: &AuthSession, root: &Path, request_file: &Path, duration_ms: u64) {
    let request = request_file
        .strip_prefix(root)
        .unwrap_or(request_file)
        .to_path_buf();
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let mut state = auth.state.lock().await;
    let duration = state.durations_ms.entry((root, request)).or_default();
    *duration = (*duration).max(duration_ms);
}

fn failed_execution(
    doc: &RequestDocument,
    diagnostic: Diagnostic,
    started: Instant,
) -> RunExecution {
    RunExecution {
        result: RunResult {
            request_id: doc.meta.id.clone(),
            status: RunStatus::Error,
            skip_reason: None,
            http: None,
            assertions: Vec::new(),
            runtime: BTreeMap::new(),
            diagnostics: vec![diagnostic],
            duration_ms: started.elapsed().as_millis() as u64,
        },
        response: None,
        response_unmasked: None,
        runtime_unmasked: BTreeMap::new(),
    }
}

fn skipped_execution(doc: &RequestDocument, reason: String, started: Instant) -> RunExecution {
    RunExecution {
        result: RunResult {
            request_id: doc.meta.id.clone(),
            status: RunStatus::Skipped,
            skip_reason: Some(reason),
            http: None,
            assertions: Vec::new(),
            runtime: BTreeMap::new(),
            diagnostics: Vec::new(),
            duration_ms: started.elapsed().as_millis() as u64,
        },
        response: None,
        response_unmasked: None,
        runtime_unmasked: BTreeMap::new(),
    }
}

fn skip_reason(
    doc: &RequestDocument,
    env: &Value,
    secret: &(dyn Fn(&str) -> Option<String> + Sync),
    runtime: &Value,
) -> Option<String> {
    let guard = doc.execution.as_ref()?.skip.as_ref()?;
    evaluate_condition(&guard.when, env, secret, runtime).then(|| guard.reason.clone())
}

fn evaluate_condition(
    condition: &ExecutionCondition,
    env: &Value,
    secret: &(dyn Fn(&str) -> Option<String> + Sync),
    runtime: &Value,
) -> bool {
    match condition {
        ExecutionCondition::Literal(condition) => condition.literal,
        ExecutionCondition::Variable(condition) => match condition.var.scope {
            ExecutionVariableScope::Env => env
                .get(condition.var.name.as_str())
                .is_some_and(bruno_truthy),
            ExecutionVariableScope::Secret => secret(&condition.var.name)
                .as_deref()
                .is_some_and(|value| !value.is_empty()),
            ExecutionVariableScope::Runtime => runtime
                .get(condition.var.name.as_str())
                .is_some_and(bruno_truthy),
        },
        ExecutionCondition::Not(condition) => {
            !evaluate_condition(&condition.not, env, secret, runtime)
        }
        ExecutionCondition::All(condition) => condition
            .all
            .iter()
            .all(|item| evaluate_condition(item, env, secret, runtime)),
        ExecutionCondition::Any(condition) => condition
            .any
            .iter()
            .any(|item| evaluate_condition(item, env, secret, runtime)),
    }
}

fn bruno_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_f64().is_some_and(|value| value != 0.0),
        Value::String(value) => !value.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

#[allow(clippy::too_many_arguments)]
async fn execute(
    doc: &RequestDocument,
    root: &Path,
    request_file: &Path,
    env: Value,
    secret: &(dyn Fn(&str) -> Option<String> + Sync),
    engine: &HttpEngine,
    mode: RunMode,
    cancel: CancellationToken,
    matrix: Value,
    runtime_in: Value,
    auth: &AuthSession,
) -> RunExecution {
    let started = Instant::now();
    if let Some(reason) = skip_reason(doc, &env, secret, &runtime_in) {
        return skipped_execution(doc, reason, started);
    }
    let auth_header = match project_auth_header(
        doc,
        root,
        request_file,
        &env,
        secret,
        engine,
        mode,
        cancel.clone(),
        auth,
    )
    .await
    {
        Ok(header) => header,
        Err(diagnostic) => return failed_execution(doc, diagnostic, started),
    };
    let execution = execute_without_project_auth(
        doc,
        root,
        request_file,
        env,
        secret,
        engine,
        mode,
        cancel,
        matrix,
        runtime_in,
        auth_header,
    )
    .await;
    record_duration(auth, root, request_file, execution.result.duration_ms).await;
    execution
}

#[allow(clippy::too_many_arguments)]
async fn execute_without_project_auth(
    doc: &RequestDocument,
    root: &Path,
    request_file: &Path,
    env: Value,
    secret: &(dyn Fn(&str) -> Option<String> + Sync),
    engine: &HttpEngine,
    mode: RunMode,
    cancel: CancellationToken,
    matrix: Value,
    runtime_in: Value,
    auth_header: Option<ProjectAuthHeader>,
) -> RunExecution {
    let started = std::time::Instant::now();

    let mut ir = match validate_case(doc, root, request_file, env, secret, matrix, runtime_in) {
        Ok(ir) => ir,
        Err(diags) => {
            return RunExecution {
                result: RunResult {
                    request_id: doc.meta.id.clone(),
                    status: RunStatus::Error,
                    skip_reason: None,
                    http: None,
                    assertions: Vec::new(),
                    runtime: BTreeMap::new(),
                    diagnostics: diags,
                    duration_ms: started.elapsed().as_millis() as u64,
                },
                response: None,
                response_unmasked: None,
                runtime_unmasked: BTreeMap::new(),
            };
        }
    };

    let mut diagnostics: Vec<Diagnostic> = Vec::new();
    let mut assertions: Vec<AssertionResult> = Vec::new();
    let mut runtime: BTreeMap<String, Value> = BTreeMap::new();
    let mut error: Option<String> = None;

    if let Some(auth_header) = auth_header {
        ir.secret_values.push(auth_header.token.clone());
        ir.headers.push(ResolvedHeader {
            name: "Authorization".to_string(),
            value: format!("Bearer {}", auth_header.token),
        });
        if auth_header.refreshed {
            diagnostics.push(Diagnostic::new(
                Code::AuthRefresh,
                format!(
                    "auth provider '{}' refreshed before request",
                    auth_header.provider_name
                ),
            ));
        }
    }

    // --- beforeRequest hooks ---
    for entry in ir
        .pipeline
        .clone()
        .iter()
        .filter(|e| e.phase == PipelinePhase::BeforeRequest)
    {
        let outcome = if is_project_asset(entry) {
            run_js_hook(entry, &ir)
        } else {
            run_before_request(entry, &ir)
        };
        match outcome {
            Ok(patch) => {
                let writes = patch.runtime.clone();
                apply_patch(&mut ir, patch, &mut diagnostics);
                merge_runtime_writes(writes, entry, &mut ir, &mut runtime, &mut diagnostics);
            }
            Err(d) => {
                error.get_or_insert_with(|| d.message.clone());
                diagnostics.push(d);
            }
        }
    }

    if error.is_none() {
        if let Some(delay_ms) = doc
            .execution
            .as_ref()
            .and_then(|execution| execution.delay_before_ms)
        {
            tokio::select! {
                _ = cancel.cancelled() => {
                    let diagnostic = Diagnostic::new(
                        Code::HttpError,
                        "request cancelled during pre-request delay",
                    );
                    error = Some(diagnostic.message.clone());
                    diagnostics.push(diagnostic);
                }
                _ = tokio::time::sleep(Duration::from_millis(delay_ms)) => {}
            }
        }
    }

    // --- send or mock (skipped if a hook or the delay already errored) ---
    let response = if error.is_some() {
        None
    } else {
        match mode {
            RunMode::Mock => match &ir.mock.clone() {
                Some(m) => mock_response(m, &ir, &mut diagnostics),
                None => {
                    let d = Diagnostic::new(
                        Code::HttpError,
                        "mock mode requested but the document has no mock",
                    );
                    error = Some(d.message.clone());
                    diagnostics.push(d);
                    None
                }
            },
            RunMode::Http => match send(&ir, engine, cancel).await {
                Ok(r) => Some(r),
                Err(d) => {
                    error = Some(d.message.clone());
                    diagnostics.push(d);
                    None
                }
            },
        }
    };

    // --- afterResponse assertions + extractors (only with a response) ---
    if let Some(response) = &response {
        for entry in ir
            .pipeline
            .clone()
            .iter()
            .filter(|e| e.phase == PipelinePhase::AfterResponse)
        {
            let outcome = reaction_asset(entry, &ir, Some(response), None);
            merge_reaction(
                outcome,
                entry,
                &mut ir,
                &mut assertions,
                &mut runtime,
                &mut diagnostics,
                &mut error,
            );
        }
    }

    // --- onError: only when the run errored (§9) ---
    if error.is_some() {
        let err = error.clone();
        for entry in ir
            .pipeline
            .clone()
            .iter()
            .filter(|e| e.phase == PipelinePhase::OnError)
        {
            let outcome = reaction_asset(entry, &ir, response.as_ref(), err.as_deref());
            merge_reaction(
                outcome,
                entry,
                &mut ir,
                &mut assertions,
                &mut runtime,
                &mut diagnostics,
                &mut error,
            );
        }
    }

    // --- finally: always (§9), for teardown/always-checks ---
    for entry in ir
        .pipeline
        .clone()
        .iter()
        .filter(|e| e.phase == PipelinePhase::Finally)
    {
        let outcome = reaction_asset(entry, &ir, response.as_ref(), error.as_deref());
        merge_reaction(
            outcome,
            entry,
            &mut ir,
            &mut assertions,
            &mut runtime,
            &mut diagnostics,
            &mut error,
        );
    }

    if let Some(response_body) = response.as_ref().and_then(ResponseView::json) {
        let mut sensitive_response_values = Vec::new();
        collect_sensitive_strings(&response_body, &mut sensitive_response_values);
        for value in sensitive_response_values {
            if !ir.secret_values.contains(&value) {
                ir.secret_values.push(value);
            }
        }
    }

    let http = response.as_ref().map(|r| HttpResultView {
        status: r.status,
        time_ms: r.time_ms,
        bytes: r.body.len(),
    });
    let has_asset_error = diagnostics.iter().any(Diagnostic::is_error);
    let status = if has_asset_error || error.is_some() || response.is_none() {
        RunStatus::Error
    } else if assertions.iter().any(|a| !a.passed) {
        RunStatus::Failed
    } else {
        RunStatus::Passed
    };
    let runtime_unmasked = runtime.clone();
    let public_response = response
        .as_ref()
        .map(|response| mask_response(&ir, response));
    let result = finish(&ir, status, http, assertions, runtime, diagnostics, started);
    RunExecution {
        result,
        response: public_response,
        response_unmasked: response,
        runtime_unmasked,
    }
}

/// Run a sequence of request files in order, threading extracted runtime
/// forward: request N's `extract-*` results are visible to request N+1 as
/// `${runtime.*}` (§9). Each file runs with a fresh matrix (no matrix), but
/// the accumulated runtime persists. `root` is the shared project root.
#[allow(clippy::too_many_arguments)]
pub async fn run_sequence(
    files: &[std::path::PathBuf],
    root: &Path,
    env: Value,
    secret: &(dyn Fn(&str) -> Option<String> + Sync),
    engine: &HttpEngine,
    mode: RunMode,
    cancel: CancellationToken,
) -> Vec<RunResult> {
    run_sequence_with_responses(files, root, env, secret, engine, mode, cancel)
        .await
        .into_iter()
        .map(|(result, _)| result)
        .collect()
}

/// [`run_sequence`] while retaining every step's response for GUI history
/// and catalog previews.
#[allow(clippy::too_many_arguments)]
pub async fn run_sequence_with_responses(
    files: &[std::path::PathBuf],
    root: &Path,
    env: Value,
    secret: &(dyn Fn(&str) -> Option<String> + Sync),
    engine: &HttpEngine,
    mode: RunMode,
    cancel: CancellationToken,
) -> Vec<(RunResult, Option<ResponseView>)> {
    let auth = AuthSession::default();
    run_sequence_with_responses_in_session(files, root, env, secret, engine, mode, cancel, &auth)
        .await
}

/// [`run_sequence_with_responses`] using a caller-owned auth cache.
#[allow(clippy::too_many_arguments)]
pub async fn run_sequence_with_responses_in_session(
    files: &[std::path::PathBuf],
    root: &Path,
    env: Value,
    secret: &(dyn Fn(&str) -> Option<String> + Sync),
    engine: &HttpEngine,
    mode: RunMode,
    cancel: CancellationToken,
    auth: &AuthSession,
) -> Vec<(RunResult, Option<ResponseView>)> {
    let environments = vec![env; files.len()];
    run_sequence_with_environment_values_impl(
        files,
        root,
        &environments,
        secret,
        engine,
        mode,
        cancel,
        auth,
    )
    .await
}

/// Run a sequence with one resolved environment per request. This preserves
/// runtime threading while allowing folder/request environment defaults.
#[allow(clippy::too_many_arguments)]
pub async fn run_sequence_with_environment_values_in_session(
    files: &[std::path::PathBuf],
    root: &Path,
    environments: &[Value],
    secret: &(dyn Fn(&str) -> Option<String> + Sync),
    engine: &HttpEngine,
    mode: RunMode,
    cancel: CancellationToken,
    auth: &AuthSession,
) -> Result<Vec<(RunResult, Option<ResponseView>)>, Diagnostic> {
    if files.len() != environments.len() {
        return Err(Diagnostic::new(
            Code::InvalidAssetInput,
            format!(
                "sequence has {} request(s), but {} environment value(s)",
                files.len(),
                environments.len()
            ),
        ));
    }
    Ok(run_sequence_with_environment_values_impl(
        files,
        root,
        environments,
        secret,
        engine,
        mode,
        cancel,
        auth,
    )
    .await)
}

#[allow(clippy::too_many_arguments)]
async fn run_sequence_with_environment_values_impl(
    files: &[std::path::PathBuf],
    root: &Path,
    environments: &[Value],
    secret: &(dyn Fn(&str) -> Option<String> + Sync),
    engine: &HttpEngine,
    mode: RunMode,
    cancel: CancellationToken,
    auth: &AuthSession,
) -> Vec<(RunResult, Option<ResponseView>)> {
    let mut runtime = empty_object();
    let mut results = Vec::with_capacity(files.len());
    for (position, file) in files.iter().enumerate() {
        let started = std::time::Instant::now();
        let doc = match super::assertions::load_request_document(file) {
            Ok(d) => d,
            Err(msg) => {
                results.push((
                    RunResult {
                        request_id: file.display().to_string(),
                        status: RunStatus::Error,
                        skip_reason: None,
                        http: None,
                        assertions: Vec::new(),
                        runtime: BTreeMap::new(),
                        diagnostics: vec![Diagnostic::new(Code::InvalidAssetInput, msg)],
                        duration_ms: started.elapsed().as_millis() as u64,
                    },
                    None,
                ));
                continue;
            }
        };
        let execution = execute(
            &doc,
            root,
            file,
            environments[position].clone(),
            secret,
            engine,
            mode,
            cancel.clone(),
            Value::Null,
            runtime.clone(),
            auth,
        )
        .await;
        // Thread this request's runtime forward to the next.
        if let Value::Object(map) = &mut runtime {
            for (k, v) in &execution.runtime_unmasked {
                map.insert(k.clone(), v.clone());
            }
        }
        results.push((execution.result, execution.response));
    }
    results
}

/// Run one afterResponse/onError/finally asset. Builtins need a response;
/// without one they are skipped with an info note. JS assets always run,
/// receiving the response (if any) and error (onError/finally).
fn reaction_asset(
    entry: &super::ir::ResolvedPipelineEntry,
    ir: &ResolvedRequest,
    response: Option<&ResponseView>,
    error: Option<&str>,
) -> Result<(Vec<AssertionResult>, BTreeMap<String, Value>), Diagnostic> {
    if is_project_asset(entry) {
        run_js_after(entry, ir, response, error)
    } else {
        match response {
            Some(r) => run_after_response(entry, r),
            None => Err(Diagnostic::new(
                Code::AssetError,
                format!(
                    "builtin {:?} skipped: no response available in this phase",
                    entry.asset.raw
                ),
            )
            .info()
            .with_ref(&entry.asset.raw)),
        }
    }
}

/// Fold one reaction result into the accumulators (assertions, runtime with
/// conflict warnings, diagnostics, and the first error seen).
fn merge_reaction(
    outcome: Result<(Vec<AssertionResult>, BTreeMap<String, Value>), Diagnostic>,
    entry: &super::ir::ResolvedPipelineEntry,
    ir: &mut ResolvedRequest,
    assertions: &mut Vec<AssertionResult>,
    runtime: &mut BTreeMap<String, Value>,
    diagnostics: &mut Vec<Diagnostic>,
    error: &mut Option<String>,
) {
    match outcome {
        Ok((results, extracted)) => {
            assertions.extend(results);
            merge_runtime_writes(extracted, entry, ir, runtime, diagnostics);
        }
        Err(d) => {
            if d.is_error() {
                error.get_or_insert_with(|| d.message.clone());
            }
            diagnostics.push(d);
        }
    }
}

fn merge_runtime_writes(
    writes: BTreeMap<String, Value>,
    entry: &ResolvedPipelineEntry,
    ir: &mut ResolvedRequest,
    runtime: &mut BTreeMap<String, Value>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    for (key, value) in writes {
        let sensitive = sensitive_name(&key)
            || entry
                .input
                .get("sensitive")
                .and_then(Value::as_bool)
                .unwrap_or(false);
        if sensitive {
            let mut values = Vec::new();
            collect_strings(&value, &mut values);
            for value in values {
                if !ir.secret_values.contains(&value) {
                    ir.secret_values.push(value);
                }
            }
        }
        if runtime.insert(key.clone(), value.clone()).is_some() {
            diagnostics.push(
                Diagnostic::new(
                    Code::PipelineConflict,
                    format!("runtime key {key:?} written by more than one hook or extractor"),
                )
                .with_ref(&entry.asset.raw),
            );
        }
        if !ir.runtime_unmasked.is_object() {
            ir.runtime_unmasked = empty_object();
        }
        ir.runtime_unmasked
            .as_object_mut()
            .expect("runtime was initialized as an object")
            .insert(key.clone(), value.clone());
        if !sensitive {
            if !ir.runtime.is_object() {
                ir.runtime = empty_object();
            }
            ir.runtime
                .as_object_mut()
                .expect("runtime was initialized as an object")
                .insert(key, value);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn finish(
    ir: &ResolvedRequest,
    status: RunStatus,
    http: Option<HttpResultView>,
    mut assertions: Vec<AssertionResult>,
    mut runtime: BTreeMap<String, Value>,
    mut diagnostics: Vec<Diagnostic>,
    started: std::time::Instant,
) -> RunResult {
    // Mask secrets in every field that leaves the runner. Sequence carry uses
    // `RunExecution::runtime_unmasked`, never this public map.
    for assertion in &mut assertions {
        mask_assertion(ir, assertion);
    }
    runtime = mask_runtime_writes(ir, runtime);
    for diagnostic in &mut diagnostics {
        mask_diagnostic(ir, diagnostic);
    }
    RunResult {
        request_id: ir.id.clone(),
        status,
        skip_reason: None,
        http,
        assertions,
        runtime,
        diagnostics,
        duration_ms: started.elapsed().as_millis() as u64,
    }
}

// ---------------------------------------------------------------------
// Project (.js) assets via the QuickJS host (§15 trusted-local tier)
// ---------------------------------------------------------------------

/// JSON snapshot of the request handed to JS assets as `ctx.request`.
fn request_ctx(ir: &ResolvedRequest) -> Value {
    serde_json::json!({
        "method": ir.method.as_str(),
        "url": ir.url,
        "headers": ir.headers.iter().map(|h| serde_json::json!({"name": h.name, "value": h.value})).collect::<Vec<_>>(),
        "body": request_body_ctx(&ir.body),
    })
}

fn request_body_type(body: &ResolvedBody) -> &'static str {
    match body {
        ResolvedBody::None => "none",
        ResolvedBody::Json(_) => "json",
        ResolvedBody::Text(_) => "text",
        ResolvedBody::Form(_) => "form",
        ResolvedBody::Multipart(_) => "multipart",
        ResolvedBody::Binary { .. } => "binary",
    }
}

fn request_body_ctx(body: &ResolvedBody) -> Value {
    match body {
        ResolvedBody::None => Value::Null,
        ResolvedBody::Json(value) => value.clone(),
        ResolvedBody::Text(value) => Value::String(value.clone()),
        ResolvedBody::Form(fields) => Value::Object(
            fields
                .iter()
                .map(|field| (field.name.clone(), Value::String(field.value.clone())))
                .collect(),
        ),
        ResolvedBody::Multipart(parts) => Value::Array(
            parts
                .iter()
                .map(|part| {
                    let mut metadata = serde_json::Map::from_iter([
                        ("name".to_string(), Value::String(part.name.clone())),
                        (
                            "contentType".to_string(),
                            part.content_type
                                .clone()
                                .map(Value::String)
                                .unwrap_or(Value::Null),
                        ),
                        (
                            "filename".to_string(),
                            part.filename
                                .clone()
                                .map(Value::String)
                                .unwrap_or(Value::Null),
                        ),
                    ]);
                    match &part.data {
                        ResolvedMultipartData::Text(value) => {
                            metadata.insert("type".to_string(), Value::String("text".to_string()));
                            metadata.insert("value".to_string(), Value::String(value.clone()));
                        }
                        ResolvedMultipartData::File(_) => {
                            metadata.insert("type".to_string(), Value::String("file".to_string()));
                        }
                    }
                    Value::Object(metadata)
                })
                .collect(),
        ),
        ResolvedBody::Binary { content_type, data } => serde_json::json!({
            "type": "binary",
            "contentType": content_type,
            "size": data.len(),
        }),
    }
}

fn is_bruno_compatibility(entry: &ResolvedPipelineEntry) -> bool {
    matches!(
        entry.input.get("compatibility").and_then(Value::as_str),
        Some("bruno-tests-v1") | Some("bruno-scripts-v1")
    )
}

fn bruno_runtime_ctx(entry: &ResolvedPipelineEntry, ir: &ResolvedRequest) -> Value {
    let mut runtime = ir.runtime.clone();
    let Some(runtime_object) = runtime.as_object_mut() else {
        return runtime;
    };
    let Some(unmasked) = ir.runtime_unmasked.as_object() else {
        return runtime;
    };
    for name in entry
        .input
        .get("runtimeAccess")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        if let Some(value) = unmasked.get(name) {
            runtime_object.insert(name.to_string(), value.clone());
        }
    }
    runtime
}

fn has_bruno_compatibility(ir: &ResolvedRequest) -> bool {
    ir.pipeline.iter().any(is_bruno_compatibility)
}

fn sensitive_context_name(name: &str) -> bool {
    sensitive_name(name)
}

fn sanitize_bruno_value(ir: &ResolvedRequest, value: &Value) -> Value {
    match value {
        Value::String(text) => Value::String(ir.mask(text)),
        Value::Array(values) => Value::Array(
            values
                .iter()
                .map(|value| sanitize_bruno_value(ir, value))
                .collect(),
        ),
        Value::Object(values) => Value::Object(
            values
                .iter()
                .filter(|(name, _)| !sensitive_context_name(name))
                .map(|(name, value)| (name.clone(), sanitize_bruno_value(ir, value)))
                .collect(),
        ),
        other => other.clone(),
    }
}

fn sanitize_bruno_response_value(ir: &ResolvedRequest, value: &Value) -> Value {
    match value {
        Value::String(text) => Value::String(ir.mask(text)),
        Value::Array(values) => Value::Array(
            values
                .iter()
                .map(|value| sanitize_bruno_response_value(ir, value))
                .collect(),
        ),
        Value::Object(values) => Value::Object(
            values
                .iter()
                .map(|(name, value)| (name.clone(), sanitize_bruno_response_value(ir, value)))
                .collect(),
        ),
        other => other.clone(),
    }
}

fn collect_sensitive_strings(value: &Value, output: &mut Vec<String>) {
    match value {
        Value::Array(values) => {
            for value in values {
                collect_sensitive_strings(value, output);
            }
        }
        Value::Object(values) => {
            for (name, value) in values {
                if sensitive_context_name(name) {
                    collect_strings(value, output);
                } else {
                    collect_sensitive_strings(value, output);
                }
            }
        }
        _ => {}
    }
}

fn collect_strings(value: &Value, output: &mut Vec<String>) {
    match value {
        Value::String(value) if !value.is_empty() => output.push(value.clone()),
        Value::Array(values) => {
            for value in values {
                collect_strings(value, output);
            }
        }
        Value::Object(values) => {
            for value in values.values() {
                collect_strings(value, output);
            }
        }
        _ => {}
    }
}

fn mask_additional(text: &str, values: &[String]) -> String {
    values
        .iter()
        .filter(|value| !value.is_empty())
        .fold(text.to_string(), |text, value| text.replace(value, "***"))
}

fn bruno_request_ctx(ir: &ResolvedRequest) -> Value {
    let headers = ir
        .headers
        .iter()
        .filter(|header| !sensitive_context_name(&header.name))
        .map(|header| serde_json::json!({"name": header.name, "value": ir.mask(&header.value)}))
        .collect::<Vec<_>>();
    let body = match request_body_ctx(&ir.body) {
        Value::Array(parts) if matches!(&ir.body, ResolvedBody::Multipart(_)) => Value::Array(
            parts
                .into_iter()
                .filter(|part| {
                    part.get("name")
                        .and_then(Value::as_str)
                        .is_none_or(|name| !sensitive_context_name(name))
                })
                .collect(),
        ),
        body => body,
    };
    sanitize_bruno_value(
        ir,
        &serde_json::json!({
            "method": ir.method.as_str(),
            "url": ir.mask(&ir.url),
            "headers": headers,
            "body": body,
            "bodyType": request_body_type(&ir.body),
        }),
    )
}

fn run_js_hook(
    entry: &ResolvedPipelineEntry,
    ir: &ResolvedRequest,
) -> Result<super::pipeline::RequestPatch, Diagnostic> {
    run_js_hook_with_logs(entry, ir).map(|(patch, _)| patch)
}

fn run_js_hook_with_logs(
    entry: &ResolvedPipelineEntry,
    ir: &ResolvedRequest,
) -> Result<(super::pipeline::RequestPatch, Vec<String>), Diagnostic> {
    let ctx = if is_bruno_compatibility(entry) {
        serde_json::json!({
            "request": bruno_request_ctx(ir),
            "response": Value::Null,
            "bindings": sanitize_bruno_value(ir, &ir.bindings),
            "environment": sanitize_bruno_value(ir, &ir.environment),
            "runtime": sanitize_bruno_value(ir, &ir.runtime),
        })
    } else {
        serde_json::json!({ "request": request_ctx(ir), "bindings": ir.bindings })
    };
    let (out, logs) =
        super::jshost::run_js_asset_with_logs(&entry.asset.address, &ctx, &entry.input)
            .map_err(|d| d.with_ref(&entry.asset.raw))?;

    let mut patch = super::pipeline::RequestPatch::default();
    if let Some(url) = out.get("url").and_then(Value::as_str) {
        patch.url = Some(url.to_string());
    }
    if let Some(headers) = out.get("headers").and_then(Value::as_array) {
        for h in headers {
            let (Some(name), Some(value)) = (
                h.get("name").and_then(Value::as_str),
                h.get("value").and_then(Value::as_str),
            ) else {
                return Err(Diagnostic::new(
                    Code::AssetError,
                    "hook returned a header without string name/value",
                )
                .with_ref(&entry.asset.raw));
            };
            patch.headers.push(ResolvedHeader {
                name: name.to_string(),
                value: value.to_string(),
            });
        }
    }
    if let Some(body) = out.get("body") {
        patch.body = Some(parse_js_body_patch(body, &entry.asset.raw)?);
    }
    if let Some(runtime) = out.get("runtime") {
        let runtime = runtime.as_object().ok_or_else(|| {
            Diagnostic::new(Code::AssetError, "hook runtime output must be an object")
                .with_ref(&entry.asset.raw)
        })?;
        patch.runtime = runtime
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
    }
    Ok((patch, logs))
}

fn parse_js_body_patch(body: &Value, asset_ref: &str) -> Result<ResolvedBody, Diagnostic> {
    let body_type = body.get("type").and_then(Value::as_str).ok_or_else(|| {
        Diagnostic::new(Code::AssetError, "hook body patch requires a string `type`")
            .with_ref(asset_ref)
    })?;
    let value = body.get("value").cloned().unwrap_or(Value::Null);
    match body_type {
        "json" => Ok(ResolvedBody::Json(value)),
        "text" => value
            .as_str()
            .map(|value| ResolvedBody::Text(value.to_string()))
            .ok_or_else(|| {
                Diagnostic::new(Code::AssetError, "text body patch requires a string value")
                    .with_ref(asset_ref)
            }),
        "form" => {
            let values = value.as_object().ok_or_else(|| {
                Diagnostic::new(Code::AssetError, "form body patch requires an object value")
                    .with_ref(asset_ref)
            })?;
            let mut fields = Vec::with_capacity(values.len());
            for (name, value) in values {
                let value = match value {
                    Value::String(value) => value.clone(),
                    Value::Bool(_) | Value::Number(_) | Value::Null => value.to_string(),
                    Value::Array(_) | Value::Object(_) => {
                        return Err(Diagnostic::new(
                            Code::AssetError,
                            format!("form body field {name:?} must be scalar"),
                        )
                        .with_ref(asset_ref));
                    }
                };
                fields.push(ResolvedHeader {
                    name: name.clone(),
                    value,
                });
            }
            Ok(ResolvedBody::Form(fields))
        }
        "multipart" | "binary" | "file" => Err(Diagnostic::new(
            Code::AssetError,
            "JavaScript hooks cannot create file, binary, or multipart bodies",
        )
        .with_ref(asset_ref)),
        other => Err(Diagnostic::new(
            Code::AssetError,
            format!("unsupported hook body patch type {other:?}"),
        )
        .with_ref(asset_ref)),
    }
}

fn run_js_after(
    entry: &ResolvedPipelineEntry,
    ir: &ResolvedRequest,
    response: Option<&ResponseView>,
    error: Option<&str>,
) -> Result<(Vec<AssertionResult>, BTreeMap<String, Value>), Diagnostic> {
    run_js_after_with_logs(entry, ir, response, error)
        .map(|(assertions, runtime, _)| (assertions, runtime))
}

fn run_js_after_with_logs(
    entry: &ResolvedPipelineEntry,
    ir: &ResolvedRequest,
    response: Option<&ResponseView>,
    error: Option<&str>,
) -> Result<ReactionWithLogs, Diagnostic> {
    let response_ctx = response.map(|response| {
        serde_json::json!({
            "status": response.status,
            "headers": response.headers.iter().map(|(k, v)| serde_json::json!({"name": k, "value": v})).collect::<Vec<_>>(),
            "body": response.json().unwrap_or(Value::Null),
            "bodyText": response.text(),
            "timeMs": response.time_ms,
        })
    });
    let ctx = if is_bruno_compatibility(entry) {
        let response_ctx = response.map(|response| {
            let snapshot = serde_json::json!({
                "status": response.status,
                "headers": response.headers.iter()
                    .filter(|(name, _)| !sensitive_context_name(name))
                    .map(|(name, value)| serde_json::json!({"name": name, "value": value}))
                    .collect::<Vec<_>>(),
                "body": response.json().unwrap_or_else(|| Value::String(response.text().into_owned())),
                "bodyText": response.text(),
                "timeMs": response.time_ms,
            });
            sanitize_bruno_response_value(ir, &snapshot)
        });
        serde_json::json!({
            "request": bruno_request_ctx(ir),
            "bindings": sanitize_bruno_value(ir, &ir.bindings),
            "environment": sanitize_bruno_value(ir, &ir.environment),
            "runtime": bruno_runtime_ctx(entry, ir),
            "response": response_ctx,
            "error": error.map(|error| ir.mask(error)),
        })
    } else {
        serde_json::json!({
            "request": request_ctx(ir),
            "bindings": ir.bindings,
            "response": response_ctx,
            "error": error,
        })
    };
    let (out, mut logs) =
        super::jshost::run_js_asset_with_logs(&entry.asset.address, &ctx, &entry.input)
            .map_err(|d| d.with_ref(&entry.asset.raw))?;

    // The return shape decides the meaning (§5): `runtime` → extractor,
    // `passed` (object or array of objects) → assertion result(s).
    let mut assertions = Vec::new();
    let mut runtime = BTreeMap::new();
    if let Some(rt) = out.get("runtime").and_then(Value::as_object) {
        for (k, v) in rt {
            runtime.insert(k.clone(), v.clone());
        }
    }
    let mut push_assertion = |item: &Value| -> Result<(), Diagnostic> {
        let passed = item.get("passed").and_then(Value::as_bool).ok_or_else(|| {
            Diagnostic::new(
                Code::AssetError,
                "assertion result must have a boolean `passed`",
            )
            .with_ref(&entry.asset.raw)
        })?;
        assertions.push(AssertionResult {
            passed,
            message: item
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("(no message)")
                .to_string(),
            expected: item.get("expected").cloned(),
            actual: item.get("actual").cloned(),
            path: item.get("path").and_then(Value::as_str).map(str::to_string),
        });
        Ok(())
    };
    match &out {
        Value::Array(items) => {
            for item in items {
                push_assertion(item)?;
            }
        }
        Value::Object(map) if map.contains_key("passed") => push_assertion(&out)?,
        Value::Object(map) if map.contains_key("runtime") => {}
        Value::Null => {}
        other => {
            return Err(Diagnostic::new(
                Code::AssetError,
                format!("unrecognized afterResponse asset result: {other}"),
            )
            .with_ref(&entry.asset.raw));
        }
    }
    if is_bruno_compatibility(entry) {
        let mut sensitive_response_values = Vec::new();
        if let Some(response) = response.and_then(ResponseView::json) {
            collect_sensitive_strings(&response, &mut sensitive_response_values);
        }
        for assertion in &mut assertions {
            assertion.message = mask_additional(&assertion.message, &sensitive_response_values);
            if let Some(value) = &mut assertion.expected {
                mask_sensitive_named_values(value);
                mask_value_strings(value, &sensitive_response_values);
            }
            if let Some(value) = &mut assertion.actual {
                mask_sensitive_named_values(value);
                mask_value_strings(value, &sensitive_response_values);
            }
        }
        for log in &mut logs {
            *log = mask_additional(log, &sensitive_response_values);
        }
    }
    Ok((assertions, runtime, logs))
}

fn mask_value_strings(value: &mut Value, sensitive: &[String]) {
    match value {
        Value::String(text) => *text = mask_additional(text, sensitive),
        Value::Array(values) => {
            for value in values {
                mask_value_strings(value, sensitive);
            }
        }
        Value::Object(values) => {
            for value in values.values_mut() {
                mask_value_strings(value, sensitive);
            }
        }
        _ => {}
    }
}

fn mask_sensitive_named_values(value: &mut Value) {
    match value {
        Value::Array(values) => {
            for value in values {
                mask_sensitive_named_values(value);
            }
        }
        Value::Object(values) => {
            for (name, value) in values {
                if sensitive_context_name(name) {
                    *value = Value::String("***".to_string());
                } else {
                    mask_sensitive_named_values(value);
                }
            }
        }
        _ => {}
    }
}

/// Execute one resolved catalog asset without sending a request.
///
/// `beforeRequest` previews apply the selected hook to an IR clone.
/// `afterResponse` previews evaluate against the retained last response.
pub fn preview_asset(
    ir: &ResolvedRequest,
    entry: &ResolvedPipelineEntry,
    response: Option<&ResponseView>,
) -> Result<CatalogPreview, Diagnostic> {
    let preview = (|| match entry.phase {
        PipelinePhase::BeforeRequest => {
            let request_before = catalog_request_view(ir);
            let (patch, logs) = if is_project_asset(entry) {
                run_js_hook_with_logs(entry, ir)?
            } else {
                (run_before_request(entry, ir)?, Vec::new())
            };
            let runtime_writes = patch.runtime.clone();
            let mut request_after = ir.clone();
            let mut diagnostics = Vec::new();
            apply_patch(&mut request_after, patch, &mut diagnostics);
            Ok(CatalogPreview {
                request_before: Some(request_before),
                request_after: Some(catalog_request_view(&request_after)),
                assertions: Vec::new(),
                runtime_writes,
                logs,
                diagnostics,
            })
        }
        PipelinePhase::AfterResponse => {
            let response = response.ok_or_else(|| {
                Diagnostic::new(
                    Code::InvalidAssetInput,
                    "run the request once before previewing an afterResponse asset",
                )
                .with_ref(&entry.asset.raw)
            })?;
            let (assertions, runtime_writes, logs) = if is_project_asset(entry) {
                run_js_after_with_logs(entry, ir, Some(response), None)?
            } else {
                let (assertions, runtime) = run_after_response(entry, response)?;
                (assertions, runtime, Vec::new())
            };
            Ok(CatalogPreview {
                request_before: None,
                request_after: None,
                assertions,
                runtime_writes,
                logs,
                diagnostics: Vec::new(),
            })
        }
        PipelinePhase::OnError | PipelinePhase::Finally => Err(Diagnostic::new(
            Code::InvalidAssetInput,
            format!("catalog preview does not support phase {:?}", entry.phase),
        )
        .with_ref(&entry.asset.raw)),
    })();
    preview
        .map(|mut preview| {
            mask_catalog_preview(ir, &mut preview);
            preview
        })
        .map_err(|mut diagnostic| {
            mask_diagnostic(ir, &mut diagnostic);
            diagnostic
        })
}

fn catalog_request_view(ir: &ResolvedRequest) -> CatalogRequestView {
    CatalogRequestView {
        url: ir.url.clone(),
        headers: ir
            .headers
            .iter()
            .map(|header| (header.name.clone(), header.value.clone()))
            .collect(),
        body: request_body_ctx(&ir.body),
    }
}

fn mask_catalog_preview(ir: &ResolvedRequest, preview: &mut CatalogPreview) {
    for request in [&mut preview.request_before, &mut preview.request_after]
        .into_iter()
        .flatten()
    {
        request.url = ir.mask(&request.url);
        for (name, value) in &mut request.headers {
            *name = ir.mask(name);
            *value = ir.mask(value);
        }
        mask_value(ir, &mut request.body);
    }
    for assertion in &mut preview.assertions {
        mask_assertion(ir, assertion);
    }
    preview.runtime_writes = mask_runtime_writes(ir, std::mem::take(&mut preview.runtime_writes));
    for log in &mut preview.logs {
        *log = ir.mask(log);
    }
    for diagnostic in &mut preview.diagnostics {
        mask_diagnostic(ir, diagnostic);
    }
}

fn mask_assertion(ir: &ResolvedRequest, assertion: &mut AssertionResult) {
    assertion.message = ir.mask(&assertion.message);
    if let Some(value) = &mut assertion.expected {
        mask_value(ir, value);
    }
    if let Some(value) = &mut assertion.actual {
        mask_value(ir, value);
    }
    if let Some(path) = &mut assertion.path {
        *path = ir.mask(path);
    }
}

fn mask_runtime_writes(
    ir: &ResolvedRequest,
    runtime: BTreeMap<String, Value>,
) -> BTreeMap<String, Value> {
    runtime
        .into_iter()
        .map(|(key, mut value)| {
            if sensitive_context_name(&key) {
                value = Value::String("***".to_string());
            } else {
                mask_value(ir, &mut value);
            }
            (ir.mask(&key), value)
        })
        .collect()
}

fn mask_response(ir: &ResolvedRequest, response: &ResponseView) -> ResponseView {
    if ir.secret_values.is_empty() {
        return response.clone();
    }
    let body = match serde_json::from_slice::<Value>(&response.body) {
        Ok(mut value) => {
            mask_value(ir, &mut value);
            serde_json::to_vec(&value).unwrap_or_else(|_| response.body.clone())
        }
        Err(_) => {
            let mut body = response.body.clone();
            for secret in &ir.secret_values {
                if secret.is_empty() {
                    continue;
                }
                let needle = secret.as_bytes();
                let mut masked = Vec::with_capacity(body.len());
                let mut remaining = body.as_slice();
                while let Some(position) = remaining
                    .windows(needle.len())
                    .position(|window| window == needle)
                {
                    masked.extend_from_slice(&remaining[..position]);
                    masked.extend_from_slice(b"***");
                    remaining = &remaining[position + needle.len()..];
                }
                masked.extend_from_slice(remaining);
                body = masked;
            }
            body
        }
    };
    ResponseView {
        status: response.status,
        headers: response
            .headers
            .iter()
            .map(|(name, value)| (ir.mask(name), ir.mask(value)))
            .collect(),
        body,
        time_ms: response.time_ms,
    }
}

fn mask_value(ir: &ResolvedRequest, value: &mut Value) {
    match value {
        Value::String(text) => *text = ir.mask(text),
        Value::Array(items) => {
            for item in items {
                mask_value(ir, item);
            }
        }
        Value::Object(object) => {
            *object = std::mem::take(object)
                .into_iter()
                .map(|(key, mut value)| {
                    if has_bruno_compatibility(ir) && sensitive_context_name(&key) {
                        value = Value::String("***".to_string());
                    } else {
                        mask_value(ir, &mut value);
                    }
                    (ir.mask(&key), value)
                })
                .collect();
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

fn mask_diagnostic(ir: &ResolvedRequest, diagnostic: &mut Diagnostic) {
    diagnostic.message = ir.mask(&diagnostic.message);
    if let Some(path) = &mut diagnostic.instance_path {
        *path = ir.mask(path);
    }
    if let Some(asset_ref) = &mut diagnostic.asset_ref {
        *asset_ref = ir.mask(asset_ref);
    }
}

fn apply_patch(
    ir: &mut ResolvedRequest,
    patch: super::pipeline::RequestPatch,
    diagnostics: &mut Vec<Diagnostic>,
) {
    if let Some(u) = patch.url {
        ir.url = u;
    }
    if let Some(body) = patch.body {
        if ir.body != body && !matches!(&ir.body, ResolvedBody::None) {
            diagnostics.push(Diagnostic::new(
                Code::PipelineConflict,
                "request body overwritten by a beforeRequest hook",
            ));
        }
        ir.body = body;
    }
    for h in patch.headers {
        // Upsert by case-insensitive name; warn on overwrite.
        if let Some(existing) = ir
            .headers
            .iter_mut()
            .find(|e| e.name.eq_ignore_ascii_case(&h.name))
        {
            if existing.value != h.value {
                diagnostics.push(Diagnostic::new(
                    Code::PipelineConflict,
                    format!("header {} overwritten by a beforeRequest hook", h.name),
                ));
            }
            existing.value = h.value;
        } else {
            ir.headers.push(h);
        }
    }
}

/// Map the IR to the exec engine's request and send it.
async fn send(
    ir: &ResolvedRequest,
    engine: &HttpEngine,
    cancel: CancellationToken,
) -> Result<ResponseView, Diagnostic> {
    let exec = request_to_exec(ir);

    let result = engine
        .execute(exec, cancel)
        .await
        .map_err(|e| Diagnostic::new(Code::HttpError, format!("request failed: {e}")))?;

    Ok(ResponseView {
        status: result.status,
        headers: result.headers.clone(),
        body: result.body.clone(),
        time_ms: result.timing.total.as_millis() as u64,
    })
}

fn request_to_exec(ir: &ResolvedRequest) -> ExecRequest {
    let mut exec = ExecRequest::new(ir.method, ir.url.clone());
    exec.headers = ir
        .headers
        .iter()
        .map(|h| (h.name.clone(), h.value.clone()))
        .collect();
    for q in &ir.query {
        append_query_pair(&mut exec.url, &q.name, &q.value, ir.encode_url);
    }
    exec.body = body_to_exec(&ir.body, &exec.headers);
    exec.timeout = ir.timeout;
    exec.follow_redirects = ir.follow_redirects;
    exec.max_redirects = ir.max_redirects;
    exec
}

fn append_query_pair(url: &mut String, name: &str, value: &str, encode: bool) {
    let fragment = url.find('#').unwrap_or(url.len());
    let separator = if url[..fragment].contains('?') {
        '&'
    } else {
        '?'
    };
    let (name, value) = if encode {
        (urlencoding_encode(name), urlencoding_encode(value))
    } else {
        (name.to_string(), value.to_string())
    };
    url.insert_str(fragment, &format!("{separator}{name}={value}"));
}

fn body_to_exec(body: &ResolvedBody, headers: &[(String, String)]) -> ExecBody {
    let has_ct = headers
        .iter()
        .any(|(k, _)| k.eq_ignore_ascii_case("content-type"));
    match body {
        ResolvedBody::None => ExecBody::None,
        ResolvedBody::Json(v) => {
            let data = serde_json::to_vec(v).unwrap_or_default();
            ExecBody::Bytes {
                content_type: (!has_ct).then(|| "application/json".to_string()),
                data,
            }
        }
        ResolvedBody::Text(s) => ExecBody::Bytes {
            content_type: (!has_ct).then(|| "text/plain".to_string()),
            data: s.clone().into_bytes(),
        },
        ResolvedBody::Form(fields) => ExecBody::Form(
            fields
                .iter()
                .map(|h| (h.name.clone(), h.value.clone()))
                .collect(),
        ),
        ResolvedBody::Multipart(parts) => ExecBody::Multipart(
            parts
                .iter()
                .map(|part| ExecPart {
                    name: part.name.clone(),
                    content_type: part.content_type.clone(),
                    file_name: part.filename.clone(),
                    data: match &part.data {
                        ResolvedMultipartData::Text(value) => ExecPartData::Text(value.clone()),
                        ResolvedMultipartData::File(path) => ExecPartData::File(path.clone()),
                    },
                })
                .collect(),
        ),
        ResolvedBody::Binary { content_type, data } => ExecBody::Bytes {
            content_type: Some(
                content_type
                    .clone()
                    .unwrap_or_else(|| "application/octet-stream".to_string()),
            ),
            data: data.clone(),
        },
    }
}

/// Render a resolved request's mock to a [`ResponseView`] (static or the
/// dynamic JS mock). Used by the mock server. Returns the response and any
/// diagnostics; `None` if the document has no mock or the mock failed.
pub fn render_mock(ir: &ResolvedRequest) -> (Option<ResponseView>, Vec<Diagnostic>) {
    let mut diags = Vec::new();
    let response = match &ir.mock {
        Some(m) => mock_response(m, ir, &mut diags),
        None => None,
    };
    (response, diags)
}

fn mock_response(
    mock: &ResolvedMock,
    ir: &ResolvedRequest,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<ResponseView> {
    match mock {
        ResolvedMock::Static {
            status,
            headers,
            body,
            ..
        } => {
            let body_bytes = match body {
                ResolvedBody::None => Vec::new(),
                ResolvedBody::Json(v) => serde_json::to_vec(v).unwrap_or_default(),
                ResolvedBody::Text(s) => s.clone().into_bytes(),
                ResolvedBody::Form(f) => f
                    .iter()
                    .map(|h| format!("{}={}", h.name, h.value))
                    .collect::<Vec<_>>()
                    .join("&")
                    .into_bytes(),
                ResolvedBody::Binary { data, .. } => data.clone(),
                ResolvedBody::Multipart(_) => Vec::new(),
            };
            Some(ResponseView {
                status: *status,
                headers: headers
                    .iter()
                    .map(|h| (h.name.clone(), h.value.clone()))
                    .collect(),
                body: body_bytes,
                time_ms: 0,
            })
        }
        ResolvedMock::Dynamic { asset, input } => {
            let ctx = serde_json::json!({ "request": request_ctx(ir), "bindings": ir.bindings });
            let out = match super::jshost::run_js_asset(&asset.address, &ctx, input) {
                Ok(v) => v,
                Err(d) => {
                    diagnostics.push(d.with_ref(&asset.raw));
                    return None;
                }
            };
            let Some(status) = out.get("status").and_then(Value::as_u64) else {
                diagnostics.push(
                    Diagnostic::new(Code::AssetError, "mock asset must return { status, ... }")
                        .with_ref(&asset.raw),
                );
                return None;
            };
            let headers = out
                .get("headers")
                .and_then(Value::as_array)
                .map(|hs| {
                    hs.iter()
                        .filter_map(|h| {
                            Some((
                                h.get("name")?.as_str()?.to_string(),
                                h.get("value")?.as_str()?.to_string(),
                            ))
                        })
                        .collect()
                })
                .unwrap_or_default();
            let body = out
                .get("body")
                .map(|b| serde_json::to_vec(b).unwrap_or_default())
                .unwrap_or_default();
            Some(ResponseView {
                status: status as u16,
                headers,
                body,
                time_ms: 0,
            })
        }
    }
}

/// Minimal percent-encoding for query values (space and reserved chars).
fn urlencoding_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Method;
    use serde_json::json;

    fn request() -> ResolvedRequest {
        ResolvedRequest {
            id: "preview".into(),
            name: "Preview".into(),
            method: Method::Get,
            url: "https://example.test/original".into(),
            headers: Vec::new(),
            query: Vec::new(),
            body: ResolvedBody::None,
            timeout: Some(Duration::from_secs(30)),
            follow_redirects: true,
            max_redirects: 10,
            encode_url: true,
            pipeline: Vec::new(),
            mock: None,
            bindings: json!({ "token": "secret-value" }),
            environment: json!({}),
            runtime: json!({}),
            runtime_unmasked: json!({}),
            secret_values: vec!["secret-value".into()],
        }
    }

    #[test]
    fn query_encoding_mode_is_deterministic_and_preserves_raw_url_query() {
        let mut raw = "https://example.test/items?already=a%2Fb#result".to_string();
        append_query_pair(&mut raw, "filter", "a b/x", false);
        assert_eq!(
            raw,
            "https://example.test/items?already=a%2Fb&filter=a b/x#result"
        );

        let mut encoded = "https://example.test/items?already=a%2Fb".to_string();
        append_query_pair(&mut encoded, "filter", "a b/x", true);
        assert_eq!(
            encoded,
            "https://example.test/items?already=a%2Fb&filter=a%20b%2Fx"
        );
    }

    #[test]
    fn maps_binary_and_transport_settings_to_exec_request() {
        let mut ir = request();
        ir.body = ResolvedBody::Binary {
            content_type: Some("application/x-test".into()),
            data: vec![0, 1, 255],
        };
        ir.timeout = None;
        ir.follow_redirects = false;
        ir.max_redirects = 2;

        let exec = request_to_exec(&ir);
        assert_eq!(exec.timeout, None);
        assert!(!exec.follow_redirects);
        assert_eq!(exec.max_redirects, 2);
        assert!(matches!(
            exec.body,
            ExecBody::Bytes { content_type: Some(content_type), data }
                if content_type == "application/x-test" && data == vec![0, 1, 255]
        ));
    }

    #[test]
    fn maps_multipart_parts_to_exec_request() {
        let file = PathBuf::from("/project/assets/upload.txt");
        let mut ir = request();
        ir.body = ResolvedBody::Multipart(vec![
            super::super::ir::ResolvedMultipartPart {
                name: "note".into(),
                content_type: Some("text/plain".into()),
                filename: None,
                data: ResolvedMultipartData::Text("hello".into()),
            },
            super::super::ir::ResolvedMultipartPart {
                name: "upload".into(),
                content_type: None,
                filename: Some("sent.txt".into()),
                data: ResolvedMultipartData::File(file.clone()),
            },
        ]);

        let exec = request_to_exec(&ir);
        let ExecBody::Multipart(parts) = exec.body else {
            panic!("multipart body")
        };
        assert_eq!(parts.len(), 2);
        assert!(matches!(&parts[0].data, ExecPartData::Text(value) if value == "hello"));
        assert!(matches!(&parts[1].data, ExecPartData::File(path) if path == &file));
        assert_eq!(parts[1].file_name.as_deref(), Some("sent.txt"));
    }

    #[test]
    fn javascript_body_patches_cover_inline_types_and_reject_files() {
        assert_eq!(
            parse_js_body_patch(&json!({"type": "json", "value": {"ok": true}}), "hook").unwrap(),
            ResolvedBody::Json(json!({"ok": true}))
        );
        assert_eq!(
            parse_js_body_patch(&json!({"type": "text", "value": "payload"}), "hook").unwrap(),
            ResolvedBody::Text("payload".into())
        );
        assert_eq!(
            parse_js_body_patch(
                &json!({"type": "form", "value": {"active": true, "count": 2}}),
                "hook",
            )
            .unwrap(),
            ResolvedBody::Form(vec![
                ResolvedHeader {
                    name: "active".into(),
                    value: "true".into(),
                },
                ResolvedHeader {
                    name: "count".into(),
                    value: "2".into(),
                },
            ])
        );
        assert!(parse_js_body_patch(
            &json!({"type": "multipart", "value": [{"type": "file"}]}),
            "hook",
        )
        .is_err());
    }

    #[test]
    fn request_environment_prefers_explicit_then_inherits() {
        let root = tempfile::tempdir().unwrap();
        let story = root.path().join("requests/story");
        let request = story.join("get.request.json");
        std::fs::create_dir_all(root.path().join("environments")).unwrap();
        std::fs::create_dir_all(&story).unwrap();
        std::fs::write(
            root.path().join("environments/local.json"),
            r#"{"name":"local"}"#,
        )
        .unwrap();
        std::fs::write(
            root.path().join("environments/staging.json"),
            r#"{"name":"staging"}"#,
        )
        .unwrap();
        std::fs::write(&request, "{}").unwrap();
        super::super::environment_scope::set_environment(&story, "staging").unwrap();

        assert_eq!(
            load_request_environment(root.path(), &request, None).unwrap(),
            json!({"name": "staging"})
        );
        assert_eq!(
            load_request_environment(root.path(), &request, Some("local")).unwrap(),
            json!({"name": "local"})
        );
        assert!(load_request_environment(root.path(), &request, Some("../secret")).is_err());
    }

    #[test]
    fn previews_builtin_assertion_against_retained_response() {
        let entry = ResolvedPipelineEntry {
            phase: PipelinePhase::AfterResponse,
            asset: super::super::refs::AssetDescriptor {
                raw: "builtin:assert-status@1".into(),
                scheme: super::super::refs::RefScheme::Builtin,
                address: "assert-status".into(),
                pointer: None,
                version: Some(1),
            },
            input: json!({ "expected": 201 }),
        };
        let response = ResponseView {
            status: 201,
            headers: Vec::new(),
            body: Vec::new(),
            time_ms: 1,
        };

        let preview = preview_asset(&request(), &entry, Some(&response)).unwrap();

        assert!(preview.assertions[0].passed);
        assert!(preview.runtime_writes.is_empty());
    }

    #[test]
    fn previews_project_hook_diff_and_masks_logs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hook.js");
        std::fs::write(
            &path,
            r#"function run(ctx) {
                console.log("token", ctx.bindings.token);
                return {
                    url: "https://example.test/changed",
                    headers: [{ name: "Authorization", value: ctx.bindings.token }]
                };
            }"#,
        )
        .unwrap();
        let entry = ResolvedPipelineEntry {
            phase: PipelinePhase::BeforeRequest,
            asset: super::super::refs::AssetDescriptor {
                raw: "./hook.js".into(),
                scheme: super::super::refs::RefScheme::File,
                address: path.to_string_lossy().into_owned(),
                pointer: None,
                version: None,
            },
            input: json!({}),
        };

        let preview = preview_asset(&request(), &entry, None).unwrap();

        assert_eq!(
            preview.request_before.unwrap().url,
            "https://example.test/original"
        );
        let after = preview.request_after.unwrap();
        assert_eq!(after.url, "https://example.test/changed");
        assert_eq!(
            after.headers,
            vec![("Authorization".to_string(), "***".to_string())]
        );
        assert_eq!(preview.logs, vec!["token ***"]);
    }

    #[test]
    fn bruno_assertion_context_includes_body_and_omits_secrets() {
        let dir = tempfile::tempdir().unwrap();
        let assets = dir.path().join("assets/bruno");
        let assertions = assets.join("assertions");
        std::fs::create_dir_all(&assertions).unwrap();
        std::fs::write(assets.join("compat.js"), "").unwrap();
        let path = assertions.join("bruno.js");
        std::fs::write(
            &path,
            r#"function run(ctx) {
                console.log(ctx.environment.note, ctx.request.body.note);
                const passed = ctx.environment.region === "test"
                    && ctx.runtime.run_id === "run-7"
                    && ctx.bindings.tenant === "acme"
                    && ctx.request.body.operation === "create"
                     && ctx.request.body.note === "***"
                     && ctx.response.body.note === "***"
                     && ctx.response.body.api_token === "not-exported"
                     && ctx.environment.api_token === undefined
                    && ctx.runtime.credential === undefined
                    && ctx.bindings.access_token === undefined
                    && ctx.request.body.secret === undefined
                    && !ctx.request.headers.some(h => h.name === "Authorization")
                    && !ctx.response.headers.some(h => h.name === "Set-Cookie");
                return { passed, message: "sanitized Bruno context", actual: ctx };
            }"#,
        )
        .unwrap();
        let mut ir = request();
        ir.body = ResolvedBody::Json(json!({
            "operation": "create",
            "note": "secret-value",
            "secret": "not-exported"
        }));
        ir.headers = vec![
            ResolvedHeader {
                name: "Authorization".into(),
                value: "Bearer secret-value".into(),
            },
            ResolvedHeader {
                name: "X-Mode".into(),
                value: "test".into(),
            },
        ];
        ir.bindings = json!({"tenant": "acme", "access_token": "not-exported"});
        ir.environment =
            json!({"region": "test", "note": "secret-value", "api_token": "not-exported"});
        ir.runtime = json!({"run_id": "run-7"});
        ir.runtime_unmasked = json!({"run_id": "run-7", "credential": "not-exported"});
        let entry = ResolvedPipelineEntry {
            phase: PipelinePhase::AfterResponse,
            asset: super::super::refs::AssetDescriptor {
                raw: "../assets/assertions/bruno/test.js".into(),
                scheme: super::super::refs::RefScheme::File,
                address: path.to_string_lossy().into_owned(),
                pointer: None,
                version: None,
            },
            input: json!({"compatibility": "bruno-tests-v1"}),
        };
        let response = ResponseView {
            status: 200,
            headers: vec![
                ("Set-Cookie".into(), "session=not-exported".into()),
                ("X-Mode".into(), "test".into()),
            ],
            body: br#"{"note":"secret-value","api_token":"not-exported"}"#.to_vec(),
            time_ms: 1,
        };

        let (assertions, _, logs) =
            run_js_after_with_logs(&entry, &ir, Some(&response), None).unwrap();

        assert!(assertions[0].passed, "{:?}", assertions[0].actual);
        assert_eq!(logs, ["*** ***"]);
        let serialized = serde_json::to_string(&(assertions, logs)).unwrap();
        for forbidden in ["secret-value", "not-exported"] {
            assert!(!serialized.contains(forbidden), "leaked {forbidden}");
        }
    }

    #[test]
    fn preview_masks_secrets_in_nested_assertions_and_runtime_writes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("assertion.js");
        std::fs::write(
            &path,
            r#"function run(ctx) {
                const secret = ctx.bindings.token;
                console.log("log", secret);
                return {
                    passed: false,
                    message: "message " + secret,
                    expected: { [secret]: [secret, { nested: secret }] },
                    actual: secret,
                    path: "$." + secret,
                    runtime: { [secret]: { nested: [secret] } }
                };
            }"#,
        )
        .unwrap();
        let entry = ResolvedPipelineEntry {
            phase: PipelinePhase::AfterResponse,
            asset: super::super::refs::AssetDescriptor {
                raw: path.to_string_lossy().into_owned(),
                scheme: super::super::refs::RefScheme::File,
                address: path.to_string_lossy().into_owned(),
                pointer: None,
                version: None,
            },
            input: json!({}),
        };
        let response = ResponseView {
            status: 200,
            headers: Vec::new(),
            body: Vec::new(),
            time_ms: 1,
        };

        let preview = preview_asset(&request(), &entry, Some(&response)).unwrap();

        assert_eq!(preview.assertions[0].message, "message ***");
        assert_eq!(preview.assertions[0].path.as_deref(), Some("$.***"));
        assert_eq!(preview.logs, vec!["log ***"]);
        assert!(preview.runtime_writes.contains_key("***"));
        assert!(
            !format!("{preview:?}").contains("secret-value"),
            "secret leaked from catalog preview"
        );

        let mut error_entry = entry;
        error_entry.asset.raw = "project:secret-value".into();
        let error = preview_asset(&request(), &error_entry, None).unwrap_err();
        assert_eq!(error.asset_ref.as_deref(), Some("project:***"));
    }

    #[test]
    fn public_run_result_masks_assertions_runtime_and_diagnostics() {
        let ir = request();
        let assertions = vec![AssertionResult {
            passed: false,
            message: "message secret-value".into(),
            expected: Some(json!({"secret-value": ["secret-value"]})),
            actual: Some(json!("secret-value")),
            path: Some("$.secret-value".into()),
        }];
        let runtime_unmasked = BTreeMap::from([(
            "secret-value".to_string(),
            json!({"nested": "secret-value"}),
        )]);
        let diagnostics = vec![Diagnostic::new(Code::AssetError, "diagnostic secret-value")
            .at("/secret-value")
            .with_ref("project:secret-value")];

        let result = finish(
            &ir,
            RunStatus::Failed,
            None,
            assertions,
            runtime_unmasked.clone(),
            diagnostics,
            std::time::Instant::now(),
        );

        assert_eq!(
            runtime_unmasked["secret-value"]["nested"],
            json!("secret-value"),
            "sequence carry must remain usable"
        );
        assert!(!format!("{result:?}").contains("secret-value"));
        assert!(!serde_json::to_string(&result)
            .unwrap()
            .contains("secret-value"));
    }
}
