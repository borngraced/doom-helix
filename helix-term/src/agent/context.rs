use helix_core::coords_at_pos;
use helix_view::Editor;
use serde::Serialize;

const MAX_SELECTION_TEXT_CHARS: usize = 2_000;
const MAX_RECENT_COMMANDS: usize = 20;

#[derive(Debug, Serialize)]
pub struct EditorSnapshot {
    pub workspace_root: String,
    pub cwd: String,
    pub theme: String,
    pub mode: String,
    pub active_file: ActiveFileSnapshot,
    pub cursor: CursorSnapshot,
    pub visible_ranges: Vec<VisibleRangeSnapshot>,
    pub selections: Vec<SelectionSnapshot>,
    pub open_buffers: Vec<BufferSnapshot>,
    pub diagnostics: Vec<DiagnosticSnapshot>,
    pub recent_commands: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct ActiveFileSnapshot {
    pub id: String,
    pub display_name: String,
    pub path: Option<String>,
    pub language: Option<String>,
    pub language_id: Option<String>,
    pub modified: bool,
    pub line_ending: String,
    pub encoding: String,
}

#[derive(Debug, Serialize)]
pub struct CursorSnapshot {
    pub line: usize,
    pub column: usize,
    pub char: usize,
}

#[derive(Debug, Serialize)]
pub struct VisibleRangeSnapshot {
    pub file: String,
    pub start_line: usize,
    pub end_line: usize,
}

#[derive(Debug, Serialize)]
pub struct SelectionSnapshot {
    pub index: usize,
    pub primary: bool,
    pub anchor: usize,
    pub head: usize,
    pub start: PositionSnapshot,
    pub end: PositionSnapshot,
    pub text: String,
    pub truncated: bool,
}

#[derive(Debug, Serialize)]
pub struct PositionSnapshot {
    pub line: usize,
    pub column: usize,
}

#[derive(Debug, Serialize)]
pub struct BufferSnapshot {
    pub id: String,
    pub display_name: String,
    pub path: Option<String>,
    pub language: Option<String>,
    pub modified: bool,
    pub diagnostics: usize,
}

#[derive(Debug, Serialize)]
pub struct DiagnosticSnapshot {
    pub line: usize,
    pub severity: Option<String>,
    pub source: Option<String>,
    pub message: String,
}

pub fn current_snapshot(editor: &Editor) -> EditorSnapshot {
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

    let selections = selection
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

            SelectionSnapshot {
                index,
                primary: index == selection.primary_index(),
                anchor: range.anchor,
                head: range.head,
                start: PositionSnapshot {
                    line: start.row,
                    column: start.col,
                },
                end: PositionSnapshot {
                    line: end.row,
                    column: end.col,
                },
                text: selected_text,
                truncated: range.len() > MAX_SELECTION_TEXT_CHARS,
            }
        })
        .collect();

    let open_buffers = editor
        .documents()
        .map(|doc| BufferSnapshot {
            id: doc.id().to_string(),
            display_name: doc.display_name().to_string(),
            path: doc.path().map(|path| path.display().to_string()),
            language: doc.language_name().map(ToOwned::to_owned),
            modified: doc.is_modified(),
            diagnostics: doc.diagnostics().len(),
        })
        .collect();

    let diagnostics = doc
        .diagnostics()
        .iter()
        .take(50)
        .map(|diagnostic| DiagnosticSnapshot {
            line: diagnostic.line,
            severity: diagnostic.severity.map(|severity| format!("{severity:?}")),
            source: diagnostic.source.clone(),
            message: diagnostic.message.clone(),
        })
        .collect();

    let recent_commands = editor
        .registers
        .read(':', editor)
        .map(|commands| {
            commands
                .take(MAX_RECENT_COMMANDS)
                .map(|command| format!(":{command}"))
                .collect()
        })
        .unwrap_or_default();

    EditorSnapshot {
        workspace_root: workspace_root.display().to_string(),
        cwd: cwd.display().to_string(),
        theme: editor.theme.name().to_string(),
        mode: format!("{:?}", editor.mode()).to_lowercase(),
        active_file: ActiveFileSnapshot {
            id: doc.id().to_string(),
            display_name: doc.display_name().to_string(),
            path: doc.path().map(|path| path.display().to_string()),
            language: doc.language_name().map(ToOwned::to_owned),
            language_id: doc.language_id().map(ToOwned::to_owned),
            modified: doc.is_modified(),
            line_ending: doc.line_ending.as_str().to_string(),
            encoding: doc.encoding().name().to_string(),
        },
        cursor: CursorSnapshot {
            line: cursor.row,
            column: cursor.col,
            char: primary.head,
        },
        visible_ranges: vec![VisibleRangeSnapshot {
            file: doc.display_name().to_string(),
            start_line: visible_start,
            end_line: visible_end,
        }],
        selections,
        open_buffers,
        diagnostics,
        recent_commands,
    }
}

pub fn current_snapshot_pretty(editor: &Editor) -> anyhow::Result<String> {
    Ok(serde_json::to_string_pretty(&current_snapshot(editor))?)
}
