//! Document loading, validation, and revision-aware persistence.

use super::*;

impl V1EditorState {
    pub(crate) fn reveal_right_tools(&mut self) {
        self.right_panel_open = true;
        self.right_tool = RightTool::OpenApi;
    }

    pub fn active_file(&self) -> Option<&Path> {
        self.file.as_deref()
    }

    pub fn last_execution_ms(&self) -> Option<u64> {
        self.last_response.as_ref().map(|response| response.time_ms)
    }

    pub fn has_unsaved_request_under(&self, directory: &Path) -> bool {
        self.dirty
            && self
                .file
                .as_deref()
                .is_some_and(|file| file.starts_with(directory))
    }

    pub fn set_regression(&mut self, file: &Path, enabled: bool) -> Result<bool, String> {
        if self.file.as_deref() != Some(file) {
            return Ok(false);
        }
        let mut document = forge_core::reqv1::RequestDocument::parse(&self.text)
            .map_err(|error| format!("invalid request JSON: {error}"))?;
        document.meta.set_regression(enabled);
        self.text = serialize_request(&document)?;
        self.dirty = true;
        if save_now(self) {
            Ok(true)
        } else {
            Err(self
                .diagnostics
                .first()
                .cloned()
                .unwrap_or_else(|| "failed to save regression property".to_string()))
        }
    }

    pub fn reload_clean_request_under(&mut self, directory: &Path) -> Result<(), String> {
        let Some(file) = self
            .file
            .as_deref()
            .filter(|file| file.starts_with(directory) && file.is_file())
        else {
            return Ok(());
        };
        if self.dirty {
            return Err("cannot reload a request with unsaved edits".to_string());
        }
        self.open_file(file.to_path_buf(), self.env_name.clone())
    }

    /// Open the editor on `file` (an existing document). Rescans its project.
    pub fn open_file(&mut self, file: PathBuf, active_env: Option<String>) -> Result<(), String> {
        if self.dirty && self.file.as_ref() != Some(&file) {
            if !self.auto_save {
                return Err(
                    "the current request has unsaved edits; save it before switching requests"
                        .to_string(),
                );
            }
            if !save_now(self) {
                return Err("auto-save failed; the current request remains open".to_string());
            }
        }
        let _lock = project_root_of(&file)
            .join("project.json")
            .is_file()
            .then(|| forge_core::reqv1::request_lock(&file))
            .transpose()?;
        let revision = forge_core::reqv1::request_revision(&file)?;
        let mut text = std::fs::read_to_string(&file)
            .map_err(|error| format!("failed to read {}: {error}", file.display()))?;
        let mut assertions = AssertionDocument::load_for_request(&file)?;
        let mut hooks = HookDocument::load_for_request(&file)?;
        let mut migrated = false;
        if let Ok(mut request) = forge_core::reqv1::RequestDocument::parse(&text) {
            let inline = AssertionDocument::take_from_request(&mut request);
            let inline_hooks = HookDocument::take_from_request(&mut request);
            if !inline.assertions.is_empty() {
                assertions.extend(inline);
                migrated = true;
            }
            if !inline_hooks.hooks.is_empty() {
                hooks.extend(inline_hooks);
                migrated = true;
            }
            if migrated {
                text = serialize_request(&request)?;
            }
        }
        if forge_core::reqv1::request_revision(&file)? != revision {
            return Err("request changed while loading; reload it".to_string());
        }
        self.revision = Some(revision);
        self.save_conflict = None;
        self.text = text;
        validate_editor_json(self);
        self.assertions = assertions;
        self.hooks = hooks;
        self.root = Some(project_root_of(&file));
        self.prepare_right_tools();
        self.env_name = active_env;
        self.file = Some(file);
        self.new_file = false;
        self.dirty = migrated;
        self.active_run = None;
        self.in_flight = false;
        self.results.clear();
        self.selected_result = 0;
        self.last_response = None;
        self.result_tab = ResultTab::Result;
        self.editing_assertion = None;
        self.editing_hook = None;
        self.auth_dirty = false;
        self.auth_notice = None;
        self.allow_project_code = false;
        self.clear_preview();
        self.diagnostics = self.load_index().err().into_iter().collect();
        self.right_tool = RightTool::OpenApi;
        self.right_panel_open = self.openapi.is_some();
        if self.split_ratio <= 0.0 {
            self.split_ratio = 0.6;
        }
        self.open = true;
        Ok(())
    }

    /// Open a new skeleton request at the next free conventional project path.
    pub fn open_new(&mut self, root: PathBuf, active_env: Option<String>) {
        let directory = root.join("requests");
        self.open_new_in(root, directory, active_env);
    }

