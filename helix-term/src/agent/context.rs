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
    pub empty: bool,
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
    pub agent_transcript: bool,
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
    let agent_config = editor.config().agent.clone();
    let (view, doc) = current_ref!(editor);
    let transcript_doc_id = super::runtime::transcript_doc_id();
    let doc = if doc.path().is_none() && Some(doc.id()) == transcript_doc_id {
        agent_context_document(editor, view, transcript_doc_id).unwrap_or(doc)
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
    let selection_mode = matches!(editor.mode(), helix_view::document::Mode::Select);
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
            let empty = range.is_empty() || (range.len() <= 1 && !selection_mode);
            let selected_text = if empty {
                String::new()
            } else {
                text.slice(range.from()..range.to())
                    .chars()
                    .take(MAX_SELECTION_TEXT_CHARS)
                    .collect::<String>()
            };

            SelectionSnapshot {
                index,
                primary: index == selection.primary_index(),
                empty,
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

    let open_buffers = if agent_config.include_visible_buffer {
        editor
            .documents()
            .map(|doc| BufferSnapshot {
                id: doc.id().to_string(),
                display_name: doc.display_name().to_string(),
                path: doc.path().map(|path| path.display().to_string()),
                language: doc.language_name().map(ToOwned::to_owned),
                modified: doc.is_modified(),
                diagnostics: if agent_config.include_diagnostics {
                    doc.diagnostics().len()
                } else {
                    0
                },
                agent_transcript: Some(doc.id()) == transcript_doc_id,
            })
            .collect()
    } else {
        Vec::new()
    };

    let diagnostics = if agent_config.include_diagnostics {
        doc.diagnostics()
            .iter()
            .take(50)
            .map(|diagnostic| DiagnosticSnapshot {
                line: diagnostic.line,
                severity: diagnostic.severity.map(|severity| format!("{severity:?}")),
                source: diagnostic.source.clone(),
                message: diagnostic.message.clone(),
            })
            .collect()
    } else {
        Vec::new()
    };

    let recent_commands = if agent_config.include_command_history {
        editor
            .registers
            .read(':', editor)
            .map(|commands| {
                commands
                    .take(MAX_RECENT_COMMANDS)
                    .map(|command| format!(":{command}"))
                    .collect()
            })
            .unwrap_or_default()
    } else {
        Vec::new()
    };

    let lsp_servers = doc
        .language_servers()
        .map(|server| server.name().to_string())
        .collect();

    let git = git_snapshot(editor, doc.path(), &workspace_root);

    EditorSnapshot {
        workspace_root: workspace_root.display().to_string(),
        cwd: cwd.display().to_string(),
        theme: if agent_config.include_theme {
            editor.theme.name().to_string()
        } else {
            String::new()
        },
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
        visible_ranges: if agent_config.include_visible_buffer {
            vec![VisibleRangeSnapshot {
                file: doc.display_name().to_string(),
                start_line: visible_start,
                end_line: visible_end,
            }]
        } else {
            Vec::new()
        },
        selections,
        open_buffers,
        diagnostics,
        lsp_servers,
        git,
        recent_commands,
    }
}

fn agent_context_document<'a>(
    editor: &'a Editor,
    view: &helix_view::View,
    transcript_doc_id: Option<helix_view::DocumentId>,
) -> Option<&'a helix_view::Document> {
    view.docs_access_history
        .iter()
        .rev()
        .find_map(|doc_id| {
            editor.document(*doc_id).filter(|document| {
                document.path().is_some() && Some(document.id()) != transcript_doc_id
            })
        })
        .or_else(|| {
            editor.documents().find(|document| {
                document.path().is_some() && Some(document.id()) != transcript_doc_id
            })
        })
}

pub fn current_snapshot_pretty(editor: &Editor) -> anyhow::Result<String> {
    Ok(serde_json::to_string_pretty(&current_snapshot(editor))?)
}

pub fn prompt_with_primary_selection(editor: &Editor, prompt: &str) -> String {
    let snapshot = current_snapshot(editor);
    prompt_with_primary_selection_snapshot(&snapshot, prompt)
}

pub fn prompt_with_editor_context_snapshot(snapshot: &EditorSnapshot, prompt: &str) -> String {
    let prompt = prompt_with_context_summary_snapshot(snapshot, prompt);
    prompt_with_primary_selection_snapshot(snapshot, &prompt)
}

