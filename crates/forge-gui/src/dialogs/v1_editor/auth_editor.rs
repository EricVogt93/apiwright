//! Project authentication setup.

use super::*;

pub(super) fn auth_pane(ui: &mut egui::Ui, d: &mut V1EditorState) {
    ui.heading("Authentication").on_hover_text(
        "Fetches and caches a token, then refreshes it before a protected request can outlive it.",
    );
    ui.add_space(8.0);

    let current = current_request_path(d);
    let requests: Vec<(String, String)> = d
        .index
        .as_ref()
        .map(|index| {
            index
                .requests
                .iter()
                .map(|request| (request.rel_path.clone(), request.name.clone()))
                .collect()
        })
        .unwrap_or_default();
    if !requests
        .iter()
        .any(|(path, _)| path == &d.auth_request_choice)
    {
        d.auth_request_choice = d
            .project_auth
            .as_ref()
            .map(|auth| auth.request.clone())
            .filter(|path| requests.iter().any(|(candidate, _)| candidate == path))
            .or_else(|| requests.first().map(|(path, _)| path.clone()))
            .unwrap_or_default();
    }

    let mut activate = None;
    let mut create = false;
    let mut save = false;
    let mut disable = false;

    egui::Frame::NONE
        .fill(ui.visuals().faint_bg_color)
        .stroke(ui.visuals().widgets.noninteractive.bg_stroke)
        .corner_radius(7)
        .inner_margin(egui::Margin::same(10))
        .show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                if let Some(auth) = &d.project_auth {
                    ui.label(RichText::new(format!("{}  Active", icons::CHECK)).strong());
                    ui.monospace(&auth.request);
                    if ui.small_button("Disable").clicked() {
                        disable = true;
                    }
                } else {
                    ui.label("Not configured");
                }
                if ui
                    .add_enabled(
                        current.is_some() && !d.new_file,
                        egui::Button::new("Use current request"),
                    )
                    .on_disabled_hover_text("Save this request first")
                    .clicked()
                {
                    activate = current.clone();
                }
            });
        });

    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.strong("Source");
        egui::ComboBox::from_id_salt("auth-setup-source")
            .selected_text(d.auth_setup.label())
            .show_ui(ui, |ui| {
                ui.selectable_value(
                    &mut d.auth_setup,
                    AuthSetup::ExistingRequest,
                    AuthSetup::ExistingRequest.label(),
                );
                ui.selectable_value(
                    &mut d.auth_setup,
                    AuthSetup::Provider,
                    AuthSetup::Provider.label(),
                );
            });
    });

    match d.auth_setup {
        AuthSetup::ExistingRequest => {
            ui.horizontal_wrapped(|ui| {
                let selected = requests
                    .iter()
                    .find(|(path, _)| path == &d.auth_request_choice)
                    .map(|(path, name)| format!("{name} — {path}"))
                    .unwrap_or_else(|| "Select a request".to_string());
                egui::ComboBox::from_id_salt("auth-request-choice")
                    .selected_text(selected)
                    .width(320.0)
                    .show_ui(ui, |ui| {
                        for (path, name) in &requests {
                            ui.selectable_value(
                                &mut d.auth_request_choice,
                                path.clone(),
                                format!("{name} — {path}"),
                            );
                        }
                    });
                if ui
                    .add_enabled(
                        !d.auth_request_choice.is_empty(),
                        egui::Button::new("Use selected"),
                    )
                    .clicked()
                {
                    activate = Some(d.auth_request_choice.clone());
                }
            });
        }
        AuthSetup::Provider => {
            egui::Grid::new("auth-provider-form")
                .num_columns(2)
                .spacing([12.0, 7.0])
                .show(ui, |ui| {
                    ui.label("Provider");
                    egui::ComboBox::from_id_salt("auth-provider")
                        .selected_text(d.auth_draft.provider.label())
                        .show_ui(ui, |ui| {
                            for provider in AuthProvider::ALL {
                                ui.selectable_value(
                                    &mut d.auth_draft.provider,
                                    provider,
                                    provider.label(),
                                );
                            }
                        });
                    ui.end_row();

                    ui.label(d.auth_draft.provider.endpoint_label());
                    ui.text_edit_singleline(&mut d.auth_draft.endpoint);
                    ui.end_row();

                    if d.auth_draft.provider == AuthProvider::Keycloak {
                        ui.label("Realm");
                        ui.text_edit_singleline(&mut d.auth_draft.realm);
                        ui.end_row();
                    }

                    ui.label("Client ID");
                    ui.text_edit_singleline(&mut d.auth_draft.client_id);
                    ui.end_row();

                    ui.label("Client secret");
                    ui.add(TextEdit::singleline(&mut d.auth_draft.client_secret).password(true));
                    ui.end_row();

                    ui.label(d.auth_draft.provider.scope_label());
                    ui.text_edit_singleline(&mut d.auth_draft.scope);
                    ui.end_row();
                });
            if ui
                .button("Create and use auth request")
                .on_hover_text(format!(
                    "Stores the secret locally as {} in .env.local. Leave it empty to reuse an environment value.",
                    d.auth_draft.provider.secret_name()
                ))
                .clicked()
            {
                create = true;
            }
        }
    }

    if let Some(auth) = d.project_auth.as_mut() {
        ui.separator();
        egui::CollapsingHeader::new("Token and refresh settings")
            .id_salt("auth-runtime-settings")
            .show(ui, |ui| {
                let mut changed = false;
                egui::Grid::new("project-auth-form")
                    .num_columns(2)
                    .spacing([12.0, 7.0])
                    .show(ui, |ui| {
                        ui.label("Token JSONPath");
                        changed |= ui.text_edit_singleline(&mut auth.token_path).changed();
                        ui.end_row();

                        ui.label("Lifetime");
                        changed |= ui
                            .add(
                                egui::DragValue::new(&mut auth.lifetime_seconds)
                                    .range(1..=31_536_000)
                                    .suffix(" s"),
                            )
                            .changed();
                        ui.end_row();

                        ui.label("Refresh reserve");
                        changed |= ui
                            .add(
                                egui::DragValue::new(&mut auth.refresh_before_seconds)
                                    .range(0..=31_536_000)
                                    .suffix(" s"),
                            )
                            .changed();
                        ui.end_row();

                        ui.label("Apply to");
                        changed |= ui.text_edit_singleline(&mut auth.apply_to).changed();
                        ui.end_row();
                    });
                if changed {
                    d.auth_dirty = true;
                    d.auth_notice = None;
                }
                ui.label("Scope rules").on_hover_text(
                    "Apply to accepts a project-relative request folder or file. Explicit Authorization headers win.",
                );
                if ui
                    .add_enabled(d.auth_dirty, egui::Button::new("Save settings"))
                    .clicked()
                {
                    save = true;
                }
            });
    }

    if disable {
        d.project_auth = None;
        d.auth_dirty = true;
        save = true;
    }
    if let Some(request) = activate {
        activate_auth_request(d, request);
    }
    if create {
        create_provider_auth_request(d);
    }
    if save {
        save_project_auth(d);
    }
    if let Some(notice) = &d.auth_notice {
        ui.label(notice);
    }
}

