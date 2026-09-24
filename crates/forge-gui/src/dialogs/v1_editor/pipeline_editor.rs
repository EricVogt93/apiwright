//! Request-adjacent assertion and hook editing.

use super::*;

pub(super) fn assertions_editor(ui: &mut egui::Ui, d: &mut V1EditorState) {
    d.ensure_pipeline_row_ids();
    let snapshot = d.snapshot();
    let mut edit = None;
    ui.horizontal(|ui| {
        ui.strong(format!("Tests ({})", d.assertions.assertions.len()));
        if ui.button("+ Add test").clicked() {
            d.open_catalog(CatalogContext::Assertion);
        }
    });
    if let Some(file) = d.file.as_deref() {
        ui.weak(format!(
            "Stored in {}",
            forge_core::reqv1::assertions_path(file)
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
        ));
    }
    let mut remove = None;
    let mut move_entry = None;
    let mut changed = false;
    let assertion_count = d.assertions.assertions.len();
    for index in 0..assertion_count {
        let row_id = d.assertion_row_ids[index];
        let mut assertion = d.assertions.assertions[index].clone();
        let mut row_changed = false;
        ui.horizontal(|ui| {
            row_changed |= ui.checkbox(&mut assertion.enabled, "").changed();
            ui.label(RichText::new(catalog_entry_title(&assertion.uses)).strong());
            if ui
                .small_button("Replace…")
                .on_hover_text("Choose a different test asset")
                .clicked()
            {
                edit = Some(index);
            }
            if index > 0 && ui.small_button("↑").on_hover_text("Move test up").clicked() {
                move_entry = Some((index, index - 1));
            }
            if index + 1 < assertion_count
                && ui
                    .small_button("↓")
                    .on_hover_text("Move test down")
                    .clicked()
            {
                move_entry = Some((index, index + 1));
            }
            if ui.small_button("Remove").clicked() {
                remove = Some(index);
            }
        });
        egui::CollapsingHeader::new(format!("Parameters · {}", assertion.with.len()))
            .id_salt(("assertion-with", row_id))
            .show(ui, |ui| {
                row_changed |= inline_with_editor(
                    ui,
                    row_id,
                    &mut assertion.with,
                    &mut d.assertion_with_drafts,
                );
            });
        if row_changed {
            d.assertions.assertions[index] = assertion;
            changed = true;
        }
    }
    if let Some(index) = remove {
        let removed_id = d.assertion_row_ids.remove(index);
        d.assertions.assertions.remove(index);
        d.assertion_with_drafts.remove(&removed_id);
        if d.editing_assertion == Some(removed_id) {
            d.editing_assertion = None;
        }
        changed = true;
    }
    if let Some((from, to)) = move_entry {
        d.assertions.assertions.swap(from, to);
        d.assertion_row_ids.swap(from, to);
        changed = true;
    }
    if let Some(index) = edit {
        if let Err(error) = begin_assertion_edit(d, index) {
            d.catalog_error = Some(error);
        } else {
            d.catalog_open = true;
            d.catalog_context = CatalogContext::Assertion;
            d.catalog_intent = Some("Validate".to_string());
            d.catalog_view = CatalogView::All;
        }
    }
    if changed {
        d.dirty = true;
        d.record_undo(snapshot);
    }
    if d.assertions.assertions.is_empty() {
        ui.weak("No tests yet. Add a check here and configure it before it is saved.");
    }
}

pub(super) fn assertion_results(ui: &mut egui::Ui, d: &mut V1EditorState) {
    ui.label(RichText::new("TEST RESULTS").small().strong().weak());
    let Some(r) = selected_result(d) else {
        ui.weak("Run the request to see assertion results.");
        return;
    };
    if r.assertions.is_empty() {
        ui.weak("No assertion results.");
        return;
    }
    for a in &r.assertions {
        let (mark, color) = if a.passed {
            ("✓", egui::Color32::from_rgb(0x49, 0x9C, 0x54))
        } else {
            ("✗", ui.visuals().error_fg_color)
        };
        ui.horizontal(|ui| {
            ui.label(RichText::new(mark).color(color).strong());
            ui.label(&a.message);
        });
        if !a.passed {
            if let Some(exp) = &a.expected {
                ui.label(RichText::new(format!("    expected: {exp}")).small().weak());
            }
            if let Some(act) = &a.actual {
                ui.label(RichText::new(format!("    actual:   {act}")).small().weak());
            }
        }
    }
}

