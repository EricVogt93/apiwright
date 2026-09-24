//! Bruno OpenCollection YAML import directly into request format v1.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

use regex::Regex;
use serde::Serialize;
use serde_json::{Map, Value};
use serde_yaml_ng::Value as Yaml;
use sha2::{Digest, Sha256};

use crate::model::{AuthConfig, BodyDef, Method};
use crate::reqv1::assertions::{AssertionDocument, AssertionEntry};
use crate::reqv1::hooks::HookDocument;
use crate::reqv1::model::{
    AllCondition, AnyCondition, BinaryBody, BinaryBodyType, Binding, BodySpec, BodyType,
    ExecutionCondition, ExecutionPolicy, ExecutionVariable, ExecutionVariableScope, FormatVersion,
    HeaderSpec, InlineBody, LiteralCondition, MultipartBody, MultipartBodyType,
    MultipartPart as V1MultipartPart, NotCondition, PipelineEntry, PipelinePhase,
    ProjectAuthConfig, ProjectConfig, RequestAuthSelection, RequestDocument, RequestKind,
    RequestMeta, RequestSpec, RequestTransportSettings, SkipGuard, ValueBinding, VariableCondition,
};
use crate::reqv1::{SequenceDocument, SequenceKind};

use super::common::encode_lower_hex;
use super::quarantine::{
    sync_import_quarantine, ImportQuarantineEntry, ImportQuarantineManifest, ImportSourceFormat,
    QuarantineCategory, QuarantineDisposition, IMPORT_QUARANTINE_PATH,
};
use super::{import_bruno, ImportedItem};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum BrunoSourceFormat {
    OpenCollectionYaml,
    ClassicBru,
}

#[derive(Debug, Clone, Copy)]
pub struct BrunoV1ImportOptions {
    /// Build and report the complete plan without touching the destination.
    pub inspect: bool,
    /// Exclude directories whose names begin with `_`, such as `_probes`.
    pub exclude_underscore_dirs: bool,
}

impl Default for BrunoV1ImportOptions {
    fn default() -> Self {
        Self {
            inspect: false,
            exclude_underscore_dirs: true,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportedEnvironmentSummary {
    pub name: String,
    pub variable_count: usize,
    pub secret_count: usize,
}

/// Stable machine-readable result returned by inspect and real imports.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BrunoV1ImportReport {
    pub detected_format: BrunoSourceFormat,
    pub scanned_request_count: usize,
    pub imported_request_count: usize,
    pub excluded_request_count: usize,
    pub excluded_by_policy: BTreeMap<String, usize>,
    pub environments: Vec<ImportedEnvironmentSummary>,
    pub native_skip_request_count: usize,
    pub native_delay_request_count: usize,
    pub native_assertion_script_count: usize,
    pub custom_assertion_script_count: usize,
    pub blocked_assertion_script_count: usize,
    pub direct_native_contract_assertion_count: usize,
    pub transformed_native_contract_assertion_count: usize,
    pub blocked_contract_assertion_count: usize,
    pub custom_before_request_script_count: usize,
    pub blocked_before_request_script_count: usize,
    pub custom_after_response_script_count: usize,
    pub blocked_after_response_script_count: usize,
    pub generated_auth_provider_count: usize,
    pub generated_helper_request_count: usize,
    pub recognized_auth_script_count: usize,
    pub recognized_data_manager_auth_count: usize,
    pub remaining_blocked_auth_script_count: usize,
    pub quarantined_script_count: usize,
    /// Generated compatibility assets are project-owned code and require
    /// explicit runner trust before execution.
    pub requires_project_code: bool,
    /// Source path -> feature -> messages. No environment values are included.
    pub diagnostics: BTreeMap<String, BTreeMap<String, Vec<String>>>,
    pub output_file_count: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum BrunoV1ImportError {
    #[error("not a Bruno collection: {0} has neither opencollection.yml nor bruno.json")]
    NotACollection(String),
    #[error("failed to read {path}: {message}")]
    Io { path: String, message: String },
    #[error("invalid YAML in {path}: {message}")]
    Yaml { path: String, message: String },
    #[error("cannot materialize import output: {0}")]
    Materialize(String),
}

#[derive(Debug, Clone)]
struct GeneratedFile {
    relative: PathBuf,
    bytes: Vec<u8>,
}

#[derive(Debug, Clone)]
struct RequestSource {
    source_relative: PathBuf,
    output_relative: PathBuf,
    leaf_relative: PathBuf,
    leaf_name: String,
    leaf_description: Option<String>,
    leaf_tags: Vec<String>,
    yaml: Yaml,
    inherited_auth: Option<Yaml>,
    inherited_vars: BTreeMap<String, Value>,
    inherited_secrets: BTreeSet<String>,
    inherited_skip: Vec<SourceCondition>,
    inherited_delay_ms: u64,
    inherited_scripts: Vec<SourceScript>,
}

#[derive(Debug, Clone)]
struct SourceScript {
    source_relative: PathBuf,
    source_path: String,
    source_index: usize,
    kind: String,
    code: String,
    inherited: bool,
}

#[derive(Debug, Clone, Default)]
struct WalkContext {
    auth: Option<Yaml>,
    vars: BTreeMap<String, Value>,
    secrets: BTreeSet<String>,
    folder_name: String,
    folder_description: Option<String>,
    folder_tags: Vec<String>,
    skip: Vec<SourceCondition>,
    delay_ms: u64,
    scripts: Vec<SourceScript>,
}

#[derive(Debug, Clone)]
struct EnvironmentSource {
    name: String,
    file_name: String,
    values: BTreeMap<String, Value>,
    plain_names: BTreeSet<String>,
    secret_names: BTreeSet<String>,
}

#[derive(Debug, Clone)]
enum SourceCondition {
    Literal(bool),
    Variable(SourceVariable),
    Not(Box<SourceCondition>),
    All(Vec<SourceCondition>),
    Any(Vec<SourceCondition>),
}

#[derive(Debug, Clone)]
struct SourceVariable {
    kind: SourceVariableKind,
    name: String,
}

#[derive(Debug, Clone, Copy)]
enum SourceVariableKind {
    Environment,
    Runtime,
}

#[derive(Debug, Default)]
struct PlanState {
    requests: Vec<RequestSource>,
    scanned: usize,
    excluded: BTreeMap<String, usize>,
    diagnostics: BTreeMap<String, BTreeMap<String, Vec<String>>>,
    native_skips: usize,
    native_delays: usize,
    native_assertion_scripts: usize,
    custom_assertion_scripts: usize,
    blocked_assertion_scripts: usize,
    direct_native_contract_assertions: usize,
    transformed_native_contract_assertions: usize,
    blocked_contract_assertions: usize,
    custom_before_scripts: BTreeSet<PathBuf>,
    blocked_before_scripts: BTreeSet<PathBuf>,
    custom_after_scripts: BTreeSet<PathBuf>,
    blocked_after_scripts: BTreeSet<PathBuf>,
    recognized_auth_scripts: BTreeSet<PathBuf>,
    blocked_auth_scripts: BTreeSet<PathBuf>,
    quarantine: BTreeMap<String, ImportQuarantineEntry>,
}

#[derive(Default)]
struct AuthMigration {
    providers: BTreeMap<String, ProjectAuthConfig>,
    files: Vec<GeneratedFile>,
    helpers_before: BTreeMap<PathBuf, Vec<AuthHelper>>,
    recognized_data_manager_auth_count: usize,
}

#[derive(Clone)]
struct AuthHelper {
    request_path: String,
    runtime_target: String,
}

const BRUNO_KEYCLOAK_PROVIDER: &str = "bruno-keycloak";
const BRUNO_RESTRICTED_KEYCLOAK_PROVIDER: &str = "bruno-keycloak-restricted";
const BRUNO_CRID_PROVIDER: &str = "bruno-crid-azure";
const BRUNO_IPD5_PROVIDER: &str = "bruno-ipd5";
const IPD452_SOURCE: &str = "services/internal/DataManager/IPD-452_Contracts2_OAuth2/04c_ipd_service_token_und_deployment_pruefen.yml";

fn plan_auth_migration(requests: &[RequestSource]) -> Result<AuthMigration, BrunoV1ImportError> {
    let mut standard_keycloak = false;
    let mut restricted_keycloak = false;
    let mut crid_azure = false;
    let mut ipd5 = false;
    let mut restricted_scope = None;

    for source in requests {
        let data_manager_auth = request_scripts(source).iter().any(|script| {
            script.kind == "before-request"
                && recognized_data_manager_admin_discovery_script(source, &script.code)
        });
        standard_keycloak |= data_manager_auth;
        match keycloak_request_kind(&source.yaml) {
            Some(BRUNO_KEYCLOAK_PROVIDER) => standard_keycloak = true,
            Some(BRUNO_RESTRICTED_KEYCLOAK_PROVIDER) => restricted_keycloak = true,
            _ => {}
        }
        ipd5 |= is_ipd5_token_request(source);
        let own_scripts = request_scripts(source);
        for script in source.inherited_scripts.iter().chain(own_scripts.iter()) {
            standard_keycloak |= recognized_keycloak_script(&script.code);
            crid_azure |= recognized_crid_script(&script.code);
        }
        if restricted_scope.is_none()
            && source_bearer_variable(effective_source_auth(source)).is_some_and(|name| {
                matches!(
                    name.as_str(),
                    "restricted_access_token"
                        | "leg1012_restricted_access_token"
                        | "crm4044_restricted_access_token"
                )
            })
        {
            restricted_scope = Some(display_path(
                &PathBuf::from("requests").join(&source.output_relative),
            ));
        }
    }

    let mut migration = AuthMigration::default();
    if standard_keycloak {
        add_generated_provider(
            &mut migration,
            BRUNO_KEYCLOAK_PROVIDER,
            "keycloak-client-credentials",
            "${env.keycloak_url}",
            BodyType::Form,
            Value::Object(Map::from_iter([
                (
                    "grant_type".to_string(),
                    Value::String("client_credentials".to_string()),
                ),
                (
                    "client_id".to_string(),
                    Value::String("team-bp-tester".to_string()),
                ),
                (
                    "client_secret".to_string(),
                    Value::String("${secret.keycloak_client_secret}".to_string()),
                ),
            ])),
            300,
            "requests".to_string(),
        )?;
    }
    if restricted_keycloak {
        add_generated_provider(
            &mut migration,
            BRUNO_RESTRICTED_KEYCLOAK_PROVIDER,
            "keycloak-restricted-client-credentials",
            "${env.keycloak_url}",
            BodyType::Form,
            Value::Object(Map::from_iter([
                (
                    "grant_type".to_string(),
                    Value::String("client_credentials".to_string()),
                ),
                (
                    "client_id".to_string(),
                    Value::String("${secret.kc_client_id_no_bpcr}".to_string()),
                ),
                (
                    "client_secret".to_string(),
                    Value::String("${secret.kc_client_secret_no_bpcr}".to_string()),
                ),
            ])),
            300,
            restricted_scope.unwrap_or_else(|| "requests".to_string()),
        )?;
    }
    if crid_azure {
        add_generated_provider(
            &mut migration,
            BRUNO_CRID_PROVIDER,
            "crid-azure-client-credentials",
            "https://login.microsoftonline.com/${env.azure_tenant_id}/oauth2/v2.0/token",
            BodyType::Form,
            Value::Object(Map::from_iter([
                (
                    "grant_type".to_string(),
                    Value::String("client_credentials".to_string()),
                ),
                (
                    "client_id".to_string(),
                    Value::String("${secret.azure_client_id}".to_string()),
                ),
                (
                    "client_secret".to_string(),
                    Value::String("${secret.azure_client_secret}".to_string()),
                ),
                (
                    "scope".to_string(),
                    Value::String("${env.azure_scope}".to_string()),
                ),
            ])),
            3600,
            "requests/services/external/CRID".to_string(),
        )?;
    }
    if ipd5 {
        add_generated_provider(
            &mut migration,
            BRUNO_IPD5_PROVIDER,
            "ipd5-token",
            "${env.urlIPD5}/ApiToken/GenerateToken",
            BodyType::Json,
            Value::Object(Map::from_iter([
                (
                    "Username".to_string(),
                    Value::String("${env.userIPD5}".to_string()),
                ),
                (
                    "Password".to_string(),
                    Value::String("${secret.passwordIPD5}".to_string()),
                ),
            ])),
            900,
            "requests/services/external/IPD5".to_string(),
        )?;
    }
    for source in requests {
        if request_scripts(source).iter().any(|script| {
            script.kind == "before-request"
                && recognized_data_manager_admin_discovery_script(source, &script.code)
        }) {
            add_data_manager_auth_helpers(&mut migration, source)?;
        }
    }
    Ok(migration)
}

fn add_data_manager_auth_helpers(
    migration: &mut AuthMigration,
    source: &RequestSource,
) -> Result<(), BrunoV1ImportError> {
    let helper_dir = PathBuf::from("requests")
        .join(source.output_relative.parent().unwrap_or(Path::new("")))
        .join("_helpers");
    let specs = [
        (
            "04c-01-keycloak-client",
            "Discover IPD-452 Keycloak client",
            Method::Get,
            "${env.keycloak_admin_url}/clients",
            vec![HeaderSpec {
                name: "clientId".to_string(),
                value: "ipd-upload-data-manager".to_string(),
                enabled: true,
            }],
            None,
            BRUNO_KEYCLOAK_PROVIDER,
            "$[0].id",
            "ipd452_client_id",
            false,
        ),
        (
            "04c-02-keycloak-client-secret",
            "Discover IPD-452 Keycloak client secret",
            Method::Get,
            "${env.keycloak_admin_url}/clients/${runtime.ipd452_client_id}/client-secret",
            Vec::new(),
            None,
            BRUNO_KEYCLOAK_PROVIDER,
            "$.value",
            "ipd452_client_secret",
            true,
        ),
        (
            "04c-03-service-token",
            "Acquire IPD-452 service token",
            Method::Post,
            "${env.keycloak_http_token_url}",
            Vec::new(),
            Some(BodySpec::Inline(InlineBody {
                body_type: BodyType::Form,
                value: Some(Value::Object(Map::from_iter([
                    (
                        "grant_type".to_string(),
                        Value::String("client_credentials".to_string()),
                    ),
                    (
                        "client_id".to_string(),
                        Value::String("ipd-upload-data-manager".to_string()),
                    ),
                    (
                        "client_secret".to_string(),
                        Value::String("${runtime.ipd452_client_secret}".to_string()),
                    ),
                ]))),
            })),
            "none",
            "$.access_token",
            "ipd452_service_token",
            true,
        ),
    ];
    let mut helpers = Vec::new();
    for (stem, name, method, url, query, body, auth, json_path, target, sensitive) in specs {
        let relative = helper_dir.join(format!("{stem}.request.json"));
        let request_path = display_path(&relative);
        let document = RequestDocument {
            schema: None,
            format_version: FormatVersion,
            kind: RequestKind::Request,
            meta: RequestMeta {
                id: format!("auth.bruno.ipd452.{stem}"),
                name: name.to_string(),
                description: Some(
                    "Generated from the recognized DataManager IPD-452 admin discovery flow."
                        .to_string(),
                ),
                tags: vec![
                    "hidden".to_string(),
                    "helper".to_string(),
                    "auth".to_string(),
                    "bruno-import".to_string(),
                ],
            },
            bindings: BTreeMap::new(),
            matrix: BTreeMap::new(),
            execution: None,
            auth: Some(if auth == "none" {
                RequestAuthSelection::none()
            } else {
                RequestAuthSelection::provider(auth)
            }),
            request: RequestSpec {
                method,
                url: url.to_string(),
                headers: if body.is_some() {
                    vec![HeaderSpec {
                        name: "Content-Type".to_string(),
                        value: "application/x-www-form-urlencoded".to_string(),
                        enabled: true,
                    }]
                } else {
                    Vec::new()
                },
                query,
                body,
                settings: RequestTransportSettings::default(),
            },
            pipeline: vec![PipelineEntry {
                phase: PipelinePhase::AfterResponse,
                uses: "builtin:extract-json-path@1".to_string(),
                with: Map::from_iter([
                    ("path".to_string(), Value::String(json_path.to_string())),
                    ("target".to_string(), Value::String(target.to_string())),
                    ("sensitive".to_string(), Value::Bool(sensitive)),
                ]),
                enabled: true,
            }],
            mock: None,
        };
        migration.files.push(json_file(relative, &document)?);
        helpers.push(AuthHelper {
            request_path,
            runtime_target: target.to_string(),
        });
    }
    migration
        .helpers_before
        .insert(source.source_relative.clone(), helpers);
    migration.recognized_data_manager_auth_count += 1;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn add_generated_provider(
    migration: &mut AuthMigration,
    provider_name: &str,
    file_stem: &str,
    url: &str,
    body_type: BodyType,
    body_value: Value,
    lifetime_seconds: u64,
    apply_to: String,
) -> Result<(), BrunoV1ImportError> {
    let relative = PathBuf::from("requests/auth/bruno").join(format!("{file_stem}.request.json"));
    let request_path = display_path(&relative);
    let document = RequestDocument {
        schema: None,
        format_version: FormatVersion,
        kind: RequestKind::Request,
        meta: RequestMeta {
            id: format!("auth.bruno.{file_stem}"),
            name: format!("Bruno auth: {provider_name}"),
            description: Some(
                "Generated from a recognized OpenCollection auth pattern.".to_string(),
            ),
            tags: vec!["auth".to_string(), "bruno-import".to_string()],
        },
        bindings: BTreeMap::new(),
        matrix: BTreeMap::new(),
        execution: None,
        auth: Some(RequestAuthSelection::none()),
        request: RequestSpec {
            method: Method::Post,
            url: url.to_string(),
            headers: vec![HeaderSpec {
                name: "Content-Type".to_string(),
                value: match body_type {
                    BodyType::Form => "application/x-www-form-urlencoded",
                    BodyType::Json => "application/json",
                    _ => "text/plain",
                }
                .to_string(),
                enabled: true,
            }],
            query: Vec::new(),
            body: Some(BodySpec::Inline(InlineBody {
                body_type,
                value: Some(body_value),
            })),
            settings: RequestTransportSettings::default(),
        },
        pipeline: Vec::new(),
        mock: None,
    };
    migration.files.push(json_file(relative, &document)?);
    migration.providers.insert(
        provider_name.to_string(),
        ProjectAuthConfig {
            request: request_path,
            token_path: "$.access_token".to_string(),
            lifetime_seconds,
            refresh_before_seconds: 30,
            apply_to,
        },
    );
    Ok(())
}

fn request_scripts(source: &RequestSource) -> Vec<SourceScript> {
    value_at(&source.yaml, &["runtime", "scripts"])
        .and_then(Yaml::as_sequence)
        .into_iter()
        .flatten()
        .enumerate()
        .filter(|(_, script)| is_enabled(script))
        .map(|(index, script)| SourceScript {
            source_relative: source.source_relative.clone(),
            source_path: display_path(&source.source_relative),
            source_index: index + 1,
            kind: text_at(script, &["type"]).unwrap_or_else(|| "unknown".to_string()),
            code: text_at(script, &["code"]).unwrap_or_default(),
            inherited: false,
        })
        .collect()
}

fn effective_source_auth(source: &RequestSource) -> Option<&Yaml> {
    value_at(&source.yaml, &["http", "auth"])
        .or_else(|| value_at(&source.yaml, &["auth"]))
        .and_then(|auth| {
            if auth.as_str() == Some("inherit") {
                source.inherited_auth.as_ref()
            } else {
                Some(auth)
            }
        })
        .or(source.inherited_auth.as_ref())
}

fn source_auth_kind(auth: Option<&Yaml>) -> String {
    auth.and_then(|auth| {
        auth.as_str()
            .map(str::to_string)
            .or_else(|| text_at(auth, &["type"]))
    })
    .unwrap_or_else(|| "inherit".to_string())
}

fn source_bearer_variable(auth: Option<&Yaml>) -> Option<String> {
    let auth = auth?;
    if source_auth_kind(Some(auth)) != "bearer" {
        return None;
    }
    Some(
        text_at(auth, &["token"])?
            .trim()
            .strip_prefix("{{")?
            .strip_suffix("}}")?
            .trim()
            .to_string(),
    )
}

fn imported_auth_selection(
    source: &RequestSource,
    auth: Option<&Yaml>,
    migration: &AuthMigration,
) -> Option<RequestAuthSelection> {
    if source_auth_kind(auth) == "none" {
        return Some(RequestAuthSelection::none());
    }
    if request_scripts(source)
        .iter()
        .any(|script| recognized_auth_network_script(&script.code))
    {
        return Some(RequestAuthSelection::none());
    }
    let variable = source_bearer_variable(auth)?;
    let provider = match variable.as_str() {
        "access_token" => BRUNO_KEYCLOAK_PROVIDER,
        "crid_access_token" => BRUNO_CRID_PROVIDER,
        "token" if display_path(&source.source_relative).starts_with("services/external/IPD5/") => {
            BRUNO_IPD5_PROVIDER
        }
        "restricted_access_token"
        | "leg1012_restricted_access_token"
        | "crm4044_restricted_access_token" => BRUNO_RESTRICTED_KEYCLOAK_PROVIDER,
        _ => return None,
    };
    migration
        .providers
        .contains_key(provider)
        .then(|| RequestAuthSelection::provider(provider))
}

fn keycloak_request_kind(yaml: &Yaml) -> Option<&'static str> {
    if text_at(yaml, &["http", "url"]).as_deref() != Some("{{keycloak_url}}")
        || form_value(yaml, "grant_type").as_deref() != Some("client_credentials")
    {
        return None;
    }
    let client_id = form_value(yaml, "client_id")?;
    let client_secret = form_value(yaml, "client_secret")?;
    if client_id == "team-bp-tester" && client_secret == "{{keycloak_client_secret}}" {
        Some(BRUNO_KEYCLOAK_PROVIDER)
    } else if client_id == "{{kc_client_id_no_bpcr}}"
        && client_secret == "{{kc_client_secret_no_bpcr}}"
    {
        Some(BRUNO_RESTRICTED_KEYCLOAK_PROVIDER)
    } else {
        None
    }
}

fn form_value(yaml: &Yaml, name: &str) -> Option<String> {
    rows_at(yaml, &["http", "body", "data"])
        .into_iter()
        .find(|row| text_at(row, &["name"]).as_deref() == Some(name))
        .map(|row| scalar_text(value_at(row, &["value"])))
}

fn is_ipd5_token_request(source: &RequestSource) -> bool {
    display_path(&source.source_relative) == "services/external/IPD5/ApiToken/GenerateToken.yml"
        && text_at(&source.yaml, &["http", "url"]).as_deref()
            == Some("{{urlIPD5}}/ApiToken/GenerateToken")
}

fn recognized_keycloak_script(code: &str) -> bool {
    code.contains("bru.sendRequest")
        && code.contains("keycloak_url")
        && code.contains("keycloak_client_secret")
        && code.contains("team-bp-tester")
        && code.contains("access_token")
        && code.contains("client_credentials")
}

fn recognized_crid_script(code: &str) -> bool {
    code.contains("axios.post")
        && code.contains("login.microsoftonline.com")
        && code.contains("azure_tenant_id")
        && code.contains("azure_client_id")
        && code.contains("azure_client_secret")
        && code.contains("azure_scope")
        && code.contains("crid_access_token")
        && code.contains("client_credentials")
}

fn recognized_auth_network_script(code: &str) -> bool {
    recognized_keycloak_script(code) || recognized_crid_script(code)
}

fn recognized_data_manager_admin_discovery_script(source: &RequestSource, code: &str) -> bool {
    display_path(&source.source_relative) == IPD452_SOURCE
        && source_auth_kind(effective_source_auth(source)) == "none"
        && text_at(&source.yaml, &["http", "method"]).as_deref() == Some("get")
        && text_at(&source.yaml, &["http", "url"]).as_deref()
            == Some("{{data_manager_url}}/actuator/info")
        && [
            "const axios = require('axios')",
            "bru.getVar('access_token') || bru.getEnvVar('access_token')",
            "bru.getEnvVar('keycloak_url')",
            "replace(/^https:/, 'http:')",
            "replace('/realms/gvl/protocol/openid-connect/token', '/admin/realms/gvl')",
            "axios.get(adminBase + '/clients?clientId=ipd-upload-data-manager'",
            "expect(clients.data).to.be.an('array').that.is.not.empty",
            "clients.data[0].id",
            "axios.get(adminBase + '/clients/' + clientUuid + '/client-secret'",
            "'grant_type=client_credentials'",
            "'client_id=ipd-upload-data-manager'",
            "encodeURIComponent(secretResponse.data.value)",
            "].join('&')",
            "axios.post(scriptTokenUrl, formBody",
            "'Content-Type': 'application/x-www-form-urlencoded'",
            "bru.setVar('ipd452_service_token', tokenResponse.data.access_token)",
        ]
        .iter()
        .all(|part| code.contains(part))
}

fn is_auth_network_script(code: &str) -> bool {
    (code.contains("axios") || code.contains("bru.sendRequest"))
        && (code.contains("access_token") || code.contains("client_secret"))
}

#[derive(Debug)]
struct ImportPlan {
    report: BrunoV1ImportReport,
    files: Vec<GeneratedFile>,
    import_key: String,
}

/// Detect a Bruno source. OpenCollection wins when both markers exist.
pub fn detect_bruno_source(root: &Path) -> Result<BrunoSourceFormat, BrunoV1ImportError> {
    if root.join("opencollection.yml").is_file() {
        Ok(BrunoSourceFormat::OpenCollectionYaml)
    } else if root.join("bruno.json").is_file() {
        Ok(BrunoSourceFormat::ClassicBru)
    } else {
        Err(BrunoV1ImportError::NotACollection(
            root.display().to_string(),
        ))
    }
}

/// Inspect or materialize a Bruno collection as direct request-format-v1 files.
pub fn import_bruno_v1(
    source: &Path,
    destination: &Path,
    options: BrunoV1ImportOptions,
) -> Result<BrunoV1ImportReport, BrunoV1ImportError> {
    validate_source_destination(source, destination)?;
    let format = detect_bruno_source(source)?;
    let mut plan = match format {
        BrunoSourceFormat::OpenCollectionYaml => plan_open_collection(source, options)?,
        BrunoSourceFormat::ClassicBru => plan_classic(source)?,
    };
    let canonical_source =
        std::fs::canonicalize(source).map_err(|error| io_error(source, error))?;
    let source_digest = Sha256::digest(canonical_source.to_string_lossy().as_bytes());
    plan.import_key = format!("bruno-source:{}", encode_lower_hex(&source_digest));
    validate_generated_files(destination, &plan.files)?;
    if !options.inspect {
        for file in &plan.files {
            if file.relative == Path::new(IMPORT_QUARANTINE_PATH) {
                continue;
            }
            let target = destination.join(&file.relative);
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent).map_err(|error| io_error(parent, error))?;
            }
            std::fs::write(&target, &file.bytes).map_err(|error| io_error(&target, error))?;
        }
        let entries = plan
            .files
            .iter()
            .find(|file| file.relative == Path::new(IMPORT_QUARANTINE_PATH))
            .map(|file| serde_json::from_slice::<ImportQuarantineManifest>(&file.bytes))
            .transpose()
            .map_err(|error| BrunoV1ImportError::Materialize(error.to_string()))?
            .map(|manifest| manifest.entries)
            .unwrap_or_default();
        let canonical_destination = canonical_with_missing(destination)?;
        let quarantine_target = canonical_with_missing(&destination.join(IMPORT_QUARANTINE_PATH))?;
        if !quarantine_target.starts_with(&canonical_destination) {
            return Err(BrunoV1ImportError::Materialize(
                "import quarantine path escapes the destination".to_string(),
            ));
        }
        sync_import_quarantine(destination, &plan.import_key, entries)
            .map_err(|error| BrunoV1ImportError::Materialize(error.to_string()))?;
    }
    Ok(plan.report)
}

fn plan_open_collection(
    root: &Path,
    options: BrunoV1ImportOptions,
) -> Result<ImportPlan, BrunoV1ImportError> {
    let marker = root.join("opencollection.yml");
    let root_yaml = read_yaml(&marker)?;
    let collection_name = text_at(&root_yaml, &["info", "name"])
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| file_stem(root));
    let ignored = string_list_at(&root_yaml, &["extensions", "bruno", "ignore"]);
    let mut state = PlanState::default();
    diagnose_unknown(
        &root_yaml,
        &[
            "opencollection",
            "info",
            "request",
            "runtime",
            "vars",
            "docs",
            "tags",
            "bundled",
            "extensions",
        ],
        "opencollection.yml",
        "root",
        &mut state.diagnostics,
    );

