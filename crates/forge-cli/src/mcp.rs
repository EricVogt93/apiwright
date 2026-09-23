use std::path::{Component, Path, PathBuf};

use forge_core::exec::HttpEngine;
use forge_core::reqv1::{
    self, AssertionDocument, AssertionEntry, HookDocument, ProjectFileKind, ProjectIndex,
    RequestDocument, RunMode, SequenceDocument,
};
use forge_core::runner::CancellationToken;
use rmcp::{
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, Implementation, ServerCapabilities, ServerInfo},
    schemars, tool, tool_handler, tool_router, ServerHandler, ServiceExt,
};
use serde::Deserialize;
use serde_json::{json, Value};

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct ProjectInput {
    #[schemars(description = "Absolute path to a saved ApiWright project containing project.json")]
    root: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct RequestInput {
    #[schemars(description = "Absolute path to a saved ApiWright project containing project.json")]
    root: String,
    #[schemars(description = "Project-relative path ending in .request.json")]
    request: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct AssetInput {
    #[schemars(description = "Absolute path to a saved ApiWright project containing project.json")]
    root: String,
    #[schemars(description = "Project-relative asset path returned by inspect_project")]
    asset: String,
}

#[derive(Debug, Clone, Copy, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
enum ProjectFileKindInput {
    Environment,
    Sequence,
    Asset,
}

impl From<ProjectFileKindInput> for ProjectFileKind {
    fn from(value: ProjectFileKindInput) -> Self {
        match value {
            ProjectFileKindInput::Environment => Self::Environment,
            ProjectFileKindInput::Sequence => Self::Sequence,
            ProjectFileKindInput::Asset => Self::Asset,
        }
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct ProjectFileInput {
    #[schemars(description = "Absolute path to an ApiWright request-v1 project")]
    root: String,
    #[schemars(description = "Project-relative resource path from inspect_project")]
    path: String,
    #[schemars(description = "Resource category: environment, sequence, or asset")]
    kind: ProjectFileKindInput,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct WriteProjectFileInput {
    #[schemars(description = "Absolute path to an ApiWright request-v1 project")]
    root: String,
    #[schemars(description = "Project-relative resource path")]
    path: String,
    #[schemars(description = "Resource category: environment, sequence, or asset")]
    kind: ProjectFileKindInput,
    #[schemars(description = "SHA-256 revision from read_project_file, or new when creating")]
    expected_revision: String,
    #[schemars(description = "JSON object for environments/sequences, UTF-8 string for assets")]
    content: Value,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct DeleteProjectFileInput {
    #[schemars(description = "Absolute path to an ApiWright request-v1 project")]
    root: String,
    #[schemars(description = "Project-relative resource path")]
    path: String,
    #[schemars(description = "Resource category: environment, sequence, or asset")]
    kind: ProjectFileKindInput,
    #[schemars(description = "SHA-256 revision returned by read_project_file")]
    expected_revision: String,
    #[serde(default)]
    #[schemars(
        description = "Allow deleting a selected environment or referenced asset after reviewing affected tests"
    )]
    allow_broken_references: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct DeleteRequestInput {
    #[schemars(description = "Absolute path to an ApiWright request-v1 project")]
    root: String,
    #[schemars(description = "Project-relative request path ending in .request.json")]
    request: String,
    #[schemars(description = "Revision returned by read_request")]
    expected_revision: String,
    #[serde(default)]
    #[schemars(
        description = "Allow removing a request still referenced by a sequence or project authentication"
    )]
    allow_broken_references: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct RunSequenceInput {
    #[schemars(description = "Absolute path to an ApiWright request-v1 project")]
    root: String,
    #[schemars(description = "Project-relative path ending in .sequence.json")]
    sequence: String,
    #[schemars(
        description = "Optional environment name; each request's scoped selection is used when omitted"
    )]
    environment: Option<String>,
    #[serde(default)]
    #[schemars(description = "Send real external HTTP instead of mock execution")]
    real_http: bool,
    #[serde(default)]
    #[schemars(
        description = "Permit project-owned JavaScript only after explicit review and trust"
    )]
    allow_project_code: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct ValidateInput {
    #[schemars(description = "Absolute path to a saved ApiWright project containing project.json")]
    root: String,
    #[schemars(description = "Project-relative path ending in .request.json")]
    request: String,
    #[schemars(
        description = "Optional environment name; inherited project selection is used when omitted"
    )]
    environment: Option<String>,
    #[serde(default)]
    #[schemars(description = "Permit trusted project JavaScript to execute during validation")]
    allow_project_code: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct WriteInput {
    #[schemars(description = "Absolute path to a saved ApiWright project containing project.json")]
    root: String,
    #[schemars(description = "Project-relative path ending in .request.json")]
    request: String,
    #[schemars(
        description = "Revision returned by read_request, or the literal new when create is true"
    )]
    expected_revision: String,
    #[schemars(description = "The main request JSON document")]
    document: Value,
    #[schemars(description = "Assertions sidecar JSON; omit to keep the current sidecar")]
    assertions: Option<Value>,
    #[schemars(description = "Hooks sidecar JSON; omit to keep the current sidecar")]
    hooks: Option<Value>,
    #[serde(default)]
    #[schemars(description = "Create a new request in an existing project directory")]
    create: bool,
    #[schemars(description = "Optional environment used for the returned validation result")]
    environment: Option<String>,
    #[serde(default)]
    #[schemars(description = "Permit trusted project JavaScript to execute during validation")]
    allow_project_code: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct AddAssertionInput {
    #[schemars(description = "Absolute path to an ApiWright request-v1 project")]
    root: String,
    #[schemars(description = "Project-relative request path returned by inspect_project")]
    request: String,
    #[schemars(description = "Revision returned by read_request")]
    expected_revision: String,
    #[schemars(description = "One assertion entry with use, optional with, and enabled fields")]
    assertion: Value,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct RemoveAssertionInput {
    #[schemars(description = "Absolute path to an ApiWright request-v1 project")]
    root: String,
    #[schemars(description = "Project-relative request path returned by inspect_project")]
    request: String,
    #[schemars(description = "Revision returned by read_request")]
    expected_revision: String,
    #[schemars(description = "Zero-based assertion index from read_request")]
    index: usize,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct RunInput {
    #[schemars(description = "Absolute path to a saved ApiWright project containing project.json")]
    root: String,
    #[schemars(description = "Project-relative path ending in .request.json")]
    request: String,
    #[schemars(
        description = "Optional environment name; inherited project selection is used when omitted"
    )]
    environment: Option<String>,
    #[schemars(description = "Optional revision that must still match before execution")]
    expected_revision: Option<String>,
    #[serde(default)]
    #[schemars(description = "Send real external HTTP instead of the safe default mock execution")]
    real_http: bool,
    #[serde(default)]
    #[schemars(description = "Permit trusted project JavaScript to execute")]
    allow_project_code: bool,
}

