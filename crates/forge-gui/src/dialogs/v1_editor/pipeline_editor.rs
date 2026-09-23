//! Request-adjacent assertion and hook editing.

use super::*;

pub(super) fn assertions_pane(ui: &mut egui::Ui, d: &mut V1EditorState) {
    let mut add = None;
    let mut edit = None;
    ui.horizontal(|ui| {
        ui.strong(format!("Configured ({})", d.assertions.assertions.len()));
        ui.menu_button("+ Add assertion", |ui| {
            for definition in builtin_catalog()
                .iter()
                .filter(|definition| definition.intent == BuiltinIntent::Validate)
            {
                if ui.button(definition.title).clicked() {
                    add = Some(*definition);
                    ui.close();
                }
            }
        });
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
    if let Some(definition) = add {
        match assertion_from_builtin(&definition) {
            Ok(assertion) => {
                d.assertions.push(assertion.clone());
                d.dirty = true;
                edit = d
                    .assertions
                    .assertions
                    .iter()
                    .position(|candidate| candidate == &assertion);
            }
            Err(error) => d.diagnostics.push(error),
        }
    }

    let mut remove = None;
    let mut changed = false;
    for (index, assertion) in d.assertions.assertions.iter_mut().enumerate() {
        ui.horizontal(|ui| {
            changed |= ui.checkbox(&mut assertion.enabled, "").changed();
            ui.label(RichText::new(catalog_entry_title(&assertion.uses)).strong());
            ui.weak(&assertion.uses);
            if ui.small_button("Edit in catalog").clicked() {
                edit = Some(index);
            }
            if ui.small_button("Remove").clicked() {
                remove = Some(index);
            }
        });
        if !assertion.with.is_empty() {
            ui.label(
                RichText::new(serde_json::Value::Object(assertion.with.clone()).to_string())
                    .monospace()
                    .small()
                    .weak(),
            );
        }
    }
    if let Some(index) = remove {
        d.assertions.assertions.remove(index);
        d.editing_assertion = match d.editing_assertion {
            Some(editing) if editing == index => None,
            Some(editing) if editing > index => Some(editing - 1),
            editing => editing,
        };
        changed = true;
    }
    if let Some(index) = edit {
        if let Err(error) = begin_assertion_edit(d, index) {
            d.catalog_error = Some(error);
        }
    }
    if changed {
        d.dirty = true;
    }
    if d.assertions.assertions.is_empty() {
        ui.weak("No assertions configured. Add a built-in here or configure one in the catalog.");
    }

    ui.add_space(8.0);
    ui.separator();
    ui.label(RichText::new("LAST RUN").small().strong().weak());
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

pub(super) fn hooks_pane(ui: &mut egui::Ui, d: &mut V1EditorState) {
    let mut add = None;
    let mut edit = None;
    ui.horizontal(|ui| {
        ui.strong(format!("Configured ({})", d.hooks.hooks.len()));
        ui.menu_button("+ Add hook", |ui| {
            for definition in builtin_catalog().iter().filter(|definition| {
                matches!(
                    definition.intent,
                    BuiltinIntent::Prepare | BuiltinIntent::Capture
                ) && matches!(definition.target, BuiltinTarget::Pipeline(_))
            }) {
                if ui.button(definition.title).clicked() {
                    add = Some(*definition);
                    ui.close();
                }
            }
        });
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
    if let Some(definition) = add {
        match hook_from_builtin(&definition) {
            Ok(hook) => {
                d.hooks.push(hook.clone());
                d.dirty = true;
                edit = d.hooks.hooks.iter().position(|candidate| {
                    candidate.phase == hook.phase
                        && candidate.uses == hook.uses
                        && candidate.with == hook.with
                        && candidate.enabled == hook.enabled
                });
            }
            Err(error) => d.diagnostics.push(error),
        }
    }

    let mut remove = None;
    let mut changed = false;
    for (index, hook) in d.hooks.hooks.iter_mut().enumerate() {
        ui.horizontal(|ui| {
            changed |= ui.checkbox(&mut hook.enabled, "").changed();
            ui.label(RichText::new(catalog_entry_title(&hook.uses)).strong());
            ui.weak(target_label(BuiltinTarget::Pipeline(hook.phase)));
            ui.weak(&hook.uses);
            if ui.small_button("Edit in catalog").clicked() {
                edit = Some(index);
            }
            if ui.small_button("Remove").clicked() {
                remove = Some(index);
            }
        });
        if !hook.with.is_empty() {
            ui.label(
                RichText::new(serde_json::Value::Object(hook.with.clone()).to_string())
                    .monospace()
                    .small()
                    .weak(),
            );
        }
    }
    if let Some(index) = remove {
        d.hooks.hooks.remove(index);
        d.editing_hook = match d.editing_hook {
            Some(editing) if editing == index => None,
            Some(editing) if editing > index => Some(editing - 1),
            editing => editing,
        };
        changed = true;
    }
    if let Some(index) = edit {
        if let Err(error) = begin_hook_edit(d, index) {
            d.catalog_error = Some(error);
        }
    }
    if changed {
        d.dirty = true;
    }
    if d.hooks.hooks.is_empty() {
        ui.weak("No hooks configured. Add a built-in here or configure one in the catalog.");
    }
}

pub(super) fn assertion_from_builtin(
    definition: &BuiltinDefinition,
) -> Result<AssertionEntry, String> {
    let with = serde_json::from_str::<serde_json::Value>(definition.example)
        .map_err(|error| format!("invalid catalog example for {}: {error}", definition.name))?
        .as_object()
        .cloned()
        .ok_or_else(|| format!("catalog example for {} is not an object", definition.name))?;
    Ok(AssertionEntry {
        uses: definition.reference.to_string(),
        with,
        enabled: true,
    })
}

pub(super) fn hook_from_builtin(definition: &BuiltinDefinition) -> Result<PipelineEntry, String> {
    let BuiltinTarget::Pipeline(phase) = definition.target else {
        return Err(format!("{} is not a hook", definition.title));
    };
    let with = serde_json::from_str::<serde_json::Value>(definition.example)
        .map_err(|error| format!("invalid catalog example for {}: {error}", definition.name))?
        .as_object()
        .cloned()
        .ok_or_else(|| format!("catalog example for {} is not an object", definition.name))?;
    Ok(PipelineEntry {
        phase,
        uses: definition.reference.to_string(),
        with,
        enabled: true,
    })
}

pub(super) fn begin_assertion_edit(d: &mut V1EditorState, index: usize) -> Result<(), String> {
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
                    asset.kind == AssetKind::Assertion
                        && asset_reference(asset) == assertion.uses
                        && asset.metadata.is_some()
                })
            })
            .cloned()
            .ok_or_else(|| format!("no catalog metadata found for {}", assertion.uses))?;
        let parameters = asset
            .metadata
            .as_ref()
            .ok_or_else(|| format!("no catalog metadata found for {}", assertion.uses))?
            .parameters
            .iter()
            .map(ParameterDefinition::project)
            .collect::<Vec<_>>();
        select_project_asset(d, &asset);
        load_catalog_inputs(d, &assertion.with, &parameters);
    }
    d.editing_assertion = Some(index);
    d.catalog_notice = Some("Editing configured assertion; save it in this form.".to_string());
    Ok(())
}

pub(super) fn begin_hook_edit(d: &mut V1EditorState, index: usize) -> Result<(), String> {
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
                        && asset.metadata.is_some()
                })
            })
            .cloned()
            .ok_or_else(|| format!("no catalog metadata found for {}", hook.uses))?;
        let parameters = asset
            .metadata
            .as_ref()
            .ok_or_else(|| format!("no catalog metadata found for {}", hook.uses))?
            .parameters
            .iter()
            .map(ParameterDefinition::project)
            .collect::<Vec<_>>();
        select_project_asset(d, &asset);
        load_catalog_inputs(d, &hook.with, &parameters);
    }
    d.editing_hook = Some(index);
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
