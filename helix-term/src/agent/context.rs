use helix_core::coords_at_pos;
use helix_view::Editor;
use serde_json::{json, Value};

const MAX_SELECTION_TEXT_CHARS: usize = 2_000;

pub fn current_snapshot(editor: &Editor) -> Value {
    let (view, doc) = current_ref!(editor);
    let text = doc.text().slice(..);
    let selection = doc.selection(view.id);
    let primary = selection.primary();
    let cursor = coords_at_pos(text, primary.head);
    let cwd = helix_stdx::env::current_working_dir();
    let workspace_root = doc
        .path()
        .map(|path| helix_loader::find_workspace_in(path).0)
        .unwrap_or_else(|| helix_loader::find_workspace().0);
    let view_offset = doc.view_offset(view.id);
    let visible_start = coords_at_pos(text, view_offset.anchor).row;
    let visible_end = visible_start.saturating_add(view.area.height as usize);

    let selections: Vec<Value> = selection
        .ranges()
        .iter()
        .enumerate()
        .map(|(index, range)| {
            let start = coords_at_pos(text, range.from());
            let end = coords_at_pos(text, range.to());
            let selected_text = text
                .slice(range.from()..range.to())
                .chars()
                .take(MAX_SELECTION_TEXT_CHARS)
                .collect::<String>();

            json!({
                "index": index,
                "primary": index == selection.primary_index(),
                "anchor": range.anchor,
                "head": range.head,
                "start": { "line": start.row, "column": start.col },
                "end": { "line": end.row, "column": end.col },
                "text": selected_text,
                "truncated": range.len() > MAX_SELECTION_TEXT_CHARS,
            })
        })
        .collect();

    let open_buffers: Vec<Value> = editor
        .documents()
        .map(|doc| {
            json!({
                "id": doc.id().to_string(),
                "display_name": doc.display_name().to_string(),
                "path": doc.path().map(|path| path.display().to_string()),
                "language": doc.language_name(),
                "modified": doc.is_modified(),
                "diagnostics": doc.diagnostics().len(),
            })
        })
        .collect();

    let diagnostics: Vec<Value> = doc
        .diagnostics()
        .iter()
        .take(50)
        .map(|diagnostic| {
            json!({
                "line": diagnostic.line,
                "severity": diagnostic.severity,
                "source": diagnostic.source,
                "message": diagnostic.message,
            })
        })
        .collect();

    json!({
        "workspace_root": workspace_root.display().to_string(),
        "cwd": cwd.display().to_string(),
        "theme": editor.theme.name(),
        "mode": format!("{:?}", editor.mode()).to_lowercase(),
        "active_file": {
            "id": doc.id().to_string(),
            "display_name": doc.display_name().to_string(),
            "path": doc.path().map(|path| path.display().to_string()),
            "language": doc.language_name(),
            "language_id": doc.language_id(),
            "modified": doc.is_modified(),
            "line_ending": doc.line_ending.as_str(),
            "encoding": doc.encoding().name(),
        },
        "cursor": {
            "line": cursor.row,
            "column": cursor.col,
            "char": primary.head,
        },
        "visible_ranges": [{
            "file": doc.display_name().to_string(),
            "start_line": visible_start,
            "end_line": visible_end,
        }],
        "selections": selections,
        "open_buffers": open_buffers,
        "diagnostics": diagnostics,
        "recent_commands": [],
    })
}

pub fn current_snapshot_pretty(editor: &Editor) -> anyhow::Result<String> {
    Ok(serde_json::to_string_pretty(&current_snapshot(editor))?)
}
