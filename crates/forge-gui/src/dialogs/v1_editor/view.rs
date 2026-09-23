//! Editor layout and toolbar rendering.

use super::*;

pub(super) fn editor_column_widths(available_width: f32) -> (f32, f32) {
    let editor_area_width = available_width.max(0.0);
    let usable_width = (editor_area_width - EDITOR_COLUMN_GAP).max(0.0);
    let catalog_width = (editor_area_width * 0.34)
        .clamp(250.0, 330.0)
        .min((usable_width - 280.0).max(0.0))
        .min(usable_width);
    let request_width = usable_width - catalog_width;
    (catalog_width, request_width)
}

pub(super) fn zoom_editor_font_size(current: f32, zoom_delta: f32) -> f32 {
    (current * zoom_delta).clamp(9.0, 24.0)
}

/// Render the central v1 editor if open.
pub fn show(ui: &mut egui::Ui, state: &mut AppState, bridge: &Bridge) {
    if !state.dialogs.v1_editor.open {
        return;
    }
    let mut pending_insert: Option<PendingInsert> = None;
    let mut refresh_project = false;
    let mut zoom_target_hovered = false;
    let accent = state.theme.accent_color();
    let mut editor_font_size = state.editor_font_size;
    let auto_save = state.auto_save;
    let show_right_tools = !state.zen_mode || state.zen_right_revealed;
    let show_catalog = !state.zen_mode || state.zen_left_revealed;
    let configured_openapi = state.openapi.clone();
    let configured_openapi_source = state.openapi_source.clone();
    let configured_openapi_error = state.openapi_error.clone();
    let selected_project_dir = state.assets.selected_directory();
    let d = &mut state.dialogs.v1_editor;
    d.auto_save = auto_save;
    if let Some(source) = configured_openapi_source {
        let source = PathBuf::from(source);
        if d.openapi_source.as_ref() != Some(&source)
            || (d.openapi.is_none() && configured_openapi.is_some())
            || d.openapi_error != configured_openapi_error
        {
            d.openapi_source = Some(source);
            d.openapi = configured_openapi;
            d.openapi_error = configured_openapi_error;
            d.auto_covered_operations =
                scan_covered_operations(d.root.as_deref(), d.index.as_ref(), d.openapi.as_ref());
        }
    }
    refresh_editor_validation(d, ui.ctx());
    let surface_stroke = ui.visuals().widgets.noninteractive.bg_stroke;
    egui::Frame::NONE
        .fill(ui.visuals().panel_fill)
        .stroke(surface_stroke)
        .corner_radius(8)
        .inner_margin(egui::Margin::symmetric(12, 8))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("REQUEST").small().strong().color(accent));
                ui.label(
                    RichText::new(
                        d.root
                            .as_deref()
                            .zip(d.file.as_deref())
                            .and_then(|(root, file)| file.strip_prefix(root).ok())
                            .unwrap_or_else(|| d.file.as_deref().unwrap_or_else(|| Path::new("")))
                            .display()
                            .to_string(),
                    )
                    .strong(),
                );
                if d.dirty {
                    ui.label(RichText::new(icons::DIRTY).color(accent));
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .small_button(icons::CLOSE)
                        .on_hover_text("Close request")
                        .clicked()
                    {
                        if !d.dirty || (d.auto_save && save_now(d)) {
                            d.open = false;
                        } else if !d.auto_save {
                            d.diagnostics =
                                vec!["save the request before closing to keep your edits"
                                    .to_string()];
                            d.result_tab = ResultTab::Diagnostics;
                        }
                    }
                });
            });
        });
    ui.add_space(8.0);

    if let Some(saved) = &d.save_conflict {
        ui.colored_label(
            ui.visuals().error_fg_color,
            "Save conflict: your edits are retained. Compare the saved version before reloading.",
        );
        ui.collapsing(
            "Compare saved version (including hooks and assertions)",
            |ui| {
                let mut saved_text = saved.as_str();
                ui.add(
                    TextEdit::multiline(&mut saved_text)
                        .code_editor()
                        .desired_rows(12)
                        .desired_width(f32::INFINITY),
                );
            },
        );
    }

    if show_right_tools {
        let mut right_panel_open = d.right_panel_open;
        let collapsed_panel = egui::Panel::right("v1-right-tools-collapsed")
            .exact_size(38.0)
            .resizable(true);
        let expanded_panel = egui::Panel::right("v1-right-tools-expanded")
            .default_size(300.0)
            .resizable(true)
            .size_range(240.0..=460.0);
        let (requested_open, generated_suite) = egui::Panel::show_switched(
            ui,
            &mut right_panel_open,
            collapsed_panel,
            expanded_panel,
            |ui, expanded| right_sidebar(ui, d, bridge, selected_project_dir.as_deref(), expanded),
        )
        .inner;
        if let Some(open) = requested_open {
            right_panel_open = open;
        }
        d.right_panel_open = right_panel_open;
        refresh_project |= generated_suite;
    }

    egui::CentralPanel::no_frame().show(ui, |ui| {
        let body_size = ui.available_size();
        let (catalog_width, request_width) = if show_catalog {
            editor_column_widths(body_size.x)
        } else {
            (0.0, body_size.x)
        };
        ui.with_layout(egui::Layout::left_to_right(egui::Align::Min), |ui| {
            if show_catalog {
                ui.allocate_ui_with_layout(
                    egui::vec2(catalog_width, body_size.y),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| {
                        egui::Frame::NONE
                            .fill(ui.visuals().panel_fill)
                            .stroke(surface_stroke)
                            .corner_radius(8)
                            .inner_margin(egui::Margin::same(12))
                            .show(ui, |ui| {
                                ui.set_min_height((body_size.y - 24.0).max(0.0));
                                ui.label(RichText::new("Catalog").size(18.0).strong())
                                    .on_hover_text(
                                    "Configure reusable behavior once and insert typed references.",
                                );
                                ui.add_space(8.0);
                                palette(ui, d, bridge, &mut pending_insert);
                            });
                    },
                );
                ui.add_space(10.0);
                ui.separator();
                ui.add_space(10.0);
            }
            ui.allocate_ui_with_layout(
                egui::vec2(request_width, body_size.y),
                egui::Layout::top_down(egui::Align::Min),
                |ui| {
                    ui.allocate_ui_with_layout(
                        egui::vec2(ui.available_width(), 48.0),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| {
                            egui::Frame::NONE
                                .fill(ui.visuals().panel_fill)
                                .stroke(surface_stroke)
                                .corner_radius(8)
                                .inner_margin(egui::Margin::symmetric(10, 7))
                                .show(ui, |ui| {
                                    let compact_toolbar = ui.available_width() < 620.0;
                                    let can_run = !d.in_flight && d.root.is_some();
                                    egui_extras::StripBuilder::new(ui)
                                        .size(egui_extras::Size::remainder())
                                        .size(egui_extras::Size::exact(TOOLBAR_MENU_CELL_WIDTH))
                                        .size(egui_extras::Size::exact(TOOLBAR_TRAILING_GUTTER))
                                        .clip(true)
                                        .horizontal(|mut strip| {
                                            strip.cell(|ui| {
                                                ui.horizontal(|ui| {
                                                    let run_label = if d.in_flight {
                                                        "Stop".to_string()
                                                    } else if document_has_matrix(&d.text)
                                                    {
                                                        format!("{}  Run matrix", icons::PLAY)
                                                    } else {
                                                        format!("{}  Run", icons::PLAY)
                                                    };
                                                    if ui
                                                        .add_enabled(
                                                            can_run || d.in_flight,
                                                            crate::theme::primary_button(
                                                                run_label, accent,
                                                            ),
                                                        )
                                                        .on_hover_text(if d.in_flight {
                                                            "Cancel the active request, matrix, sequence or batch"
                                                        } else if document_has_matrix(&d.text) {
                                                            "Execute every case in the request matrix"
                                                        } else {
                                                            "Execute the active request"
                                                        })
                                                        .clicked()
                                                    {
                                                        if d.in_flight {
                                                            cancel_now(d, bridge);
                                                        } else {
                                                            run_now(d, bridge);
                                                        }
                                                    }
                                                    if ui
                                                        .button(format!("{}  Save", icons::SAVE))
                                                        .on_hover_text("Save the request and its assertion and hook sidecars")
                                                        .clicked()
                                                    {
                                                        refresh_project = save_now(d);
                                                    }
                                                    if !compact_toolbar {
                                                        if ui.button("Format")
                                                            .on_hover_text("Beautify the request JSON")
                                                            .clicked() {
                                                            format_request(d);
                                                        }
                                                        if ui.button("Validate")
                                                            .on_hover_text("Validate JSON, references and OpenAPI compatibility")
                                                            .clicked() {
                                                            validate_now(d);
                                                        }
                                                    }

                                                    let envs: Vec<String> = d
                                                        .index
                                                        .as_ref()
                                                        .map(|index| index.environments.clone())
                                                        .unwrap_or_default();
                                                    let inherited =
                                                d.root.as_deref().zip(d.file.as_deref()).and_then(
                                                    |(root, file)| {
                                                        forge_core::reqv1::effective_environment(
                                                            root, file,
                                                        )
                                                        .ok()
                                                        .flatten()
                                                    },
                                                );
                                                    let selected =
                                                        d.env_name.clone().unwrap_or_else(|| {
                                                            inherited
                                                                .as_ref()
                                                                .map(|selection| {
                                                                    format!(
                                                                        "{} · inherited",
                                                                        selection.value
                                                                    )
                                                                })
                                                                .unwrap_or_else(|| {
                                                                    "Automatic · none".to_string()
                                                                })
                                                        });
                                                    let previous_env = d.env_name.clone();
                                                    egui::ComboBox::from_id_salt("v1-env")
                                                        .selected_text(selected)
                                                        .show_ui(ui, |ui| {
                                                            ui.selectable_value(
                                                                &mut d.env_name,
                                                                None,
                                                                "Automatic (properties)",
                                                            );
                                                            for env in &envs {
                                                                ui.selectable_value(
                                                                    &mut d.env_name,
                                                                    Some(env.clone()),
                                                                    env,
                                                                );
                                                            }
                                                        });
                                                    ui.response().on_hover_text("Override the environment inherited from project properties");
                                                    if d.env_name != previous_env {
                                                        d.clear_preview();
                                                    }
                                                    if d.in_flight {
                                                        ui.spinner();
                                                    }
                                                });
                                            });
                                            strip.cell(|ui| {
                                                ui.centered_and_justified(|ui| {
                                                    ui.menu_button(icons::ELLIPSIS, |ui| {
                                                        if compact_toolbar {
                                                            if ui.button("Format")
                                                                .on_hover_text("Beautify the request JSON")
                                                                .clicked() {
                                                                format_request(d);
                                                                ui.close();
                                                            }
                                                            if ui.button("Validate")
                                                                .on_hover_text("Validate JSON, references and OpenAPI compatibility")
                                                                .clicked() {
                                                                validate_now(d);
                                                                ui.close();
                                                            }
                                                            ui.separator();
                                                        }
                                                        ui.checkbox(
                                                            &mut d.mock,
                                                            "Use mock response",
                                                        )
                                                        .on_hover_text("Run the request against its deterministic mock instead of the network");
                                                        ui.checkbox(
                                                    &mut d.allow_project_code,
                                                    "Allow project code",
                                                )
                                                .on_hover_text(
                                                    "Executes reviewed project-owned JavaScript.",
                                                );
                                                        ui.separator();
                                                        if ui
                                                            .add_enabled(
                                                                can_run,
                                                                egui::Button::new("Run sequence…"),
                                                            )
                                                            .on_hover_text("Choose and execute an ordered request sequence")
                                                            .clicked()
                                                        {
                                                            run_sequence_now(d, bridge);
                                                            ui.close();
                                                        }
                                                    })
                                                    .response
                                                    .on_hover_text("Run mode and additional editor actions");
                                                });
                                            });
                                            strip.empty();
                                        });
                                });
                        },
                    );
                    ui.add_space(8.0);

                    let total_h = ui.available_height();
                    let top_h =
                        (total_h * d.split_ratio).clamp(180.0, (total_h - 120.0).max(180.0));
                    ui.allocate_ui(egui::vec2(ui.available_width(), top_h), |ui| {
                        ui.label(RichText::new("REQUEST").small().strong().weak());
                        let assist_height = request_editor_footer_height(d);
                        let editor_height = (ui.available_height() - assist_height).max(120.0);
                        let editor_content_height = (editor_height - 16.0).max(104.0);
                        let editor_frame = egui::Frame::NONE
                            .fill(ui.visuals().extreme_bg_color)
                            .stroke(surface_stroke)
                            .corner_radius(6)
                            .inner_margin(egui::Margin::same(8))
                            .show(ui, |ui| {
                                ui.set_min_height(editor_content_height);
                                let diagnostic = d.json_diagnostic.clone();
                                let mut editor_response = None;
                                egui_extras::StripBuilder::new(ui)
                                    .size(egui_extras::Size::remainder())
                                    .size(egui_extras::Size::exact(CODE_MINIMAP_WIDTH))
                                    .horizontal(|mut strip| {
                                        strip.cell(|ui| {
                                            editor_response = Some(
                                                egui::ScrollArea::both()
                                                    .id_salt("v1-json")
                                                    .max_height(editor_content_height)
                                                    .auto_shrink([false, false])
                                                    .show(ui, |ui| {
                                                        code_editor_numbered_diagnostic(
                                                            ui,
                                                            "v1-request-json",
                                                            &mut d.text,
                                                            Lang::Json,
                                                            None,
                                                            false,
                                                            18,
                                                            false,
                                                            diagnostic.as_ref(),
                                                        )
                                                    })
                                                    .inner,
                                            );
                                        });
                                        strip.cell(|ui| {
                                            code_minimap(
                                                ui,
                                                &d.text,
                                                diagnostic.as_ref(),
                                                ui.available_height(),
                                            );
                                        });
                                    });
                                editor_response.unwrap_or_else(|| {
                                    ui.allocate_response(egui::Vec2::ZERO, egui::Sense::hover())
                                })
                            });
                        zoom_target_hovered |= ui.rect_contains_pointer(editor_frame.response.rect);
                        let response = editor_frame.inner;
                        if response.changed() {
                            d.dirty = true;
                            d.clear_preview();
                            schedule_editor_validation(d, ui.ctx());
                        }
                        if let Some(diagnostic) = &d.json_diagnostic {
                            ui.colored_label(
                                ui.visuals().error_fg_color,
                                format!(
                                    "Line {}, column {}: {}",
                                    diagnostic.line, diagnostic.column, diagnostic.message
                                ),
                            );
                        }
                        openapi_assist(ui, d, &response);
                    });

                    let splitter = ui.allocate_response(
                        egui::vec2(ui.available_width(), 8.0),
                        egui::Sense::drag(),
                    );
                    ui.painter().hline(
                        splitter.rect.x_range(),
                        splitter.rect.center().y,
                        ui.visuals().widgets.noninteractive.bg_stroke,
                    );
                    if splitter.dragged() && total_h > 1.0 {
                        d.split_ratio =
                            ((top_h + splitter.drag_delta().y) / total_h).clamp(0.2, 0.85);
                    }
                    if splitter.hovered() || splitter.dragged() {
                        ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeVertical);
                    }

                    let results = ui.allocate_ui(
                        egui::vec2(ui.available_width(), ui.available_height()),
                        |ui| results_pane(ui, d, editor_font_size),
                    );
                    zoom_target_hovered |= ui.rect_contains_pointer(results.response.rect);
                },
            );
        });
    });

    if zoom_target_hovered {
        let zoom_delta = ui.input(|input| {
            if input.modifiers.ctrl {
                input.zoom_delta()
            } else {
                1.0
            }
        });
        if (zoom_delta - 1.0).abs() > f32::EPSILON {
            editor_font_size = zoom_editor_font_size(editor_font_size, zoom_delta);
            ui.ctx().request_repaint();
        }
    }

    if let Some(insert) = pending_insert {
        match apply_insert(d, insert) {
            Ok((text, notice)) => {
                if let Some(text) = text {
                    d.text = text;
                }
                d.dirty = true;
                d.catalog_error = None;
                d.catalog_notice = Some(notice);
                d.clear_preview();
            }
            Err(error) => d.catalog_error = Some(error),
        }
    }
    if refresh_project {
        if let Some(root) = state.assets.project_root() {
            state.assets.load(root);
        }
    }
    if (editor_font_size - state.editor_font_size).abs() > f32::EPSILON {
        state.editor_font_size = editor_font_size;
        crate::dialogs::settings::apply_typography(ui.ctx(), state);
    }
}