pub(super) fn current_request_path(d: &V1EditorState) -> Option<String> {
    let root = d.root.as_ref()?;
    let file = d.file.as_ref()?;
    Some(
        file.strip_prefix(root)
            .ok()?
            .to_string_lossy()
            .replace('\\', "/"),
    )
}

pub(super) fn activate_auth_request(d: &mut V1EditorState, request: String) {
    let mut auth = d
        .project_auth
        .clone()
        .unwrap_or_else(|| ProjectAuthConfig::for_request(request.clone()));
    auth.request = request;
    d.project_auth = Some(auth);
    d.auth_dirty = true;
    d.auth_notice = None;
    save_project_auth(d);
}

pub(super) fn create_provider_auth_request(d: &mut V1EditorState) {
    let result = (|| {
        let root = d
            .root
            .as_deref()
            .ok_or_else(|| "no project root".to_string())?;
        let directory = root.join("requests/auth");
        std::fs::create_dir_all(&directory)
            .map_err(|error| format!("cannot create {}: {error}", directory.display()))?;
        let path = forge_core::reqv1::available_path(
            &directory,
            d.auth_draft.provider.file_stem(),
            ".request.json",
        );
        let stem = path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_suffix(".request.json"))
            .ok_or_else(|| "cannot derive auth request name".to_string())?;
        let document = provider_auth_document(&d.auth_draft, stem)?;
        let text = serialize_request(&document)?;
        std::fs::write(&path, text)
            .map_err(|error| format!("cannot write {}: {error}", path.display()))?;

        let after_write: Result<(String, ProjectAuthConfig), String> = (|| {
            if !d.auth_draft.client_secret.is_empty() {
                save_local_secret(
                    root,
                    d.auth_draft.provider.secret_name(),
                    &d.auth_draft.client_secret,
                )?;
            }
            let request = path
                .strip_prefix(root)
                .map_err(|error| error.to_string())?
                .to_string_lossy()
                .replace('\\', "/");
            let mut auth = d
                .project_auth
                .clone()
                .unwrap_or_else(|| ProjectAuthConfig::for_request(request.clone()));
            auth.request = request.clone();
            persist_project_auth(root, Some(&auth))?;
            Ok((request, auth))
        })();
        if after_write.is_err() {
            let _ = std::fs::remove_file(&path);
        }
        after_write
    })();

    match result {
        Ok((request, auth)) => {
            d.project_auth = Some(auth);
            d.auth_request_choice = request.clone();
            d.auth_draft.client_secret.clear();
            d.auth_dirty = false;
            d.auth_notice = Some(format!("Created and activated {request}."));
            if let Err(error) = d.load_index() {
                d.diagnostics.push(error);
            }
        }
        Err(error) => d.auth_notice = Some(format!("Auth request not created: {error}")),
    }
}