    /// Open a new request inside a selected story folder under `requests/`.
    pub fn open_new_in(&mut self, root: PathBuf, directory: PathBuf, active_env: Option<String>) {
        if self.dirty {
            if !self.auto_save {
                self.diagnostics = vec![
                    "the current request has unsaved edits; save it before creating another request"
                        .to_string(),
                ];
                self.result_tab = ResultTab::Diagnostics;
                return;
            }
            if !save_now(self) {
                return;
            }
        }
        let requests = root.join("requests");
        let directory = if directory.starts_with(&requests) {
            directory
        } else {
            requests
        };
        self.text = SKELETON.to_string();
        validate_editor_json(self);
        self.assertions = AssertionDocument::default();
        self.hooks = HookDocument::default();
        self.file = Some(forge_core::reqv1::available_path(
            &directory,
            "new",
            ".request.json",
        ));
        self.new_file = true;
        self.revision = None;
        self.save_conflict = None;
        self.root = Some(root);
        self.prepare_right_tools();
        self.env_name = active_env;
        self.dirty = true;
        self.active_run = None;
        self.in_flight = false;
        self.results.clear();
        self.selected_result = 0;
        self.last_response = None;
        self.result_tab = ResultTab::Result;
        self.editing_assertion = None;
        self.editing_hook = None;
        self.auth_dirty = false;
        self.auth_notice = None;
        self.allow_project_code = false;
        self.clear_preview();
        self.diagnostics = self.load_index().err().into_iter().collect();
        self.right_tool = RightTool::OpenApi;
        self.right_panel_open = self.openapi.is_some();
        if self.split_ratio <= 0.0 {
            self.split_ratio = 0.6;
        }
        self.open = true;
    }

    /// Save the current request using its revision-checked persistence path.
    pub fn save(&mut self) -> bool {
        save_now(self)
    }

    pub fn has_unsaved_edits(&self) -> bool {
        self.dirty
    }

    pub(super) fn load_index(&mut self) -> Result<(), String> {
        let root = self.root.clone();
        if !self.auth_dirty {
            self.project_auth = match root.as_deref() {
                Some(root) => {
                    forge_core::reqv1::load_project(root)
                        .map_err(|diagnostic| diagnostic.message)?
                        .auth
                }
                None => None,
            };
        }
        self.index = match root.as_ref() {
            Some(root) => Some(
                ProjectIndex::scan(root)
                    .map_err(|diagnostic| format!("asset index: {}", diagnostic.message))?,
            ),
            None => None,
        };
        let (openapi_source, openapi, openapi_error) =
            root.as_deref().map(discover_openapi).unwrap_or_default();
        self.openapi_source = openapi_source;
        self.openapi = openapi;
        self.openapi_error = openapi_error;
        self.auto_covered_operations =
            scan_covered_operations(root.as_deref(), self.index.as_ref(), self.openapi.as_ref());
        self.marked_operations = root
            .as_deref()
            .and_then(|root| load_marked_operations(root).ok())
            .unwrap_or_default();
        Ok(())
    }

    /// Route a bridge `Evt::V1Run` outcome.
    pub fn handle_result(&mut self, run_id: u64, result: Result<V1RunOutput, String>) {
        if self.active_run != Some(run_id) {
            return;
        }
        self.in_flight = false;
        self.active_run = None;
        self.clear_preview();
        match result {
            Ok(output) => {
                self.results = output.items;
                self.selected_result = 0;
                self.last_response = self.results.first().and_then(|item| item.response.clone());
                self.result_tab = ResultTab::Result;
            }
            Err(e) => {
                self.diagnostics = vec![e];
                self.result_tab = ResultTab::Diagnostics;
            }
        }
    }

    /// Route a bridge `Evt::V1Preview` outcome and ignore stale replies.
    pub fn handle_preview(&mut self, preview_id: u64, result: Result<CatalogPreview, String>) {
        if self.active_preview != Some(preview_id) {
            return;
        }
        self.preview_in_flight = false;
        self.active_preview = None;
        match result {
            Ok(preview) => {
                self.preview = Some(preview);
                self.preview_error = None;
            }
            Err(error) => {
                self.preview = None;
                self.preview_error = Some(error);
            }
        }
    }

    pub fn handle_advisor(&mut self, advisor_id: u64, result: Result<String, String>) {
        if self.active_advisor != Some(advisor_id) {
            return;
        }
        self.active_advisor = None;
        match result {
            Ok(answer) => {
                self.advisor_answer = Some(answer);
                self.advisor_error = None;
            }
            Err(error) => {
                self.advisor_answer = None;
                self.advisor_error = Some(error);
            }
        }
    }