fn prompt_with_context_summary_snapshot(snapshot: &EditorSnapshot, prompt: &str) -> String {
    let active_path = snapshot
        .active_file
        .path
        .as_deref()
        .unwrap_or(snapshot.active_file.display_name.as_str());
    let language = snapshot
        .active_file
        .language_id
        .as_deref()
        .or(snapshot.active_file.language.as_deref())
        .unwrap_or("unknown");
    let selection_count = snapshot
        .selections
        .iter()
        .filter(|selection| !selection.empty && !selection.text.trim().is_empty())
        .count();
    let diagnostics = snapshot.diagnostics.len();

    format!(
        "{prompt}\n\nEditor context:\n- Workspace root: {}\n- Active file: {active_path}\n- Language: {language}\n- Cursor: {}:{}\n- Non-empty selections: {selection_count}\n- Diagnostics in active file: {diagnostics}",
        snapshot.workspace_root,
        snapshot.cursor.line + 1,
        snapshot.cursor.column + 1,
    )
}

pub fn prompt_with_primary_selection_snapshot(snapshot: &EditorSnapshot, prompt: &str) -> String {
    let Some(selection) = snapshot.selections.iter().find(|selection| {
        selection.primary && !selection.empty && !selection.text.trim().is_empty()
    }) else {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot_with_selection(selection: SelectionSnapshot) -> EditorSnapshot {
        EditorSnapshot {
            workspace_root: "/repo".to_string(),
            cwd: "/repo".to_string(),
            theme: "default".to_string(),
            mode: "normal".to_string(),
            active_file: ActiveFileSnapshot {
                id: "1".to_string(),
                display_name: "main.go".to_string(),
                path: Some("/repo/main.go".to_string()),
                language: Some("go".to_string()),
                language_id: Some("go".to_string()),
                modified: false,
                line_ending: "LF".to_string(),
                encoding: "UTF-8".to_string(),
            },
            cursor: CursorSnapshot {
                line: 0,
                column: 0,
                char: 0,
            },
            visible_ranges: Vec::new(),
            selections: vec![selection],
            open_buffers: Vec::new(),
            diagnostics: Vec::new(),
            lsp_servers: Vec::new(),
            git: GitSnapshot {
                branch: None,
                changed_files: Vec::new(),
                changed_files_truncated: false,
            },
            recent_commands: Vec::new(),
        }
    }

    fn selection_snapshot(empty: bool, text: &str) -> SelectionSnapshot {
        SelectionSnapshot {
            index: 0,
            primary: true,
            empty,
            anchor: 0,
            head: if empty { 0 } else { text.chars().count() },
            start: PositionSnapshot { line: 0, column: 0 },
            end: PositionSnapshot {
                line: 0,
                column: text.chars().count(),
            },
            text: text.to_string(),
            truncated: false,
        }
    }

    #[test]
    fn prompt_ignores_cursor_only_selection() {
        let snapshot = snapshot_with_selection(selection_snapshot(true, ""));
        assert_eq!(
            prompt_with_primary_selection_snapshot(&snapshot, "Explain this."),
            "Explain this."
        );
    }

    #[test]
    fn prompt_includes_extended_selection() {
        let snapshot = snapshot_with_selection(selection_snapshot(false, "fmt.Println(x)"));
        let prompt = prompt_with_primary_selection_snapshot(&snapshot, "Explain this.");
        assert!(prompt.contains("Selected text from /repo/main.go:1:1-1:15"));
        assert!(prompt.contains("```go\nfmt.Println(x)\n```"));
    }

    #[test]
    fn prompt_includes_active_file_context_without_selection() {
        let snapshot = snapshot_with_selection(selection_snapshot(true, ""));
        let prompt = prompt_with_editor_context_snapshot(&snapshot, "Review this file.");

        assert!(prompt.contains("Editor context:"));
        assert!(prompt.contains("- Workspace root: /repo"));
        assert!(prompt.contains("- Active file: /repo/main.go"));
        assert!(prompt.contains("- Language: go"));
        assert!(prompt.contains("- Cursor: 1:1"));
    }

    #[test]
    fn prompt_includes_context_and_extended_selection() {
        let snapshot = snapshot_with_selection(selection_snapshot(false, "fmt.Println(x)"));
        let prompt = prompt_with_editor_context_snapshot(&snapshot, "Explain this.");

        assert!(prompt.contains("- Active file: /repo/main.go"));
        assert!(prompt.contains("- Non-empty selections: 1"));
        assert!(prompt.contains("Selected text from /repo/main.go:1:1-1:15"));
    }
}