    let mut root_context = WalkContext {
        auth: value_at(&root_yaml, &["request", "auth"]).cloned(),
        folder_name: collection_name.clone(),
        folder_description: docs_at(&root_yaml),
        folder_tags: tags_at(&root_yaml),
        ..WalkContext::default()
    };
    add_vars(
        &root_yaml,
        "opencollection.yml",
        &mut root_context.vars,
        &mut root_context.secrets,
        &mut state.diagnostics,
    );
    lower_inherited_scripts(
        &root_yaml,
        "opencollection.yml",
        Path::new("opencollection.yml"),
        &mut root_context.skip,
        &mut root_context.delay_ms,
        &mut root_context.scripts,
    );
    walk_collection_dir(root, root, &root_context, &ignored, options, &mut state)?;

    let mut environments = parse_environments(root, &mut state.diagnostics)?;
    let declared_secrets = environments
        .iter()
        .flat_map(|environment| environment.secret_names.iter().cloned())
        .collect::<BTreeSet<_>>();
    for environment in &mut environments {
        for name in &declared_secrets {
            if environment.plain_names.remove(name) {
                environment.values.remove(name);
                environment.secret_names.insert(name.clone());
                push_diagnostic(
                    &mut state.diagnostics,
                    &format!("environments/{}.yml", environment.file_name),
                    "secrets",
                    &format!(
                        "variable '{name}' is secret in another environment; its plain value was not exported"
                    ),
                );
            }
        }
    }
    let env_names = environments
        .iter()
        .flat_map(|environment| environment.plain_names.iter().cloned())
        .collect::<BTreeSet<_>>();
    let secret_names = environments
        .iter()
        .flat_map(|environment| environment.secret_names.iter().cloned())
        .collect::<BTreeSet<_>>();
    let auth_migration = plan_auth_migration(&state.requests)?;

    let mut files = Vec::new();
    files.push(json_file(
        PathBuf::from("project.json"),
        &ProjectConfig {
            format_version: Some(1),
            aliases: BTreeMap::new(),
            secrets: vec!["env".to_string()],
            auth: None,
            auth_providers: auth_migration.providers.clone(),
        },
    )?);
    files.extend(auth_migration.files.clone());

    for environment in &environments {
        files.push(json_file(
            PathBuf::from("environments").join(format!("{}.json", environment.file_name)),
            &environment.values,
        )?);
    }

    let mut leaf_order = Vec::new();
    let mut leaf_requests = BTreeMap::<PathBuf, Vec<usize>>::new();
    for (index, request) in state.requests.iter().enumerate() {
        if !leaf_requests.contains_key(&request.leaf_relative) {
            leaf_order.push(request.leaf_relative.clone());
        }
        leaf_requests
            .entry(request.leaf_relative.clone())
            .or_default()
            .push(index);
    }

    let mut service_order = Vec::new();
    let mut service_requests = BTreeMap::<PathBuf, Vec<String>>::new();
    for leaf in leaf_order {
        let indices = &leaf_requests[&leaf];
        let first = &state.requests[indices[0]];
        let mut sequence_paths = Vec::new();
        let mut known_runtime = BTreeSet::new();
        for index in indices {
            let source = &state.requests[*index];
            if let Some(helpers) = auth_migration.helpers_before.get(&source.source_relative) {
                for helper in helpers {
                    sequence_paths.push(helper.request_path.clone());
                    known_runtime.insert(helper.runtime_target.clone());
                }
            }
            let path = display_path(&source.source_relative);
            let lowered = lower_request(
                root,
                source,
                &env_names,
                &secret_names,
                &known_runtime,
                &auth_migration,
                &mut state.diagnostics,
            );
            if lowered
                .request
                .execution
                .as_ref()
                .is_some_and(|policy| policy.skip.is_some())
            {
                state.native_skips += 1;
            }
            if lowered
                .request
                .execution
                .as_ref()
                .is_some_and(|policy| policy.delay_before_ms.is_some())
            {
                state.native_delays += 1;
            }
            state.native_assertion_scripts += lowered.native_assertion_scripts;
            state.custom_assertion_scripts += lowered.custom_assertion_scripts;
            state.blocked_assertion_scripts += lowered.blocked_assertion_scripts;
            state.direct_native_contract_assertions += lowered.direct_native_contract_assertions;
            state.transformed_native_contract_assertions +=
                lowered.transformed_native_contract_assertions;
            state.blocked_contract_assertions += lowered.blocked_contract_assertions;
            state
                .custom_before_scripts
                .extend(lowered.custom_before_scripts);
            state
                .blocked_before_scripts
                .extend(lowered.blocked_before_scripts);
            state
                .custom_after_scripts
                .extend(lowered.custom_after_scripts);
            state
                .blocked_after_scripts
                .extend(lowered.blocked_after_scripts);
            state
                .recognized_auth_scripts
                .extend(lowered.recognized_auth_scripts);
            state
                .blocked_auth_scripts
                .extend(lowered.blocked_auth_scripts);
            state.quarantine.extend(lowered.quarantine);
            let request_path = PathBuf::from("requests").join(&source.output_relative);
            let request_path_text = display_path(&request_path);
            sequence_paths.push(request_path_text.clone());
            files.push(json_file(request_path.clone(), &lowered.request)?);
            for asset in lowered.assets {
                if !files.iter().any(|file| file.relative == asset.relative) {
                    files.push(asset);
                }
            }
            if !lowered.assertions.assertions.is_empty() {
                files.push(json_file(
                    sidecar_path(&request_path, "assertions"),
                    &lowered.assertions,
                )?);
            }
            if !lowered.hooks.hooks.is_empty() {
                files.push(json_file(
                    sidecar_path(&request_path, "hooks"),
                    &lowered.hooks,
                )?);
            }
            known_runtime.extend(lowered.runtime_writes);
            if lowered.unresolved_behavior {
                push_diagnostic(
                    &mut state.diagnostics,
                    &path,
                    "materialization",
                    "request was generated with the source behavior retained where possible; review diagnostics before running",
                );
            }
        }

        files.push(sequence_file(
            leaf_sequence_path(&leaf),
            sequence_id("leaf", &leaf),
            first.leaf_name.clone(),
            first.leaf_description.clone(),
            first.leaf_tags.clone(),
            sequence_paths.clone(),
        )?);

        if let Some(service) = service_path(&leaf) {
            if !service_requests.contains_key(&service) {
                service_order.push(service.clone());
            }
            service_requests
                .entry(service)
                .or_default()
                .extend(sequence_paths);
        }
    }

    for service in service_order {
        let name = service
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| collection_name.clone());
        files.push(sequence_file(
            service_sequence_path(&service),
            sequence_id("service", &service),
            format!("{name} (all direct requests)"),
            docs_at(&root_yaml),
            tags_at(&root_yaml),
            service_requests.remove(&service).unwrap_or_default(),
        )?);
    }

    if !state.quarantine.is_empty() {
        files.push(json_file(
            PathBuf::from(IMPORT_QUARANTINE_PATH),
            &ImportQuarantineManifest::new(
                state
                    .quarantine
                    .values()
                    .cloned()
                    .map(|entry| entry.with_import_key(&collection_name))
                    .collect(),
            ),
        )?);
    }
    let excluded_request_count = state.excluded.values().sum();
    let report = BrunoV1ImportReport {
        detected_format: BrunoSourceFormat::OpenCollectionYaml,
        scanned_request_count: state.scanned,
        imported_request_count: state.requests.len(),
        excluded_request_count,
        excluded_by_policy: state.excluded,
        environments: environments
            .iter()
            .map(|environment| ImportedEnvironmentSummary {
                name: environment.name.clone(),
                variable_count: environment.plain_names.len() + environment.secret_names.len(),
                secret_count: environment.secret_names.len(),
            })
            .collect(),
        native_skip_request_count: state.native_skips,
        native_delay_request_count: state.native_delays,
        native_assertion_script_count: state.native_assertion_scripts,
        custom_assertion_script_count: state.custom_assertion_scripts,
        blocked_assertion_script_count: state.blocked_assertion_scripts,
        direct_native_contract_assertion_count: state.direct_native_contract_assertions,
        transformed_native_contract_assertion_count: state.transformed_native_contract_assertions,
        blocked_contract_assertion_count: state.blocked_contract_assertions,
        custom_before_request_script_count: state.custom_before_scripts.len(),
        blocked_before_request_script_count: state.blocked_before_scripts.len(),
        custom_after_response_script_count: state.custom_after_scripts.len(),
        blocked_after_response_script_count: state.blocked_after_scripts.len(),
        generated_auth_provider_count: auth_migration.providers.len(),
        generated_helper_request_count: auth_migration.helpers_before.values().map(Vec::len).sum(),
        recognized_auth_script_count: state.recognized_auth_scripts.len(),
        recognized_data_manager_auth_count: auth_migration.recognized_data_manager_auth_count,
        remaining_blocked_auth_script_count: state.blocked_auth_scripts.len(),
        quarantined_script_count: state.quarantine.len(),
        requires_project_code: state.custom_assertion_scripts > 0
            || state.blocked_assertion_scripts > 0
            || !state.custom_before_scripts.is_empty()
            || !state.blocked_before_scripts.is_empty()
            || !state.custom_after_scripts.is_empty()
            || !state.blocked_after_scripts.is_empty(),
        diagnostics: state.diagnostics,
        output_file_count: files.len(),
    };
    Ok(ImportPlan {
        report,
        files,
        import_key: collection_name,
    })
}

fn walk_collection_dir(
    root: &Path,
    dir: &Path,
    parent_context: &WalkContext,
    ignored: &BTreeSet<String>,
    options: BrunoV1ImportOptions,
    state: &mut PlanState,
) -> Result<(), BrunoV1ImportError> {
    let relative_dir = dir.strip_prefix(root).unwrap_or(dir);
    let mut context = parent_context.clone();
    if !relative_dir.as_os_str().is_empty() {
        context.folder_name = dir
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Folder".to_string());
        context.folder_description = None;
        context.folder_tags.clear();
    }
    let folder_file = dir.join("folder.yml");
    if folder_file.is_file() {
        let yaml = read_yaml(&folder_file)?;
        let path = display_path(folder_file.strip_prefix(root).unwrap_or(&folder_file));
        context.folder_name = text_at(&yaml, &["info", "name"])
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_else(|| context.folder_name.clone());
        context.folder_description = docs_at(&yaml)
            .or_else(|| text_at(&yaml, &["info", "description"]))
            .filter(|description| !description.trim().is_empty());
        context.folder_tags = tags_at(&yaml);
        if let Some(auth) = value_at(&yaml, &["request", "auth"])
            .or_else(|| value_at(&yaml, &["auth"]))
            .filter(|value| !matches!(value.as_str(), Some("inherit")))
        {
            context.auth = Some(auth.clone());
        }
        add_vars(
            &yaml,
            &path,
            &mut context.vars,
            &mut context.secrets,
            &mut state.diagnostics,
        );
        lower_inherited_scripts(
            &yaml,
            &path,
            folder_file.strip_prefix(root).unwrap_or(&folder_file),
            &mut context.skip,
            &mut context.delay_ms,
            &mut context.scripts,
        );
        diagnose_unknown(
            &yaml,
            &["info", "request", "auth", "runtime", "vars", "docs", "tags"],
            &path,
            "folder",
            &mut state.diagnostics,
        );
    }

    let mut entries = Vec::new();
    for entry in std::fs::read_dir(dir).map_err(|error| io_error(dir, error))? {
        let entry = entry.map_err(|error| io_error(dir, error))?;
        let path = entry.path();
        let metadata = std::fs::symlink_metadata(&path).map_err(|error| io_error(&path, error))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if metadata.file_type().is_symlink() && path.is_dir() {
            push_diagnostic(
                &mut state.diagnostics,
                &display_path(path.strip_prefix(root).unwrap_or(&path)),
                "traversal",
                "symlink directory was not traversed",
            );
            continue;
        }
        if metadata.is_dir() {
            if relative_dir.as_os_str().is_empty() && name == "environments" {
                continue;
            }
            if crate::is_ignored_dir(&name) {
                continue;
            }
            let reason = if ignored.contains(&name) {
                Some(format!("extensions.bruno.ignore:{name}"))
            } else if options.exclude_underscore_dirs && name.starts_with('_') {
                Some("underscore-directory".to_string())
            } else {
                None
            };
            if let Some(reason) = reason {
                // Ignore entries can name unrelated roots (shared, node_modules,
                // generated goldens). Only a Bruno folder is part of the request
                // corpus and therefore contributes excluded request counts.
                let count = if path.join("folder.yml").is_file() {
                    count_http_requests(root, &path, state)?
                } else {
                    0
                };
                state.scanned += count;
                if count > 0 {
                    *state.excluded.entry(reason).or_default() += count;
                }
                continue;
            }
            let seq = folder_seq(&path).unwrap_or(f64::MAX);
            entries.push((seq, name, path, true, None));
        } else if metadata.is_file()
            && matches!(
                path.extension().and_then(|value| value.to_str()),
                Some("yml" | "yaml")
            )
            && name != "folder.yml"
            && name != "opencollection.yml"
        {
            let yaml = match read_yaml(&path) {
                Ok(yaml) => yaml,
                Err(BrunoV1ImportError::Yaml { message, .. }) => {
                    push_diagnostic(
                        &mut state.diagnostics,
                        &display_path(path.strip_prefix(root).unwrap_or(&path)),
                        "yaml",
                        &message,
                    );
                    continue;
                }
                Err(error) => return Err(error),
            };
            if text_at(&yaml, &["info", "type"]).as_deref() != Some("http") {
                continue;
            }
            let seq = number_at(&yaml, &["info", "seq"]).unwrap_or(f64::MAX);
            entries.push((seq, name, path, false, Some(yaml)));
        }
    }
    entries.sort_by(|left, right| {
        left.0
            .total_cmp(&right.0)
            .then_with(|| left.1.cmp(&right.1))
    });

    for (_, _, path, is_dir, yaml) in entries {
        if is_dir {
            walk_collection_dir(root, &path, &context, ignored, options, state)?;
            continue;
        }
        let yaml = yaml.expect("request entry has parsed YAML");
        state.scanned += 1;
        let source_relative = path.strip_prefix(root).unwrap_or(&path).to_path_buf();
        if !is_enabled(&yaml) {
            *state
                .excluded
                .entry("disabled-request".to_string())
                .or_default() += 1;
            continue;
        }
        let output_relative = source_relative.with_extension("request.json");
        state.requests.push(RequestSource {
            source_relative,
            output_relative,
            leaf_relative: relative_dir.to_path_buf(),
            leaf_name: context.folder_name.clone(),
            leaf_description: context.folder_description.clone(),
            leaf_tags: context.folder_tags.clone(),
            yaml,
            inherited_auth: context.auth.clone(),
            inherited_vars: context.vars.clone(),
            inherited_secrets: context.secrets.clone(),
            inherited_skip: context.skip.clone(),
            inherited_delay_ms: context.delay_ms,
            inherited_scripts: context.scripts.clone(),
        });
    }
    Ok(())
}

fn count_http_requests(
    root: &Path,
    dir: &Path,
    state: &mut PlanState,
) -> Result<usize, BrunoV1ImportError> {
    let mut count = 0;
    for entry in std::fs::read_dir(dir).map_err(|error| io_error(dir, error))? {
        let entry = entry.map_err(|error| io_error(dir, error))?;
        let path = entry.path();
        let metadata = std::fs::symlink_metadata(&path).map_err(|error| io_error(&path, error))?;
        if metadata.file_type().is_symlink() && path.is_dir() {
            continue;
        }
        if metadata.is_dir() {
            if !crate::is_ignored_dir(&entry.file_name().to_string_lossy()) {
                count += count_http_requests(root, &path, state)?;
            }
        } else if metadata.is_file()
            && matches!(
                path.extension().and_then(|value| value.to_str()),
                Some("yml" | "yaml")
            )
            && path.file_name().and_then(|name| name.to_str()) != Some("folder.yml")
        {
            match read_yaml(&path) {
                Ok(yaml) if text_at(&yaml, &["info", "type"]).as_deref() == Some("http") => {
                    count += 1;
                }
                Ok(_) => {}
                Err(BrunoV1ImportError::Yaml { message, .. }) => push_diagnostic(
                    &mut state.diagnostics,
                    &display_path(path.strip_prefix(root).unwrap_or(&path)),
                    "yaml",
                    &message,
                ),
                Err(error) => return Err(error),
            }
        }
    }
    Ok(count)
}

fn folder_seq(path: &Path) -> Option<f64> {
    let text = std::fs::read_to_string(path.join("folder.yml")).ok()?;
    let yaml: Yaml = serde_yaml_ng::from_str(&text).ok()?;
    number_at(&yaml, &["info", "seq"])
}

