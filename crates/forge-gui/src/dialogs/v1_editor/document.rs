//! Document loading, validation, and revision-aware persistence.

use super::*;

fn tab_identity(root: &Path, file: &Path) -> String {
    let relative = file.strip_prefix(root).unwrap_or(file);
    format!("{}::{}", root.display(), relative.display())
}

fn new_request_path(directory: &Path, editor: &V1EditorState) -> PathBuf {
    for suffix in 1usize.. {
        let stem = if suffix == 1 {
            "new".to_string()
        } else {
            format!("new-{suffix}")
        };
        let path = directory.join(format!("{stem}.request.json"));
        if !path.exists() && !editor.has_open_file(&path) {
            return path;
        }
    }
    unreachable!("request path suffix space is unbounded")
}

fn request_id_for_path(root: &Path, file: &Path) -> String {
    let relative = file.strip_prefix(root).unwrap_or(file);
    let mut id = relative
        .components()
        .filter_map(|component| component.as_os_str().to_str())
        .collect::<Vec<_>>()
        .join(".");
    if let Some(stem) = id.strip_suffix(".request.json") {
        id = stem.to_string();
    }
    id = id
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-') {
                character
            } else {
                '-'
            }
        })
        .collect();
    if id.is_empty() {
        "new-request".to_string()
    } else {
        id
    }
}

impl V1EditorState {
    pub(super) fn snapshot(&self) -> EditorSnapshot {
        EditorSnapshot {
            request: self.text.clone(),
            assertions: self.assertions.clone(),
            hooks: self.hooks.clone(),
            assertion_row_ids: self.assertion_row_ids.clone(),
            hook_row_ids: self.hook_row_ids.clone(),
            assertion_with_drafts: self.assertion_with_drafts.clone(),
            hook_with_drafts: self.hook_with_drafts.clone(),
            body_draft: self.body_draft.clone(),
            body_draft_origin: self.body_draft_origin.clone(),
            body_draft_error: self.body_draft_error.clone(),
            body_mode_drafts: self.body_mode_drafts.clone(),
        }
    }

    pub(super) fn record_undo(&mut self, snapshot: EditorSnapshot) {
        self.undo_stack.push(snapshot);
        if self.undo_stack.len() > 40 {
            self.undo_stack.remove(0);
        }
    }

    pub(super) fn undo_editor_change(&mut self) {
        let Some(snapshot) = self.undo_stack.pop() else {
            return;
        };
        self.text = snapshot.request;
        self.assertions = snapshot.assertions;
        self.hooks = snapshot.hooks;
        self.assertion_row_ids = snapshot.assertion_row_ids;
        self.hook_row_ids = snapshot.hook_row_ids;
        self.assertion_with_drafts = snapshot.assertion_with_drafts;
        self.hook_with_drafts = snapshot.hook_with_drafts;
        self.body_draft = snapshot.body_draft;
        self.body_draft_origin = snapshot.body_draft_origin;
        self.body_draft_error = snapshot.body_draft_error;
        self.body_mode_drafts = snapshot.body_mode_drafts;
        self.dirty = true;
        self.clear_preview();
        validate_editor_json(self);
    }

    pub(super) fn ensure_pipeline_row_ids(&mut self) {
        while self.assertion_row_ids.len() < self.assertions.assertions.len() {
            let row_id = self.alloc_pipeline_row_id();
            self.assertion_row_ids.push(row_id);
        }
        self.assertion_row_ids
            .truncate(self.assertions.assertions.len());
        while self.hook_row_ids.len() < self.hooks.hooks.len() {
            let row_id = self.alloc_pipeline_row_id();
            self.hook_row_ids.push(row_id);
        }
        self.hook_row_ids.truncate(self.hooks.hooks.len());
    }

    pub(super) fn alloc_pipeline_row_id(&mut self) -> u64 {
        self.next_pipeline_row_id = self.next_pipeline_row_id.wrapping_add(1).max(1);
        self.next_pipeline_row_id
    }

