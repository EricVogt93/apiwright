//! Run and cancellation commands from the editor.

use super::*;

pub(super) fn cancel_now(d: &mut V1EditorState, bridge: &Bridge) {
    if let Some(run_id) = d.active_run {
        if let Err(error) = bridge.send(Cmd::CancelV1 { run_id }) {
            d.diagnostics = vec![error];
            d.result_tab = ResultTab::Diagnostics;
        }
    }
}

pub(super) fn run_now(d: &mut V1EditorState, bridge: &Bridge) {
    let Some(root) = d.root.clone() else { return };
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
    let run_id = d.next_run_id;
    d.next_run_id += 1;
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
        text,
        env_name: d.env_name.clone(),
        mock: d.mock,
        allow_project_code: d.allow_project_code,
    }) {
        d.active_run = None;
        d.in_flight = false;
        d.diagnostics = vec![error];
        d.result_tab = ResultTab::Diagnostics;
    }
}

pub(super) fn run_sequence_now(d: &mut V1EditorState, bridge: &Bridge) {
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
    d.run_sequence(root, files, env, bridge);
}
