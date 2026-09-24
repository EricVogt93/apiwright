//! reqv1 request editor: the central editor for authoring a
//! `*.request.json` with *chill* access to the asset store. Store palette on
//! the left (data fixtures, hooks, assertions, extractors, generators,
//! mocks) — click "insert" to add a ready `ref`/`use` to the typed document,
//! so you reference a stored dataset/assertion instead of rewriting it. JSON
//! editor on the right with Validate / Save / Run.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use egui::{RichText, TextEdit};
use forge_core::model::Method;
use forge_core::openapi::{ParsedSpec, SpecOperation, SpecResponse};
use forge_core::reqv1::index::AssetEntry;
use forge_core::reqv1::model::{
    BodySpec, BodyType, HeaderSpec, InlineBody, MockDef, PipelineEntry, ValueBinding,
};
use forge_core::reqv1::runner::CatalogPreview;
use forge_core::reqv1::{
    builtin_catalog, find_builtin, AssertionDocument, AssertionEntry, AssetKind, Binding,
    BuiltinDefinition, BuiltinIntent, BuiltinParameter, BuiltinParameterKind, BuiltinTarget,
    HookDocument, ProjectAssetMetadata, ProjectAssetParameter, ProjectAuthConfig, ProjectIndex,
    ResponseView, RunResult, RunStatus,
};

use crate::bridge::{Bridge, Cmd, V1RunItem, V1RunOutput};
use crate::state::AppState;
use crate::theme::icons;
use crate::widgets::code_editor::{
    code_editor_numbered, code_editor_numbered_diagnostic, code_minimap, EditorDiagnostic, Lang,
    CODE_MINIMAP_WIDTH,
};

mod document;
use document::*;
mod view;
#[cfg(test)]
use view::*;
mod openapi_tools;
use openapi_tools::*;
mod advisor_tools;
use advisor_tools::*;
mod catalog;
use catalog::*;
mod results;
use results::*;
mod pipeline_editor;
use pipeline_editor::*;
mod auth_editor;
use auth_editor::*;
mod execution;
use execution::*;

pub use view::show;

#[cfg(test)]
mod tests;

