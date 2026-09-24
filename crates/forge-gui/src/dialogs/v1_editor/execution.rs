//! Run and cancellation commands from the editor.

use super::*;

impl V1EditorState {
    pub(crate) fn run_current(&mut self, bridge: &Bridge) {
        run_now(self, bridge);
    }
}

pub(super) fn cancel_now(d: &mut V1EditorState, bridge: &Bridge) {
    if let Some(run_id) = d.active_run {
        if let Err(error) = bridge.send(Cmd::CancelV1 { run_id }) {
            d.diagnostics = vec![error];
            d.result_tab = ResultTab::Diagnostics;
        }
    }
}

pub(super) fn run_now(d: &mut V1EditorState, bridge: &Bridge) {
    if d.in_flight {
        return;
    }
    if let Some(error) = d.run_block_error() {
        d.diagnostics = vec![error];
        d.result_tab = ResultTab::Diagnostics;
        return;
    }
    let Some(root) = d.root.clone() else {
        d.diagnostics = vec!["no request-v1 project is open".to_string()];
        d.result_tab = ResultTab::Diagnostics;
        return;
    };
    // Run needs a file path for relative refs; use the saved file, or a
    // temp path under root for an unsaved buffer.
    let file = d
        .file
        .clone()
        .unwrap_or_else(|| root.join("__unsaved__.request.json"));
    let text = match effective_document(d).and_then(|document| serialize_request(&document)) {
        Ok(text) => text,
        Err(error) => {
            d.diagnostics = vec![format!("invalid JSON: {error}")];
            d.result_tab = ResultTab::Diagnostics;
            return;
        }
    };
    let run_environment = d.env_name.clone().or_else(|| {
        d.root
            .as_deref()
            .zip(d.file.as_deref())
            .and_then(|(root, file)| {
                forge_core::reqv1::effective_environment(root, file)
                    .ok()
                    .flatten()
                    .map(|selection| selection.value)
            })
    });
    let run_id = crate::state::allocate_global_run_id();
    d.active_run = Some(run_id);
    d.in_flight = true;
    d.results.clear();
    d.selected_result = 0;
    d.last_response = None;
    d.diagnostics.clear();
    d.result_tab = ResultTab::Result;
    if let Err(error) = bridge.send(Cmd::RunV1 {
        run_id,
        root,
        file,
        text: text.clone(),
        env_name: d.env_name.clone(),
        mock: d.mock,
        allow_project_code: d.allow_project_code,
    }) {
        d.active_run = None;
        d.in_flight = false;
        d.diagnostics = vec![error];
        d.result_tab = ResultTab::Diagnostics;
    } else {
        d.last_run_request = Some(text);
        d.last_run_mock = Some(d.mock);
        d.last_run_environment = run_environment;
        d.last_run_at = Some(Instant::now());
    }
}

pub(super) fn run_sequence_now(d: &mut V1EditorState, bridge: &Bridge) {
    if d.in_flight {
        return;
    }
    if let Some(error) = d.run_block_error() {
        d.diagnostics = vec![error];
        d.result_tab = ResultTab::Diagnostics;
        return;
    }
    let Some(root) = d.root.clone() else { return };
    let Some(sequence_file) = rfd::FileDialog::new()
        .set_directory(&root)
        .add_filter("sequence", &["json"])
        .pick_file()
    else {
        return;
    };
    let files = std::fs::read_to_string(&sequence_file)
        .map_err(|error| format!("cannot read {}: {error}", sequence_file.display()))
        .and_then(|text| {
            forge_core::reqv1::SequenceDocument::parse(&text)
                .map_err(|error| format!("invalid sequence: {error}"))
        })
        .and_then(|sequence| sequence.resolve_files(&root));
    let files = match files {
        Ok(files) => files,
        Err(error) => {
            d.diagnostics = vec![error];
            d.result_tab = ResultTab::Diagnostics;
            return;
        }
    };
    let env = d.env_name.clone();
    d.last_run_request = None;
    d.last_run_mock = None;
    d.last_run_environment = None;
    d.last_run_at = None;
    d.run_sequence(root, files, env, bridge);
}
