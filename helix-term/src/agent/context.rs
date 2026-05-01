use helix_core::coords_at_pos;
use helix_vcs::FileChange;
use helix_view::Editor;
use serde::Serialize;
use std::path::{Path, PathBuf};

const MAX_SELECTION_TEXT_CHARS: usize = 2_000;
const MAX_RECENT_COMMANDS: usize = 20;
const MAX_CHANGED_FILES: usize = 100;

#[derive(Debug, Serialize)]
pub struct AgentRequest {
    pub schema_version: u32,
    pub kind: &'static str,
    pub prompt: String,
    pub context: EditorSnapshot,
}

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
    pub lsp_servers: Vec<String>,
    pub git: GitSnapshot,
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

#[derive(Debug, Serialize)]
pub struct GitSnapshot {
    pub branch: Option<String>,
    pub changed_files: Vec<FileChangeSnapshot>,
    pub changed_files_truncated: bool,
}

#[derive(Debug, Serialize)]
pub struct FileChangeSnapshot {
    pub change: &'static str,
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from_path: Option<String>,
}

pub fn current_snapshot(editor: &Editor) -> EditorSnapshot {
    let (view, doc) = current_ref!(editor);
    let transcript_doc_id = super::runtime::transcript_doc_id();
    let doc = if doc.path().is_none() && Some(doc.id()) == transcript_doc_id {
        editor
            .documents()
            .find(|document| document.path().is_some() && Some(document.id()) != transcript_doc_id)
            .unwrap_or(doc)
    } else {
        doc
    };
    let snapshot_view_id = if doc.selections().contains_key(&view.id) {
        view.id
    } else {
        doc.selections().keys().next().copied().unwrap_or(view.id)
    };
    let text = doc.text().slice(..);
    let selection = doc.selection(snapshot_view_id);
    let primary = selection.primary();
    let cursor = coords_at_pos(text, primary.head);
    let cwd = helix_stdx::env::current_working_dir();
    let workspace_root = doc
        .path()
        .map(|path| helix_loader::find_workspace_in(path).0)
        .unwrap_or_else(|| helix_loader::find_workspace().0);
    let view_offset = doc.view_offset(snapshot_view_id);
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

    let lsp_servers = doc
        .language_servers()
        .map(|server| server.name().to_string())
        .collect();

    let git = git_snapshot(editor, doc.path(), &workspace_root);

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
        lsp_servers,
        git,
        recent_commands,
    }
}

pub fn current_snapshot_pretty(editor: &Editor) -> anyhow::Result<String> {
    Ok(serde_json::to_string_pretty(&current_snapshot(editor))?)
}

pub fn prompt_with_primary_selection(editor: &Editor, prompt: &str) -> String {
    let snapshot = current_snapshot(editor);
    let Some(selection) = snapshot
        .selections
        .iter()
        .find(|selection| selection.primary && !selection.text.trim().is_empty())
    else {
        return prompt.to_string();
    };

    let path = snapshot
        .active_file
        .path
        .as_deref()
        .unwrap_or(snapshot.active_file.display_name.as_str());
    let truncated = if selection.truncated {
        "\n\nNote: the selected text was truncated in the editor context."
    } else {
        ""
    };

    format!(
        "{prompt}\n\nSelected text from {path}:{}:{}-{}:{}:\n```{}\n{}\n```{truncated}",
        selection.start.line + 1,
        selection.start.column + 1,
        selection.end.line + 1,
        selection.end.column + 1,
        snapshot
            .active_file
            .language_id
            .as_deref()
            .unwrap_or_default(),
        selection.text
    )
}

pub fn current_request_pretty(editor: &Editor, prompt: &str) -> anyhow::Result<String> {
    let request = AgentRequest {
        schema_version: 1,
        kind: "ask",
        prompt: prompt.to_string(),
        context: current_snapshot(editor),
    };

    Ok(serde_json::to_string_pretty(&request)?)
}

fn git_snapshot(
    editor: &Editor,
    active_path: Option<&PathBuf>,
    workspace_root: &Path,
) -> GitSnapshot {
    let branch = active_path
        .and_then(|_| doc!(editor).version_control_head())
        .map(|head| head.as_ref().as_ref().to_string());

    let changed_files = editor
        .diff_providers
        .changed_files(workspace_root)
        .unwrap_or_default();
    let changed_files_truncated = changed_files.len() > MAX_CHANGED_FILES;
    let changed_files = changed_files
        .into_iter()
        .take(MAX_CHANGED_FILES)
        .map(|change| file_change_snapshot(change, workspace_root))
        .collect();

    GitSnapshot {
        branch,
        changed_files,
        changed_files_truncated,
    }
}

fn file_change_snapshot(change: FileChange, root: &Path) -> FileChangeSnapshot {
    let display_path = |path: &Path| {
        path.strip_prefix(root)
            .unwrap_or(path)
            .display()
            .to_string()
    };

    match change {
        FileChange::Untracked { path } => FileChangeSnapshot {
            change: "untracked",
            path: display_path(&path),
            from_path: None,
        },
        FileChange::Modified { path } => FileChangeSnapshot {
            change: "modified",
            path: display_path(&path),
            from_path: None,
        },
        FileChange::Conflict { path } => FileChangeSnapshot {
            change: "conflict",
            path: display_path(&path),
            from_path: None,
        },
        FileChange::Deleted { path } => FileChangeSnapshot {
            change: "deleted",
            path: display_path(&path),
            from_path: None,
        },
        FileChange::Renamed { from_path, to_path } => FileChangeSnapshot {
            change: "renamed",
            path: display_path(&to_path),
            from_path: Some(display_path(&from_path)),
        },
    }
}