pub(super) fn hooks_editor(ui: &mut egui::Ui, d: &mut V1EditorState) {
    d.ensure_pipeline_row_ids();
    let snapshot = d.snapshot();
    let mut edit = None;
    ui.horizontal(|ui| {
        ui.strong(format!("Preparation and capture ({})", d.hooks.hooks.len()));
        if ui.button("+ Add preparation").clicked() {
            d.open_catalog(CatalogContext::Hook);
        }
    });
    if let Some(file) = d.file.as_deref() {
        ui.weak(format!(
            "Stored in {}",
            forge_core::reqv1::hooks_path(file)
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
        ));
    }
    let mut remove = None;
    let mut move_entry = None;
    let mut changed = false;
    let hook_count = d.hooks.hooks.len();
    for index in 0..hook_count {
        let row_id = d.hook_row_ids[index];
        let mut hook = d.hooks.hooks[index].clone();
        let mut row_changed = false;
        ui.horizontal(|ui| {
            row_changed |= ui.checkbox(&mut hook.enabled, "").changed();
            ui.label(RichText::new(catalog_entry_title(&hook.uses)).strong());
            ui.weak(target_label(BuiltinTarget::Pipeline(hook.phase)));
            if ui
                .small_button("Replace…")
                .on_hover_text("Choose a different preparation or capture asset")
                .clicked()
            {
                edit = Some(index);
            }
            if index > 0 && ui.small_button("↑").on_hover_text("Move step up").clicked() {
                move_entry = Some((index, index - 1));
            }
            if index + 1 < hook_count
                && ui
                    .small_button("↓")
                    .on_hover_text("Move step down")
                    .clicked()
            {
                move_entry = Some((index, index + 1));
            }
            if ui.small_button("Remove").clicked() {
                remove = Some(index);
            }
        });
        egui::CollapsingHeader::new(format!("Parameters · {}", hook.with.len()))
            .id_salt(("hook-with", row_id))
            .show(ui, |ui| {
                row_changed |=
                    inline_with_editor(ui, row_id, &mut hook.with, &mut d.hook_with_drafts);
            });
        if row_changed {
            d.hooks.hooks[index] = hook;
            changed = true;
        }
    }
    if let Some(index) = remove {
        let removed_id = d.hook_row_ids.remove(index);
        d.hooks.hooks.remove(index);
        d.hook_with_drafts.remove(&removed_id);
        if d.editing_hook == Some(removed_id) {
            d.editing_hook = None;
        }
        changed = true;
    }
    if let Some((from, to)) = move_entry {
        d.hooks.hooks.swap(from, to);
        d.hook_row_ids.swap(from, to);
        changed = true;
    }
    if let Some(index) = edit {
        if let Err(error) = begin_hook_edit(d, index) {
            d.catalog_error = Some(error);
        } else {
            d.catalog_open = true;
            d.catalog_context = CatalogContext::Hook;
            d.catalog_intent = None;
            d.catalog_view = CatalogView::All;
        }
    }
    if changed {
        d.dirty = true;
        d.record_undo(snapshot);
    }
    if d.hooks.hooks.is_empty() {
        ui.weak("No hooks configured. Add a built-in here or configure one in the catalog.");
    }
}

fn inline_with_editor(
    ui: &mut egui::Ui,
    row_id: u64,
    with: &mut serde_json::Map<String, serde_json::Value>,
    drafts: &mut std::collections::BTreeMap<u64, String>,
) -> bool {
    let initial = serde_json::to_string_pretty(&serde_json::Value::Object(with.clone()))
        .unwrap_or_else(|_| "{}".to_string());
    let draft = drafts.entry(row_id).or_insert(initial);
    let response = ui.add(
        egui::TextEdit::multiline(draft)
            .code_editor()
            .desired_rows(4)
            .desired_width(f32::INFINITY),
    );
    match serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(draft) {
        Ok(value) => {
            if response.changed() && *with != value {
                *with = value;
                true
            } else {
                false
            }
        }
        Err(error) => {
            ui.colored_label(
                ui.visuals().error_fg_color,
                format!("Parameters must be a JSON object: {error}"),
            );
            response.changed()
        }
    }
}