fn parse_environments(
    root: &Path,
    diagnostics: &mut BTreeMap<String, BTreeMap<String, Vec<String>>>,
) -> Result<Vec<EnvironmentSource>, BrunoV1ImportError> {
    let dir = root.join("environments");
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut paths = Vec::new();
    for entry in std::fs::read_dir(&dir).map_err(|error| io_error(&dir, error))? {
        let entry = entry.map_err(|error| io_error(&dir, error))?;
        let path = entry.path();
        let metadata = std::fs::symlink_metadata(&path).map_err(|error| io_error(&path, error))?;
        if metadata.is_file()
            && matches!(
                path.extension().and_then(|value| value.to_str()),
                Some("yml" | "yaml")
            )
        {
            paths.push(path);
        }
    }
    paths.sort();
    let mut output = Vec::new();
    let mut used_files = BTreeSet::new();
    for path in paths {
        let yaml = read_yaml(&path)?;
        let source_path = display_path(path.strip_prefix(root).unwrap_or(&path));
        let fallback = file_stem(&path);
        let name = text_at(&yaml, &["name"])
            .filter(|name| !name.trim().is_empty())
            .unwrap_or(fallback);
        let mut file_name = safe_file_name(&name);
        let base = file_name.clone();
        let mut suffix = 2;
        while !used_files.insert(file_name.clone()) {
            file_name = format!("{base}-{suffix}");
            suffix += 1;
        }
        let mut values = BTreeMap::new();
        let mut plain_names = BTreeSet::new();
        let mut secret_names = BTreeSet::new();
        if let Some(variables) = value_at(&yaml, &["variables"]).and_then(Yaml::as_sequence) {
            for variable in variables {
                let Some(variable_name) = text_at(variable, &["name"])
                    .filter(|variable_name| !variable_name.trim().is_empty())
                else {
                    push_diagnostic(
                        diagnostics,
                        &source_path,
                        "environment",
                        "variable without a name was skipped",
                    );
                    continue;
                };
                let explicit_secret = bool_at(variable, &["secret"]).unwrap_or(false);
                if explicit_secret || is_sensitive_name(&variable_name) {
                    if !explicit_secret {
                        push_sensitive_variable_diagnostic(
                            diagnostics,
                            &source_path,
                            &variable_name,
                        );
                    }
                    plain_names.remove(&variable_name);
                    values.remove(&variable_name);
                    secret_names.insert(variable_name);
                    continue;
                }
                if secret_names.contains(&variable_name) {
                    continue;
                }
                plain_names.insert(variable_name.clone());
                if let Some(value) = value_at(variable, &["value"]) {
                    values.insert(variable_name, yaml_to_json(value));
                } else {
                    values.insert(variable_name, Value::String(String::new()));
                }
            }
        }
        if let Some(Value::String(token_url)) = values.get("keycloak_url") {
            if let Some((admin_url, http_token_url)) = derive_keycloak_admin_urls(token_url) {
                values.insert("keycloak_admin_url".to_string(), Value::String(admin_url));
                values.insert(
                    "keycloak_http_token_url".to_string(),
                    Value::String(http_token_url),
                );
                plain_names.insert("keycloak_admin_url".to_string());
                plain_names.insert("keycloak_http_token_url".to_string());
            }
        }
        output.push(EnvironmentSource {
            name,
            file_name,
            values,
            plain_names,
            secret_names,
        });
    }
    Ok(output)
}

fn derive_keycloak_admin_urls(token_url: &str) -> Option<(String, String)> {
    const TOKEN_PATH: &str = "/realms/gvl/protocol/openid-connect/token";
    let http_token_url = token_url
        .strip_prefix("https:")
        .map(|rest| format!("http:{rest}"))
        .unwrap_or_else(|| token_url.to_string());
    let base = http_token_url.strip_suffix(TOKEN_PATH)?;
    Some((format!("{base}/admin/realms/gvl"), http_token_url))
}

struct LoweredRequest {
    request: RequestDocument,
    assertions: AssertionDocument,
    hooks: HookDocument,
    runtime_writes: BTreeSet<String>,
    unresolved_behavior: bool,
    assets: Vec<GeneratedFile>,
    native_assertion_scripts: usize,
    custom_assertion_scripts: usize,
    blocked_assertion_scripts: usize,
    direct_native_contract_assertions: usize,
    transformed_native_contract_assertions: usize,
    blocked_contract_assertions: usize,
    custom_before_scripts: BTreeSet<PathBuf>,
    blocked_before_scripts: BTreeSet<PathBuf>,
    custom_after_scripts: BTreeSet<PathBuf>,
    blocked_after_scripts: BTreeSet<PathBuf>,
    recognized_auth_scripts: BTreeSet<PathBuf>,
    blocked_auth_scripts: BTreeSet<PathBuf>,
    quarantine: BTreeMap<String, ImportQuarantineEntry>,
}