    pub(super) fn prepare_right_tools(&mut self) {
        self.right_panel_open = true;
        self.openapi_query.clear();
        self.openapi_filter = OpenApiFilter::All;
        self.openapi_operation = None;
        self.advisor_include_response = false;
        self.active_advisor = None;
        self.advisor_answer = None;
        self.advisor_error = None;
        self.suite_notice = None;
        self.suite_error = None;
        if self.advisor_question.is_empty() {
            self.advisor_question =
                "Review this request against its OpenAPI contract and suggest concrete fixes."
                    .to_string();
        }
        if let Some(root) = &self.root {
            match crate::advisor::load(root) {
                Ok(config) => self.advisor_config = config,
                Err(error) => self.advisor_error = Some(error),
            }
        }
    }

    pub(super) fn clear_preview(&mut self) {
        self.active_preview = None;
        self.preview_in_flight = false;
        self.preview = None;
        self.preview_error = None;
    }

    /// Run every request that uses a selected asset as independent matrix
    /// runs and show the combined result in this editor.
    pub fn run_affected(
        &mut self,
        root: PathBuf,
        files: Vec<PathBuf>,
        active_env: Option<String>,
        bridge: &Bridge,
    ) {
        if self.root.as_ref() != Some(&root) {
            self.open_new(root.clone(), active_env.clone());
        } else {
            self.open = true;
        }
        let run_id = self.next_run_id;
        self.next_run_id += 1;
        self.active_run = Some(run_id);
        self.in_flight = true;
        self.results.clear();
        self.selected_result = 0;
        self.last_response = None;
        self.diagnostics.clear();
        self.result_tab = ResultTab::Result;
        if let Err(error) = bridge.send(Cmd::RunV1Batch {
            run_id,
            root,
            files,
            env_name: active_env,
            mock: self.mock,
            allow_project_code: self.allow_project_code,
        }) {
            self.active_run = None;
            self.in_flight = false;
            self.diagnostics = vec![error];
            self.result_tab = ResultTab::Diagnostics;
        }
    }

    /// Run a persisted sequence after its document has resolved the ordered
    /// request paths.
    pub fn run_sequence(
        &mut self,
        root: PathBuf,
        files: Vec<PathBuf>,
        active_env: Option<String>,
        bridge: &Bridge,
    ) {
        if self.root.as_ref() != Some(&root) {
            self.open_new(root.clone(), active_env.clone());
        } else {
            self.open = true;
        }
        let run_id = self.next_run_id;
        self.next_run_id += 1;
        self.active_run = Some(run_id);
        self.in_flight = true;
        self.results.clear();
        self.selected_result = 0;
        self.last_response = None;
        self.diagnostics.clear();
        self.result_tab = ResultTab::Result;
        if let Err(error) = bridge.send(Cmd::RunV1Sequence {
            run_id,
            root,
            files,
            env_name: active_env,
            mock: self.mock,
            allow_project_code: self.allow_project_code,
        }) {
            self.active_run = None;
            self.in_flight = false;
            self.diagnostics = vec![error];
            self.result_tab = ResultTab::Diagnostics;
        }
    }
}

pub(super) fn serialize_request(
    document: &forge_core::reqv1::RequestDocument,
) -> Result<String, String> {
    let mut text = serde_json::to_string_pretty(document).map_err(|error| error.to_string())?;
    text.push('\n');
    Ok(text)
}

pub(super) fn pretty_json(text: &str) -> Result<String, String> {
    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|error| format!("invalid JSON: {error}"))?;
    let mut text = serde_json::to_string_pretty(&value).map_err(|error| error.to_string())?;
    text.push('\n');
    Ok(text)
}

pub(super) const EDITOR_VALIDATION_DELAY: Duration = Duration::from_millis(180);

pub(super) fn schedule_editor_validation(d: &mut V1EditorState, ctx: &egui::Context) {
    d.validation_due = Some(Instant::now() + EDITOR_VALIDATION_DELAY);
    ctx.request_repaint_after(EDITOR_VALIDATION_DELAY);
}

pub(super) fn refresh_editor_validation(d: &mut V1EditorState, ctx: &egui::Context) {
    if d.validated_text == d.text {
        return;
    }
    let now = Instant::now();
    let due = d
        .validation_due
        .get_or_insert(now + EDITOR_VALIDATION_DELAY);
    if now < *due {
        ctx.request_repaint_after(*due - now);
        return;
    }
    validate_editor_json(d);
}

pub(super) fn validate_editor_json(d: &mut V1EditorState) {
    d.validated_text.clone_from(&d.text);
    d.validation_due = None;
    match forge_core::reqv1::RequestDocument::parse(&d.text) {
        Ok(document) => {
            d.validated_document = Some(document);
            d.json_diagnostic = None;
        }
        Err(error) => {
            d.validated_document = None;
            d.json_diagnostic = Some(EditorDiagnostic {
                line: error.line().max(1),
                column: error.column().max(1),
                message: error.to_string(),
            });
        }
    }
}