#[derive(Default)]
pub struct V1EditorState {
    pub open: bool,
    /// Inactive request-v1 editor buffers. The active buffer remains in the
    /// fields below so existing editor actions keep working on one document.
    tabs: Vec<V1EditorState>,
    tab_order: Vec<String>,
    tab_id: Option<String>,
    /// File being edited (its parent's project root is derived).
    file: Option<PathBuf>,
    new_file: bool,
    revision: Option<String>,
    save_conflict: Option<String>,
    root: Option<PathBuf>,
    text: String,
    validated_text: String,
    validated_document: Option<forge_core::reqv1::RequestDocument>,
    json_diagnostic: Option<EditorDiagnostic>,
    validation_due: Option<Instant>,
    assertions: AssertionDocument,
    hooks: HookDocument,
    assertion_row_ids: Vec<u64>,
    hook_row_ids: Vec<u64>,
    next_pipeline_row_id: u64,
    assertion_with_drafts: BTreeMap<u64, String>,
    hook_with_drafts: BTreeMap<u64, String>,
    project_auth: Option<ProjectAuthConfig>,
    auth_dirty: bool,
    auth_notice: Option<String>,
    auth_setup: AuthSetup,
    auth_request_choice: String,
    auth_draft: AuthDraft,
    dirty: bool,
    auto_save: bool,
    index: Option<ProjectIndex>,
    openapi: Option<ParsedSpec>,
    openapi_source: Option<PathBuf>,
    openapi_error: Option<String>,
    right_panel_open: bool,
    right_tool: RightTool,
    openapi_query: String,
    openapi_filter: OpenApiFilter,
    openapi_operation: Option<String>,
    marked_operations: BTreeSet<String>,
    auto_covered_operations: BTreeSet<String>,
    advisor_config: crate::advisor::AdvisorConfig,
    advisor_question: String,
    advisor_include_response: bool,
    active_advisor: Option<u64>,
    advisor_answer: Option<String>,
    advisor_error: Option<String>,
    suite_notice: Option<String>,
    suite_error: Option<String>,
    /// JSON tree expansion in the palette (by asset rel_path).
    expanded: HashSet<String>,
    catalog_query: String,
    catalog_intent: Option<String>,
    catalog_view: CatalogView,
    catalog_open: bool,
    catalog_context: CatalogContext,
    selected_builtin: Option<String>,
    selected_project: Option<String>,
    editing_assertion: Option<u64>,
    editing_hook: Option<u64>,
    scroll_to_catalog_form: bool,
    catalog_inputs: BTreeMap<String, ParameterInput>,
    catalog_drafts: BTreeMap<String, BTreeMap<String, ParameterInput>>,
    catalog_value_drafts: BTreeMap<(String, String, ParameterSource), String>,
    untyped_with_draft: String,
    untyped_with_drafts: BTreeMap<String, String>,
    body_draft: Option<String>,
    body_draft_origin: Option<String>,
    body_draft_error: Option<String>,
    body_mode_drafts: BTreeMap<String, BodySpec>,
    catalog_error: Option<String>,
    catalog_notice: Option<String>,
    env_name: Option<String>,
    mock: bool,
    allow_project_code: bool,
    editor_section: EditorSection,
    request_view: RequestView,
    close_prompt_open: bool,
    pending_close_action: Option<PendingEditorAction>,
    undo_stack: Vec<EditorSnapshot>,
    /// Vertical splitter: fraction of height given to the request (top).
    split_ratio: f32,
    /// Which results pane is shown in the bottom split.
    result_tab: ResultTab,
    response_raw: bool,
    // Run plumbing.
    active_run: Option<u64>,
    in_flight: bool,
    diagnostics: Vec<String>,
    results: Vec<V1RunItem>,
    selected_result: usize,
    last_response: Option<ResponseView>,
    last_run_request: Option<String>,
    last_run_mock: Option<bool>,
    last_run_environment: Option<String>,
    last_run_at: Option<Instant>,
    active_preview: Option<u64>,
    preview_in_flight: bool,
    preview: Option<CatalogPreview>,
    preview_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EditorTabInfo {
    pub id: String,
    pub title: String,
    pub dirty: bool,
    pub running: bool,
    pub active: bool,
}

#[derive(Clone)]
enum PendingEditorAction {
    Close,
    SwitchWorkspace,
    Quit,
}

impl PendingEditorAction {
    fn prompt_label(&self) -> &'static str {
        match self {
            Self::Close => "close this request",
            Self::SwitchWorkspace => "switch projects",
            Self::Quit => "quit ApiWright",
        }
    }
}