pub(super) fn begin_assertion_edit(d: &mut V1EditorState, index: usize) -> Result<(), String> {
    d.ensure_pipeline_row_ids();
    let assertion = d
        .assertions
        .assertions
        .get(index)
        .cloned()
        .ok_or_else(|| "the assertion no longer exists".to_string())?;
    if let Some(definition) = assertion
        .uses
        .strip_prefix("builtin:")
        .and_then(|reference| reference.split('@').next())
        .and_then(find_builtin)
    {
        select_builtin(d, definition);
        let parameters = definition
            .parameters
            .iter()
            .map(ParameterDefinition::builtin)
            .collect::<Vec<_>>();
        load_catalog_inputs(d, &assertion.with, &parameters);
    } else {
        let asset = d
            .index
            .as_ref()
            .and_then(|index| {
                index.assets.iter().find(|asset| {
                    asset.kind == AssetKind::Assertion && asset_reference(asset) == assertion.uses
                })
            })
            .cloned()
            .ok_or_else(|| format!("no catalog metadata found for {}", assertion.uses))?;
        let parameters = asset
            .metadata
            .as_ref()
            .map(|metadata| {
                metadata
                    .parameters
                    .iter()
                    .map(ParameterDefinition::project)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        select_project_asset(d, &asset);
        d.untyped_with_draft = serde_json::Value::Object(assertion.with.clone()).to_string();
        load_catalog_inputs(d, &assertion.with, &parameters);
    }
    d.editing_assertion = d.assertion_row_ids.get(index).copied();
    d.catalog_notice = Some("Editing configured assertion; save it in this form.".to_string());
    Ok(())
}

pub(super) fn begin_hook_edit(d: &mut V1EditorState, index: usize) -> Result<(), String> {
    d.ensure_pipeline_row_ids();
    let hook = d
        .hooks
        .hooks
        .get(index)
        .cloned()
        .ok_or_else(|| "the hook no longer exists".to_string())?;
    if let Some(definition) = hook
        .uses
        .strip_prefix("builtin:")
        .and_then(|reference| reference.split('@').next())
        .and_then(find_builtin)
    {
        if !matches!(definition.target, BuiltinTarget::Pipeline(_))
            || definition.intent == BuiltinIntent::Validate
        {
            return Err(format!("{} is not a hook", definition.title));
        }
        select_builtin(d, definition);
        let parameters = definition
            .parameters
            .iter()
            .map(ParameterDefinition::builtin)
            .collect::<Vec<_>>();
        load_catalog_inputs(d, &hook.with, &parameters);
    } else {
        let asset = d
            .index
            .as_ref()
            .and_then(|index| {
                index.assets.iter().find(|asset| {
                    matches!(asset.kind, AssetKind::Hook | AssetKind::Extractor)
                        && asset_reference(asset) == hook.uses
                })
            })
            .cloned()
            .ok_or_else(|| format!("no catalog metadata found for {}", hook.uses))?;
        let parameters = asset
            .metadata
            .as_ref()
            .map(|metadata| {
                metadata
                    .parameters
                    .iter()
                    .map(ParameterDefinition::project)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        select_project_asset(d, &asset);
        d.untyped_with_draft = serde_json::Value::Object(hook.with.clone()).to_string();
        load_catalog_inputs(d, &hook.with, &parameters);
    }
    d.editing_hook = d.hook_row_ids.get(index).copied();
    d.catalog_notice = Some("Editing configured hook; save it in this form.".to_string());
    Ok(())
}

pub(super) fn load_catalog_inputs(
    d: &mut V1EditorState,
    with: &serde_json::Map<String, serde_json::Value>,
    parameters: &[ParameterDefinition],
) {
    d.catalog_inputs.clear();
    for parameter in parameters {
        d.catalog_inputs.insert(
            parameter.name.clone(),
            with.get(&parameter.name)
                .map(|value| parameter_input_from_value(parameter.kind, value))
                .unwrap_or_default(),
        );
    }
}

pub(super) fn asset_reference(asset: &AssetEntry) -> &str {
    asset
        .alias
        .as_deref()
        .or(asset.prefix_ref.as_deref())
        .unwrap_or(&asset.rel_path)
}

pub(super) fn catalog_entry_title(reference: &str) -> &str {
    reference
        .strip_prefix("builtin:")
        .and_then(|reference| reference.split('@').next())
        .and_then(find_builtin)
        .map(|definition| definition.title)
        .unwrap_or(reference)
}