pub(super) fn format_request(d: &mut V1EditorState) {
    match pretty_json(&d.text) {
        Ok(text) => {
            d.text = text;
            d.dirty = true;
            d.diagnostics.clear();
            d.clear_preview();
            validate_editor_json(d);
        }
        Err(error) => {
            d.diagnostics = vec![error];
            d.result_tab = ResultTab::Diagnostics;
        }
    }
}

pub(super) fn validate_now(d: &mut V1EditorState) {
    d.results.clear();
    d.selected_result = 0;
    d.diagnostics.clear();
    d.result_tab = ResultTab::Diagnostics;
    let (Some(root), Some(file)) = (d.root.clone(), d.file.clone().or_else(|| d.root.clone()))
    else {
        d.diagnostics = vec!["no project root".to_string()];
        return;
    };
    match effective_document(d) {
        Ok(doc) => {
            let permissive = |_n: &str| Some("<secret>".to_string());
            let env =
                forge_core::reqv1::load_request_environment(&root, &file, d.env_name.as_deref())
                    .unwrap_or(serde_json::Value::Null);
            match forge_core::reqv1::validate(&doc, &root, &file, env, &permissive) {
                Ok(ir) => {
                    d.diagnostics = vec![format!("ok — {} {}", ir.method, ir.url)];
                    if let Some(spec) = &d.openapi {
                        match spec.find_operation(doc.request.method, &doc.request.url) {
                            Some(operation) => d.diagnostics.extend(
                                openapi_request_issues(&doc, operation)
                                    .into_iter()
                                    .map(|issue| format!("[openapi] {issue}")),
                            ),
                            None => d
                                .diagnostics
                                .push("[openapi] request does not match an operation".to_string()),
                        }
                    }
                }
                Err(diags) => {
                    d.diagnostics = diags
                        .iter()
                        .map(|x| {
                            format!(
                                "[{}] {} {}",
                                x.code,
                                x.instance_path.clone().unwrap_or_default(),
                                x.message
                            )
                        })
                        .collect();
                }
            }
        }
        Err(e) => d.diagnostics = vec![format!("invalid JSON: {e}")],
    }
}

pub(super) fn save_now(d: &mut V1EditorState) -> bool {
    use forge_core::reqv1;
    let Some(path) = d.file.clone() else {
        d.diagnostics = vec!["no project request path".to_string()];
        return false;
    };
    let result = (|| -> Result<String, String> {
        if let Ok(document) = reqv1::RequestDocument::parse(&d.text) {
            let (document, assertions, hooks, revision) = if d.new_file {
                reqv1::save_request_document(
                    &path,
                    document,
                    d.assertions.clone(),
                    d.hooks.clone(),
                    true,
                )?
            } else {
                let expected = d
                    .revision
                    .as_deref()
                    .ok_or("missing saved revision; reload before saving")?;
                reqv1::save_request_document_at_revision(
                    &path,
                    document,
                    d.assertions.clone(),
                    d.hooks.clone(),
                    expected,
                )?
            };
            d.text = serialize_request(&document)?;
            d.assertions = assertions;
            d.hooks = hooks;
            Ok(revision)
        } else if d.new_file {
            reqv1::save_request_text(&path, &d.text, &d.assertions, &d.hooks, true)
        } else {
            let expected = d
                .revision
                .as_deref()
                .ok_or("missing saved revision; reload before saving")?;
            reqv1::save_request_text_at_revision(&path, &d.text, &d.assertions, &d.hooks, expected)
        }
    })();
    match result {
        Ok(revision) => {
            d.revision = Some(revision);
            d.save_conflict = None;
            d.new_file = false;
            d.dirty = false;
            validate_editor_json(d);
            d.diagnostics = d.load_index().err().into_iter().collect();
            true
        }
        Err(error) => {
            if error.contains("revision conflict") {
                d.save_conflict = Some(
                    reqv1::load_request_document(&path)
                        .and_then(|doc| serialize_request(&doc))
                        .or_else(|_| std::fs::read_to_string(&path).map_err(|e| e.to_string()))
                        .unwrap_or_else(|e| format!("Cannot read saved version: {e}")),
                );
            }
            d.diagnostics = vec![format!("failed to save {}: {error}", path.display())];
            d.result_tab = ResultTab::Diagnostics;
            false
        }
    }
}

pub(super) fn effective_document(
    d: &V1EditorState,
) -> Result<forge_core::reqv1::RequestDocument, String> {
    let mut document =
        forge_core::reqv1::RequestDocument::parse(&d.text).map_err(|error| error.to_string())?;
    let mut assertions = d.assertions.clone();
    assertions.extend(AssertionDocument::take_from_request(&mut document));
    let mut hooks = d.hooks.clone();
    hooks.extend(HookDocument::take_from_request(&mut document));
    hooks.apply_to(&mut document);
    assertions.apply_to(&mut document);
    Ok(document)
}