pub(super) fn provider_auth_document(
    draft: &AuthDraft,
    stem: &str,
) -> Result<forge_core::reqv1::RequestDocument, String> {
    let token_url = provider_token_url(draft)?;
    if draft.client_id.trim().is_empty() {
        return Err("client ID must not be empty".to_string());
    }
    if matches!(draft.provider, AuthProvider::Auth0 | AuthProvider::Entra)
        && draft.scope.trim().is_empty()
    {
        return Err(format!(
            "{} must not be empty",
            draft.provider.scope_label().to_lowercase()
        ));
    }
    let mut form = serde_json::Map::from_iter([
        (
            "grant_type".to_string(),
            serde_json::Value::String("client_credentials".to_string()),
        ),
        (
            "client_id".to_string(),
            serde_json::Value::String(draft.client_id.trim().to_string()),
        ),
        (
            "client_secret".to_string(),
            serde_json::Value::String(format!("${{secret.{}}}", draft.provider.secret_name())),
        ),
    ]);
    if !draft.scope.trim().is_empty() {
        form.insert(
            if draft.provider == AuthProvider::Auth0 {
                "audience"
            } else {
                "scope"
            }
            .to_string(),
            serde_json::Value::String(draft.scope.trim().to_string()),
        );
    }
    let value = serde_json::json!({
        "formatVersion": 1,
        "kind": "request",
        "meta": {
            "id": format!("auth.{stem}"),
            "name": format!("{} token", draft.provider.label())
        },
        "request": {
            "method": "POST",
            "url": token_url,
            "body": { "type": "form", "value": form }
        }
    });
    forge_core::reqv1::RequestDocument::parse(&value.to_string()).map_err(|error| error.to_string())
}