    pub(super) fn open_catalog(&mut self, context: CatalogContext) {
        self.catalog_open = true;
        self.catalog_context = context;
        self.catalog_view = if context == CatalogContext::Body {
            CatalogView::Project
        } else {
            CatalogView::All
        };
        self.catalog_intent = context.intent().map(str::to_string);
        if context != CatalogContext::General {
            store_catalog_inputs(self);
            self.selected_builtin = None;
            self.selected_project = None;
            self.editing_assertion = None;
            self.editing_hook = None;
            self.catalog_inputs.clear();
            self.catalog_query.clear();
        }
        self.catalog_error = None;
    }

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

    pub(crate) fn active_run_ids(&self) -> Vec<u64> {
        self.active_run
            .into_iter()
            .chain(self.tabs.iter().filter_map(|tab| tab.active_run))
            .collect()
    }

    pub(crate) fn run_id_for_tab(&self, id: &str) -> Option<u64> {
        if self.tab_id.as_deref() == Some(id) {
            self.active_run
        } else {
            self.tabs
                .iter()
                .find(|tab| tab.tab_id.as_deref() == Some(id))
                .and_then(|tab| tab.active_run)
        }
    }

    pub(crate) fn open_tabs(&self) -> Vec<super::EditorTabInfo> {
        self.tab_order
            .iter()
            .filter_map(|id| {
                let active = self.tab_id.as_ref() == Some(id);
                let tab = if active {
                    Some(self)
                } else {
                    self.tabs.iter().find(|tab| tab.tab_id.as_ref() == Some(id))
                }?;
                let file = tab.file.as_deref()?;
                let title = file
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| file.display().to_string());
                Some(super::EditorTabInfo {
                    id: id.clone(),
                    title,
                    dirty: tab.has_current_unsaved_edits(),
                    running: tab.in_flight,
                    active,
                })
            })
            .collect()
    }

    pub(crate) fn tab_paths(&self) -> Vec<String> {
        self.tab_order
            .iter()
            .filter_map(|id| {
                let tab = if self.tab_id.as_ref() == Some(id) {
                    Some(self)
                } else {
                    self.tabs.iter().find(|tab| tab.tab_id.as_ref() == Some(id))
                }?;
                let root = tab.root.as_deref()?;
                tab.file
                    .as_deref()?
                    .strip_prefix(root)
                    .ok()
                    .map(|relative| relative.to_string_lossy().into_owned())
            })
            .collect()
    }

    pub(crate) fn active_tab_path(&self) -> Option<String> {
        let root = self.root.as_deref()?;
        let file = self.file.as_deref()?;
        file.strip_prefix(root)
            .ok()
            .map(|relative| relative.to_string_lossy().into_owned())
    }

    pub(crate) fn search_items(&self) -> (Vec<(PathBuf, String)>, Vec<String>) {
        let Some(index) = &self.index else {
            return (Vec::new(), Vec::new());
        };
        let requests = index
            .requests
            .iter()
            .map(|request| {
                (
                    PathBuf::from(&request.path),
                    format!("{} / {}", request.rel_path, request.name),
                )
            })
            .collect();
        (requests, index.environments.clone())
    }

    pub(crate) fn activate_tab(&mut self, id: &str) {
        let Some(index) = self
            .tabs
            .iter()
            .position(|tab| tab.tab_id.as_deref() == Some(id))
        else {
            return;
        };
        self.swap_active_tab(index);
    }

    pub(crate) fn close_tab(&mut self, id: &str) {
        if self.tab_id.as_deref() == Some(id) {
            self.request_close();
        } else if let Some(index) = self
            .tabs
            .iter()
            .position(|tab| tab.tab_id.as_deref() == Some(id))
        {
            self.swap_active_tab(index);
            self.request_close();
        }
    }

    pub(crate) fn next_open_tab(&mut self) {
        self.cycle_open_tab(1);
    }

    pub(crate) fn previous_open_tab(&mut self) {
        self.cycle_open_tab(-1);
    }

    pub(crate) fn current_has_unsaved_edits(&self) -> bool {
        self.has_current_unsaved_edits()
    }

    fn cycle_open_tab(&mut self, delta: isize) {
        if self.tab_order.len() < 2 {
            return;
        }
        let current = self
            .tab_id
            .as_ref()
            .and_then(|active| self.tab_order.iter().position(|id| id == active))
            .unwrap_or(0);
        let next = (current as isize + delta).rem_euclid(self.tab_order.len() as isize) as usize;
        if let Some(id) = self.tab_order.get(next).cloned() {
            self.activate_tab(&id);
        }
    }

    pub(crate) fn restore_tabs(&mut self, root: &Path, paths: &[String], active: Option<&str>) {
        for path in paths {
            let _ = self.open_file(root.join(path), self.env_name.clone());
        }
        if let Some(active) = active {
            let file = root.join(active);
            let id = tab_identity(root, &file);
            self.activate_tab(&id);
        }
    }

    pub(crate) fn save_all_tabs(&mut self) -> bool {
        let active = self.tab_id.clone();
        for id in self.tab_order.clone() {
            if self.tab_id.as_deref() != Some(&id) {
                self.activate_tab(&id);
            }
            if self.has_current_unsaved_edits() && !self.save() {
                // Keep the tab whose save failed selected so its diagnostics
                // and recoverable draft stay visible to the user.
                return false;
            }
        }
        if let Some(active) = active.as_deref() {
            self.activate_tab(active);
        }
        true
    }

    pub(crate) fn save_before_pending_action(&mut self) -> bool {
        match self
            .pending_close_action
            .as_ref()
            .unwrap_or(&PendingEditorAction::Close)
        {
            PendingEditorAction::Close => self.save(),
            PendingEditorAction::SwitchWorkspace | PendingEditorAction::Quit => {
                self.save_all_tabs()
            }
        }
    }

    fn start_new_tab(&mut self) {
        let tabs = std::mem::take(&mut self.tabs);
        let order = std::mem::take(&mut self.tab_order);
        if self.file.is_some() {
            let mut previous = std::mem::take(self);
            previous.tabs.clear();
            previous.tab_order.clear();
            self.tabs = tabs;
            self.tabs.push(previous);
        } else {
            self.tabs = tabs;
        }
        self.tab_order = order;
    }

    fn swap_active_tab(&mut self, index: usize) {
        if index >= self.tabs.len() {
            return;
        }
        let mut tabs = std::mem::take(&mut self.tabs);
        let order = std::mem::take(&mut self.tab_order);
        let mut selected = tabs.remove(index);
        if self.root == selected.root {
            selected.project_auth = self.project_auth.clone();
            selected.auth_dirty = self.auth_dirty;
            selected.auth_notice = self.auth_notice.clone();
        }
        selected.tabs.clear();
        selected.tab_order.clear();
        let mut previous = std::mem::replace(self, selected);
        previous.tabs.clear();
        previous.tab_order.clear();
        if previous.file.is_some() {
            tabs.push(previous);
        }
        self.tabs = tabs;
        self.tab_order = order;
        self.sync_project_auth_to_tabs();
        self.open = true;
    }

    pub(super) fn sync_project_auth_to_tabs(&mut self) {
        let Some(root) = self.root.as_ref() else {
            return;
        };
        for tab in &mut self.tabs {
            if tab.root.as_ref() == Some(root) {
                tab.project_auth = self.project_auth.clone();
                tab.auth_dirty = self.auth_dirty;
                tab.auth_notice = self.auth_notice.clone();
            }
        }
    }

    fn close_current_tab(&mut self) {
        if let Some(id) = self.tab_id.take() {
            self.tab_order.retain(|open| open != &id);
        }
        if let Some(mut next) = self.tabs.pop() {
            let tabs = std::mem::take(&mut self.tabs);
            let order = std::mem::take(&mut self.tab_order);
            next.tabs.clear();
            next.tab_order.clear();
            let mut closed = std::mem::replace(self, next);
            closed.tabs.clear();
            self.tabs = tabs;
            self.tab_order = order;
            self.open = true;
        } else {
            let tabs = std::mem::take(&mut self.tabs);
            let order = std::mem::take(&mut self.tab_order);
            *self = Self::default();
            self.tabs = tabs;
            self.tab_order = order;
            self.open = false;
        }
    }

    pub fn has_unsaved_request_under(&self, directory: &Path) -> bool {
        (self.has_current_unsaved_edits()
            && self
                .file
                .as_deref()
                .is_some_and(|file| file.starts_with(directory)))
            || self
                .tabs
                .iter()
                .any(|tab| tab.has_unsaved_request_under(directory))
    }

    pub(crate) fn request_close(&mut self) {
        if self.has_current_unsaved_edits() {
            self.pending_close_action = Some(PendingEditorAction::Close);
            self.close_prompt_open = true;
            self.open = true;
        } else {
            self.close_current_tab();
        }
    }

    pub(super) fn discard_pending_editor_changes(&mut self) {
        let action = self
            .pending_close_action
            .as_ref()
            .cloned()
            .unwrap_or(PendingEditorAction::Close);
        match action {
            PendingEditorAction::Close => self.discard_current_edits(),
            PendingEditorAction::SwitchWorkspace | PendingEditorAction::Quit => {
                self.discard_all_edits();
            }
        }
    }

    fn discard_current_edits(&mut self) {
        self.dirty = false;
        self.body_draft = None;
        self.body_draft_origin = None;
        self.body_draft_error = None;
        self.body_mode_drafts.clear();
        self.assertion_with_drafts.clear();
        self.hook_with_drafts.clear();
        self.undo_stack.clear();
        self.clear_preview();
        if self.auth_dirty {
            self.project_auth = self
                .root
                .as_deref()
                .and_then(|root| forge_core::reqv1::load_project(root).ok())
                .and_then(|project| project.auth);
            self.auth_dirty = false;
            self.auth_notice = None;
            self.sync_project_auth_to_tabs();
        }
    }

    fn discard_all_edits(&mut self) {
        self.discard_current_edits();
        for tab in &mut self.tabs {
            tab.discard_all_edits();
        }
    }

    pub(crate) fn request_workspace_switch(&mut self, ctx: &egui::Context) {
        self.request_editor_action(PendingEditorAction::SwitchWorkspace, ctx);
    }

    pub(crate) fn request_quit(&mut self, ctx: &egui::Context) {
        self.request_editor_action(PendingEditorAction::Quit, ctx);
    }

    pub(super) fn request_editor_action(
        &mut self,
        action: PendingEditorAction,
        ctx: &egui::Context,
    ) {
        if self.has_unsaved_edits() {
            self.pending_close_action = Some(action);
            self.close_prompt_open = true;
            self.open = true;
        } else if let Err(error) = self.finish_editor_action(action, ctx) {
            self.diagnostics = vec![error];
        }
    }

    pub(super) fn finish_pending_editor_action(
        &mut self,
        ctx: &egui::Context,
    ) -> Result<(), String> {
        let action = self
            .pending_close_action
            .take()
            .unwrap_or(PendingEditorAction::Close);
        self.finish_editor_action(action, ctx)
    }

    fn finish_editor_action(
        &mut self,
        action: PendingEditorAction,
        ctx: &egui::Context,
    ) -> Result<(), String> {
        match action {
            PendingEditorAction::Close => self.close_current_tab(),
            PendingEditorAction::SwitchWorkspace => self.open = false,
            PendingEditorAction::Quit => {
                self.open = false;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
        Ok(())
    }

    pub(super) fn cancel_pending_editor_action(&mut self) {
        self.pending_close_action = None;
        self.close_prompt_open = false;
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
        if self.has_current_unsaved_edits() {
            return Err("cannot reload a request with unsaved edits".to_string());
        }
        self.open_file_impl(file.to_path_buf(), self.env_name.clone(), true)
    }

    /// Replace the active buffer with the saved version. `discard_edits` is
    /// explicit because loading after a revision conflict discards local work.
    pub fn reload_current_request(&mut self, discard_edits: bool) -> Result<(), String> {
        if !discard_edits && self.has_current_unsaved_edits() {
            return Err("cannot reload a request with unsaved edits".to_string());
        }
        let file = self
            .file
            .clone()
            .ok_or_else(|| "no project request is open".to_string())?;
        self.open_file_impl(file, self.env_name.clone(), true)
    }

    /// Open the editor on `file` (an existing document). Rescans its project.
    pub fn open_file(&mut self, file: PathBuf, active_env: Option<String>) -> Result<(), String> {
        self.open_file_impl(file, active_env, false)
    }

    fn open_file_impl(
        &mut self,
        file: PathBuf,
        active_env: Option<String>,
        reload_active: bool,
    ) -> Result<(), String> {
        let target_root = project_root_of(&file);
        let inherited_auth = (self.root.as_ref() == Some(&target_root)).then(|| {
            (
                self.project_auth.clone(),
                self.auth_dirty,
                self.auth_notice.clone(),
            )
        });
        let target_id = tab_identity(&target_root, &file);
        let is_active_file =
            self.tab_id.as_deref() == Some(&target_id) && self.file.as_ref() == Some(&file);
        if is_active_file && !reload_active {
            // Opening the active file again should focus its buffer. Reloading
            // from disk here would silently replace unsaved form/JSON edits.
            self.open = true;
            return Ok(());
        }
        if !is_active_file
            && self
                .tabs
                .iter()
                .any(|tab| tab.tab_id.as_deref() == Some(&target_id))
        {
            self.activate_tab(&target_id);
            return Ok(());
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
        if !reload_active {
            self.start_new_tab();
        }
        self.revision = Some(revision);
        self.save_conflict = None;
        self.text = text;
        validate_editor_json(self);
        self.assertions = assertions;
        self.hooks = hooks;
        self.assertion_row_ids.clear();
        self.hook_row_ids.clear();
        self.assertion_with_drafts.clear();
        self.hook_with_drafts.clear();
        self.ensure_pipeline_row_ids();
        self.root = Some(target_root);
        self.prepare_right_tools();
        self.env_name = active_env;
        self.file = Some(file);
        self.tab_id = Some(target_id.clone());
        if !self.tab_order.contains(&target_id) {
            self.tab_order.push(target_id);
        }
        self.new_file = false;
        self.dirty = migrated;
        self.active_run = None;
        self.in_flight = false;
        self.results.clear();
        self.selected_result = 0;
        self.last_response = None;
        self.last_run_request = None;
        self.last_run_mock = None;
        self.last_run_environment = None;
        self.last_run_at = None;
        self.result_tab = ResultTab::Result;
        self.undo_stack.clear();
        self.editing_assertion = None;
        self.editing_hook = None;
        self.catalog_open = false;
        self.close_prompt_open = false;
        self.pending_close_action = None;
        self.body_draft = None;
        self.body_draft_origin = None;
        self.body_draft_error = None;
        self.body_mode_drafts.clear();
        if let Some((auth, dirty, notice)) = inherited_auth {
            self.project_auth = auth;
            self.auth_dirty = dirty;
            self.auth_notice = notice;
        } else {
            self.auth_dirty = false;
            self.auth_notice = None;
        }
        self.allow_project_code = false;
        self.clear_preview();
        self.diagnostics = self.load_index().err().into_iter().collect();
        self.sync_project_auth_to_tabs();
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
        let inherited_auth = (self.root.as_ref() == Some(&root)).then(|| {
            (
                self.project_auth.clone(),
                self.auth_dirty,
                self.auth_notice.clone(),
            )
        });
        self.start_new_tab();
        let requests = root.join("requests");
        let directory = if directory.starts_with(&requests) {
            directory
        } else {
            requests
        };
        self.text = SKELETON.replace(
            "\"new.request\"",
            &serde_json::to_string(&request_id_for_path(
                &root,
                &new_request_path(&directory, self),
            ))
            .expect("request id is a string"),
        );
        validate_editor_json(self);
        self.body_draft = None;
        self.body_draft_origin = None;
        self.body_draft_error = None;
        self.body_mode_drafts.clear();
        self.assertions = AssertionDocument::default();
        self.hooks = HookDocument::default();
        self.assertion_row_ids.clear();
        self.hook_row_ids.clear();
        self.assertion_with_drafts.clear();
        self.hook_with_drafts.clear();
        self.ensure_pipeline_row_ids();
        self.file = Some(new_request_path(&directory, self));
        let file = self.file.as_deref().expect("new request path was set");
        let tab_id = tab_identity(&root, file);
        self.tab_id = Some(tab_id.clone());
        if !self.tab_order.contains(&tab_id) {
            self.tab_order.push(tab_id);
        }
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
        self.last_run_request = None;
        self.last_run_mock = None;
        self.last_run_environment = None;
        self.last_run_at = None;
        self.result_tab = ResultTab::Result;
        self.undo_stack.clear();
        self.editing_assertion = None;
        self.editing_hook = None;
        self.catalog_open = false;
        self.close_prompt_open = false;
        self.pending_close_action = None;
        if let Some((auth, dirty, notice)) = inherited_auth {
            self.project_auth = auth;
            self.auth_dirty = dirty;
            self.auth_notice = notice;
        } else {
            self.auth_dirty = false;
            self.auth_notice = None;
        }
        self.allow_project_code = false;
        self.clear_preview();
        self.diagnostics = self.load_index().err().into_iter().collect();
        self.sync_project_auth_to_tabs();
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
        self.has_current_unsaved_edits() || self.tabs.iter().any(V1EditorState::has_unsaved_edits)
    }

    fn has_open_file(&self, file: &Path) -> bool {
        self.file.as_deref() == Some(file) || self.tabs.iter().any(|tab| tab.has_open_file(file))
    }

    fn has_current_unsaved_edits(&self) -> bool {
        self.dirty
            || self.auth_dirty
            || self.body_draft_error.is_some()
            || self.invalid_pipeline_draft().is_some()
    }

    pub fn invalid_pipeline_draft(&self) -> Option<String> {
        for (row, draft) in &self.assertion_with_drafts {
            if let Err(error) =
                serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(draft)
            {
                return Some(format!("test parameters ({row}): {error}"));
            }
        }
        for (row, draft) in &self.hook_with_drafts {
            if let Err(error) =
                serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(draft)
            {
                return Some(format!("hook parameters ({row}): {error}"));
            }
        }
        None
    }

    pub fn run_block_error(&self) -> Option<String> {
        if let Some(error) = &self.body_draft_error {
            return Some(format!(
                "resolve the body form error before running: {error}"
            ));
        }
        self.invalid_pipeline_draft().map(|error| {
            format!("resolve the test or hook parameter draft before running: {error}")
        })
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
            if let Some(tab) = self
                .tabs
                .iter_mut()
                .find(|tab| tab.active_run == Some(run_id))
            {
                tab.handle_result(run_id, result);
            }
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
            if let Some(tab) = self
                .tabs
                .iter_mut()
                .find(|tab| tab.active_preview == Some(preview_id))
            {
                tab.handle_preview(preview_id, result);
            }
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
            if let Some(tab) = self
                .tabs
                .iter_mut()
                .find(|tab| tab.active_advisor == Some(advisor_id))
            {
                tab.handle_advisor(advisor_id, result);
            }
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
        if self.in_flight {
            return;
        }
        if let Some(error) = self.run_block_error() {
            self.diagnostics = vec![error];
            self.result_tab = ResultTab::Diagnostics;
            return;
        }
        if self.root.as_ref() != Some(&root) {
            self.open_new(root.clone(), active_env.clone());
        } else {
            self.open = true;
        }
        let run_id = crate::state::allocate_global_run_id();
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
        if self.in_flight {
            return;
        }
        if let Some(error) = self.run_block_error() {
            self.diagnostics = vec![error];
            self.result_tab = ResultTab::Diagnostics;
            return;
        }
        if self.root.as_ref() != Some(&root) {
            self.open_new(root.clone(), active_env.clone());
        } else {
            self.open = true;
        }
        let run_id = crate::state::allocate_global_run_id();
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
    if let Some(error) = &d.body_draft_error {
        d.diagnostics = vec![format!(
            "resolve the body form error before saving: {error}"
        )];
        d.result_tab = ResultTab::Diagnostics;
        return false;
    }
    if let Some(error) = d.invalid_pipeline_draft() {
        d.diagnostics = vec![format!(
            "resolve the test or hook parameter draft before saving: {error}"
        )];
        d.result_tab = ResultTab::Diagnostics;
        return false;
    }
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
            if d.auth_dirty {
                let auth_result = d
                    .root
                    .as_deref()
                    .ok_or_else(|| "no project root".to_string())
                    .and_then(|root| persist_project_auth(root, d.project_auth.as_ref()));
                if let Err(error) = auth_result {
                    d.diagnostics = vec![format!("failed to save project auth: {error}")];
                    d.result_tab = ResultTab::Diagnostics;
                    return false;
                }
                d.auth_dirty = false;
                d.auth_notice = Some("Project auth saved.".to_string());
                d.sync_project_auth_to_tabs();
            }
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