fn lower_request(
    root: &Path,
    source: &RequestSource,
    environment_names: &BTreeSet<String>,
    secret_names: &BTreeSet<String>,
    known_runtime: &BTreeSet<String>,
    auth_migration: &AuthMigration,
    diagnostics: &mut BTreeMap<String, BTreeMap<String, Vec<String>>>,
) -> LoweredRequest {
    let path = display_path(&source.source_relative);
    diagnose_unknown(
        &source.yaml,
        &[
            "info", "http", "settings", "runtime", "vars", "docs", "tags", "auth",
        ],
        &path,
        "request",
        diagnostics,
    );
    if let Some(http) = value_at(&source.yaml, &["http"]) {
        diagnose_unknown(
            http,
            &["method", "url", "headers", "params", "body", "auth"],
            &path,
            "http",
            diagnostics,
        );
    }
    let name = text_at(&source.yaml, &["info", "name"])
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| file_stem(&source.source_relative));
    let method_text = text_at(&source.yaml, &["http", "method"]).unwrap_or_default();
    let method = Method::parse(&method_text).unwrap_or_else(|| {
        push_diagnostic(
            diagnostics,
            &path,
            "method",
            &format!("unsupported method '{method_text}', materialized as GET"),
        );
        Method::Get
    });

    let mut bindings = source
        .inherited_vars
        .iter()
        .map(|(name, value)| {
            (
                name.clone(),
                Binding::Value(ValueBinding {
                    value: value.clone(),
                }),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let mut request_vars = BTreeMap::new();
    let mut request_secrets = source.inherited_secrets.clone();
    add_vars(
        &source.yaml,
        &path,
        &mut request_vars,
        &mut request_secrets,
        diagnostics,
    );
    for secret in &request_secrets {
        bindings.remove(secret);
    }
    for (name, value) in request_vars {
        bindings.insert(name, Binding::Value(ValueBinding { value }));
    }
    let binding_names = bindings.keys().cloned().collect::<BTreeSet<_>>();
    let all_secrets = secret_names
        .union(&request_secrets)
        .cloned()
        .collect::<BTreeSet<_>>();
    let scopes = VariableScopes {
        bindings: &binding_names,
        environments: environment_names,
        secrets: &all_secrets,
        runtime: known_runtime,
    };

    let mut unresolved = BTreeSet::new();
    let mut url = translate_vars(
        &text_at(&source.yaml, &["http", "url"]).unwrap_or_default(),
        &scopes,
        &mut unresolved,
    );
    let crid = lower_crid_script(root, source, &path, diagnostics);
    let mut headers = Vec::new();
    for row in rows_at(&source.yaml, &["http", "headers"]) {
        let Some(row_name) = text_at(row, &["name"]) else {
            push_diagnostic(
                diagnostics,
                &path,
                "headers",
                "header without a name was skipped",
            );
            continue;
        };
        let name = translate_vars(&row_name, &scopes, &mut unresolved);
        let value = translate_vars(
            &scalar_text(value_at(row, &["value"])),
            &scopes,
            &mut unresolved,
        );
        headers.push(HeaderSpec {
            value: redact_sensitive_row(&name, value, &path, "header", diagnostics),
            name,
            enabled: is_enabled(row),
        });
    }
    let mut query = Vec::new();
    for row in rows_at(&source.yaml, &["http", "params"]) {
        let Some(parameter_name) = text_at(row, &["name"]) else {
            push_diagnostic(
                diagnostics,
                &path,
                "params",
                "parameter without a name was skipped",
            );
            continue;
        };
        let value = translate_vars(
            &scalar_text(value_at(row, &["value"])),
            &scopes,
            &mut unresolved,
        );
        match text_at(row, &["type"]).as_deref().unwrap_or("query") {
            "path" if is_enabled(row) => {
                url = url.replace(&format!(":{parameter_name}"), &value);
                url = url.replace(&format!("{{{{{parameter_name}}}}}"), &value);
            }
            "path" => push_diagnostic(
                diagnostics,
                &path,
                "params",
                &format!("disabled path parameter '{parameter_name}' is not applied"),
            ),
            "query" => {
                let name = translate_vars(&parameter_name, &scopes, &mut unresolved);
                query.push(HeaderSpec {
                    value: redact_sensitive_row(
                        &name,
                        value,
                        &path,
                        "query parameter",
                        diagnostics,
                    ),
                    name,
                    enabled: is_enabled(row),
                });
            }
            other => push_diagnostic(
                diagnostics,
                &path,
                "params",
                &format!("unsupported parameter type '{other}' was skipped"),
            ),
        }
    }
    let encode_url = bool_at(&source.yaml, &["settings", "encodeUrl"]);
    sanitize_url_query(&mut url, &path, diagnostics);
    reconcile_url_query(&mut url, &mut query, encode_url);
    let settings = lower_settings(&source.yaml, &path, diagnostics);

    let (body, mut assets) = if is_ipd5_token_request(source) {
        (
            Some(BodySpec::Inline(InlineBody {
                body_type: BodyType::Json,
                value: Some(Value::Object(Map::from_iter([
                    (
                        "Username".to_string(),
                        Value::String("${env.userIPD5}".to_string()),
                    ),
                    (
                        "Password".to_string(),
                        Value::String("${secret.passwordIPD5}".to_string()),
                    ),
                ]))),
            })),
            Vec::new(),
        )
    } else if let Some(crid) = &crid {
        (Some(crid.body.clone()), vec![crid.asset.clone()])
    } else {
        lower_body(
            root,
            source,
            &source.yaml,
            &path,
            &scopes,
            &mut unresolved,
            diagnostics,
        )
    };
    if matches!(&body, Some(BodySpec::Multipart(_))) {
        headers.retain(|header| {
            let value = header.value.to_ascii_lowercase();
            !(header.name.eq_ignore_ascii_case("content-type")
                && value.starts_with("multipart/form-data")
                && value.contains("boundary="))
        });
    }
    let mut hooks = HookDocument::default();
    let effective_auth = effective_source_auth(source);
    let request_auth = imported_auth_selection(source, effective_auth, auth_migration);
    lower_auth(
        effective_auth,
        request_auth.as_ref(),
        &path,
        &scopes,
        &mut unresolved,
        &mut hooks,
        diagnostics,
    );

    let mut execution = ExecutionPolicy {
        delay_before_ms: (source.inherited_delay_ms > 0).then_some(source.inherited_delay_ms),
        ..ExecutionPolicy::default()
    };
    if !source.inherited_skip.is_empty() {
        execution.skip = Some(SkipGuard {
            when: lower_source_conditions(&source.inherited_skip, &all_secrets),
            reason: "Skipped by imported Bruno before-request guard".to_string(),
        });
    }
    let mut assertions = AssertionDocument::default();
    let mut runtime_writes = BTreeSet::new();
    let assertion_scripts = lower_scripts(
        root,
        source,
        &source.yaml,
        &path,
        &mut assertions,
        &mut hooks,
        &mut runtime_writes,
        &mut execution,
        &all_secrets,
        crid.as_ref().map(|crid| crid.script.as_str()),
        &mut assets,
        diagnostics,
    );
    for name in &unresolved {
        push_diagnostic(
            diagnostics,
            &path,
            "variables",
            &format!("unresolved Bruno variable '{{{{{name}}}}}' was retained verbatim"),
        );
    }

    let tags = string_list_at(&source.yaml, &["info", "tags"])
        .into_iter()
        .chain(string_list_at(&source.yaml, &["tags"]))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let id = source
        .source_relative
        .with_extension("")
        .components()
        .map(|component| safe_id_part(&component.as_os_str().to_string_lossy()))
        .collect::<Vec<_>>()
        .join(".");
    let request = RequestDocument {
        schema: None,
        format_version: FormatVersion,
        kind: RequestKind::Request,
        meta: RequestMeta {
            id,
            name,
            description: docs_at(&source.yaml)
                .or_else(|| text_at(&source.yaml, &["info", "description"])),
            tags,
        },
        bindings,
        matrix: BTreeMap::new(),
        execution: (execution.delay_before_ms.is_some() || execution.skip.is_some())
            .then_some(execution),
        auth: request_auth,
        request: RequestSpec {
            method,
            url,
            headers,
            query,
            body,
            settings,
        },
        pipeline: Vec::new(),
        mock: None,
    };
    deduplicate_imported_pipeline(&mut assertions, &mut hooks);
    LoweredRequest {
        request,
        assertions,
        hooks,
        runtime_writes,
        unresolved_behavior: !unresolved.is_empty(),
        assets,
        native_assertion_scripts: assertion_scripts.native,
        custom_assertion_scripts: assertion_scripts.custom,
        blocked_assertion_scripts: assertion_scripts.blocked,
        direct_native_contract_assertions: assertion_scripts.direct_contracts,
        transformed_native_contract_assertions: assertion_scripts.transformed_contracts,
        blocked_contract_assertions: assertion_scripts.blocked_contracts,
        custom_before_scripts: assertion_scripts.custom_before,
        blocked_before_scripts: assertion_scripts.blocked_before,
        custom_after_scripts: assertion_scripts.custom_after,
        blocked_after_scripts: assertion_scripts.blocked_after,
        recognized_auth_scripts: assertion_scripts.recognized_auth,
        blocked_auth_scripts: assertion_scripts.blocked_auth,
        quarantine: assertion_scripts.quarantine,
    }
}

fn deduplicate_imported_pipeline(assertions: &mut AssertionDocument, hooks: &mut HookDocument) {
    let mut seen_assertions = Vec::new();
    assertions.assertions.retain(|assertion| {
        if seen_assertions.contains(assertion) {
            false
        } else {
            seen_assertions.push(assertion.clone());
            true
        }
    });

    let mut seen_hooks: Vec<PipelineEntry> = Vec::new();
    hooks.hooks.retain(|hook| {
        if seen_hooks.iter().any(|seen| {
            seen.phase == hook.phase
                && seen.uses == hook.uses
                && seen.with == hook.with
                && seen.enabled == hook.enabled
        }) {
            false
        } else {
            seen_hooks.push(hook.clone());
            true
        }
    });
}

fn lower_body(
    root: &Path,
    source: &RequestSource,
    yaml: &Yaml,
    path: &str,
    scopes: &VariableScopes<'_>,
    unresolved: &mut BTreeSet<String>,
    diagnostics: &mut BTreeMap<String, BTreeMap<String, Vec<String>>>,
) -> (Option<BodySpec>, Vec<GeneratedFile>) {
    let Some(body) = value_at(yaml, &["http", "body"]) else {
        return (None, Vec::new());
    };
    if !is_enabled(body) {
        return (None, Vec::new());
    }
    let kind = text_at(body, &["type"]).unwrap_or_else(|| "none".to_string());
    let data = value_at(body, &["data"]);
    let inline = |body_type, value| {
        (
            Some(BodySpec::Inline(InlineBody {
                body_type,
                value: Some(value),
            })),
            Vec::new(),
        )
    };
    match kind.as_str() {
        "none" => (None, Vec::new()),
        "json" => match data {
            Some(Yaml::String(text)) => {
                let translated = translate_vars(text, scopes, unresolved);
                match serde_json::from_str(&translated) {
                    Ok(mut value) => {
                        redact_json_secrets(&mut value, path, diagnostics);
                        inline(BodyType::Json, value)
                    }
                    Err(error) => {
                        push_diagnostic(
                            diagnostics,
                            path,
                            "body",
                            &format!(
                                "JSON body is not strict JSON after variable translation ({error}); retained as text"
                            ),
                        );
                        inline(BodyType::Text, Value::String(translated))
                    }
                }
            }
            Some(value) => {
                let mut value = translate_json_vars(yaml_to_json(value), scopes, unresolved);
                redact_json_secrets(&mut value, path, diagnostics);
                inline(BodyType::Json, value)
            }
            None => inline(BodyType::Json, Value::Null),
        },
        "text" | "xml" => inline(
            BodyType::Text,
            Value::String(translate_vars(&scalar_text(data), scopes, unresolved)),
        ),
        "form-urlencoded" => {
            let mut object = Map::new();
            for row in data.and_then(Yaml::as_sequence).into_iter().flatten() {
                let Some(name) = text_at(row, &["name"]) else {
                    push_diagnostic(
                        diagnostics,
                        path,
                        "body",
                        "form field without a name was skipped",
                    );
                    continue;
                };
                if !is_enabled(row) {
                    push_diagnostic(
                        diagnostics,
                        path,
                        "body",
                        &format!("disabled form field '{name}' cannot be represented in a v1 form object"),
                    );
                    continue;
                }
                let value =
                    translate_vars(&scalar_text(value_at(row, &["value"])), scopes, unresolved);
                let value = Value::String(redact_sensitive_row(
                    &name,
                    value,
                    path,
                    "form field",
                    diagnostics,
                ));
                if object.insert(name.clone(), value).is_some() {
                    push_diagnostic(
                        diagnostics,
                        path,
                        "body",
                        &format!("duplicate form field '{name}' cannot be represented losslessly"),
                    );
                }
            }
            inline(BodyType::Form, Value::Object(object))
        }
        "multipart-form" => {
            lower_multipart_body(root, source, data, path, scopes, unresolved, diagnostics)
        }
        "file" => lower_binary_body(root, source, body, data, path, diagnostics),
        "graphql" => {
            push_diagnostic(
                diagnostics,
                path,
                "body",
                "GraphQL body was retained as text because request-v1 has no GraphQL body kind",
            );
            let text = match data {
                Some(Yaml::Mapping(_) | Yaml::Sequence(_)) => {
                    let mut value = translate_json_vars(
                        data.map(yaml_to_json).unwrap_or(Value::Null),
                        scopes,
                        unresolved,
                    );
                    redact_json_secrets(&mut value, path, diagnostics);
                    serde_json::to_string(&value).unwrap_or_default()
                }
                _ => translate_vars(&scalar_text(data), scopes, unresolved),
            };
            inline(BodyType::Text, Value::String(text))
        }
        other => {
            push_diagnostic(
                diagnostics,
                path,
                "body",
                &format!("unsupported body type '{other}' was retained as text"),
            );
            let text = match data {
                Some(Yaml::Mapping(_) | Yaml::Sequence(_)) => {
                    let mut value = translate_json_vars(
                        data.map(yaml_to_json).unwrap_or(Value::Null),
                        scopes,
                        unresolved,
                    );
                    redact_json_secrets(&mut value, path, diagnostics);
                    serde_json::to_string(&value).unwrap_or_default()
                }
                _ => translate_vars(&scalar_text(data), scopes, unresolved),
            };
            inline(BodyType::Text, Value::String(text))
        }
    }
}

fn lower_multipart_body(
    root: &Path,
    source: &RequestSource,
    data: Option<&Yaml>,
    path: &str,
    scopes: &VariableScopes<'_>,
    unresolved: &mut BTreeSet<String>,
    diagnostics: &mut BTreeMap<String, BTreeMap<String, Vec<String>>>,
) -> (Option<BodySpec>, Vec<GeneratedFile>) {
    let mut parts = Vec::new();
    let mut assets = Vec::new();
    for row in data.and_then(Yaml::as_sequence).into_iter().flatten() {
        let Some(name) = text_at(row, &["name"]).filter(|name| !name.is_empty()) else {
            push_diagnostic(
                diagnostics,
                path,
                "body",
                "multipart part without a name was skipped",
            );
            continue;
        };
        let enabled = is_enabled(row);
        let filename = text_at(row, &["filename"]).filter(|value| !value.is_empty());
        let content_type = text_at(row, &["contentType"]).filter(|value| !value.is_empty());
        if text_at(row, &["type"]).as_deref() == Some("file") {
            let raw = file_value(value_at(row, &["value"]));
            let Some((reference, asset)) = raw
                .as_deref()
                .and_then(|raw| materialize_body_file(root, source, raw, path, diagnostics, false))
            else {
                if raw.is_none() {
                    push_diagnostic(
                        diagnostics,
                        path,
                        "body",
                        &format!("multipart file part '{name}' has no file path"),
                    );
                }
                continue;
            };
            assets.push(asset);
            parts.push(V1MultipartPart::File {
                name,
                file: reference,
                filename,
                content_type,
                enabled,
            });
        } else {
            parts.push(V1MultipartPart::Text {
                name,
                value: translate_vars(&scalar_text(value_at(row, &["value"])), scopes, unresolved),
                filename,
                content_type,
                enabled,
            });
        }
    }
    (
        Some(BodySpec::Multipart(MultipartBody {
            body_type: MultipartBodyType::Multipart,
            parts,
        })),
        assets,
    )
}

fn lower_binary_body(
    root: &Path,
    source: &RequestSource,
    body: &Yaml,
    data: Option<&Yaml>,
    path: &str,
    diagnostics: &mut BTreeMap<String, BTreeMap<String, Vec<String>>>,
) -> (Option<BodySpec>, Vec<GeneratedFile>) {
    let Some(raw) = file_value(data) else {
        push_diagnostic(diagnostics, path, "body", "file body has no file path");
        return (None, Vec::new());
    };
    let Some((reference, asset)) =
        materialize_body_file(root, source, &raw, path, diagnostics, false)
    else {
        return (None, Vec::new());
    };
    (
        Some(BodySpec::Binary(BinaryBody {
            body_type: BinaryBodyType::Binary,
            file: reference,
            content_type: text_at(body, &["contentType"]).filter(|value| !value.is_empty()),
        })),
        vec![asset],
    )
}

fn file_value(value: Option<&Yaml>) -> Option<String> {
    match value? {
        Yaml::Sequence(values) => values.first().and_then(Yaml::as_str).map(str::to_string),
        value => value.as_str().map(str::to_string),
    }
}

#[derive(Clone)]
struct CridLowering {
    body: BodySpec,
    asset: GeneratedFile,
    script: String,
}

fn lower_crid_script(
    root: &Path,
    source: &RequestSource,
    path: &str,
    diagnostics: &mut BTreeMap<String, BTreeMap<String, Vec<String>>>,
) -> Option<CridLowering> {
    let scripts = value_at(&source.yaml, &["runtime", "scripts"]).and_then(Yaml::as_sequence)?;
    let field = |name: &str| {
        Regex::new(&format!(r#"(?s)\b{name}\s*:\s*["']([^"']+)["']"#)).expect("CRID field regex")
    };
    let temp_value = Regex::new(r#"(?s)\bvalue\s*:\s*\[\s*([A-Za-z_$][A-Za-z0-9_$]*)\s*\]"#)
        .expect("CRID temp value regex");
    let set_body =
        Regex::new(r"(?s)req\.setBody\s*\(\s*\[\s*\{(.*?)\}\s*\]\s*(?:,\s*\{.*?\})?\s*\)")
            .expect("CRID setBody regex");
    for script in scripts.iter().filter(|script| is_enabled(script)) {
        if text_at(script, &["type"]).as_deref() != Some("before-request") {
            continue;
        }
        let code = text_at(script, &["code"]).unwrap_or_default();
        if !code.contains("req.setBody") {
            continue;
        }
        if code.matches("req.setBody").count() != 1 {
            continue;
        }
        let Some(body_code) = set_body
            .captures(&code)
            .and_then(|captures| captures.get(1))
            .map(|capture| capture.as_str())
        else {
            continue;
        };
        if field("type")
            .captures(body_code)
            .map(|capture| capture[1].to_string())
            .as_deref()
            != Some("file")
        {
            continue;
        }
        let Some(name) = field("name")
            .captures(body_code)
            .map(|capture| capture[1].to_string())
        else {
            continue;
        };
        let Some(temp) = temp_value
            .captures(body_code)
            .map(|capture| capture[1].to_string())
        else {
            continue;
        };
        let static_fixture = static_crid_fixture_for_temp(&code, &temp);
        let runtime_fixture = static_fixture
            .is_none()
            .then(|| runtime_crid_fixture_for_temp(root, source, &code, &temp))
            .flatten();
        let fixture = static_fixture.clone().or(runtime_fixture.clone());
        let materialized = fixture
            .and_then(|raw| materialize_body_file(root, source, &raw, path, diagnostics, true));
        let generated_contents = generated_crid_xml_for_temp(&code, &temp);
        if !isolated_crid_body_script(
            &code,
            &temp,
            static_fixture.is_some(),
            generated_contents.is_some(),
        ) {
            continue;
        }
        let generated =
            generated_contents.map(|contents| materialize_generated_crid_xml(source, contents));
        if materialized.is_some() == generated.is_some() {
            continue;
        }
        let Some((reference, asset)) = materialized.or(generated) else {
            continue;
        };
        return Some(CridLowering {
            body: BodySpec::Multipart(MultipartBody {
                body_type: MultipartBodyType::Multipart,
                parts: vec![V1MultipartPart::File {
                    name,
                    file: reference,
                    filename: None,
                    content_type: Some("application/xml".to_string()),
                    enabled: true,
                }],
            }),
            asset,
            script: code,
        });
    }
    None
}

fn isolated_crid_body_script(code: &str, temp: &str, copied: bool, generated: bool) -> bool {
    let set_body =
        Regex::new(r"(?s)req\.setBody\s*\(\s*\[\s*\{.*?\}\s*\]\s*(?:,\s*\{.*?\})?\s*\)\s*;?")
            .expect("CRID set-body removal regex");
    let mut remainder = set_body.replace(code, "").into_owned();
    if copied {
        let copy = Regex::new(&format!(
            r"(?s)(?:fs\.)?copyFileSync\s*\(\s*[^,]+?\s*,\s*{}\s*\)\s*;?",
            regex::escape(temp)
        ))
        .expect("CRID copy removal regex");
        remainder = copy.replace(&remainder, "").into_owned();
    }
    if generated {
        let write = Regex::new(&format!(
            r#"(?s)(?:fs\.)?writeFileSync\s*\(\s*{}\s*,\s*(?:'[^']*'|"[^"]*"|`[^`]*`)\s*\)\s*;?"#,
            regex::escape(temp)
        ))
        .expect("CRID write removal regex");
        remainder = write.replace(&remainder, "").into_owned();
    }
    if remainder.contains("/*") || remainder.contains("//") {
        return false;
    }
    let declaration = Regex::new(r"(?s)^(?:const|let|var)\s+[A-Za-z_$][A-Za-z0-9_$]*\s*=\s*(.+)$")
        .expect("CRID declaration regex");
    remainder.split(';').all(|statement| {
        let statement = statement.trim();
        if statement.is_empty() {
            return true;
        }
        let Some(value) = declaration
            .captures(statement)
            .and_then(|capture| capture.get(1))
            .map(|capture| capture.as_str())
        else {
            return false;
        };
        static_crid_path_expression(value)
            || Regex::new(r#"^bru\.getVar\s*\(\s*["'][^"']+["']\s*\)$"#)
                .expect("CRID runtime read regex")
                .is_match(value.trim())
    })
}

fn static_crid_path_expression(value: &str) -> bool {
    Regex::new(
        r#"^(?:path\.(?:join|resolve)\s*\(\s*(?:(?:process\.cwd|os\.tmpdir)\s*\(\s*\)\s*,\s*)?["'][^"']+["']\s*\)|["'][^"']+["'])$"#,
    )
    .expect("static CRID path expression regex")
    .is_match(value.trim())
}

fn static_crid_fixture_for_temp(code: &str, temp: &str) -> Option<String> {
    let copy = Regex::new(&format!(
        r#"(?s)copyFileSync\s*\(\s*([^,]+?)\s*,\s*{}\s*\)"#,
        regex::escape(temp)
    ))
    .ok()?;
    let source = copy.captures(code)?.get(1)?.as_str().trim();
    if let Some(capture) = Regex::new(r#"(?i)^["']([^"']+\.xml)["']$"#)
        .ok()?
        .captures(source)
    {
        return Some(capture[1].to_string());
    }
    if !Regex::new(r"^[A-Za-z_$][A-Za-z0-9_$]*$")
        .ok()?
        .is_match(source)
    {
        return None;
    }
    let assignment = Regex::new(&format!(
        r#"(?i)(?:const|let|var)\s+{}\s*=\s*(?:path\.(?:join|resolve)\s*\(\s*(?:(?:process\.cwd|os\.tmpdir)\s*\(\s*\)\s*,\s*)?)?["']([^"']+\.xml)["']\s*\)?\s*;"#,
        regex::escape(source)
    ))
    .ok()?;
    let mut fixtures = assignment
        .captures_iter(code)
        .filter_map(|capture| capture.get(1).map(|value| value.as_str().to_string()));
    let fixture = fixtures.next()?;
    fixtures.next().is_none().then_some(fixture)
}

fn generated_crid_xml_for_temp(code: &str, temp: &str) -> Option<String> {
    let generated = Regex::new(&format!(
        r#"(?s)writeFileSync\s*\(\s*{}\s*,\s*(?:'([^']*)'|"([^"]*)"|`([^`]*)`)\s*\)"#,
        regex::escape(temp)
    ))
    .ok()?;
    let mut matches = generated.captures_iter(code);
    let capture = matches.next()?;
    if matches.next().is_some() {
        return None;
    }
    let contents =
        (1..=3).find_map(|index| capture.get(index).map(|value| value.as_str().to_string()))?;
    (!contents.contains("${")).then_some(contents)
}

fn runtime_crid_fixture_for_temp(
    root: &Path,
    source: &RequestSource,
    code: &str,
    temp: &str,
) -> Option<String> {
    let runtime_read = Regex::new(&format!(
        r#"(?:const|let|var)\s+{}\s*=\s*bru\.getVar\s*\(\s*["']([^"']+)["']\s*\)"#,
        regex::escape(temp)
    ))
    .ok()?;
    let runtime_name = runtime_read.captures(code)?.get(1)?.as_str();
    let leaf = root.join(&source.leaf_relative);
    let assignment = Regex::new(&format!(
        r#"bru\.setVar\s*\(\s*["']{}["']\s*,\s*([A-Za-z_$][A-Za-z0-9_$]*)\s*\)"#,
        regex::escape(runtime_name)
    ))
    .ok()?;
    let current_seq = number_at(&source.yaml, &["info", "seq"]);
    let mut fixtures = walkdir::WalkDir::new(leaf)
        .follow_links(false)
        .sort_by_file_name()
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .filter_map(|entry| {
            let relative = entry.path().strip_prefix(root).ok()?.to_path_buf();
            let text = std::fs::read_to_string(entry.path()).ok()?;
            let yaml = serde_yaml_ng::from_str::<Yaml>(&text).ok()?;
            let producer_seq = number_at(&yaml, &["info", "seq"]).unwrap_or(f64::MAX);
            let current_seq = current_seq.unwrap_or(f64::MAX);
            let preceding = producer_seq < current_seq
                || (producer_seq == current_seq && relative < source.source_relative);
            preceding.then_some(text)
        })
        .filter_map(|text| {
            let producer_temp = assignment.captures(&text)?.get(1)?.as_str();
            static_crid_fixture_for_temp(&text, producer_temp)
        });
    let fixture = fixtures.next()?;
    fixtures.next().is_none().then_some(fixture)
}

fn materialize_generated_crid_xml(
    source: &RequestSource,
    contents: String,
) -> (String, GeneratedFile) {
    let asset_relative = PathBuf::from("assets")
        .join("bruno/generated")
        .join(source.source_relative.with_extension("xml"));
    let request_relative = PathBuf::from("requests").join(&source.output_relative);
    let reference = crate::reqv1::index::relative_path(
        request_relative.parent().unwrap_or(Path::new("requests")),
        &asset_relative,
    );
    (
        reference,
        GeneratedFile {
            relative: asset_relative,
            bytes: contents.into_bytes(),
        },
    )
}

fn materialize_body_file(
    root: &Path,
    source: &RequestSource,
    raw: &str,
    path: &str,
    diagnostics: &mut BTreeMap<String, BTreeMap<String, Vec<String>>>,
    search_by_name: bool,
) -> Option<(String, GeneratedFile)> {
    let raw_path = Path::new(raw.trim_start_matches("file://"));
    let request_dir = root
        .join(&source.source_relative)
        .parent()
        .unwrap_or(root)
        .to_path_buf();
    let mut candidates = if raw_path.is_absolute() {
        vec![raw_path.to_path_buf()]
    } else {
        vec![request_dir.join(raw_path), root.join(raw_path)]
    };
    if search_by_name && !candidates.iter().any(|candidate| candidate.is_file()) {
        let name = raw_path.file_name()?;
        candidates.extend(
            walkdir::WalkDir::new(root)
                .follow_links(false)
                .into_iter()
                .filter_map(Result::ok)
                .filter(|entry| entry.file_type().is_file() && entry.file_name() == name)
                .map(|entry| entry.into_path()),
        );
    }
    let canonical_root = std::fs::canonicalize(root).ok()?;
    let matches = candidates
        .into_iter()
        .filter_map(|candidate| std::fs::canonicalize(candidate).ok())
        .filter(|candidate| candidate.is_file() && candidate.starts_with(&canonical_root))
        .collect::<BTreeSet<_>>();
    if matches.len() != 1 {
        push_diagnostic(
            diagnostics,
            path,
            "body",
            &format!(
                "body file reference '{}' was missing, escaped the collection, or was ambiguous",
                raw_path.display()
            ),
        );
        return None;
    }
    let source_file = matches.into_iter().next().expect("one body file");
    let source_relative = source_file.strip_prefix(&canonical_root).ok()?;
    let asset_relative = PathBuf::from("assets").join("bruno").join(source_relative);
    let request_relative = PathBuf::from("requests").join(&source.output_relative);
    let reference = crate::reqv1::index::relative_path(
        request_relative.parent().unwrap_or(Path::new("requests")),
        &asset_relative,
    );
    let bytes = match std::fs::read(&source_file) {
        Ok(bytes) => bytes,
        Err(error) => {
            push_diagnostic(
                diagnostics,
                path,
                "body",
                &format!(
                    "body file '{}' could not be read: {error}",
                    raw_path.display()
                ),
            );
            return None;
        }
    };
    Some((
        reference,
        GeneratedFile {
            relative: asset_relative,
            bytes,
        },
    ))
}

fn lower_auth(
    auth: Option<&Yaml>,
    selection: Option<&RequestAuthSelection>,
    path: &str,
    scopes: &VariableScopes<'_>,
    unresolved: &mut BTreeSet<String>,
    hooks: &mut HookDocument,
    diagnostics: &mut BTreeMap<String, BTreeMap<String, Vec<String>>>,
) {
    if selection.is_some() {
        return;
    }
    let Some(auth) = auth else {
        return;
    };
    let kind = auth
        .as_str()
        .map(str::to_string)
        .or_else(|| text_at(auth, &["type"]))
        .unwrap_or_else(|| "inherit".to_string());
    let before = |uses: &str, with: Map<String, Value>| PipelineEntry {
        phase: PipelinePhase::BeforeRequest,
        uses: uses.to_string(),
        with,
        enabled: is_enabled(auth),
    };
    match kind.as_str() {
        "none" | "inherit" => {}
        "bearer" => hooks.push(before(
            "builtin:bearer@1",
            Map::from_iter([(
                "token".to_string(),
                Value::String(lower_secret_value(
                    &text_at(auth, &["token"]).unwrap_or_default(),
                    "bruno_bearer_token",
                    "bearer token",
                    path,
                    scopes,
                    unresolved,
                    diagnostics,
                )),
            )]),
        )),
        "basic" => hooks.push(before(
            "builtin:basic@1",
            Map::from_iter([
                (
                    "username".to_string(),
                    Value::String(translate_vars(
                        &text_at(auth, &["username"]).unwrap_or_default(),
                        scopes,
                        unresolved,
                    )),
                ),
                (
                    "password".to_string(),
                    Value::String(lower_secret_value(
                        &text_at(auth, &["password"]).unwrap_or_default(),
                        "bruno_basic_password",
                        "basic auth password",
                        path,
                        scopes,
                        unresolved,
                        diagnostics,
                    )),
                ),
            ]),
        )),
        other => push_diagnostic(
            diagnostics,
            path,
            "auth",
            &format!("auth type '{other}' has no safe direct request-v1 conversion"),
        ),
    }
}

fn lower_secret_value(
    raw: &str,
    fallback_name: &str,
    field: &str,
    path: &str,
    scopes: &VariableScopes<'_>,
    unresolved: &mut BTreeSet<String>,
    diagnostics: &mut BTreeMap<String, BTreeMap<String, Vec<String>>>,
) -> String {
    let translated = translate_vars(raw, scopes, unresolved);
    if contains_dynamic_reference(&translated) {
        return translated;
    }
    push_diagnostic(
        diagnostics,
        path,
        "secrets",
        &format!("literal {field} was replaced by a secret-provider reference"),
    );
    secret_reference(fallback_name)
}

fn redact_sensitive_row(
    name: &str,
    value: String,
    path: &str,
    field: &str,
    diagnostics: &mut BTreeMap<String, BTreeMap<String, Vec<String>>>,
) -> String {
    if !is_sensitive_name(name) || contains_dynamic_reference(&value) {
        return value;
    }
    push_diagnostic(
        diagnostics,
        path,
        "secrets",
        &format!("literal sensitive {field} '{name}' was replaced by a secret-provider reference"),
    );
    secret_reference(name)
}

fn redact_json_secrets(
    value: &mut Value,
    path: &str,
    diagnostics: &mut BTreeMap<String, BTreeMap<String, Vec<String>>>,
) {
    match value {
        Value::Array(values) => {
            for value in values {
                redact_json_secrets(value, path, diagnostics);
            }
        }
        Value::Object(values) => {
            for (name, value) in values {
                if is_sensitive_name(name) {
                    if value.as_str().is_some_and(contains_dynamic_reference) {
                        continue;
                    }
                    *value = Value::String(secret_reference(name));
                    push_diagnostic(
                        diagnostics,
                        path,
                        "secrets",
                        &format!(
                            "literal sensitive body field '{name}' was replaced by a secret-provider reference"
                        ),
                    );
                } else {
                    redact_json_secrets(value, path, diagnostics);
                }
            }
        }
        _ => {}
    }
}

fn sanitize_url_query(
    url: &mut String,
    path: &str,
    diagnostics: &mut BTreeMap<String, BTreeMap<String, Vec<String>>>,
) {
    let Some(question) = url.find('?') else {
        return;
    };
    let fragment = url[question + 1..]
        .find('#')
        .map(|index| question + 1 + index)
        .unwrap_or(url.len());
    let query = &url[question + 1..fragment];
    let mut changed = false;
    let sanitized = query
        .split('&')
        .map(|part| {
            let (name, value) = part.split_once('=').unwrap_or((part, ""));
            if is_sensitive_name(name) && !contains_dynamic_reference(value) {
                changed = true;
                push_diagnostic(
                    diagnostics,
                    path,
                    "secrets",
                    &format!(
                        "literal sensitive URL query parameter '{name}' was replaced by a secret-provider reference"
                    ),
                );
                format!("{name}={}", secret_reference(name))
            } else {
                part.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("&");
    if changed {
        *url = format!("{}{}{}", &url[..question + 1], sanitized, &url[fragment..]);
    }
}

fn reconcile_url_query(url: &mut String, query: &mut Vec<HeaderSpec>, encode_url: Option<bool>) {
    let Some(question) = url.find('?') else {
        return;
    };
    let fragment = url[question + 1..]
        .find('#')
        .map(|index| question + 1 + index);
    let query_end = fragment.unwrap_or(url.len());
    let raw_pairs = url[question + 1..query_end]
        .split('&')
        .filter(|part| !part.is_empty())
        .map(|part| {
            let (name, value) = part.split_once('=').unwrap_or((part, ""));
            (name.to_string(), value.to_string())
        })
        .collect::<Vec<_>>();
    if raw_pairs.is_empty() {
        return;
    }

    let mut enabled_counts = BTreeMap::<(String, String), usize>::new();
    for row in query.iter().filter(|row| row.enabled) {
        *enabled_counts
            .entry((row.name.clone(), row.value.clone()))
            .or_default() += 1;
    }
    let mut raw_counts = BTreeMap::<(String, String), usize>::new();
    for pair in &raw_pairs {
        *raw_counts.entry(pair.clone()).or_default() += 1;
    }
    if encode_url != Some(false) && fragment.is_none() {
        let mut available = enabled_counts;
        let mut missing_rows = Vec::new();
        for (name, value) in &raw_pairs {
            let pair = (name.clone(), value.clone());
            let count = available.entry(pair).or_default();
            if *count > 0 {
                *count -= 1;
            } else {
                missing_rows.push(HeaderSpec {
                    name: name.clone(),
                    value: value.clone(),
                    enabled: true,
                });
            }
        }
        missing_rows.append(query);
        *query = missing_rows;
        url.truncate(question);
        return;
    }

    // The raw URL is authoritative for encodeUrl:false and for mixed raw/row
    // queries. Remove only enabled rows already present in it; disabled rows
    // remain available for editing without affecting execution.
    let mut remaining = raw_counts;
    query.retain(|row| {
        if !row.enabled {
            return true;
        }
        let key = (row.name.clone(), row.value.clone());
        let Some(count) = remaining.get_mut(&key) else {
            return true;
        };
        if *count == 0 {
            true
        } else {
            *count -= 1;
            false
        }
    });
}

fn lower_settings(
    yaml: &Yaml,
    path: &str,
    diagnostics: &mut BTreeMap<String, BTreeMap<String, Vec<String>>>,
) -> RequestTransportSettings {
    let mut output = RequestTransportSettings::default();
    let Some(settings) = value_at(yaml, &["settings"]) else {
        return output;
    };
    let Some(mapping) = settings.as_mapping() else {
        push_diagnostic(
            diagnostics,
            path,
            "settings",
            "settings must be a mapping and were not materialized",
        );
        return output;
    };
    for key in mapping.keys().filter_map(Yaml::as_str) {
        match key {
            "encodeUrl" => {
                if let Some(value) = bool_at(yaml, &["settings", "encodeUrl"]) {
                    output.encode_url = Some(value);
                } else {
                    push_diagnostic(
                        diagnostics,
                        path,
                        "settings.encodeUrl",
                        "encodeUrl is not boolean and could not be interpreted",
                    );
                }
            }
            "timeout" => {
                if let Some(value) = unsigned_integer_at(yaml, &["settings", "timeout"])
                    .and_then(|value| u64::try_from(value).ok())
                {
                    output.timeout_ms = Some(value);
                } else {
                    push_diagnostic(
                        diagnostics,
                        path,
                        "settings.timeout",
                        "timeout is not a non-negative integer and could not be interpreted",
                    );
                }
            }
            "followRedirects" => {
                if let Some(value) = bool_at(yaml, &["settings", "followRedirects"]) {
                    output.follow_redirects = Some(value);
                } else {
                    push_diagnostic(
                        diagnostics,
                        path,
                        "settings.followRedirects",
                        "followRedirects is not boolean and could not be interpreted",
                    );
                }
            }
            "maxRedirects" => {
                if let Some(value) = unsigned_integer_at(yaml, &["settings", "maxRedirects"])
                    .and_then(|value| u32::try_from(value).ok())
                {
                    output.max_redirects = Some(value);
                } else {
                    push_diagnostic(
                        diagnostics,
                        path,
                        "settings.maxRedirects",
                        "maxRedirects is not a non-negative 32-bit integer and could not be interpreted",
                    );
                }
            }
            other => push_diagnostic(
                diagnostics,
                path,
                &format!("settings.{other}"),
                &format!("unrecognized settings field '{other}' was not materialized"),
            ),
        }
    }
    output
}

fn unsigned_integer_at(value: &Yaml, path: &[&str]) -> Option<u128> {
    let value = value_at(value, path)?;
    value
        .as_u64()
        .map(u128::from)
        .or_else(|| value.as_str()?.parse().ok())
}

#[derive(Default)]
struct BeforeRequestAnalysis {
    skip: Vec<SourceCondition>,
    delays: Vec<u64>,
    complete: bool,
}

fn analyze_before_request(code: &str) -> BeforeRequestAnalysis {
    let aliases_pattern = Regex::new(
        r#"\b(?:const|let|var)\s+([A-Za-z_$][A-Za-z0-9_$]*)\s*=\s*bru\.(getEnvVar|getVar)\s*\(\s*["']([^"']+)["']\s*\)\s*;?"#,
    )
    .expect("Bruno variable alias regex");
    let mut aliases = BTreeMap::new();
    let mut alias_ranges = BTreeMap::new();
    for captures in aliases_pattern.captures_iter(code) {
        let Some(whole) = captures.get(0) else {
            continue;
        };
        if !top_level_at(code, whole.start()) {
            continue;
        }
        let name = captures[1].to_string();
        aliases.insert(
            name.clone(),
            SourceVariable {
                kind: if &captures[2] == "getVar" {
                    SourceVariableKind::Runtime
                } else {
                    SourceVariableKind::Environment
                },
                name: captures[3].to_string(),
            },
        );
        alias_ranges.insert(name, whole.start()..whole.end());
    }

    let delay_pattern = Regex::new(
        r"await\s+new\s+Promise\s*\(\s*([A-Za-z_$][A-Za-z0-9_$]*)\s*=>\s*setTimeout\s*\(\s*([A-Za-z_$][A-Za-z0-9_$]*)\s*,\s*(5000|30000|60000|90000)\s*\)\s*\)\s*;?",
    )
    .expect("Bruno fixed delay regex");
    let mut delays = Vec::new();
    let mut consumed = Vec::<std::ops::Range<usize>>::new();
    for captures in delay_pattern.captures_iter(code) {
        if captures[1] != captures[2] {
            continue;
        }
        let Some(whole) = captures.get(0) else {
            continue;
        };
        if !top_level_at(code, whole.start()) {
            continue;
        }
        delays.push(captures[3].parse().expect("fixed numeric delay"));
        consumed.push(whole.start()..whole.end());
    }

    let if_pattern = Regex::new(r"\bif\s*\(").expect("Bruno if regex");
    let skip_statement = Regex::new(r"^\s*bru\.runner\.skipRequest\s*\(\s*\)\s*;?\s*$")
        .expect("Bruno skip statement regex");
    let inline_skip_statement = Regex::new(r"^\s*bru\.runner\.skipRequest\s*\(\s*\)\s*;?")
        .expect("Bruno inline skip regex");
    let direct_skip =
        Regex::new(r"bru\.runner\.skipRequest\s*\(\s*\)\s*;?").expect("Bruno direct skip regex");
    let mut skip = Vec::new();
    let mut used_aliases = BTreeSet::new();
    let mut has_unconverted_behavior = false;
    for matched in if_pattern.find_iter(code) {
        if consumed_at(matched.start(), &consumed) || !top_level_at(code, matched.start()) {
            continue;
        }
        let open = matched.end() - 1;
        let Some(close) = matching_delimiter(code, open, b'(', b')') else {
            continue;
        };
        let mut body_start = close + 1;
        while code
            .as_bytes()
            .get(body_start)
            .is_some_and(u8::is_ascii_whitespace)
        {
            body_start += 1;
        }
        let Some(next) = code.as_bytes().get(body_start) else {
            continue;
        };
        let (body, end) = if *next == b'{' {
            let Some(body_close) = matching_delimiter(code, body_start, b'{', b'}') else {
                continue;
            };
            (&code[body_start + 1..body_close], body_close + 1)
        } else {
            let rest = &code[body_start..];
            let Some(statement) = inline_skip_statement.find(rest) else {
                continue;
            };
            (
                &rest[statement.start()..statement.end()],
                body_start + statement.end(),
            )
        };
        let body_is_only_skip = skip_statement.is_match(&strip_js_comments(body));
        let body_skip_count = direct_skip
            .find_iter(body)
            .filter(|matched| top_level_at(body, matched.start()))
            .count();
        if !body_is_only_skip && body_skip_count != 1 {
            continue;
        }
        let Some((condition, aliases_in_condition)) =
            ConditionParser::parse(&code[open + 1..close], &aliases)
        else {
            continue;
        };
        used_aliases.extend(aliases_in_condition);
        skip.push(condition);
        consumed.push(matched.start()..end);
        has_unconverted_behavior |= !body_is_only_skip;
    }

    for matched in direct_skip.find_iter(code) {
        if consumed_at(matched.start(), &consumed) || !top_level_at(code, matched.start()) {
            continue;
        }
        skip.push(SourceCondition::Literal(true));
        consumed.push(matched.start()..matched.end());
    }
    for alias in used_aliases {
        if let Some(range) = alias_ranges.remove(&alias) {
            consumed.push(range);
        }
    }

    let mut remainder = code.as_bytes().to_vec();
    for range in consumed {
        for byte in &mut remainder[range] {
            *byte = b' ';
        }
    }
    let remainder = String::from_utf8(remainder).expect("JavaScript source was valid UTF-8");
    let remainder = strip_js_comments(&remainder);
    let complete = !has_unconverted_behavior
        && remainder
            .chars()
            .all(|character| character.is_whitespace() || character == ';');
    BeforeRequestAnalysis {
        skip,
        delays,
        complete,
    }
}

fn strip_js_comments(code: &str) -> String {
    Regex::new(r"(?s)/\*.*?\*/|(?m)//[^\r\n]*")
        .expect("comment regex")
        .replace_all(code, "")
        .into_owned()
}

fn consumed_at(position: usize, ranges: &[std::ops::Range<usize>]) -> bool {
    ranges
        .iter()
        .any(|range| range.start <= position && position < range.end)
}

fn matching_delimiter(code: &str, open: usize, left: u8, right: u8) -> Option<usize> {
    let bytes = code.as_bytes();
    let mut depth = 0_u32;
    let mut quote = None;
    let mut escaped = false;
    for (index, byte) in bytes.iter().copied().enumerate().skip(open) {
        if let Some(current_quote) = quote {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == current_quote {
                quote = None;
            }
            continue;
        }
        if matches!(byte, b'\'' | b'"' | b'`') {
            quote = Some(byte);
        } else if byte == left {
            depth += 1;
        } else if byte == right {
            depth = depth.checked_sub(1)?;
            if depth == 0 {
                return Some(index);
            }
        }
    }
    None
}

fn top_level_at(code: &str, end: usize) -> bool {
    let bytes = code.as_bytes();
    let mut delimiters = Vec::new();
    let mut quote = None;
    let mut escaped = false;
    let mut index = 0;
    while index < end {
        let byte = bytes[index];
        if let Some(current_quote) = quote {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == current_quote {
                quote = None;
            }
            index += 1;
            continue;
        }
        if byte == b'/' && bytes.get(index + 1) == Some(&b'/') {
            index += 2;
            while index < end && !matches!(bytes[index], b'\r' | b'\n') {
                index += 1;
            }
            if index == end {
                return false;
            }
            continue;
        }
        if byte == b'/' && bytes.get(index + 1) == Some(&b'*') {
            index += 2;
            while index + 1 < end && !(bytes[index] == b'*' && bytes[index + 1] == b'/') {
                index += 1;
            }
            if index + 1 >= end {
                return false;
            }
            index = (index + 2).min(end);
            continue;
        }
        match byte {
            b'\'' | b'"' | b'`' => quote = Some(byte),
            b'(' | b'{' | b'[' => delimiters.push(byte),
            b')' | b'}' | b']' => {
                delimiters.pop();
            }
            _ => {}
        }
        index += 1;
    }
    delimiters.is_empty() && quote.is_none()
}

struct ConditionParser<'a> {
    input: &'a str,
    position: usize,
    aliases: &'a BTreeMap<String, SourceVariable>,
    used_aliases: BTreeSet<String>,
}

impl<'a> ConditionParser<'a> {
    fn parse(
        input: &'a str,
        aliases: &'a BTreeMap<String, SourceVariable>,
    ) -> Option<(SourceCondition, BTreeSet<String>)> {
        let mut parser = Self {
            input,
            position: 0,
            aliases,
            used_aliases: BTreeSet::new(),
        };
        let condition = parser.parse_any()?;
        parser.skip_whitespace();
        (parser.position == input.len()).then_some((condition, parser.used_aliases))
    }

    fn parse_any(&mut self) -> Option<SourceCondition> {
        let mut conditions = vec![self.parse_all()?];
        while self.consume("||") {
            conditions.push(self.parse_all()?);
        }
        Some(if conditions.len() == 1 {
            conditions.pop().expect("one condition")
        } else {
            SourceCondition::Any(conditions)
        })
    }

    fn parse_all(&mut self) -> Option<SourceCondition> {
        let mut conditions = vec![self.parse_not()?];
        while self.consume("&&") {
            conditions.push(self.parse_not()?);
        }
        Some(if conditions.len() == 1 {
            conditions.pop().expect("one condition")
        } else {
            SourceCondition::All(conditions)
        })
    }

    fn parse_not(&mut self) -> Option<SourceCondition> {
        if self.consume("!") {
            Some(SourceCondition::Not(Box::new(self.parse_not()?)))
        } else {
            self.parse_primary()
        }
    }

    fn parse_primary(&mut self) -> Option<SourceCondition> {
        if self.consume("(") {
            let condition = self.parse_any()?;
            return self.consume(")").then_some(condition);
        }
        let identifier = self.identifier()?;
        match identifier.as_str() {
            "true" => Some(SourceCondition::Literal(true)),
            "false" => Some(SourceCondition::Literal(false)),
            "bru.getEnvVar" | "bru.getVar" => {
                if !self.consume("(") {
                    return None;
                }
                let name = self.quoted_string()?;
                if !self.consume(")") {
                    return None;
                }
                Some(SourceCondition::Variable(SourceVariable {
                    kind: if identifier == "bru.getVar" {
                        SourceVariableKind::Runtime
                    } else {
                        SourceVariableKind::Environment
                    },
                    name,
                }))
            }
            name => {
                let variable = self.aliases.get(name)?.clone();
                self.used_aliases.insert(name.to_string());
                Some(SourceCondition::Variable(variable))
            }
        }
    }

    fn identifier(&mut self) -> Option<String> {
        self.skip_whitespace();
        let start = self.position;
        while self
            .input
            .as_bytes()
            .get(self.position)
            .is_some_and(|byte| byte.is_ascii_alphanumeric() || matches!(*byte, b'_' | b'$' | b'.'))
        {
            self.position += 1;
        }
        (self.position > start).then(|| self.input[start..self.position].to_string())
    }

    fn quoted_string(&mut self) -> Option<String> {
        self.skip_whitespace();
        let quote = *self.input.as_bytes().get(self.position)?;
        if !matches!(quote, b'\'' | b'"') {
            return None;
        }
        self.position += 1;
        let start = self.position;
        while let Some(byte) = self.input.as_bytes().get(self.position) {
            if *byte == quote {
                let value = self.input[start..self.position].to_string();
                self.position += 1;
                return Some(value);
            }
            if *byte == b'\\' {
                return None;
            }
            self.position += 1;
        }
        None
    }

    fn consume(&mut self, token: &str) -> bool {
        self.skip_whitespace();
        if self.input[self.position..].starts_with(token) {
            self.position += token.len();
            true
        } else {
            false
        }
    }

    fn skip_whitespace(&mut self) {
        while self
            .input
            .as_bytes()
            .get(self.position)
            .is_some_and(u8::is_ascii_whitespace)
        {
            self.position += 1;
        }
    }
}

fn lower_source_conditions(
    conditions: &[SourceCondition],
    secret_names: &BTreeSet<String>,
) -> ExecutionCondition {
    let mut conditions = conditions
        .iter()
        .map(|condition| lower_source_condition(condition, secret_names))
        .collect::<Vec<_>>();
    if conditions.len() == 1 {
        conditions.pop().expect("one condition")
    } else {
        ExecutionCondition::Any(AnyCondition { any: conditions })
    }
}

fn lower_source_condition(
    condition: &SourceCondition,
    secret_names: &BTreeSet<String>,
) -> ExecutionCondition {
    match condition {
        SourceCondition::Literal(literal) => {
            ExecutionCondition::Literal(LiteralCondition { literal: *literal })
        }
        SourceCondition::Variable(variable) => {
            let scope = match variable.kind {
                SourceVariableKind::Runtime => ExecutionVariableScope::Runtime,
                SourceVariableKind::Environment
                    if secret_names.contains(&variable.name)
                        || is_sensitive_name(&variable.name) =>
                {
                    ExecutionVariableScope::Secret
                }
                SourceVariableKind::Environment => ExecutionVariableScope::Env,
            };
            ExecutionCondition::Variable(VariableCondition {
                var: ExecutionVariable {
                    scope,
                    name: variable.name.clone(),
                },
            })
        }
        SourceCondition::Not(condition) => ExecutionCondition::Not(NotCondition {
            not: Box::new(lower_source_condition(condition, secret_names)),
        }),
        SourceCondition::All(conditions) => ExecutionCondition::All(AllCondition {
            all: conditions
                .iter()
                .map(|condition| lower_source_condition(condition, secret_names))
                .collect(),
        }),
        SourceCondition::Any(conditions) => ExecutionCondition::Any(AnyCondition {
            any: conditions
                .iter()
                .map(|condition| lower_source_condition(condition, secret_names))
                .collect(),
        }),
    }
}

fn append_skip_conditions(
    execution: &mut ExecutionPolicy,
    conditions: Vec<SourceCondition>,
    secret_names: &BTreeSet<String>,
) {
    if conditions.is_empty() {
        return;
    }
    let condition = lower_source_conditions(&conditions, secret_names);
    execution.skip = Some(match execution.skip.take() {
        Some(existing) => SkipGuard {
            when: ExecutionCondition::Any(AnyCondition {
                any: vec![existing.when, condition],
            }),
            reason: existing.reason,
        },
        None => SkipGuard {
            when: condition,
            reason: "Skipped by imported Bruno before-request guard".to_string(),
        },
    });
}

#[derive(Default)]
struct ScriptCounts {
    native: usize,
    custom: usize,
    blocked: usize,
    direct_contracts: usize,
    transformed_contracts: usize,
    blocked_contracts: usize,
    custom_before: BTreeSet<PathBuf>,
    blocked_before: BTreeSet<PathBuf>,
    custom_after: BTreeSet<PathBuf>,
    blocked_after: BTreeSet<PathBuf>,
    recognized_auth: BTreeSet<PathBuf>,
    blocked_auth: BTreeSet<PathBuf>,
    quarantine: BTreeMap<String, ImportQuarantineEntry>,
}

#[allow(clippy::too_many_arguments)]
fn lower_scripts(
    root: &Path,
    source: &RequestSource,
    yaml: &Yaml,
    path: &str,
    assertions: &mut AssertionDocument,
    hooks: &mut HookDocument,
    runtime_writes: &mut BTreeSet<String>,
    execution: &mut ExecutionPolicy,
    secret_names: &BTreeSet<String>,
    lowered_crid_script: Option<&str>,
    assets: &mut Vec<GeneratedFile>,
    diagnostics: &mut BTreeMap<String, BTreeMap<String, Vec<String>>>,
) -> ScriptCounts {
    let status = Regex::new(r"expect\s*\(\s*res\.status\s*\)\s*\.to\.equal\s*\(\s*(\d{3})\s*\)")
        .expect("status regex");
    let extraction = Regex::new(
        r#"bru\.setVar\s*\(\s*["']([^"']+)["']\s*,\s*res\.body\.([A-Za-z_$][A-Za-z0-9_$.]*)\s*\)"#,
    )
    .expect("extraction regex");
    let mut scripts = source.inherited_scripts.clone();
    scripts.extend(
        value_at(yaml, &["runtime", "scripts"])
            .and_then(Yaml::as_sequence)
            .into_iter()
            .flatten()
            .enumerate()
            .filter(|(_, script)| is_enabled(script))
            .map(|(index, script)| SourceScript {
                source_relative: source.source_relative.clone(),
                source_path: path.to_string(),
                source_index: index + 1,
                kind: text_at(script, &["type"]).unwrap_or_else(|| "unknown".to_string()),
                code: text_at(script, &["code"]).unwrap_or_default(),
                inherited: false,
            }),
    );
    let runtime_target = Regex::new(r#"bru\.(?:setVar|setEnvVar)\s*\(\s*["']([^"']+)["']"#)
        .expect("Bruno runtime target regex");
    let mut counts = ScriptCounts::default();
    for script in scripts {
        let kind = script.kind.as_str();
        let code = script.code.as_str();
        if lowered_crid_script == Some(code) {
            continue;
        }
        if kind == "before-request" {
            if recognized_auth_network_script(code)
                || recognized_data_manager_admin_discovery_script(source, code)
            {
                counts
                    .recognized_auth
                    .insert(bruno_script_asset_path(&script, "auth", "migrated"));
                continue;
            }
            let analysis = analyze_before_request(code);
            if analysis.complete {
                append_skip_conditions(execution, analysis.skip, secret_names);
                let delay = analysis.delays.into_iter().sum::<u64>();
                if delay > 0 {
                    execution.delay_before_ms = Some(
                        execution
                            .delay_before_ms
                            .unwrap_or_default()
                            .saturating_add(delay),
                    );
                }
                continue;
            }
            if code.trim().is_empty() {
                continue;
            }
            for capture in runtime_target.captures_iter(code) {
                runtime_writes.insert(capture[1].to_string());
            }
            let asset_relative = bruno_script_asset_path(&script, "hooks", "before");
            push_bruno_compatibility_asset(assets);
            let blocked_reason = unsafe_bruno_script_reason(code).or_else(|| {
                code.contains("bru.runner").then(|| {
                    "dynamic bru.runner behavior is unsupported outside exact native skip lowering"
                        .to_string()
                })
            });
            let asset_source = match &blocked_reason {
                Some(reason) => blocked_bruno_hook_asset(&script.source_path, reason),
                None => bruno_before_asset(code),
            };
            hooks.push(PipelineEntry {
                phase: PipelinePhase::BeforeRequest,
                uses: asset_reference(source, &asset_relative),
                with: bruno_compatibility_input(code),
                enabled: true,
            });
            assets.push(GeneratedFile {
                relative: asset_relative.clone(),
                bytes: asset_source.into_bytes(),
            });
            if let Some(reason) = blocked_reason {
                counts.blocked_before.insert(asset_relative.clone());
                let entry = open_collection_quarantine_entry(
                    &script,
                    QuarantineCategory::BeforeRequest,
                    &reason,
                );
                counts.quarantine.entry(entry.id.clone()).or_insert(entry);
                if is_auth_network_script(code) {
                    counts.blocked_auth.insert(asset_relative.clone());
                }
                push_diagnostic(
                    diagnostics,
                    &script.source_path,
                    "before-request-scripts",
                    &format!("before-request script was blocked: {reason}"),
                );
            } else {
                counts.custom_before.insert(asset_relative.clone());
            }
            continue;
        }
        if kind == "tests" {
            let ContractLowering {
                remaining,
                assertions: contract_assertions,
                direct,
                transformed,
                blocked,
            } = lower_contract_tests(root, source, &script, code, assets, diagnostics);
            for assertion in contract_assertions {
                assertions.push(assertion);
            }
            counts.direct_contracts += direct;
            counts.transformed_contracts += transformed;
            counts.blocked_contracts += blocked;
            let code = remaining.as_str();
            if (direct + transformed + blocked) > 0 && only_whitespace_and_comments(code) {
                counts.native += 1;
                continue;
            }
            if let Some(native) = lower_named_native_tests(code) {
                for assertion in native {
                    assertions.push(assertion);
                }
                counts.native += 1;
                continue;
            }
            let mut converted = 0;
            if !code.contains("test") {
                for capture in status.captures_iter(code) {
                    let expected = capture[1].parse::<u16>().unwrap_or_default();
                    assertions.push(AssertionEntry {
                        uses: "builtin:assert-status@1".to_string(),
                        with: Map::from_iter([("expected".to_string(), Value::from(expected))]),
                        enabled: true,
                    });
                    converted += 1;
                }
                if script_is_only_simple_patterns(code, kind, converted) {
                    counts.native += 1;
                    continue;
                }
            }

            let asset_relative = bruno_script_asset_path(&script, "assertions", "tests");
            push_bruno_compatibility_asset(assets);
            let contract_reason = code.contains("validateContract").then(|| {
                "unrecognized validateContract response transform cannot be lowered safely"
                    .to_string()
            });
            let (asset_source, blocked_reason) =
                match contract_reason.or_else(|| unsafe_test_script_reason(code)) {
                    Some(reason) => (
                        blocked_bruno_assertion_asset(&script.source_path, &reason),
                        Some(reason),
                    ),
                    None => (bruno_assertion_asset(code), None),
                };
            assertions.assertions.push(AssertionEntry {
                uses: asset_reference(source, &asset_relative),
                with: bruno_tests_compatibility_input(code),
                enabled: true,
            });
            assets.push(GeneratedFile {
                relative: asset_relative.clone(),
                bytes: asset_source.into_bytes(),
            });
            if let Some(reason) = blocked_reason {
                counts.blocked += 1;
                let entry = open_collection_quarantine_entry(
                    &script,
                    QuarantineCategory::Assertion,
                    &reason,
                );
                counts.quarantine.entry(entry.id.clone()).or_insert(entry);
                if reason.contains("validateContract") {
                    counts.blocked_contracts += 1;
                }
                push_diagnostic(
                    diagnostics,
                    &script.source_path,
                    "assertion-scripts",
                    &format!(
                        "tests script was blocked and replaced by a failing assertion: {reason}"
                    ),
                );
            } else {
                counts.custom += 1;
            }
            continue;
        }
        if kind == "after-response" {
            if lowered_crid_script.is_some() && is_native_crid_cleanup(code) {
                continue;
            }
            let mut native_assertions = Vec::new();
            let mut native_hooks = Vec::new();
            let mut converted = 0;
            for capture in status.captures_iter(code) {
                let expected = capture[1].parse::<u16>().unwrap_or_default();
                native_assertions.push(PipelineEntry {
                    phase: PipelinePhase::AfterResponse,
                    uses: "builtin:assert-status@1".to_string(),
                    with: Map::from_iter([("expected".to_string(), Value::from(expected))]),
                    enabled: true,
                });
                converted += 1;
            }
            for capture in extraction.captures_iter(code) {
                let target = capture[1].to_string();
                let json_path = format!("$.{}", &capture[2]);
                native_hooks.push(PipelineEntry {
                    phase: PipelinePhase::AfterResponse,
                    uses: "builtin:extract-json-path@1".to_string(),
                    with: Map::from_iter([
                        ("path".to_string(), Value::String(json_path)),
                        ("target".to_string(), Value::String(target.clone())),
                    ]),
                    enabled: true,
                });
                runtime_writes.insert(target);
                converted += 1;
            }
            if script_is_only_simple_patterns(code, kind, converted) {
                for assertion in native_assertions {
                    hooks.push(assertion);
                }
                for hook in native_hooks {
                    hooks.push(hook);
                }
                continue;
            }
            if code.trim().is_empty() {
                continue;
            }
            for capture in runtime_target.captures_iter(code) {
                runtime_writes.insert(capture[1].to_string());
            }
            let asset_relative = bruno_script_asset_path(&script, "extractors", "after");
            push_bruno_compatibility_asset(assets);
            let blocked_reason = unsafe_bruno_script_reason(code);
            let asset_source = match &blocked_reason {
                Some(reason) => blocked_bruno_after_asset(&script.source_path, reason),
                None => bruno_after_asset(code),
            };
            hooks.push(PipelineEntry {
                phase: PipelinePhase::AfterResponse,
                uses: asset_reference(source, &asset_relative),
                with: bruno_compatibility_input(code),
                enabled: true,
            });
            assets.push(GeneratedFile {
                relative: asset_relative.clone(),
                bytes: asset_source.into_bytes(),
            });
            if let Some(reason) = blocked_reason {
                counts.blocked_after.insert(asset_relative.clone());
                let entry = open_collection_quarantine_entry(
                    &script,
                    QuarantineCategory::AfterResponse,
                    &reason,
                );
                counts.quarantine.entry(entry.id.clone()).or_insert(entry);
                push_diagnostic(
                    diagnostics,
                    &script.source_path,
                    "after-response-scripts",
                    &format!("after-response script was blocked: {reason}"),
                );
            } else {
                counts.custom_after.insert(asset_relative.clone());
            }
            continue;
        }
        if !code.trim().is_empty() {
            push_diagnostic(
                diagnostics,
                &script.source_path,
                "scripts",
                &format!("unsupported {kind} script was not materialized"),
            );
        }
    }
    counts
}

fn open_collection_quarantine_entry(
    script: &SourceScript,
    category: QuarantineCategory,
    reason: &str,
) -> ImportQuarantineEntry {
    let category = if script.inherited {
        match category {
            QuarantineCategory::BeforeRequest => QuarantineCategory::BeforeEach,
            QuarantineCategory::Assertion | QuarantineCategory::AfterResponse => {
                QuarantineCategory::AfterEach
            }
            category => category,
        }
    } else {
        category
    };
    ImportQuarantineEntry::new(
        ImportSourceFormat::BrunoOpenCollection,
        category,
        QuarantineDisposition::Blocked,
        &script.source_path,
        script.source_index,
        &script.code,
        reason,
    )
}

fn is_native_crid_cleanup(code: &str) -> bool {
    Regex::new(
        r#"(?s)^\s*const\s+[A-Za-z_$][A-Za-z0-9_$]*\s*=\s*bru\.getVar\s*\(\s*["'][^"']+["']\s*\)\s*;\s*if\s*\([^)]*\)\s*\{\s*try\s*\{\s*require\s*\(\s*["']fs["']\s*\)\.unlinkSync\s*\([^)]*\)\s*;?\s*\}\s*catch\s*\([^)]*\)\s*\{\s*\}\s*\}\s*$"#,
    )
    .expect("CRID cleanup regex")
    .is_match(code)
}

const CONTRACT_BUNDLE_SOURCE: &str = "shared/contracts/bp2.bundled.json";
const CONTRACT_BUNDLE_ASSET: &str = "assets/schemas/bruno/bp2.bundled.json";

#[derive(Default)]
struct ContractLowering {
    remaining: String,
    assertions: Vec<AssertionEntry>,
    direct: usize,
    transformed: usize,
    blocked: usize,
}

fn lower_contract_tests(
    root: &Path,
    source: &RequestSource,
    script: &SourceScript,
    code: &str,
    assets: &mut Vec<GeneratedFile>,
    diagnostics: &mut BTreeMap<String, BTreeMap<String, Vec<String>>>,
) -> ContractLowering {
    let helper = Regex::new(
        r#"(?m)^[ \t]*(?:const|let|var)\s*\{\s*validateContract\s*\}\s*=\s*require\s*\(\s*["']\./shared/contracts/validate\.js["']\s*\)\s*;?[ \t]*(?:\n|$)"#,
    )
    .expect("contract helper regex");
    if !helper.is_match(code) {
        return ContractLowering {
            remaining: code.to_string(),
            ..ContractLowering::default()
        };
    }

    let mut output = ContractLowering {
        remaining: helper.replace_all(code, "").into_owned(),
        ..ContractLowering::default()
    };
    let transformed = Regex::new(
        r#"(?s)test\s*\(\s*(?:\"((?:\\.|[^\"\\])*)\"|'((?:\\.|[^'\\])*)')\s*,\s*(?:function\s*\(\s*\)|\(\s*\)\s*=>)\s*\{\s*const\s+([A-Za-z_$][A-Za-z0-9_$]*)\s*=\s*Object\.assign\s*\(\s*\{\s*\}\s*,\s*res\.body\s*,\s*\{\s*data\s*:\s*\[\s*\]\s*\}\s*\)\s*;\s*validateContract\s*\(\s*["']([A-Za-z0-9_]+)["']\s*,\s*([A-Za-z_$][A-Za-z0-9_$]*)\s*,\s*expect\s*\)\s*;?\s*\}\s*\)\s*;?"#,
    )
    .expect("transformed contract test regex");
    let (remaining, matches) = extract_contract_matches(&output.remaining, &transformed, true);
    output.remaining = remaining;
    for contract in matches {
        push_contract_assertion(
            root,
            source,
            script,
            contract,
            true,
            &mut output,
            assets,
            diagnostics,
        );
    }

    let direct = Regex::new(
        r#"(?s)test\s*\(\s*(?:\"((?:\\.|[^\"\\])*)\"|'((?:\\.|[^'\\])*)')\s*,\s*(?:function\s*\(\s*\)|\(\s*\)\s*=>)\s*\{\s*validateContract\s*\(\s*["']([A-Za-z0-9_]+)["']\s*,\s*res\.body\s*,\s*expect\s*\)\s*;?\s*\}\s*\)\s*;?"#,
    )
    .expect("direct contract test regex");
    let (remaining, matches) = extract_contract_matches(&output.remaining, &direct, false);
    output.remaining = remaining;
    for contract in matches {
        push_contract_assertion(
            root,
            source,
            script,
            contract,
            false,
            &mut output,
            assets,
            diagnostics,
        );
    }

    let unsupported = Regex::new(
        r#"(?s)test\s*\(\s*(?:\"((?:\\.|[^\"\\])*)\"|'((?:\\.|[^'\\])*)')\s*,\s*(?:function\s*\(\s*\)|\(\s*\)\s*=>)\s*\{([^{}]*validateContract\s*\([^{}]*\)[^{}]*)\}\s*\)\s*;?"#,
    )
    .expect("unsupported contract test regex");
    let mut remaining = String::new();
    let mut end = 0;
    for captures in unsupported.captures_iter(&output.remaining) {
        let whole = captures.get(0).expect("whole unsupported contract match");
        remaining.push_str(&output.remaining[end..whole.start()]);
        end = whole.end();
        let name =
            contract_test_name(&captures).unwrap_or_else(|| "Blocked contract test".to_string());
        output.assertions.push(blocked_contract_assertion(&name));
        output.blocked += 1;
        push_diagnostic(
            diagnostics,
            &script.source_path,
            "contract-validation",
            &format!(
                "contract test {name:?} was replaced by a failing assertion: unsupported validateContract response expression"
            ),
        );
    }
    remaining.push_str(&output.remaining[end..]);
    output.remaining = remaining;
    if output.direct + output.transformed + output.blocked == 0
        && !output.remaining.contains("validateContract")
    {
        output.remaining = code.to_string();
    }
    output
}

struct ContractMatch {
    name: String,
    schema_key: String,
}

fn extract_contract_matches(
    code: &str,
    pattern: &Regex,
    transformed: bool,
) -> (String, Vec<ContractMatch>) {
    let mut remaining = String::new();
    let mut matches = Vec::new();
    let mut end = 0;
    for captures in pattern.captures_iter(code) {
        if transformed
            && captures.get(3).map(|value| value.as_str())
                != captures.get(5).map(|value| value.as_str())
        {
            continue;
        }
        let whole = captures.get(0).expect("whole contract test match");
        let Some(name) = contract_test_name(&captures) else {
            continue;
        };
        let key_index = if transformed { 4 } else { 3 };
        let Some(schema_key) = captures
            .get(key_index)
            .map(|value| value.as_str().to_string())
        else {
            continue;
        };
        remaining.push_str(&code[end..whole.start()]);
        end = whole.end();
        matches.push(ContractMatch { name, schema_key });
    }
    remaining.push_str(&code[end..]);
    (remaining, matches)
}

fn contract_test_name(captures: &regex::Captures<'_>) -> Option<String> {
    if let Some(name) = captures.get(1) {
        parse_js_string_contents(name.as_str(), '"')
    } else {
        parse_js_string_contents(captures.get(2)?.as_str(), '\'')
    }
}

#[allow(clippy::too_many_arguments)]
fn push_contract_assertion(
    root: &Path,
    source: &RequestSource,
    script: &SourceScript,
    contract: ContractMatch,
    transformed: bool,
    output: &mut ContractLowering,
    assets: &mut Vec<GeneratedFile>,
    diagnostics: &mut BTreeMap<String, BTreeMap<String, Vec<String>>>,
) {
    match native_contract_assertion(root, source, &contract, transformed) {
        Ok((assertion, asset)) => {
            output.assertions.push(assertion);
            assets.push(asset);
            if transformed {
                output.transformed += 1;
            } else {
                output.direct += 1;
            }
            push_diagnostic(
                diagnostics,
                CONTRACT_BUNDLE_SOURCE,
                "contract-validation",
                "native Rust validation always enforces supported date, date-time, email, and idn-email formats; source AJV skips all format checks when optional ajv-formats is unavailable, and the two implementations may differ on RFC edge cases; unknown formats such as int64 remain annotations",
            );
        }
        Err(reason) => {
            output
                .assertions
                .push(blocked_contract_assertion(&contract.name));
            output.blocked += 1;
            push_diagnostic(
                diagnostics,
                &script.source_path,
                "contract-validation",
                &format!(
                    "contract test {:?} was replaced by a failing assertion: {reason}",
                    contract.name
                ),
            );
        }
    }
}

fn native_contract_assertion(
    root: &Path,
    source: &RequestSource,
    contract: &ContractMatch,
    transformed: bool,
) -> Result<(AssertionEntry, GeneratedFile), String> {
    let bytes = read_contract_bundle(root)?;
    let bundle: Value = serde_json::from_slice(&bytes).map_err(|error| {
        format!(
            "invalid {}: {error}",
            display_path(Path::new(CONTRACT_BUNDLE_SOURCE))
        )
    })?;
    if !bundle
        .get("$defs")
        .and_then(Value::as_object)
        .is_some_and(|definitions| definitions.contains_key(&contract.schema_key))
    {
        return Err(format!(
            "unknown contract schema key {:?} in {}",
            contract.schema_key, CONTRACT_BUNDLE_SOURCE
        ));
    }
    let asset_path = PathBuf::from(CONTRACT_BUNDLE_ASSET);
    let mut with = Map::from_iter([
        (
            "schemaRef".to_string(),
            Value::String(asset_reference(source, &asset_path)),
        ),
        (
            "definition".to_string(),
            Value::String(contract.schema_key.clone()),
        ),
        ("name".to_string(), Value::String(contract.name.clone())),
    ]);
    if transformed {
        with.insert(
            "instancePatch".to_string(),
            serde_json::json!([{"op": "add", "path": "/data", "value": []}]),
        );
    }
    Ok((
        AssertionEntry {
            uses: "builtin:assert-schema@1".to_string(),
            with,
            enabled: true,
        },
        GeneratedFile {
            relative: asset_path,
            bytes,
        },
    ))
}

fn read_contract_bundle(root: &Path) -> Result<Vec<u8>, String> {
    let source_path = root.join(CONTRACT_BUNDLE_SOURCE);
    let canonical_root = root
        .canonicalize()
        .map_err(|error| format!("cannot resolve Bruno source root: {error}"))?;
    let canonical_source = source_path
        .canonicalize()
        .map_err(|error| format!("cannot resolve {CONTRACT_BUNDLE_SOURCE}: {error}"))?;
    if !canonical_source.starts_with(&canonical_root) || !canonical_source.is_file() {
        return Err(format!(
            "{CONTRACT_BUNDLE_SOURCE} is not a regular file contained in the Bruno source"
        ));
    }
    std::fs::read(&canonical_source)
        .map_err(|error| format!("cannot read {CONTRACT_BUNDLE_SOURCE}: {error}"))
}

fn blocked_contract_assertion(name: &str) -> AssertionEntry {
    AssertionEntry {
        uses: "builtin:assert-schema@1".to_string(),
        with: Map::from_iter([
            ("schema".to_string(), Value::Bool(false)),
            ("name".to_string(), Value::String(name.to_string())),
        ]),
        enabled: true,
    }
}

fn lower_named_native_tests(code: &str) -> Option<Vec<AssertionEntry>> {
    let tests = Regex::new(
        r#"(?s)test\s*\(\s*(?:\"((?:\\.|[^\"\\])*)\"|'((?:\\.|[^'\\])*)')\s*,\s*(?:function\s*\(\s*\)|\(\s*\)\s*=>)\s*\{\s*(.*?)\s*\}\s*\)\s*;?"#,
    )
    .expect("named Bruno test regex");
    let mut lowered = Vec::new();
    let mut unmatched = String::new();
    let mut end = 0;
    for captures in tests.captures_iter(code) {
        let whole = captures.get(0).expect("whole named test match");
        unmatched.push_str(&code[end..whole.start()]);
        end = whole.end();
        let name = if let Some(name) = captures.get(1) {
            parse_js_string_contents(name.as_str(), '"')?
        } else {
            parse_js_string_contents(captures.get(2)?.as_str(), '\'')?
        };
        lowered.push(lower_native_expectation(&name, captures.get(3)?.as_str())?);
    }
    unmatched.push_str(&code[end..]);
    if lowered.is_empty() || !only_whitespace_and_comments(&unmatched) {
        None
    } else {
        Some(lowered)
    }
}

fn lower_native_expectation(name: &str, body: &str) -> Option<AssertionEntry> {
    let status = Regex::new(
        r"(?s)^\s*expect\s*\(\s*res\.status\s*\)\s*(?:\.(?:to|be|deep))*\.(?:equal|equals|eq)\s*\(\s*(\d{3})\s*\)\s*;?\s*$",
    )
    .expect("native named status regex");
    if let Some(captures) = status.captures(body) {
        return Some(AssertionEntry {
            uses: "builtin:assert-status@1".to_string(),
            with: Map::from_iter([
                (
                    "expected".to_string(),
                    Value::from(captures[1].parse::<u16>().ok()?),
                ),
                ("name".to_string(), Value::String(name.to_string())),
            ]),
            enabled: true,
        });
    }

    let body_equal = Regex::new(
        r"(?s)^\s*expect\s*\(\s*res\.body((?:\.[A-Za-z_$][A-Za-z0-9_$]*|\[\d+\])*)\s*\)\s*((?:\.(?:to|be|deep))*)\.(?:equal|equals|eq)\s*\(\s*(.+)\s*\)\s*;?\s*$",
    )
    .expect("native named body equality regex");
    let captures = body_equal.captures(body)?;
    let expected: Value = serde_json::from_str(captures.get(3)?.as_str()).ok()?;
    if matches!(expected, Value::Array(_) | Value::Object(_))
        && !captures.get(2)?.as_str().contains(".deep")
    {
        return None;
    }
    let path = format!("${}", captures.get(1)?.as_str());
    Some(AssertionEntry {
        uses: "builtin:assert-json-path@1".to_string(),
        with: Map::from_iter([
            ("path".to_string(), Value::String(path)),
            ("operator".to_string(), Value::String("equals".to_string())),
            ("value".to_string(), expected),
            ("name".to_string(), Value::String(name.to_string())),
        ]),
        enabled: true,
    })
}

fn parse_js_string_contents(contents: &str, quote: char) -> Option<String> {
    if quote == '"' {
        return serde_json::from_str::<String>(&format!("\"{contents}\"")).ok();
    }
    let mut output = String::new();
    let mut chars = contents.chars();
    while let Some(character) = chars.next() {
        if character != '\\' {
            output.push(character);
            continue;
        }
        output.push(match chars.next()? {
            '\\' => '\\',
            '\'' => '\'',
            'n' => '\n',
            'r' => '\r',
            't' => '\t',
            _ => return None,
        });
    }
    Some(output)
}

fn only_whitespace_and_comments(code: &str) -> bool {
    Regex::new(r"(?s)^(?:\s|//[^\n]*(?:\n|$)|/\*.*?\*/)*$")
        .expect("JavaScript comments regex")
        .is_match(code)
}

fn bruno_script_asset_path(script: &SourceScript, category: &str, suffix: &str) -> PathBuf {
    let mut path = PathBuf::from("assets/bruno").join(category);
    if let Some(parent) = script.source_relative.parent() {
        path.push(parent);
    }
    path.push(format!(
        "{}.{suffix}-{}.js",
        file_stem(&script.source_relative),
        script.source_index,
    ));
    path
}

fn asset_reference(source: &RequestSource, asset: &Path) -> String {
    let request = PathBuf::from("requests").join(&source.output_relative);
    let depth = request
        .parent()
        .map(|parent| parent.components().count())
        .unwrap_or(0);
    format!("{}{}", "../".repeat(depth), display_path(asset))
}

fn bruno_compatibility_input(code: &str) -> Map<String, Value> {
    compatibility_input("bruno-scripts-v1", code)
}

fn bruno_tests_compatibility_input(code: &str) -> Map<String, Value> {
    compatibility_input("bruno-tests-v1", code)
}

fn compatibility_input(kind: &str, code: &str) -> Map<String, Value> {
    let mut input =
        Map::from_iter([("compatibility".to_string(), Value::String(kind.to_string()))]);
    let runtime_access = Regex::new(r#"bru\.getVar\s*\(\s*["']([^"']+)["']\s*\)"#)
        .expect("Bruno runtime access regex")
        .captures_iter(code)
        .map(|capture| capture[1].to_string())
        .filter(|name| crate::reqv1::vars::sensitive_name(name))
        .collect::<BTreeSet<_>>();
    if !runtime_access.is_empty() {
        input.insert(
            "runtimeAccess".to_string(),
            Value::Array(runtime_access.into_iter().map(Value::String).collect()),
        );
    }
    input
}

fn push_bruno_compatibility_asset(assets: &mut Vec<GeneratedFile>) {
    assets.push(GeneratedFile {
        relative: PathBuf::from("assets/bruno/compat.js"),
        bytes: BRUNO_COMPATIBILITY_ASSET.as_bytes().to_vec(),
    });
}

fn unsafe_test_script_reason(code: &str) -> Option<String> {
    unsafe_bruno_script_reason(code)
}

fn unsafe_bruno_script_reason(code: &str) -> Option<String> {
    if code.contains("req.setBody")
        && Regex::new(r#"\btype\s*:\s*["']file["']"#)
            .expect("Bruno file body regex")
            .is_match(code)
    {
        return Some(
            "file and multipart bodies are only allowed through exact native lowering".to_string(),
        );
    }
    if Regex::new(
        r#"[\"'](?:eval|Function|constructor|__proto__|prototype|require|process|Deno|Bun|fetch|XMLHttpRequest|WebSocket|EventSource|sendRequest|axios|fs|child_process)[\"']|function\s*\*"#,
    )
        .expect("dynamic JavaScript string regex")
        .is_match(code)
    {
        return Some(
            "computed access to blocked host, network, or dynamic-evaluation APIs is not allowed"
                .to_string(),
        );
    }
    let safe_buffer_decode = Regex::new(
        r#"Buffer\s*\.\s*from\s*\(\s*(?:[A-Za-z_$][A-Za-z0-9_$]*|[A-Za-z_$][A-Za-z0-9_$]*\s*\.\s*split\s*\(\s*[\"']\.[\"']\s*\)\s*\[\s*\d+\s*\])\s*,\s*[\"']base64[\"']\s*\)\s*\.\s*toString\s*\(\s*\)"#,
    )
    .expect("safe Bruno base64 Buffer regex");
    let code = safe_buffer_decode.replace_all(code, "");
    let executable = match javascript_executable_view(&code) {
        Ok(executable) => executable,
        Err(reason) => return Some(reason.to_string()),
    };
    for (prefix, allowed) in [
        ("req", &["body", "setBody", "setHeader"][..]),
        (
            "res",
            &[
                "status",
                "body",
                "headers",
                "getStatus",
                "getBody",
                "getHeader",
            ][..],
        ),
        (
            "bru",
            &[
                "getVar",
                "getEnvVar",
                "getCollectionVar",
                "setVar",
                "setEnvVar",
            ][..],
        ),
    ] {
        let member = Regex::new(&format!(
            r"\b{}\.([A-Za-z_$][A-Za-z0-9_$]*)",
            regex::escape(prefix)
        ))
        .expect("Bruno API member regex");
        let unsupported = member
            .captures_iter(&executable)
            .map(|capture| capture[1].to_string())
            .find(|name| !allowed.contains(&name.as_str()));
        if let Some(name) = unsupported {
            return Some(format!("unsupported imported Bruno API {prefix}.{name}"));
        }
    }
    let checks = [
        (
            r"\b(?:async|await|Promise|setTimeout|setInterval|queueMicrotask)\b|\.then\s*\(",
            "async JavaScript is unsupported in imported Bruno scripts",
        ),
        (
            r"\b(?:eval|Function|constructor|__proto__|prototype)\b",
            "dynamic evaluation or prototype-constructor access is not allowed",
        ),
        (
            r"\b(?:require|import|export|process|Deno|Bun)\b",
            "module or host-process access is not allowed",
        ),
        (
            r"\b(?:fetch|XMLHttpRequest|WebSocket|EventSource|sendRequest|axios|http|https|net|tls|dns)\b",
            "network access is not allowed",
        ),
        (
            r"\b(?:fs|FileReader|readFile|readFileSync|writeFile|writeFileSync|openSync|child_process)\b",
            "filesystem or process access is not allowed",
        ),
        (
            r"\b(?:Buffer|ArrayBuffer|SharedArrayBuffer|Uint8Array|DataView)\b",
            "byte and binary body access is not allowed",
        ),
    ];
    checks.iter().find_map(|(pattern, reason)| {
        Regex::new(pattern)
            .expect("unsafe JavaScript regex")
            .is_match(&executable)
            .then(|| (*reason).to_string())
    })
}

fn javascript_executable_view(code: &str) -> Result<String, &'static str> {
    let chars = code.chars().collect::<Vec<_>>();
    let mut output = chars.clone();
    let mut index = 0;
    while index < chars.len() {
        match chars[index] {
            '/' if chars.get(index + 1) == Some(&'/') => {
                output[index] = ' ';
                index += 1;
                while index < chars.len() && chars[index] != '\n' {
                    output[index] = ' ';
                    index += 1;
                }
            }
            '/' if chars.get(index + 1) == Some(&'*') => {
                output[index] = ' ';
                output[index + 1] = ' ';
                index += 2;
                while index + 1 < chars.len() && !(chars[index] == '*' && chars[index + 1] == '/') {
                    output[index] = ' ';
                    index += 1;
                }
                if index + 1 >= chars.len() {
                    return Err("unterminated block comment");
                }
                output[index] = ' ';
                output[index + 1] = ' ';
                index += 2;
            }
            quote @ ('\'' | '"') => {
                output[index] = ' ';
                index += 1;
                while index < chars.len() {
                    output[index] = ' ';
                    if chars[index] == '\\' {
                        index += 1;
                        if index < chars.len() {
                            output[index] = ' ';
                            index += 1;
                        }
                    } else if chars[index] == quote {
                        index += 1;
                        break;
                    } else {
                        index += 1;
                    }
                }
            }
            '`' => {
                output[index] = ' ';
                index += 1;
                while index < chars.len() && chars[index] != '`' {
                    if chars[index] == '$' && chars.get(index + 1) == Some(&'{') {
                        return Err("dynamic template expressions are unsupported");
                    }
                    output[index] = ' ';
                    if chars[index] == '\\' {
                        index += 1;
                        if index < chars.len() {
                            output[index] = ' ';
                        }
                    }
                    index += 1;
                }
                if index >= chars.len() {
                    return Err("unterminated template literal");
                }
                output[index] = ' ';
                index += 1;
            }
            _ => index += 1,
        }
    }
    Ok(output.into_iter().collect())
}

fn blocked_bruno_assertion_asset(path: &str, reason: &str) -> String {
    let message = format!("Blocked Bruno tests script in {path}: {reason}");
    format!(
        "\"use strict\";\nfunction run(ctx, input) {{\n  return [{{ passed: false, message: {}, path: {} }}];\n}}\n",
        serde_json::to_string(&message).expect("blocked assertion message JSON"),
        serde_json::to_string(path).expect("blocked assertion path JSON"),
    )
}

fn blocked_bruno_hook_asset(path: &str, reason: &str) -> String {
    let message = format!("Blocked Bruno before-request script in {path}: {reason}");
    format!(
        "\"use strict\";\nfunction run(ctx, input) {{\n  throw new Error({});\n}}\n",
        serde_json::to_string(&message).expect("blocked hook message JSON"),
    )
}

fn blocked_bruno_after_asset(path: &str, reason: &str) -> String {
    let message = format!("Blocked Bruno after-response script in {path}: {reason}");
    format!(
        "\"use strict\";\nfunction run(ctx, input) {{\n  return [{{ passed: false, message: {}, path: {} }}];\n}}\n",
        serde_json::to_string(&message).expect("blocked after message JSON"),
        serde_json::to_string(path).expect("blocked after path JSON"),
    )
}

fn bruno_assertion_asset(source: &str) -> String {
    let mut asset = BRUNO_ASSERTION_PREFIX.to_string();
    asset.push_str(source);
    if !source.ends_with('\n') {
        asset.push('\n');
    }
    asset.push_str(BRUNO_ASSERTION_SUFFIX);
    asset
}

fn bruno_before_asset(source: &str) -> String {
    bruno_script_asset(BRUNO_BEFORE_PREFIX, source, BRUNO_SCRIPT_SUFFIX)
}

fn bruno_after_asset(source: &str) -> String {
    bruno_script_asset(BRUNO_AFTER_PREFIX, source, BRUNO_SCRIPT_SUFFIX)
}

fn bruno_script_asset(prefix: &str, source: &str, suffix: &str) -> String {
    let mut asset = prefix.to_string();
    asset.push_str(source);
    if !source.ends_with('\n') {
        asset.push('\n');
    }
    asset.push_str(suffix);
    asset
}

const BRUNO_ASSERTION_PREFIX: &str = r#""use strict";
function run(ctx, input) {
  return __brunoRun(ctx, function (test, expect, res, req, bru) {
"#;

const BRUNO_ASSERTION_SUFFIX: &str = r#"  });
}
"#;

const BRUNO_BEFORE_PREFIX: &str = r#""use strict";
function run(ctx, input) {
  return __brunoBefore(ctx, function (res, req, bru) {
"#;

const BRUNO_AFTER_PREFIX: &str = r#""use strict";
function run(ctx, input) {
  return __brunoAfter(ctx, function (res, req, bru) {
"#;

const BRUNO_SCRIPT_SUFFIX: &str = r#"  });
}
"#;

const BRUNO_COMPATIBILITY_ASSET: &str = r#""use strict";
function run(ctx, input) {
  return [{ passed: false, message: "Bruno compatibility support asset is not a standalone assertion" }];
}
function __brunoRun(ctx, source) {
  var results = [];
  function kind(value) {
    if (value === null) return "null";
    if (Array.isArray(value)) return "array";
    return typeof value;
  }
  function same(left, right) {
    if (left === right) return true;
    if (!left || !right || typeof left !== "object" || typeof right !== "object") return false;
    if (Array.isArray(left) !== Array.isArray(right)) return false;
    var leftKeys = Object.keys(left).sort();
    var rightKeys = Object.keys(right).sort();
    if (leftKeys.length !== rightKeys.length) return false;
    for (var i = 0; i < leftKeys.length; i++) {
      if (leftKeys[i] !== rightKeys[i] || !same(left[leftKeys[i]], right[rightKeys[i]])) return false;
    }
    return true;
  }
  function shown(value) {
    if (value === undefined) return "undefined";
    try { return JSON.stringify(value); } catch (_) { return String(value); }
  }
  function failure(actual, expected, phrase, negate) {
    var error = new Error("expected " + shown(actual) + (negate ? " not " : " ") + phrase + " " + shown(expected));
    error.actual = actual;
    error.expected = expected;
    return error;
  }
  function verify(state, passed, expected, phrase, actual) {
    var value = arguments.length > 4 ? actual : state.actual;
    if (state.negate ? passed : !passed) throw failure(value, expected, phrase, state.negate);
  }
  function subset(actual, expected, deep) {
    if (typeof actual === "string") return actual.indexOf(String(expected)) !== -1;
    if (Array.isArray(actual)) return actual.some(function (item) { return deep ? same(item, expected) : item === expected; });
    if (actual && expected && typeof actual === "object" && typeof expected === "object") {
      return Object.keys(expected).every(function (key) { return key in actual && (deep ? same(actual[key], expected[key]) : actual[key] === expected[key]); });
    }
    return false;
  }
  function nestedValue(actual, path) {
    var parts = String(path).replace(/\[(?:"([^"]+)"|'([^']+)'|(\d+))\]/g, function (_, a, b, c) {
      return "." + (a || b || c);
    }).split(".").filter(Boolean);
    var value = actual;
    for (var i = 0; i < parts.length; i++) {
      if (value == null || !Object.prototype.hasOwnProperty.call(Object(value), parts[i])) return { found: false };
      value = value[parts[i]];
    }
    return { found: true, value: value };
  }
  function chain(state) {
    var target = function () {};
    return new Proxy(target, {
      get: function (_, property) {
        if (["to", "be", "been", "is", "that", "which", "and", "has", "have", "with", "at", "of", "same"].indexOf(property) !== -1) return chain(state);
        if (property === "not") return chain(Object.assign({}, state, { negate: !state.negate }));
        if (property === "deep") return chain(Object.assign({}, state, { deep: true }));
        if (property === "nested") return chain(Object.assign({}, state, { nested: true }));
        if (property === "any") return chain(Object.assign({}, state, { any: true }));
        if (property === "all") return chain(Object.assign({}, state, { any: false }));
        if (property === "include" || property === "contain") {
          var include = function (expected) {
            verify(state, subset(state.actual, expected, state.deep), expected, "to include");
            return chain(state);
          };
          return new Proxy(include, { get: function (_, next) { return chain(Object.assign({}, state, { contain: true }))[next]; } });
        }
        if (["equal", "equals", "eq"].indexOf(property) !== -1) return function (expected) {
          verify(state, state.deep ? same(state.actual, expected) : state.actual === expected, expected, "to equal");
          return chain(state);
        };
        if (property === "property") return function (name, expected) {
          var found = state.nested ? nestedValue(state.actual, name) : {
            found: state.actual != null && Object.prototype.hasOwnProperty.call(Object(state.actual), name),
            value: state.actual == null ? undefined : state.actual[name]
          };
          verify(state, found.found, name, "to have property");
          if (arguments.length > 1 && found.found) verify(state, state.deep ? same(found.value, expected) : found.value === expected, expected, "property to equal", found.value);
          return chain(Object.assign({}, state, { actual: found.value, negate: false, nested: false }));
        };
        if (property === "oneOf") return function (expected) {
          verify(state, Array.isArray(expected) && expected.some(function (item) { return same(item, state.actual); }), expected, "to be one of");
          return chain(state);
        };
        if (property === "length" || property === "lengthOf") return function (expected) {
          var actual = state.actual == null ? undefined : state.actual.length;
          verify(state, actual === expected, expected, "to have length", actual);
          return chain(state);
        };
        if (property === "type" || property === "a" || property === "an") return function (expected) {
          verify(state, kind(state.actual) === String(expected).toLowerCase(), expected, "to have type", kind(state.actual));
          return chain(state);
        };
        if (property === "above" || property === "greaterThan") return function (expected) { verify(state, state.actual > expected, expected, "to be above"); return chain(state); };
        if (property === "below" || property === "lessThan") return function (expected) { verify(state, state.actual < expected, expected, "to be below"); return chain(state); };
        if (property === "least") return function (expected) { verify(state, state.actual >= expected, expected, "to be at least"); return chain(state); };
        if (property === "most") return function (expected) { verify(state, state.actual <= expected, expected, "to be at most"); return chain(state); };
        if (property === "match") return function (expected) { verify(state, expected instanceof RegExp && expected.test(String(state.actual)), String(expected), "to match"); return chain(state); };
        if (property === "keys") return function () {
          var expected = arguments.length === 1 && Array.isArray(arguments[0]) ? arguments[0] : Array.prototype.slice.call(arguments);
          var actual = state.actual && typeof state.actual === "object" ? Object.keys(state.actual) : [];
          var found = state.any ? expected.some(function (key) { return actual.indexOf(String(key)) !== -1; }) : expected.every(function (key) { return actual.indexOf(String(key)) !== -1; });
          var passed = found && (state.any || state.contain || actual.length === expected.length);
          verify(state, passed, expected, "to have keys", actual);
          return chain(state);
        };
        if (property === "members") return function (expected) {
          var actual = Array.isArray(state.actual) ? state.actual : [];
          var used = [];
          var contains = Array.isArray(expected) && expected.every(function (item) {
            for (var i = 0; i < actual.length; i++) {
              if (!used[i] && (state.deep ? same(actual[i], item) : actual[i] === item)) { used[i] = true; return true; }
            }
            return false;
          });
          verify(state, contains && (state.contain || actual.length === expected.length), expected, "to have members", actual);
          return chain(state);
        };
        var literals = { empty: state.actual != null && (typeof state.actual === "string" || Array.isArray(state.actual) ? state.actual.length === 0 : typeof state.actual === "object" && Object.keys(state.actual).length === 0), exist: state.actual !== null && state.actual !== undefined, true: state.actual === true, false: state.actual === false, null: state.actual === null, undefined: state.actual === undefined };
        if (Object.prototype.hasOwnProperty.call(literals, property)) {
          verify(state, literals[property], property, "to be");
          return chain(state);
        }
        return undefined;
      }
    });
  }
  function expect(actual) { return chain({ actual: actual, negate: false, deep: false, contain: false, nested: false, any: false }); }
  function test(name, callback) {
    var assertion = { passed: true, message: String(name) };
    try {
      var returned = callback();
      if (returned && typeof returned.then === "function") throw new Error("async tests are unsupported");
    } catch (error) {
      assertion.passed = false;
      assertion.message = String(name) + ": " + (error && error.message ? error.message : String(error));
      if (error && Object.prototype.hasOwnProperty.call(error, "expected")) assertion.expected = error.expected;
      if (error && Object.prototype.hasOwnProperty.call(error, "actual")) assertion.actual = error.actual;
    }
    results.push(assertion);
  }
  var headerObject = {};
  (ctx.response && ctx.response.headers || []).forEach(function (header) {
    headerObject[header.name] = header.value;
    headerObject[String(header.name).toLowerCase()] = header.value;
  });
  Object.freeze(headerObject);
  var body = ctx.response ? ctx.response.body : undefined;
  var res = Object.freeze({
    status: ctx.response ? ctx.response.status : undefined,
    body: body,
    headers: headerObject,
    getStatus: function () { return ctx.response ? ctx.response.status : undefined; },
    getBody: function () { return body; },
    getHeader: function (name) { return headerObject[String(name).toLowerCase()]; }
  });
  var req = Object.freeze({ body: ctx.request ? ctx.request.body : undefined });
  function read(scope, name) { return scope && Object.prototype.hasOwnProperty.call(scope, name) ? scope[name] : undefined; }
  var bru = Object.freeze({
    getVar: function (name) {
      var value = read(ctx.runtime, name);
      if (value !== undefined) return value;
      value = read(ctx.bindings, name);
      return value !== undefined ? value : read(ctx.environment, name);
    },
    getEnvVar: function (name) { return read(ctx.environment, name); },
    getCollectionVar: function (name) { return read(ctx.bindings, name); }
  });
  try {
    source(test, expect, res, req, bru);
  } catch (error) {
    results.push({ passed: false, message: "Bruno tests script: " + (error && error.message ? error.message : String(error)) });
  }
  return results;
}
function __brunoRead(scope, name) {
  return scope && Object.prototype.hasOwnProperty.call(scope, name) ? scope[name] : undefined;
}
function __brunoClone(value) {
  return value === undefined ? undefined : JSON.parse(JSON.stringify(value));
}
function __brunoMutable(value, touched) {
  if (!value || typeof value !== "object") return value;
  return new Proxy(value, {
    get: function (target, property) { return __brunoMutable(target[property], touched); },
    set: function (target, property, next) { touched(); target[property] = next; return true; },
    deleteProperty: function (target, property) { touched(); return delete target[property]; }
  });
}
function __brunoHeaders(rows) {
  var headers = {};
  (rows || []).forEach(function (header) {
    headers[header.name] = header.value;
    headers[String(header.name).toLowerCase()] = header.value;
  });
  return Object.freeze(headers);
}
function __brunoResponse(ctx) {
  var response = ctx.response;
  var body = response ? response.body : undefined;
  var headers = __brunoHeaders(response && response.headers);
  return Object.freeze({
    status: response ? response.status : undefined,
    body: body,
    headers: headers,
    getStatus: function () { return response ? response.status : undefined; },
    getBody: function () { return body; },
    getHeader: function (name) { return headers[String(name).toLowerCase()]; }
  });
}
function __brunoVariables(ctx, writes) {
  function write(name, value) {
    if (typeof name !== "string" || !name) throw new Error("Bruno variable name must be a non-empty string");
    writes[name] = value;
    return value;
  }
  return Object.freeze({
    getVar: function (name) {
      var value = __brunoRead(writes, name);
      if (value !== undefined) return value;
      value = __brunoRead(ctx.runtime, name);
      if (value !== undefined) return value;
      value = __brunoRead(ctx.bindings, name);
      return value !== undefined ? value : __brunoRead(ctx.environment, name);
    },
    getEnvVar: function (name) {
      var value = __brunoRead(writes, name);
      return value !== undefined ? value : __brunoRead(ctx.environment, name);
    },
    getCollectionVar: function (name) { return __brunoRead(ctx.bindings, name); },
    setVar: write,
    setEnvVar: write
  });
}
function __brunoBodyType(value, preferred) {
  if (typeof value === "string") return "text";
  if (preferred === "form" && value && typeof value === "object" && !Array.isArray(value)) return "form";
  return "json";
}
function __brunoRejectFileBody(value) {
  if (value && typeof value === "object") {
    if (String(value.type || "").toLowerCase() === "file") {
      throw new Error("file and multipart bodies cannot be created by imported Bruno JavaScript");
    }
    Object.keys(value).forEach(function (key) { __brunoRejectFileBody(value[key]); });
  }
}
function __brunoBefore(ctx, source) {
  var writes = {};
  var changedHeaders = [];
  var initial = ctx.request || {};
  var bodyTouched = false;
  var bodyType = initial.bodyType || "none";
  var body = __brunoMutable(__brunoClone(initial.body), function () { bodyTouched = true; });
  function setBody(value) {
    __brunoRejectFileBody(value);
    body = __brunoMutable(value, function () { bodyTouched = true; });
    bodyType = __brunoBodyType(value, bodyType);
    bodyTouched = true;
  }
  var req = {
    setBody: setBody,
    setHeader: function (name, value) {
      if (typeof name !== "string" || typeof value !== "string") {
        throw new Error("req.setHeader requires string name and value");
      }
      changedHeaders.push({ name: name, value: value });
    }
  };
  Object.defineProperty(req, "body", {
    enumerable: true,
    get: function () { return body; },
    set: setBody
  });
  var returned = source(__brunoResponse(ctx), req, __brunoVariables(ctx, writes));
  if (returned && typeof returned.then === "function") throw new Error("async Bruno scripts are unsupported");
  var output = { headers: changedHeaders, runtime: writes };
  if (bodyTouched) {
    __brunoRejectFileBody(body);
    output.body = { type: bodyType, value: body };
  }
  return output;
}
function __brunoAfter(ctx, source) {
  var writes = {};
  var req = Object.freeze({ body: ctx.request ? ctx.request.body : undefined });
  var returned = source(__brunoResponse(ctx), req, __brunoVariables(ctx, writes));
  if (returned && typeof returned.then === "function") throw new Error("async Bruno scripts are unsupported");
  return { runtime: writes };
}
(function () {
  var alphabet = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
  if (typeof globalThis.btoa !== "function") globalThis.btoa = function (input) {
    var text = String(input), output = "";
    for (var index = 0; index < text.length; index += 3) {
      var a = text.charCodeAt(index), b = text.charCodeAt(index + 1), c = text.charCodeAt(index + 2);
      if (a > 255 || b > 255 || c > 255) throw new Error("btoa supports Latin-1 input only");
      output += alphabet[a >> 2];
      output += alphabet[((a & 3) << 4) | (b >> 4)];
      output += isNaN(b) ? "=" : alphabet[((b & 15) << 2) | (c >> 6)];
      output += isNaN(c) ? "=" : alphabet[c & 63];
    }
    return output;
  };
  if (typeof globalThis.atob !== "function") globalThis.atob = function (input) {
    var text = String(input).replace(/\s/g, ""), output = "";
    if (text.length % 4 === 1 || /[^A-Za-z0-9+/=]/.test(text)) throw new Error("invalid base64 input");
    while (text.length % 4) text += "=";
    for (var index = 0; index < text.length; index += 4) {
      var a = alphabet.indexOf(text[index]), b = alphabet.indexOf(text[index + 1]);
      var c = text[index + 2] === "=" ? 0 : alphabet.indexOf(text[index + 2]);
      var d = text[index + 3] === "=" ? 0 : alphabet.indexOf(text[index + 3]);
      output += String.fromCharCode((a << 2) | (b >> 4));
      if (text[index + 2] !== "=") output += String.fromCharCode(((b & 15) << 4) | (c >> 2));
      if (text[index + 3] !== "=") output += String.fromCharCode(((c & 3) << 6) | d);
    }
    return output;
  };
  globalThis.Buffer = Object.freeze({
    from: function (input, encoding) {
      if (String(encoding).toLowerCase() !== "base64") throw new Error("Buffer.from only supports base64");
      var normalized = String(input).replace(/-/g, "+").replace(/_/g, "/");
      while (normalized.length % 4) normalized += "=";
      var binary = globalThis.atob(normalized);
      return Object.freeze({
        toString: function (outputEncoding) {
          if (outputEncoding !== undefined && ["utf8", "utf-8"].indexOf(String(outputEncoding).toLowerCase()) === -1) {
            throw new Error("Buffer.toString only supports UTF-8");
          }
          var escaped = "";
          for (var index = 0; index < binary.length; index++) {
            escaped += "%" + binary.charCodeAt(index).toString(16).padStart(2, "0");
          }
          return decodeURIComponent(escaped);
        }
      });
    }
  });
})();
"#;

fn script_is_only_simple_patterns(code: &str, kind: &str, converted: usize) -> bool {
    if converted == 0 {
        return false;
    }
    // A complete JavaScript equivalence proof is deliberately out of scope.
    // Mark wrappers, conditions, logging, and additional assertions for review.
    let nontrivial = [
        "if (",
        "if(",
        "forEach",
        "console.",
        "runner.",
        "sendRequest",
        "Date.",
    ];
    !nontrivial.iter().any(|needle| code.contains(needle))
        && code.matches("expect").count() <= converted
        && code.matches("bru.setVar").count() <= converted
        && (kind == "tests" || kind == "after-response")
}

struct VariableScopes<'a> {
    bindings: &'a BTreeSet<String>,
    environments: &'a BTreeSet<String>,
    secrets: &'a BTreeSet<String>,
    runtime: &'a BTreeSet<String>,
}

fn translate_vars(
    input: &str,
    scopes: &VariableScopes<'_>,
    unresolved: &mut BTreeSet<String>,
) -> String {
    let mut output = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(start) = rest.find("{{") {
        output.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else {
            output.push_str(&rest[start..]);
            return output;
        };
        let name = after[..end].trim();
        let replacement = if scopes.bindings.contains(name) {
            Some(format!("${{bindings.{name}}}"))
        } else if scopes.runtime.contains(name) {
            Some(format!("${{runtime.{name}}}"))
        } else if scopes.secrets.contains(name) {
            Some(format!("${{secret.{name}}}"))
        } else if scopes.environments.contains(name) {
            Some(format!("${{env.{name}}}"))
        } else {
            unresolved.insert(name.to_string());
            None
        };
        output.push_str(&replacement.unwrap_or_else(|| format!("{{{{{name}}}}}")));
        rest = &after[end + 2..];
    }
    output.push_str(rest);
    output
}

fn translate_json_vars(
    value: Value,
    scopes: &VariableScopes<'_>,
    unresolved: &mut BTreeSet<String>,
) -> Value {
    match value {
        Value::String(value) => Value::String(translate_vars(&value, scopes, unresolved)),
        Value::Array(values) => Value::Array(
            values
                .into_iter()
                .map(|value| translate_json_vars(value, scopes, unresolved))
                .collect(),
        ),
        Value::Object(values) => Value::Object(
            values
                .into_iter()
                .map(|(key, value)| {
                    (
                        translate_vars(&key, scopes, unresolved),
                        translate_json_vars(value, scopes, unresolved),
                    )
                })
                .collect(),
        ),
        other => other,
    }
}

fn add_vars(
    yaml: &Yaml,
    source_path: &str,
    output: &mut BTreeMap<String, Value>,
    secrets: &mut BTreeSet<String>,
    diagnostics: &mut BTreeMap<String, BTreeMap<String, Vec<String>>>,
) {
    for path in [
        &["vars", "pre-request"][..],
        &["runtime", "vars"][..],
        &["runtime", "variables"][..],
    ] {
        for row in rows_at(yaml, path) {
            if !is_enabled(row) {
                continue;
            }
            if let Some(name) = text_at(row, &["name"]) {
                let explicit_secret = bool_at(row, &["secret"]).unwrap_or(false);
                if explicit_secret || is_sensitive_name(&name) {
                    if !explicit_secret {
                        push_sensitive_variable_diagnostic(diagnostics, source_path, &name);
                    }
                    output.remove(&name);
                    secrets.insert(name);
                    continue;
                }
                if secrets.contains(&name) {
                    continue;
                }
                output.insert(
                    name,
                    value_at(row, &["value"])
                        .map(yaml_to_json)
                        .unwrap_or(Value::Null),
                );
            }
        }
    }
}

fn lower_inherited_scripts(
    yaml: &Yaml,
    path: &str,
    relative: &Path,
    inherited_skip: &mut Vec<SourceCondition>,
    inherited_delay_ms: &mut u64,
    inherited_scripts: &mut Vec<SourceScript>,
) {
    if let Some(scripts) = value_at(yaml, &["runtime", "scripts"]).and_then(Yaml::as_sequence) {
        for (index, script) in scripts
            .iter()
            .enumerate()
            .filter(|(_, script)| is_enabled(script))
        {
            let kind = text_at(script, &["type"]).unwrap_or_else(|| "unknown".to_string());
            let code = text_at(script, &["code"]).unwrap_or_default();
            if kind == "before-request" {
                let analysis = analyze_before_request(&code);
                if analysis.complete {
                    inherited_skip.extend(analysis.skip);
                    *inherited_delay_ms =
                        inherited_delay_ms.saturating_add(analysis.delays.into_iter().sum());
                } else if !code.trim().is_empty() {
                    inherited_scripts.push(SourceScript {
                        source_relative: relative.to_path_buf(),
                        source_path: path.to_string(),
                        source_index: index + 1,
                        kind,
                        code,
                        inherited: true,
                    });
                }
            } else if !code.trim().is_empty() {
                inherited_scripts.push(SourceScript {
                    source_relative: relative.to_path_buf(),
                    source_path: path.to_string(),
                    source_index: index + 1,
                    kind,
                    code,
                    inherited: true,
                });
            }
        }
    }
}

fn diagnose_unknown(
    yaml: &Yaml,
    known: &[&str],
    path: &str,
    feature: &str,
    diagnostics: &mut BTreeMap<String, BTreeMap<String, Vec<String>>>,
) {
    let Some(mapping) = yaml.as_mapping() else {
        return;
    };
    for key in mapping.keys().filter_map(Yaml::as_str) {
        if !known.contains(&key) {
            push_diagnostic(
                diagnostics,
                path,
                feature,
                &format!("unrecognized field '{key}' was not materialized"),
            );
        }
    }
}

fn push_diagnostic(
    diagnostics: &mut BTreeMap<String, BTreeMap<String, Vec<String>>>,
    path: &str,
    feature: &str,
    message: &str,
) {
    let messages = diagnostics
        .entry(path.to_string())
        .or_default()
        .entry(feature.to_string())
        .or_default();
    if !messages.iter().any(|existing| existing == message) {
        messages.push(message.to_string());
    }
}

fn read_yaml(path: &Path) -> Result<Yaml, BrunoV1ImportError> {
    let text = std::fs::read_to_string(path).map_err(|error| io_error(path, error))?;
    serde_yaml_ng::from_str(&text).map_err(|error| BrunoV1ImportError::Yaml {
        path: path.display().to_string(),
        message: error
            .location()
            .map(|location| {
                format!(
                    "could not parse YAML at line {}, column {}",
                    location.line(),
                    location.column()
                )
            })
            .unwrap_or_else(|| "could not parse YAML".to_string()),
    })
}

fn value_at<'a>(value: &'a Yaml, path: &[&str]) -> Option<&'a Yaml> {
    let mut current = value;
    for key in path {
        current = current
            .as_mapping()?
            .get(Yaml::String((*key).to_string()))?;
    }
    Some(current)
}

fn text_at(value: &Yaml, path: &[&str]) -> Option<String> {
    value_at(value, path).map(|value| match value {
        Yaml::String(value) => value.clone(),
        Yaml::Number(value) => value.to_string(),
        Yaml::Bool(value) => value.to_string(),
        _ => String::new(),
    })
}

fn number_at(value: &Yaml, path: &[&str]) -> Option<f64> {
    let value = value_at(value, path)?;
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
}

fn bool_at(value: &Yaml, path: &[&str]) -> Option<bool> {
    let value = value_at(value, path)?;
    value
        .as_bool()
        .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
}

fn rows_at<'a>(value: &'a Yaml, path: &[&str]) -> Vec<&'a Yaml> {
    value_at(value, path)
        .and_then(Yaml::as_sequence)
        .map(|rows| rows.iter().collect())
        .unwrap_or_default()
}

fn string_list_at(value: &Yaml, path: &[&str]) -> BTreeSet<String> {
    value_at(value, path)
        .and_then(Yaml::as_sequence)
        .map(|values| {
            values
                .iter()
                .filter_map(Yaml::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn is_enabled(value: &Yaml) -> bool {
    bool_at(value, &["enabled"]).unwrap_or(true) && !bool_at(value, &["disabled"]).unwrap_or(false)
}

fn docs_at(value: &Yaml) -> Option<String> {
    text_at(value, &["docs", "content"])
        .or_else(|| text_at(value, &["docs"]))
        .filter(|docs| !docs.trim().is_empty())
}

fn tags_at(value: &Yaml) -> Vec<String> {
    string_list_at(value, &["info", "tags"])
        .into_iter()
        .chain(string_list_at(value, &["tags"]))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn is_sensitive_name(name: &str) -> bool {
    let normalized = name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect::<String>();
    normalized.contains("password")
        || normalized.contains("passwd")
        || normalized.contains("secret")
        || normalized.contains("token")
        || normalized.contains("api_key")
        || normalized.contains("apikey")
        || normalized.contains("private_key")
        || normalized.contains("credential")
        || normalized == "authorization"
        || normalized == "proxy_authorization"
        || normalized == "cookie"
        || normalized == "set_cookie"
}

fn push_sensitive_variable_diagnostic(
    diagnostics: &mut BTreeMap<String, BTreeMap<String, Vec<String>>>,
    path: &str,
    name: &str,
) {
    push_diagnostic(
        diagnostics,
        path,
        "secrets",
        &format!(
            "plain variable '{name}' was treated as a secret declaration; its value was not exported"
        ),
    );
}

fn contains_dynamic_reference(value: &str) -> bool {
    value.contains("${secret.")
        || value.contains("${env.")
        || value.contains("${bindings.")
        || value.contains("${runtime.")
        || value.contains("{{")
}

fn secret_reference(name: &str) -> String {
    let name = name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '_' {
                character.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect::<String>();
    let name = name.trim_matches('_');
    let name = if name.is_empty() {
        "bruno_secret"
    } else {
        name
    };
    format!("${{secret.{name}}}")
}

fn scalar_text(value: Option<&Yaml>) -> String {
    match value {
        Some(Yaml::String(value)) => value.clone(),
        Some(Yaml::Number(value)) => value.to_string(),
        Some(Yaml::Bool(value)) => value.to_string(),
        Some(Yaml::Null) | None => String::new(),
        Some(value) => serde_json::to_string(&yaml_to_json(value)).unwrap_or_default(),
    }
}

fn yaml_to_json(value: &Yaml) -> Value {
    match value {
        Yaml::Null => Value::Null,
        Yaml::Bool(value) => Value::Bool(*value),
        Yaml::Number(value) => {
            if let Some(value) = value.as_i64() {
                Value::from(value)
            } else if let Some(value) = value.as_u64() {
                Value::from(value)
            } else {
                Value::from(value.as_f64().unwrap_or_default())
            }
        }
        Yaml::String(value) => Value::String(value.clone()),
        Yaml::Sequence(values) => Value::Array(values.iter().map(yaml_to_json).collect()),
        Yaml::Mapping(values) => Value::Object(
            values
                .iter()
                .map(|(key, value)| (scalar_text(Some(key)), yaml_to_json(value)))
                .collect(),
        ),
        Yaml::Tagged(tagged) => yaml_to_json(&tagged.value),
    }
}

fn json_file(path: PathBuf, value: &impl Serialize) -> Result<GeneratedFile, BrunoV1ImportError> {
    let mut bytes = serde_json::to_vec_pretty(value)
        .map_err(|error| BrunoV1ImportError::Materialize(error.to_string()))?;
    bytes.push(b'\n');
    Ok(GeneratedFile {
        relative: path,
        bytes,
    })
}

fn sequence_file(
    path: PathBuf,
    id: String,
    name: String,
    description: Option<String>,
    tags: Vec<String>,
    requests: Vec<String>,
) -> Result<GeneratedFile, BrunoV1ImportError> {
    json_file(
        path,
        &SequenceDocument {
            schema: None,
            format_version: FormatVersion,
            kind: SequenceKind::Sequence,
            meta: RequestMeta {
                id,
                name,
                description,
                tags,
            },
            requests,
        },
    )
}

fn leaf_sequence_path(leaf: &Path) -> PathBuf {
    let mut path = PathBuf::from("sequences");
    if leaf.as_os_str().is_empty() {
        path.push("root.sequence.json");
        return path;
    }
    for component in leaf.components() {
        path.push(safe_file_name(&component.as_os_str().to_string_lossy()));
    }
    path.push("leaf.sequence.json");
    path
}

fn service_sequence_path(service: &Path) -> PathBuf {
    let mut path = PathBuf::from("sequences");
    for component in service.components() {
        path.push(safe_file_name(&component.as_os_str().to_string_lossy()));
    }
    path.push("service.sequence.json");
    path
}

fn sequence_id(prefix: &str, source_path: &Path) -> String {
    std::iter::once(prefix.to_string())
        .chain(
            source_path
                .components()
                .map(|component| safe_id_part(&component.as_os_str().to_string_lossy())),
        )
        .collect::<Vec<_>>()
        .join(".")
}

fn service_path(leaf: &Path) -> Option<PathBuf> {
    let components = leaf
        .components()
        .filter_map(|component| match component {
            Component::Normal(value) => Some(value.to_owned()),
            _ => None,
        })
        .collect::<Vec<_>>();
    if components.len() < 3 || components[0].as_os_str() != std::ffi::OsStr::new("services") {
        return None;
    }
    Some(components.into_iter().take(3).collect())
}

fn validate_source_destination(
    source: &Path,
    destination: &Path,
) -> Result<(), BrunoV1ImportError> {
    let source = std::fs::canonicalize(source).map_err(|error| io_error(source, error))?;
    let destination = canonical_with_missing(destination)?;
    if destination.starts_with(&source) || source.starts_with(&destination) {
        return Err(BrunoV1ImportError::Materialize(
            "source and destination must not overlap".to_string(),
        ));
    }
    Ok(())
}

fn validate_generated_files(
    destination: &Path,
    files: &[GeneratedFile],
) -> Result<(), BrunoV1ImportError> {
    if destination.exists() && !destination.is_dir() {
        return Err(BrunoV1ImportError::Materialize(
            "destination exists and is not a directory".to_string(),
        ));
    }
    let destination = canonical_with_missing(destination)?;
    let mut paths = BTreeSet::new();
    for file in files {
        if file.relative.as_os_str().is_empty()
            || file.relative.is_absolute()
            || file
                .relative
                .components()
                .any(|component| !matches!(component, Component::Normal(_)))
        {
            return Err(BrunoV1ImportError::Materialize(format!(
                "generated output path is not project-relative: {}",
                display_path(&file.relative)
            )));
        }
        if !paths.insert(file.relative.clone()) {
            return Err(BrunoV1ImportError::Materialize(format!(
                "generated output path collides: {}",
                display_path(&file.relative)
            )));
        }
        let target = canonical_with_missing(&destination.join(&file.relative))?;
        if !target.starts_with(&destination) {
            return Err(BrunoV1ImportError::Materialize(format!(
                "generated output path escapes the destination: {}",
                display_path(&file.relative)
            )));
        }
    }
    Ok(())
}

fn canonical_with_missing(path: &Path) -> Result<PathBuf, BrunoV1ImportError> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|error| io_error(Path::new("."), error))?
            .join(path)
    };
    let normalized = normalize_path(&absolute);
    let mut existing = normalized.as_path();
    let mut missing = Vec::new();
    while !existing.exists() {
        let Some(name) = existing.file_name() else {
            return Err(BrunoV1ImportError::Materialize(format!(
                "cannot resolve output path {}",
                normalized.display()
            )));
        };
        missing.push(name.to_owned());
        existing = existing.parent().ok_or_else(|| {
            BrunoV1ImportError::Materialize(format!(
                "cannot resolve output path {}",
                normalized.display()
            ))
        })?;
    }
    let mut resolved =
        std::fs::canonicalize(existing).map_err(|error| io_error(existing, error))?;
    for component in missing.into_iter().rev() {
        resolved.push(component);
    }
    Ok(resolved)
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut output = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => output.push(prefix.as_os_str()),
            Component::RootDir => output.push(Path::new("/")),
            Component::CurDir => {}
            Component::ParentDir => {
                output.pop();
            }
            Component::Normal(value) => output.push(value),
        }
    }
    output
}

fn sidecar_path(request: &Path, kind: &str) -> PathBuf {
    let name = request
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("request.request.json");
    let stem = name.strip_suffix(".request.json").unwrap_or(name);
    request.with_file_name(format!("{stem}.{kind}.json"))
}

fn safe_file_name(value: &str) -> String {
    let output = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>();
    let output = output.trim_matches('-');
    if output.is_empty() {
        "bruno-import".to_string()
    } else {
        output.to_string()
    }
}

fn safe_id_part(value: &str) -> String {
    let output = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '_' || character == '-' {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    if output.is_empty() {
        "request".to_string()
    } else {
        output
    }
}

fn file_stem(path: &Path) -> String {
    path.file_stem()
        .or_else(|| path.file_name())
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "Bruno".to_string())
}

fn display_path(path: &Path) -> String {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn io_error(path: &Path, error: std::io::Error) -> BrunoV1ImportError {
    BrunoV1ImportError::Io {
        path: path.display().to_string(),
        message: error.to_string(),
    }
}

// Classic .bru remains supported by the existing parser. This direct adapter
// intentionally emits only requests that the existing lossless v1 migration
// accepts; blocked requests remain visible as path-specific diagnostics.
fn plan_classic(root: &Path) -> Result<ImportPlan, BrunoV1ImportError> {
    let imported =
        import_bruno(root).map_err(|error| BrunoV1ImportError::Materialize(error.to_string()))?;
    let mut diagnostics = BTreeMap::new();
    for (index, _) in imported.collection.skipped.iter().enumerate() {
        push_diagnostic(
            &mut diagnostics,
            "collection",
            "classic-bru",
            &format!(
                "unsupported classic Bruno feature {} was not imported; inspect the source collection",
                index + 1
            ),
        );
    }
    let mut files = vec![json_file(
        PathBuf::from("project.json"),
        &ProjectConfig::default(),
    )?];
    let mut sequence = Vec::new();
    let mut scanned = 0;
    flatten_classic(
        &imported.collection.items,
        Path::new(""),
        &mut scanned,
        &mut sequence,
        &mut files,
        &mut diagnostics,
    )?;
    let mut environment_summaries = Vec::new();
    for (environment, _) in &imported.environments {
        let source_path = format!("environments/{}.bru", environment.name);
        let values = environment
            .variables
            .iter()
            .filter(|(name, variable)| {
                if !variable.secret && is_sensitive_name(name) {
                    push_sensitive_variable_diagnostic(&mut diagnostics, &source_path, name);
                }
                !variable.secret && !is_sensitive_name(name)
            })
            .map(|(name, variable)| {
                (
                    name.clone(),
                    Value::String(variable.value.clone().unwrap_or_default()),
                )
            })
            .collect::<BTreeMap<_, _>>();
        files.push(json_file(
            PathBuf::from("environments")
                .join(format!("{}.json", safe_file_name(&environment.name))),
            &values,
        )?);
        environment_summaries.push(ImportedEnvironmentSummary {
            name: environment.name.clone(),
            variable_count: environment.variables.len(),
            secret_count: environment
                .variables
                .iter()
                .filter(|(name, value)| value.secret || is_sensitive_name(name))
                .count(),
        });
    }
    files.push(json_file(
        PathBuf::from(format!(
            "{}.sequence.json",
            safe_file_name(&imported.collection.name)
        )),
        &SequenceDocument {
            schema: None,
            format_version: FormatVersion,
            kind: SequenceKind::Sequence,
            meta: RequestMeta {
                id: safe_file_name(&imported.collection.name),
                name: imported.collection.name.clone(),
                description: (!imported.collection.description.is_empty())
                    .then(|| imported.collection.description.clone()),
                tags: Vec::new(),
            },
            requests: sequence,
        },
    )?);
    if !imported.collection.quarantine.is_empty() {
        files.push(json_file(
            PathBuf::from(IMPORT_QUARANTINE_PATH),
            &ImportQuarantineManifest::new(
                imported
                    .collection
                    .quarantine
                    .iter()
                    .cloned()
                    .map(|entry| entry.with_import_key(&imported.collection.name))
                    .collect(),
            ),
        )?);
    }
    let imported_request_count = files
        .iter()
        .filter(|file| file.relative.to_string_lossy().ends_with(".request.json"))
        .count();
    let report = BrunoV1ImportReport {
        detected_format: BrunoSourceFormat::ClassicBru,
        scanned_request_count: scanned,
        imported_request_count,
        excluded_request_count: scanned - imported_request_count,
        excluded_by_policy: BTreeMap::new(),
        environments: environment_summaries,
        native_skip_request_count: 0,
        native_delay_request_count: 0,
        native_assertion_script_count: 0,
        custom_assertion_script_count: 0,
        blocked_assertion_script_count: 0,
        direct_native_contract_assertion_count: 0,
        transformed_native_contract_assertion_count: 0,
        blocked_contract_assertion_count: 0,
        custom_before_request_script_count: 0,
        blocked_before_request_script_count: 0,
        custom_after_response_script_count: 0,
        blocked_after_response_script_count: 0,
        generated_auth_provider_count: 0,
        generated_helper_request_count: 0,
        recognized_auth_script_count: 0,
        recognized_data_manager_auth_count: 0,
        remaining_blocked_auth_script_count: 0,
        quarantined_script_count: imported.collection.quarantine.len(),
        requires_project_code: false,
        diagnostics,
        output_file_count: files.len(),
    };
    Ok(ImportPlan {
        report,
        files,
        import_key: imported.collection.name,
    })
}

fn flatten_classic(
    items: &[ImportedItem],
    parent: &Path,
    scanned: &mut usize,
    sequence: &mut Vec<String>,
    files: &mut Vec<GeneratedFile>,
    diagnostics: &mut BTreeMap<String, BTreeMap<String, Vec<String>>>,
) -> Result<(), BrunoV1ImportError> {
    for item in items {
        match item {
            ImportedItem::Folder { name, items, .. } => flatten_classic(
                items,
                &parent.join(safe_file_name(name)),
                scanned,
                sequence,
                files,
                diagnostics,
            )?,
            ImportedItem::Request(request) => {
                *scanned += 1;
                let relative =
                    parent.join(format!("{}.request.json", safe_file_name(&request.name)));
                let id = display_path(&relative.with_extension("")).replace('/', ".");
                let mut request = request.as_ref().clone();
                sanitize_classic_request(&mut request, &display_path(&relative), diagnostics);
                match crate::reqv1::migrate_request(&request, id) {
                    Ok(document) => {
                        let path = PathBuf::from("requests").join(relative);
                        sequence.push(display_path(&path));
                        files.push(json_file(path, &document)?);
                    }
                    Err(error) => push_diagnostic(
                        diagnostics,
                        &display_path(&relative),
                        "classic-bru",
                        &error.to_string(),
                    ),
                }
            }
        }
    }
    Ok(())
}

fn sanitize_classic_request(
    request: &mut crate::model::RequestDef,
    path: &str,
    diagnostics: &mut BTreeMap<String, BTreeMap<String, Vec<String>>>,
) {
    let redact =
        |value: &mut String,
         name: &str,
         diagnostics: &mut BTreeMap<String, BTreeMap<String, Vec<String>>>| {
            if !value.contains("{{") && !contains_dynamic_reference(value) {
                *value = secret_reference(name);
                push_diagnostic(
                    diagnostics,
                    path,
                    "secrets",
                    &format!("literal {name} was replaced by a secret-provider reference"),
                );
            }
        };
    match &mut request.auth {
        AuthConfig::Basic { password, .. } => redact(password, "basic_auth_password", diagnostics),
        AuthConfig::Bearer { token, .. } => redact(token, "bearer_token", diagnostics),
        AuthConfig::ApiKey { value, .. } => redact(value, "api_key", diagnostics),
        _ => {}
    }
    for header in &mut request.headers {
        if is_sensitive_name(&header.key) {
            let key = header.key.clone();
            let value = std::mem::take(&mut header.value);
            header.value = redact_sensitive_row(&key, value, path, "header", diagnostics);
        }
    }
    for parameter in &mut request.params {
        if is_sensitive_name(&parameter.kv.key) {
            let key = parameter.kv.key.clone();
            let value = std::mem::take(&mut parameter.kv.value);
            parameter.kv.value =
                redact_sensitive_row(&key, value, path, "query parameter", diagnostics);
        }
    }
    sanitize_url_query(&mut request.url, path, diagnostics);
    match &mut request.body {
        BodyDef::Json { text } => {
            if let Ok(mut value) = serde_json::from_str::<Value>(text) {
                redact_json_secrets(&mut value, path, diagnostics);
                if let Ok(redacted) = serde_json::to_string(&value) {
                    *text = redacted;
                }
            }
        }
        BodyDef::FormUrlencoded { fields } => {
            for field in fields {
                if is_sensitive_name(&field.key) {
                    let key = field.key.clone();
                    let value = std::mem::take(&mut field.value);
                    field.value =
                        redact_sensitive_row(&key, value, path, "form field", diagnostics);
                }
            }
        }
        BodyDef::GraphQl { variables, .. } => {
            if let Ok(mut value) = serde_json::from_str::<Value>(variables) {
                redact_json_secrets(&mut value, path, diagnostics);
                if let Ok(redacted) = serde_json::to_string(&value) {
                    *variables = redacted;
                }
            }
        }
        _ => {}
    }
}