pub(super) fn right_sidebar(
    ui: &mut egui::Ui,
    d: &mut V1EditorState,
    bridge: &Bridge,
    selected_directory: Option<&Path>,
    expanded: bool,
) -> (Option<bool>, bool) {
    if !expanded {
        let mut requested_open = None;
        ui.vertical_centered(|ui| {
            if ui
                .small_button(icons::CODE)
                .on_hover_text("OpenAPI")
                .clicked()
            {
                d.right_tool = RightTool::OpenApi;
                requested_open = Some(true);
            }
            if ui
                .small_button(icons::CHECK)
                .on_hover_text("Contract tests")
                .clicked()
            {
                d.right_tool = RightTool::ContractTests;
                requested_open = Some(true);
            }
            if ui
                .small_button(icons::RUN)
                .on_hover_text("API tests")
                .clicked()
            {
                d.right_tool = RightTool::ApiTests;
                requested_open = Some(true);
            }
            if ui
                .small_button(icons::PULSE)
                .on_hover_text("Load & performance")
                .clicked()
            {
                d.right_tool = RightTool::Performance;
                requested_open = Some(true);
            }
            if ui
                .small_button(icons::CONSOLE)
                .on_hover_text("AI Advisor")
                .clicked()
            {
                d.right_tool = RightTool::Advisor;
                requested_open = Some(true);
            }
        });
        return (requested_open, false);
    }

    let mut requested_open = None;
    let mut generated_suite = false;
    egui::Frame::NONE
        .inner_margin(egui::Margin::same(12))
        .show(ui, |ui| {
            ui.set_min_height((ui.available_height() - 24.0).max(0.0));
            ui.horizontal(|ui| {
                ui.add_space(4.0);
                for (tool, icon) in [
                    (RightTool::OpenApi, icons::CODE),
                    (RightTool::ContractTests, icons::CHECK),
                    (RightTool::ApiTests, icons::RUN),
                    (RightTool::Performance, icons::PULSE),
                    (RightTool::Advisor, icons::CONSOLE),
                ] {
                    let active = d.right_tool == tool;
                    if ui
                        .selectable_label(active, icon)
                        .on_hover_text(tool.label())
                        .clicked()
                    {
                        d.right_tool = tool;
                    }
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .small_button(icons::TRIANGLE_RIGHT)
                        .on_hover_text("Collapse tool window")
                        .clicked()
                    {
                        requested_open = Some(false);
                    }
                });
            });
            ui.separator();
            ui.add_space(8.0);
            match d.right_tool {
                RightTool::OpenApi => openapi_sidebar(ui, d),
                RightTool::ContractTests => {
                    generated_suite |= generated_suite_sidebar(
                        ui,
                        d,
                        selected_directory,
                        forge_core::reqv1::OpenApiSuiteKind::Contract,
                    )
                }
                RightTool::ApiTests => {
                    generated_suite |= generated_suite_sidebar(
                        ui,
                        d,
                        selected_directory,
                        forge_core::reqv1::OpenApiSuiteKind::Api,
                    )
                }
                RightTool::Performance => {
                    generated_suite |= generated_suite_sidebar(
                        ui,
                        d,
                        selected_directory,
                        forge_core::reqv1::OpenApiSuiteKind::K6,
                    )
                }
                RightTool::Advisor => advisor_sidebar(ui, d, bridge),
            }
        });
    (requested_open, generated_suite)
}

pub(super) fn request_editor_footer_height(d: &V1EditorState) -> f32 {
    let diagnostic_height = if d.json_diagnostic.is_some() {
        28.0
    } else {
        0.0
    };
    let assist_height = if d.openapi_error.is_some() || d.openapi.is_none() {
        28.0
    } else if let (Some(spec), Some(document)) = (&d.openapi, &d.validated_document) {
        let matched = spec
            .find_operation(document.request.method, &document.request.url)
            .is_some();
        if matched || spec.suggest(&document.request.url).is_empty() {
            28.0
        } else {
            64.0
        }
    } else {
        0.0
    };
    diagnostic_height + assist_height
}