pub(super) fn provider_token_url(draft: &AuthDraft) -> Result<String, String> {
    let mut url = match draft.provider {
        AuthProvider::Generic => parse_http_url(&draft.endpoint, "token URL")?,
        AuthProvider::Keycloak => {
            if draft.realm.trim().is_empty() {
                return Err("realm must not be empty".to_string());
            }
            let mut url = parse_http_url(&draft.endpoint, "server URL")?;
            url.set_query(None);
            url.set_fragment(None);
            let mut segments = url
                .path_segments_mut()
                .map_err(|_| "Keycloak server URL cannot be a base URL".to_string())?;
            segments.pop_if_empty();
            segments.extend([
                "realms",
                draft.realm.trim(),
                "protocol",
                "openid-connect",
                "token",
            ]);
            drop(segments);
            url
        }
        AuthProvider::Auth0 => {
            let mut url = parse_http_url(&draft.endpoint, "domain")?;
            url.set_path("/oauth/token");
            url.set_query(None);
            url.set_fragment(None);
            url
        }
        AuthProvider::Entra => {
            if draft.endpoint.trim().is_empty() {
                return Err("tenant ID must not be empty".to_string());
            }
            let mut url = url::Url::parse("https://login.microsoftonline.com/")
                .map_err(|error| error.to_string())?;
            url.path_segments_mut()
                .map_err(|_| "invalid Microsoft authority URL".to_string())?
                .extend([draft.endpoint.trim(), "oauth2", "v2.0", "token"]);
            url
        }
    };
    url.set_fragment(None);
    Ok(url.to_string())
}

pub(super) fn parse_http_url(value: &str, label: &str) -> Result<url::Url, String> {
    let value = value.trim();
    if value.is_empty() {
        return Err(format!("{label} must not be empty"));
    }
    let value = if value.contains("://") {
        value.to_string()
    } else {
        format!("https://{value}")
    };
    let url = url::Url::parse(&value).map_err(|error| format!("invalid {label}: {error}"))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(format!("{label} must be an HTTP(S) URL"));
    }
    Ok(url)
}

pub(super) fn save_local_secret(root: &Path, name: &str, value: &str) -> Result<(), String> {
    forge_core::store::ensure_gitignore(root).map_err(|error| error.to_string())?;
    let path = root.join(".env.local");
    let existing = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(format!("cannot read {}: {error}", path.display())),
    };
    let encoded = serde_json::to_string(value).map_err(|error| error.to_string())?;
    let mut found = false;
    let mut lines: Vec<String> = existing
        .lines()
        .map(|line| {
            let matches = line
                .split_once('=')
                .is_some_and(|(key, _)| key.trim() == name);
            if matches {
                found = true;
                format!("{name}={encoded}")
            } else {
                line.to_string()
            }
        })
        .collect();
    if !found {
        lines.push(format!("{name}={encoded}"));
    }
    std::fs::write(&path, lines.join("\n") + "\n")
        .map_err(|error| format!("cannot write {}: {error}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let permissions = std::fs::Permissions::from_mode(0o600);
        std::fs::set_permissions(&path, permissions)
            .map_err(|error| format!("cannot secure {}: {error}", path.display()))?;
    }
    Ok(())
}

pub(super) fn persist_project_auth(
    root: &Path,
    auth: Option<&ProjectAuthConfig>,
) -> Result<(), String> {
    if let Some(auth) = auth {
        auth.validate()?;
    }
    let mut project =
        forge_core::reqv1::load_project(root).map_err(|diagnostic| diagnostic.message)?;
    project.auth = auth.cloned();
    let mut text = serde_json::to_string_pretty(&project).map_err(|error| error.to_string())?;
    text.push('\n');
    std::fs::write(root.join("project.json"), text).map_err(|error| error.to_string())
}

pub(super) fn save_project_auth(d: &mut V1EditorState) {
    let result = (|| {
        let root = d
            .root
            .as_deref()
            .ok_or_else(|| "no project root".to_string())?;
        persist_project_auth(root, d.project_auth.as_ref())
    })();
    match result {
        Ok(()) => {
            d.auth_dirty = false;
            d.auth_notice = Some("Project auth saved.".to_string());
            d.sync_project_auth_to_tabs();
        }
        Err(error) => d.auth_notice = Some(format!("Auth not saved: {error}")),
    }
}