#[derive(Clone)]
struct EditorSnapshot {
    request: String,
    assertions: AssertionDocument,
    hooks: HookDocument,
    assertion_row_ids: Vec<u64>,
    hook_row_ids: Vec<u64>,
    assertion_with_drafts: BTreeMap<u64, String>,
    hook_with_drafts: BTreeMap<u64, String>,
    body_draft: Option<String>,
    body_draft_origin: Option<String>,
    body_draft_error: Option<String>,
    body_mode_drafts: BTreeMap<String, BodySpec>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum CatalogView {
    #[default]
    All,
    Builtins,
    Project,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum CatalogContext {
    #[default]
    General,
    Assertion,
    Hook,
    Body,
}

impl CatalogContext {
    fn intent(self) -> Option<&'static str> {
        match self {
            Self::General | Self::Body | Self::Hook => None,
            Self::Assertion => Some("Validate"),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::General => "Catalog",
            Self::Assertion => "Add test",
            Self::Hook => "Add preparation",
            Self::Body => "Use data in body",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum EditorSection {
    #[default]
    Request,
    Tests,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum RequestView {
    #[default]
    Form,
    Json,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum RightTool {
    #[default]
    OpenApi,
    ContractTests,
    ApiTests,
    Performance,
    Advisor,
}

impl RightTool {
    fn label(self) -> &'static str {
        match self {
            Self::OpenApi => "OpenAPI",
            Self::ContractTests => "Contract tests",
            Self::ApiTests => "API tests",
            Self::Performance => "Load & performance",
            Self::Advisor => "AI Advisor",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum OpenApiFilter {
    #[default]
    All,
    Method(Method),
    Headers,
    Query,
    Path,
    Body,
}

impl OpenApiFilter {
    fn label(self) -> &'static str {
        match self {
            Self::All => "All operations",
            Self::Method(method) => method.as_str(),
            Self::Headers => "Has headers",
            Self::Query => "Has query parameters",
            Self::Path => "Has path parameters",
            Self::Body => "Has request body",
        }
    }

    fn matches(self, operation: &SpecOperation) -> bool {
        match self {
            Self::All => true,
            Self::Method(method) => operation.method == method,
            Self::Headers => !operation.header_params.is_empty(),
            Self::Query => !operation.query_params.is_empty(),
            Self::Path => !operation.path_params.is_empty(),
            Self::Body => {
                operation.request_content_type.is_some()
                    || operation.request_schema.is_some()
                    || operation.request_example.is_some()
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
enum ParameterSource {
    #[default]
    Literal,
    Binding,
    Environment,
    Runtime,
    Matrix,
    Secret,
}

impl ParameterSource {
    const ALL: [Self; 6] = [
        Self::Literal,
        Self::Binding,
        Self::Environment,
        Self::Runtime,
        Self::Matrix,
        Self::Secret,
    ];
    const TYPED: [Self; 5] = [
        Self::Literal,
        Self::Binding,
        Self::Environment,
        Self::Runtime,
        Self::Matrix,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Literal => "Literal",
            Self::Binding => "Binding",
            Self::Environment => "Environment",
            Self::Runtime => "Runtime",
            Self::Matrix => "Matrix",
            Self::Secret => "Secret",
        }
    }

    fn namespace(self) -> Option<&'static str> {
        match self {
            Self::Literal => None,
            Self::Binding => Some("bindings"),
            Self::Environment => Some("env"),
            Self::Runtime => Some("runtime"),
            Self::Matrix => Some("matrix"),
            Self::Secret => Some("secret"),
        }
    }
}

fn parameter_sources(kind: BuiltinParameterKind) -> &'static [ParameterSource] {
    match kind {
        BuiltinParameterKind::String => &ParameterSource::ALL,
        BuiltinParameterKind::Integer
        | BuiltinParameterKind::Boolean
        | BuiltinParameterKind::Json => &ParameterSource::TYPED,
    }
}

#[derive(Debug, Clone, Default)]
struct ParameterInput {
    source: ParameterSource,
    value: String,
}

#[derive(Debug, Clone, Copy)]
enum InsertTarget {
    Binding,
    Body,
    Assertion,
    Pipeline,
    Mock,
}

#[derive(Debug, Clone)]
struct PendingInsert {
    target: InsertTarget,
    suggested_name: String,
    snippet: String,
}

#[derive(Debug, Clone)]
struct ParameterDefinition {
    name: String,
    label: String,
    kind: BuiltinParameterKind,
    required: bool,
    default: Option<serde_json::Value>,
    options: Vec<String>,
    example: String,
}

impl ParameterDefinition {
    fn builtin(parameter: &BuiltinParameter) -> Self {
        Self {
            name: parameter.name.to_string(),
            label: parameter.label.to_string(),
            kind: parameter.kind,
            required: parameter.required,
            default: parameter
                .default
                .and_then(|default| serde_json::from_str(default).ok()),
            options: parameter
                .options
                .iter()
                .map(|option| (*option).to_string())
                .collect(),
            example: parameter.example.to_string(),
        }
    }

    fn project(parameter: &ProjectAssetParameter) -> Self {
        Self {
            name: parameter.name.clone(),
            label: parameter.label.clone(),
            kind: parameter.kind,
            required: parameter.required,
            default: parameter.default.clone(),
            options: parameter.options.clone(),
            example: parameter.example.clone(),
        }
    }
}

fn sourced_value(source: ParameterSource, path: &str) -> Option<serde_json::Value> {
    source
        .namespace()
        .filter(|_| !path.trim().is_empty())
        .map(|namespace| serde_json::Value::String(format!("${{{namespace}.{}}}", path.trim())))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum ResultTab {
    #[default]
    Result,
    Assertions,
    Auth,
    Runtime,
    Diagnostics,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum AuthSetup {
    #[default]
    ExistingRequest,
    Provider,
}

impl AuthSetup {
    fn label(self) -> &'static str {
        match self {
            Self::ExistingRequest => "Existing request",
            Self::Provider => "Provider setup",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum AuthProvider {
    #[default]
    Generic,
    Keycloak,
    Auth0,
    Entra,
}

impl AuthProvider {
    const ALL: [Self; 4] = [Self::Generic, Self::Keycloak, Self::Auth0, Self::Entra];

    fn label(self) -> &'static str {
        match self {
            Self::Generic => "OAuth 2.0",
            Self::Keycloak => "Keycloak",
            Self::Auth0 => "Auth0",
            Self::Entra => "Microsoft Entra",
        }
    }

    fn endpoint_label(self) -> &'static str {
        match self {
            Self::Generic => "Token URL",
            Self::Keycloak => "Server URL",
            Self::Auth0 => "Domain",
            Self::Entra => "Tenant ID",
        }
    }

    fn scope_label(self) -> &'static str {
        if self == Self::Auth0 {
            "Audience"
        } else {
            "Scope"
        }
    }

    fn secret_name(self) -> &'static str {
        match self {
            Self::Generic => "OAUTH_CLIENT_SECRET",
            Self::Keycloak => "KEYCLOAK_CLIENT_SECRET",
            Self::Auth0 => "AUTH0_CLIENT_SECRET",
            Self::Entra => "ENTRA_CLIENT_SECRET",
        }
    }

    fn file_stem(self) -> &'static str {
        match self {
            Self::Generic => "oauth-token",
            Self::Keycloak => "keycloak-token",
            Self::Auth0 => "auth0-token",
            Self::Entra => "entra-token",
        }
    }
}

#[derive(Debug, Default)]
struct AuthDraft {
    provider: AuthProvider,
    endpoint: String,
    realm: String,
    client_id: String,
    client_secret: String,
    scope: String,
}

const SKELETON: &str = r#"{
  "formatVersion": 1,
  "kind": "request",
  "meta": { "id": "new.request", "name": "New request" },
  "bindings": {
  },
  "request": {
    "method": "GET",
    "url": "https://example.com",
    "headers": []
  }
}
"#;

const EDITOR_COLUMN_GAP: f32 = 22.0;
const TOOLBAR_MENU_CELL_WIDTH: f32 = 40.0;
const TOOLBAR_TRAILING_GUTTER: f32 = 8.0;

fn document_has_matrix(text: &str) -> bool {
    forge_core::reqv1::RequestDocument::parse(text)
        .is_ok_and(|document| !document.matrix.is_empty())
}

fn project_root_of(file: &std::path::Path) -> PathBuf {
    let mut dir = file.parent().map(std::path::Path::to_path_buf);
    while let Some(d) = dir {
        if d.join("project.json").exists() {
            return d;
        }
        dir = d.parent().map(std::path::Path::to_path_buf);
    }
    file.parent()
        .unwrap_or(std::path::Path::new("."))
        .to_path_buf()
}

fn escape_ptr(s: &str) -> String {
    s.replace('~', "~0").replace('/', "~1")
}

fn short(v: &serde_json::Value) -> String {
    let s = match v {
        serde_json::Value::String(s) => format!("\"{s}\""),
        other => other.to_string(),
    };
    if s.len() > 30 {
        format!("{}…", &s[..29])
    } else {
        s
    }
}