#[derive(Debug, Clone)]
struct ApiWrightMcp {
    tool_router: ToolRouter<Self>,
}

impl ApiWrightMcp {
    fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
        }
    }
}

#[tool_router]
impl ApiWrightMcp {
    #[tool(
        name = "inspect_project",
        description = "Inspect a saved ApiWright request-v1 project. Returns requests, sequences, environments, asset metadata and broken references, but never secret values.",
        output_schema = object_output_schema(),
        annotations(
            title = "Inspect ApiWright project",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn inspect_project(
        &self,
        Parameters(input): Parameters<ProjectInput>,
    ) -> Result<CallToolResult, String> {
        let root = project_root(&input.root)?;
        let _project_lock = reqv1::project_lock(&root).map_err(internal)?;
        let index = ProjectIndex::scan(&root).map_err(|diagnostic| invalid(diagnostic.message))?;
        let mut value = serde_json::to_value(index).map_err(internal)?;
        if let Some(assets) = value.get_mut("assets").and_then(Value::as_array_mut) {
            for asset in assets {
                if let Some(asset) = asset.as_object_mut() {
                    asset.remove("data");
                }
            }
        }
        Ok(CallToolResult::structured(value))
    }

    #[tool(
        name = "read_request",
        description = "Read one saved ApiWright request, its assertion and hook sidecars, the merged effective document, inherited settings and an optimistic-concurrency revision.",
        output_schema = object_output_schema(),
        annotations(
            title = "Read ApiWright request",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn read_request(
        &self,
        Parameters(input): Parameters<RequestInput>,
    ) -> Result<CallToolResult, String> {
        let root = project_root(&input.root)?;
        let request = request_path(&root, &input.request, false)?;
        let _request_lock = reqv1::request_lock(&request).map_err(internal)?;
        let revision = reqv1::request_revision(&request)?;
        let text = std::fs::read_to_string(&request).map_err(internal)?;
        let document: Value = serde_json::from_str(&text)
            .map_err(|error| invalid(format!("invalid {}: {error}", input.request)))?;
        let effective = reqv1::load_request_document(&request).map_err(invalid)?;
        let assertions = AssertionDocument::load_for_request(&request).map_err(invalid)?;
        let hooks = HookDocument::load_for_request(&request).map_err(invalid)?;
        let environment = reqv1::effective_environment(&root, &request)
            .map_err(invalid)?
            .map(|selection| selection_value(&root, selection.value, &selection.source));
        let openapi = reqv1::effective_openapi(&root, &request)
            .map_err(invalid)?
            .map(|selection| selection_value(&root, selection.value, &selection.source));
        ensure_revision(&request, &revision, "reading")?;

        Ok(CallToolResult::structured(json!({
            "request": relative(&root, &request),
            "revision": revision,
            "document": document,
            "assertions": assertions,
            "hooks": hooks,
            "effectiveDocument": effective,
            "effectiveEnvironment": environment,
            "effectiveOpenApi": openapi,
        })))
    }

    #[tool(
        name = "read_project_code",
        description = "Read one JavaScript or TypeScript asset already indexed under the project's assets directory. Use this to inspect project-owned code before explicitly trusting it.",
        output_schema = object_output_schema(),
        annotations(
            title = "Read ApiWright project asset",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn read_project_code(
        &self,
        Parameters(input): Parameters<AssetInput>,
    ) -> Result<CallToolResult, String> {
        const MAX_ASSET_BYTES: u64 = 256 * 1024;

        let root = project_root(&input.root)?;
        let index = ProjectIndex::scan(&root).map_err(|diagnostic| invalid(diagnostic.message))?;
        let asset = index
            .assets
            .iter()
            .find(|asset| asset.rel_path == input.asset)
            .ok_or_else(|| invalid("asset must be a project-relative path from inspect_project"))?;
        let path = Path::new(&asset.path);
        if asset.kind == reqv1::AssetKind::Data
            || !matches!(
                path.extension().and_then(|extension| extension.to_str()),
                Some("js" | "ts")
            )
        {
            return Err(invalid(
                "read_project_code accepts only indexed .js or .ts executable assets",
            ));
        }
        let metadata = std::fs::metadata(path).map_err(internal)?;
        if metadata.len() > MAX_ASSET_BYTES {
            return Err(invalid("asset exceeds the 256 KiB MCP read limit"));
        }
        let content = std::fs::read_to_string(path)
            .map_err(|error| invalid(format!("asset is not readable UTF-8 text: {error}")))?;
        Ok(CallToolResult::structured(json!({
            "asset": asset.rel_path,
            "kind": asset.kind,
            "metadata": asset.metadata,
            "content": content,
        })))
    }

    #[tool(
        name = "read_project_file",
        description = "Read one request-v1 environment, sequence, or indexed asset. Returns a revision for a safe follow-up edit and never reads .env.local or other secret stores.",
        output_schema = object_output_schema(),
        annotations(
            title = "Read ApiWright project resource",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn read_project_file(
        &self,
        Parameters(input): Parameters<ProjectFileInput>,
    ) -> Result<CallToolResult, String> {
        let root = project_root(&input.root)?;
        let file =
            reqv1::read_project_file(&root, input.kind.into(), &input.path).map_err(invalid)?;
        Ok(CallToolResult::structured(json!({
            "path": file.path,
            "revision": file.revision,
            "content": file.content,
        })))
    }

    #[tool(
        name = "write_project_file",
        description = "Create or update one environment, sequence, or project asset after a SHA-256 revision check. Environment and sequence JSON is validated before writing.",
        output_schema = object_output_schema(),
        annotations(
            title = "Write ApiWright project resource",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn write_project_file(
        &self,
        Parameters(input): Parameters<WriteProjectFileInput>,
    ) -> Result<CallToolResult, String> {
        let root = project_root(&input.root)?;
        let file = reqv1::write_project_file(
            &root,
            input.kind.into(),
            &input.path,
            &input.expected_revision,
            input.content,
        )
        .map_err(invalid)?;
        Ok(CallToolResult::structured(json!({
            "written": true,
            "path": file.path,
            "revision": file.revision,
            "content": file.content,
            "ideRefreshRequired": true,
        })))
    }

    #[tool(
        name = "delete_project_file",
        description = "Delete one environment, sequence, or asset after a revision check. Selected environments and referenced assets are protected unless allowBrokenReferences is explicitly enabled.",
        output_schema = object_output_schema(),
        annotations(
            title = "Delete ApiWright project resource",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn delete_project_file(
        &self,
        Parameters(input): Parameters<DeleteProjectFileInput>,
    ) -> Result<CallToolResult, String> {
        let root = project_root(&input.root)?;
        reqv1::delete_project_file(
            &root,
            input.kind.into(),
            &input.path,
            &input.expected_revision,
            input.allow_broken_references,
        )
        .map_err(invalid)?;
        Ok(CallToolResult::structured(json!({
            "deleted": true,
            "path": input.path,
            "ideRefreshRequired": true,
        })))
    }

    #[tool(
        name = "validate_request",
        description = "Validate one saved ApiWright request through the same canonical request-v1 pipeline used by the IDE and CLI. No HTTP is sent.",
        output_schema = object_output_schema(),
        annotations(
            title = "Validate ApiWright request",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn validate_request(
        &self,
        Parameters(input): Parameters<ValidateInput>,
    ) -> Result<CallToolResult, String> {
        let root = project_root(&input.root)?;
        let request = request_path(&root, &input.request, false)?;
        let _request_lock = reqv1::request_lock(&request).map_err(internal)?;
        let revision = reqv1::request_revision(&request)?;
        let document = reqv1::load_request_document(&request).map_err(invalid)?;
        let validation = validate_value(
            &root,
            &request,
            &document,
            input.environment.as_deref(),
            input.allow_project_code,
        )?;
        ensure_revision(&request, &revision, "validation")?;
        Ok(CallToolResult::structured(json!({
            "request": relative(&root, &request),
            "revision": revision,
            "validation": validation,
        })))
    }

    #[tool(
        name = "write_request",
        description = "Create or update one ApiWright request and its sidecars after checking the revision. The saved request is normalized exactly like an IDE save and is validated without sending HTTP.",
        output_schema = object_output_schema(),
        annotations(
            title = "Write ApiWright request",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn write_request(
        &self,
        Parameters(input): Parameters<WriteInput>,
    ) -> Result<CallToolResult, String> {
        let root = project_root(&input.root)?;
        let request = request_path(&root, &input.request, input.create)?;
        let read_lock = if input.create {
            None
        } else {
            Some(reqv1::request_lock(&request).map_err(internal)?)
        };
        let current_revision = if input.create {
            "new".to_string()
        } else {
            reqv1::request_revision(&request)?
        };
        if input.expected_revision != current_revision {
            return Ok(CallToolResult::structured_error(json!({
                "code": "revision_conflict",
                "message": "request revision changed; read it again before writing",
                "currentRevision": current_revision,
            })));
        }

        let document: RequestDocument = serde_json::from_value(input.document)
            .map_err(|error| invalid(format!("invalid request document: {error}")))?;
        let assertions = match input.assertions {
            Some(value) => serde_json::from_value(value)
                .map_err(|error| invalid(format!("invalid assertions sidecar: {error}")))?,
            None if input.create => AssertionDocument::default(),
            None => AssertionDocument::load_for_request(&request).map_err(invalid)?,
        };
        let hooks = match input.hooks {
            Some(value) => serde_json::from_value(value)
                .map_err(|error| invalid(format!("invalid hooks sidecar: {error}")))?,
            None if input.create => HookDocument::default(),
            None => HookDocument::load_for_request(&request).map_err(invalid)?,
        };
        drop(read_lock);
        let mut effective = document.clone();
        hooks.apply_to(&mut effective);
        assertions.apply_to(&mut effective);
        let validation = validate_value(
            &root,
            &request,
            &effective,
            input.environment.as_deref(),
            input.allow_project_code,
        )?;
        let (document, assertions, hooks, revision) = if input.create {
            reqv1::save_request_document(&request, document, assertions, hooks, true)
        } else {
            reqv1::save_request_document_at_revision(
                &request,
                document,
                assertions,
                hooks,
                &input.expected_revision,
            )
        }
        .map_err(internal)?;

        Ok(CallToolResult::structured(json!({
            "written": true,
            "request": relative(&root, &request),
            "revision": revision,
            "document": document,
            "assertions": assertions,
            "hooks": hooks,
            "validation": validation,
            "ideRefreshRequired": true,
        })))
    }

    #[tool(
        name = "add_assertion",
        description = "Append one manual API test assertion to a request's assertion sidecar. Requires the current read_request revision; the request JSON and hooks stay unchanged.",
        output_schema = object_output_schema(),
        annotations(
            title = "Add ApiWright test",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn add_assertion(
        &self,
        Parameters(input): Parameters<AddAssertionInput>,
    ) -> Result<CallToolResult, String> {
        let root = project_root(&input.root)?;
        let request = request_path(&root, &input.request, false)?;
        let assertion: AssertionEntry = serde_json::from_value(input.assertion)
            .map_err(|error| invalid(format!("invalid assertion entry: {error}")))?;
        let update = update_request_assertions(&request, &input.expected_revision, |document| {
            if document.assertions.contains(&assertion) {
                return Err("this assertion is already present".to_string());
            }
            document.assertions.push(assertion.clone());
            Ok(document.assertions.len() - 1)
        });
        let (assertions, revision, index) = match update {
            Ok(value) => value,
            Err(error) if error.starts_with("revision_conflict:") => {
                let current = reqv1::request_revision(&request)?;
                return Ok(CallToolResult::structured_error(json!({
                    "code": "revision_conflict",
                    "message": "request revision changed; read it again before editing assertions",
                    "currentRevision": current,
                })));
            }
            Err(error) => return Err(invalid(error)),
        };
        Ok(CallToolResult::structured(json!({
            "request": relative(&root, &request),
            "index": index,
            "assertion": assertions.assertions[index],
            "revision": revision,
            "ideRefreshRequired": true,
        })))
    }

    #[tool(
        name = "remove_assertion",
        description = "Remove one manual API test by its zero-based index from read_request. Requires the current revision and preserves every other assertion.",
        output_schema = object_output_schema(),
        annotations(
            title = "Remove ApiWright test",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn remove_assertion(
        &self,
        Parameters(input): Parameters<RemoveAssertionInput>,
    ) -> Result<CallToolResult, String> {
        let root = project_root(&input.root)?;
        let request = request_path(&root, &input.request, false)?;
        let update = update_request_assertions(&request, &input.expected_revision, |document| {
            if input.index >= document.assertions.len() {
                return Err(format!(
                    "assertion index {} is out of range for {} assertion(s)",
                    input.index,
                    document.assertions.len()
                ));
            }
            Ok(document.assertions.remove(input.index))
        });
        let (assertions, revision, removed) = match update {
            Ok(value) => value,
            Err(error) if error.starts_with("revision_conflict:") => {
                let current = reqv1::request_revision(&request)?;
                return Ok(CallToolResult::structured_error(json!({
                    "code": "revision_conflict",
                    "message": "request revision changed; read it again before editing assertions",
                    "currentRevision": current,
                })));
            }
            Err(error) => return Err(invalid(error)),
        };
        Ok(CallToolResult::structured(json!({
            "request": relative(&root, &request),
            "removed": removed,
            "remaining": assertions.assertions.len(),
            "revision": revision,
            "ideRefreshRequired": true,
        })))
    }

    #[tool(
        name = "delete_request",
        description = "Delete one request and its adjacent assertion, hook, environment, OpenAPI, Jira, and ticket sidecars after a revision check. Requests used by sequences or project authentication are protected unless allowBrokenReferences is enabled.",
        output_schema = object_output_schema(),
        annotations(
            title = "Delete ApiWright request",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn delete_request(
        &self,
        Parameters(input): Parameters<DeleteRequestInput>,
    ) -> Result<CallToolResult, String> {
        let root = project_root(&input.root)?;
        let request = request_path(&root, &input.request, false)?;
        let _request_lock = reqv1::request_lock(&request).map_err(internal)?;
        let current_revision = reqv1::request_revision(&request)?;
        if input.expected_revision != current_revision {
            return Ok(CallToolResult::structured_error(json!({
                "code": "revision_conflict",
                "message": "request revision changed; read it again before deleting",
                "currentRevision": current_revision,
            })));
        }

        if !input.allow_broken_references {
            let index =
                ProjectIndex::scan(&root).map_err(|diagnostic| invalid(diagnostic.message))?;
            let used_by_sequence = index
                .sequences
                .iter()
                .flat_map(|sequence| sequence.requests.iter())
                .filter_map(|path| root.join(path).canonicalize().ok())
                .any(|path| path == request);
            let project =
                reqv1::load_project(&root).map_err(|diagnostic| invalid(diagnostic.message))?;
            let used_by_auth = project
                .auth
                .as_ref()
                .and_then(|auth| root.join(&auth.request).canonicalize().ok())
                .is_some_and(|path| path == request);
            if used_by_sequence || used_by_auth {
                return Err(invalid(
                    "request is used by a sequence or project authentication; set allowBrokenReferences after reviewing affected tests",
                ));
            }
        }

        let sidecars = request_sidecars(&request);
        for path in &sidecars {
            match std::fs::symlink_metadata(path) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    return Err(invalid(format!(
                        "refusing to delete symbolic link {}",
                        path.display()
                    )));
                }
                Ok(metadata) if !metadata.is_file() => {
                    return Err(invalid(format!("{} is not a regular file", path.display())));
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(internal(error)),
            }
        }
        for path in sidecars {
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(invalid(format!(
                        "cannot delete {}: {error}",
                        path.display()
                    )));
                }
            }
        }
        Ok(CallToolResult::structured(json!({
            "deleted": true,
            "request": input.request,
            "ideRefreshRequired": true,
        })))
    }

    #[tool(
        name = "run_request",
        description = "Run one saved ApiWright request, including all matrix cases. Uses its configured mock by default; realHttp sends potentially mutating external HTTP and requires user authorization.",
        output_schema = object_output_schema(),
        annotations(
            title = "Run ApiWright request",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn run_request(
        &self,
        Parameters(input): Parameters<RunInput>,
    ) -> Result<CallToolResult, String> {
        let root = project_root(&input.root)?;
        let request = request_path(&root, &input.request, false)?;
        let request_lock = reqv1::request_lock(&request).map_err(internal)?;
        let revision = reqv1::request_revision(&request)?;
        let document = reqv1::load_request_document(&request).map_err(invalid)?;
        ensure_revision(&request, &revision, "loading for execution")?;
        if input
            .expected_revision
            .as_ref()
            .is_some_and(|expected| expected != &revision)
        {
            return Ok(CallToolResult::structured_error(json!({
                "code": "revision_conflict",
                "message": "request revision changed; read and validate it again before running",
                "currentRevision": revision,
            })));
        }
        drop(request_lock);
        let environment =
            reqv1::load_request_environment(&root, &request, input.environment.as_deref())
                .map_err(|diagnostic| invalid(diagnostic.message))?;
        let engine = HttpEngine::new();
        let secret = super::make_secret_provider(&root);
        let auth = reqv1::AuthSession::with_project_code_allowed(input.allow_project_code);
        let mode = if input.real_http {
            RunMode::Http
        } else {
            RunMode::Mock
        };
        let cases = match reqv1::run_matrix_with_responses_in_session(
            &document,
            &root,
            &request,
            environment,
            &secret,
            &engine,
            mode,
            CancellationToken::new(),
            &auth,
        )
        .await
        {
            Ok(cases) => cases
                .into_iter()
                .map(|(matrix, result, _response)| json!({"matrix": matrix, "result": result}))
                .collect::<Vec<_>>(),
            Err(errors) => {
                return Ok(CallToolResult::structured_error(json!({
                    "code": "execution_error",
                    "request": relative(&root, &request),
                    "mode": if input.real_http { "http" } else { "mock" },
                    "cases": [],
                    "diagnostics": errors.0,
                })));
            }
        };

        Ok(CallToolResult::structured(json!({
            "request": relative(&root, &request),
            "mode": if input.real_http { "http" } else { "mock" },
            "cases": cases,
            "responseBodyIncluded": false,
        })))
    }

    #[tool(
        name = "run_sequence",
        description = "Run one saved request-v1 sequence in its declared order. Uses mocks unless realHttp is explicitly enabled, and keeps project JavaScript disabled until reviewed.",
        output_schema = object_output_schema(),
        annotations(
            title = "Run ApiWright sequence",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn run_sequence(
        &self,
        Parameters(input): Parameters<RunSequenceInput>,
    ) -> Result<CallToolResult, String> {
        let root = project_root(&input.root)?;
        let sequence = reqv1::read_project_file(&root, ProjectFileKind::Sequence, &input.sequence)
            .map_err(invalid)?;
        let document: SequenceDocument = serde_json::from_value(sequence.content)
            .map_err(|error| invalid(format!("invalid sequence {}: {error}", input.sequence)))?;
        let mut files = document
            .resolve_files(&root)
            .map_err(invalid)?
            .into_iter()
            .map(|path| {
                let relative_path = relative(&root, &path);
                request_path(&root, &relative_path, false)
            })
            .collect::<Result<Vec<_>, _>>()?;

        // Every request in one project shares the same project.json lock.
        // Locking per request deadlocks as soon as a sequence has two files.
        // Keep one exclusive lock until the sequence completes so request and
        // sidecar revisions remain stable for all steps.
        let _project_lock = reqv1::project_lock(&root).map_err(internal)?;
        let environments = files
            .iter()
            .map(|file| {
                reqv1::load_request_environment(&root, file, input.environment.as_deref())
                    .map_err(|diagnostic| invalid(diagnostic.message))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let engine = HttpEngine::new();
        let secret = super::make_secret_provider(&root);
        let auth = reqv1::AuthSession::with_project_code_allowed(input.allow_project_code);
        let mode = if input.real_http {
            RunMode::Http
        } else {
            RunMode::Mock
        };
        let results = reqv1::run_sequence_with_environment_values_in_session(
            &files,
            &root,
            &environments,
            &secret,
            &engine,
            mode,
            CancellationToken::new(),
            &auth,
        )
        .await
        .map_err(|diagnostic| invalid(diagnostic.message))?;
        let cases = files
            .drain(..)
            .zip(results)
            .map(|(file, (result, _response))| {
                json!({"request": relative(&root, &file), "result": result})
            })
            .collect::<Vec<_>>();
        Ok(CallToolResult::structured(json!({
            "sequence": input.sequence,
            "mode": if input.real_http { "http" } else { "mock" },
            "cases": cases,
            "responseBodyIncluded": false,
        })))
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for ApiWrightMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                "apiwright",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(
                "Operate on saved ApiWright request-v1 project files only. Inspect and read before writing. Paths must be project-relative. Dedicated secret files are never returned, but read_request returns the saved request verbatim and may reveal user-authored literal values; use only with a trusted AI client. Validation sends no HTTP. run_request uses mocks unless the user authorizes realHttp. Keep project code disabled until its JavaScript was inspected and explicitly trusted. The MCP sees saved files, not an IDE tab's unsaved buffer.",
            )
    }
}

pub async fn serve() -> anyhow::Result<()> {
    let service = ApiWrightMcp::new().serve(rmcp::transport::stdio()).await?;
    service.waiting().await?;
    Ok(())
}

fn project_root(value: &str) -> Result<PathBuf, String> {
    let root = Path::new(value);
    if !root.is_absolute() {
        return Err(invalid("root must be an absolute path"));
    }
    let root = root
        .canonicalize()
        .map_err(|error| invalid(format!("cannot resolve project root {value}: {error}")))?;
    let project = root.join("project.json");
    if project.is_symlink() {
        return Err(invalid("project.json must not be a symbolic link"));
    }
    if !project.is_file() {
        return Err(invalid(format!(
            "{} is not an ApiWright request-v1 project (project.json missing)",
            root.display()
        )));
    }
    Ok(root)
}

fn object_output_schema() -> std::sync::Arc<rmcp::model::JsonObject> {
    let mut schema = rmcp::model::JsonObject::new();
    schema.insert("type".to_string(), json!("object"));
    schema.insert("properties".to_string(), json!({}));
    schema.insert("additionalProperties".to_string(), json!(true));
    std::sync::Arc::new(schema)
}

fn request_sidecars(request: &Path) -> Vec<PathBuf> {
    let file_name = request
        .file_name()
        .map(|value| value.to_string_lossy())
        .unwrap_or_default();
    let mut paths = vec![
        reqv1::assertions_path(request),
        reqv1::hooks_path(request),
        request.with_file_name(format!(".{file_name}.forge-jira")),
        request.with_file_name(format!(".{file_name}.forge-environment")),
        request.with_file_name(format!(".{file_name}.forge-openapi")),
    ];
    paths.push(request.to_path_buf());
    paths
}

fn request_path(root: &Path, value: &str, create: bool) -> Result<PathBuf, String> {
    let relative = Path::new(value);
    if value.trim().is_empty()
        || relative.is_absolute()
        || !relative
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
        || !value.ends_with(".request.json")
    {
        return Err(invalid(
            "request must be a project-relative path ending in .request.json",
        ));
    }
    let path = root.join(relative);
    if create {
        if path.exists() {
            return Err(invalid(format!("{value} already exists")));
        }
        if [reqv1::assertions_path(&path), reqv1::hooks_path(&path)]
            .iter()
            .any(|sidecar| sidecar.exists() || sidecar.is_symlink())
        {
            return Err(invalid(format!(
                "cannot create {value} because a sibling sidecar already exists"
            )));
        }
        let relative_parent = relative
            .parent()
            .ok_or_else(|| invalid("request must have a parent directory"))?;
        let mut ancestor = root.to_path_buf();
        for component in relative_parent.components() {
            ancestor.push(component.as_os_str());
            if ancestor.is_symlink() {
                return Err(invalid(
                    "new request parent directories must not be symbolic links",
                ));
            }
        }
        let parent = path
            .parent()
            .ok_or_else(|| invalid("request must have a parent directory"))?
            .canonicalize()
            .map_err(|error| {
                invalid(format!(
                    "new request parent must already exist inside the project: {error}"
                ))
            })?;
        if !parent.starts_with(root) {
            return Err(invalid("request path escapes the project root"));
        }
        return Ok(path);
    }

    if path.is_symlink() {
        return Err(invalid("request path must not be a symbolic link"));
    }
    let path = path
        .canonicalize()
        .map_err(|error| invalid(format!("cannot resolve request {value}: {error}")))?;
    if !path.starts_with(root) || !path.is_file() {
        return Err(invalid("request must be a file inside the project root"));
    }
    for sidecar in [reqv1::assertions_path(&path), reqv1::hooks_path(&path)] {
        if sidecar.is_symlink() {
            return Err(invalid(
                "request assertion and hook sidecars must not be symbolic links",
            ));
        }
    }
    Ok(path)
}

fn validate_value(
    root: &Path,
    request: &Path,
    document: &RequestDocument,
    environment: Option<&str>,
    allow_project_code: bool,
) -> Result<Value, String> {
    if document.uses_project_code() && !allow_project_code {
        return Ok(json!({
            "valid": null,
            "skipped": "request uses project JavaScript; inspect it and set allowProjectCode only after explicit trust",
        }));
    }
    let environment = reqv1::load_request_environment(root, request, environment)
        .map_err(|diagnostic| invalid(diagnostic.message))?;
    let placeholder_secret = |_name: &str| Some("<secret>".to_string());
    Ok(
        match reqv1::validate(document, root, request, environment, &placeholder_secret) {
            Ok(resolved) => json!({
                "valid": true,
                "diagnostics": [],
                "resolved": {
                    "id": resolved.id,
                    "name": resolved.name,
                    "method": resolved.method.to_string(),
                    "headerCount": resolved.headers.len(),
                    "pipelineSteps": resolved.pipeline.len(),
                }
            }),
            Err(diagnostics) => json!({
                "valid": false,
                "diagnostics": diagnostics,
            }),
        },
    )
}

fn update_request_assertions<T>(
    request: &Path,
    expected_revision: &str,
    update: impl FnOnce(&mut AssertionDocument) -> Result<T, String>,
) -> Result<(AssertionDocument, String, T), String> {
    let document_text = std::fs::read_to_string(request)
        .map_err(|error| format!("cannot read {}: {error}", request.display()))?;
    let document = RequestDocument::parse(&document_text)
        .map_err(|error| format!("invalid {}: {error}", request.display()))?;
    let mut assertions = AssertionDocument::load_for_request(request)?;
    let hooks = HookDocument::load_for_request(request)?;
    let value = update(&mut assertions)?;
    let (_, _, _, revision) = reqv1::save_request_document_at_revision(
        request,
        document,
        assertions.clone(),
        hooks,
        expected_revision,
    )
    .map_err(|error| {
        if error.contains("revision conflict") {
            "revision_conflict: request changed".to_string()
        } else {
            error
        }
    })?;
    Ok((assertions, revision, value))
}

fn ensure_revision(request: &Path, expected: &str, operation: &str) -> Result<(), String> {
    let current = reqv1::request_revision(request)?;
    if current == expected {
        Ok(())
    } else {
        Err(format!(
            "revision_conflict: request changed while {operation}; read it again (current revision {current})"
        ))
    }
}

fn selection_value(root: &Path, value: String, source: &Path) -> Value {
    json!({
        "value": value,
        "source": relative(root, source),
    })
}

fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn invalid(message: impl Into<String>) -> String {
    message.into()
}

fn internal(error: impl std::fmt::Display) -> String {
    error.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publishes_scoped_tools_with_output_schemas() {
        let server = ApiWrightMcp::new();
        let tools = server.tool_router.list_all();
        assert!(tools.iter().all(|tool| {
            tool.output_schema
                .as_ref()
                .is_some_and(|schema| schema.get("type") == Some(&json!("object")))
        }));
        let mut names = tools
            .into_iter()
            .map(|tool| tool.name.into_owned())
            .collect::<Vec<_>>();
        names.sort_unstable();
        assert_eq!(
            names,
            [
                "add_assertion".to_string(),
                "delete_project_file".to_string(),
                "delete_request".to_string(),
                "inspect_project".to_string(),
                "read_project_code".to_string(),
                "read_project_file".to_string(),
                "read_request".to_string(),
                "remove_assertion".to_string(),
                "run_request".to_string(),
                "run_sequence".to_string(),
                "validate_request".to_string(),
                "write_project_file".to_string(),
                "write_request".to_string(),
            ]
        );
    }

    #[test]
    fn identifies_apiwright_and_defaults_runs_to_mock_mode() {
        let info = ApiWrightMcp::new().get_info();
        assert_eq!(info.server_info.name, "apiwright");
        assert_eq!(info.server_info.version, env!("CARGO_PKG_VERSION"));

        let input: RunInput = serde_json::from_value(json!({
            "root": "/project",
            "request": "requests/x.request.json"
        }))
        .unwrap();
        assert!(!input.real_http);
    }

    #[cfg(unix)]
    #[test]
    fn request_paths_cannot_escape_through_parent_or_symlink() {
        use std::os::unix::fs::symlink;

        let project = tempfile::tempdir().unwrap();
        std::fs::write(project.path().join("project.json"), "{}").unwrap();
        std::fs::create_dir(project.path().join("requests")).unwrap();
        let outside = tempfile::tempdir().unwrap();
        let outside_request = outside.path().join("outside.request.json");
        std::fs::write(&outside_request, "{}").unwrap();
        symlink(
            &outside_request,
            project.path().join("requests/link.request.json"),
        )
        .unwrap();
        let root = project_root(project.path().to_str().unwrap()).unwrap();

        assert!(request_path(&root, "../outside.request.json", false).is_err());
        assert!(request_path(&root, "requests/link.request.json", false).is_err());
        symlink(
            project.path().join("requests"),
            project.path().join("request-alias"),
        )
        .unwrap();
        assert!(request_path(&root, "request-alias/new.request.json", true).is_err());

        let safe_request = project.path().join("requests/safe.request.json");
        std::fs::write(&safe_request, "{}").unwrap();
        symlink(
            outside.path().join("outside.assertions.json"),
            reqv1::assertions_path(&safe_request),
        )
        .unwrap();
        assert!(request_path(&root, "requests/safe.request.json", false).is_err());
    }

    #[test]
    fn request_revision_includes_sidecars() {
        let dir = tempfile::tempdir().unwrap();
        let request = dir.path().join("x.request.json");
        std::fs::write(&request, "{}").unwrap();
        let before = reqv1::request_revision(&request).unwrap();
        std::fs::write(reqv1::assertions_path(&request), "{}").unwrap();
        assert_ne!(before, reqv1::request_revision(&request).unwrap());
    }
}
