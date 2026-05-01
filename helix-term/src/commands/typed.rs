use std::fmt::Write;
use std::io::BufReader;
use std::ops::{self, Deref};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU16, AtomicU64, Ordering};

use crate::job::Job;

use super::*;

use helix_core::command_line::{Args, Flag, Signature, Token, TokenKind};
use helix_core::fuzzy::fuzzy_match;
use helix_core::indent::MAX_INDENT;
use helix_core::line_ending;
use helix_stdx::path::home_dir;
use helix_view::document::{read_to_string, DEFAULT_LANGUAGE_NAME};
use helix_view::editor::{AgentPanelPosition, CloseError, ConfigEvent};
use helix_view::expansion;
use serde_json::Value;
use ui::completers::{self, Completer};

const AGENT_SUBCOMMANDS: &[&str] = &[
    "ask",
    "apply",
    "chat",
    "clear",
    "context",
    "cancel",
    "edit",
    "explain",
    "fix",
    "refactor",
    "restore",
    "next",
    "new",
    "acp",
    "launch-config",
    "panel",
    "patch",
    "position",
    "prev",
    "resize",
    "start",
    "status",
    "stop",
];
static AGENT_PENDING_TURN_ID: AtomicU64 = AtomicU64::new(1);
static AGENT_PANEL_SIZE_OVERRIDE: AtomicU16 = AtomicU16::new(0);
static AGENT_PANEL_POSITION_OVERRIDE: AtomicU16 = AtomicU16::new(0);

#[derive(Clone)]
pub struct TypableCommand {
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    pub doc: &'static str,
    // params, flags, helper, completer
    pub fun: fn(&mut compositor::Context, Args, PromptEvent) -> anyhow::Result<()>,
    /// What completion methods, if any, does this command have?
    pub completer: CommandCompleter,
    pub signature: Signature,
}

#[derive(Clone)]
pub struct CommandCompleter {
    // Arguments with specific completion methods based on their position.
    positional_args: &'static [Completer],

    // All remaining arguments will use this completion method, if set.
    var_args: Completer,
}

impl CommandCompleter {
    const fn none() -> Self {
        Self {
            positional_args: &[],
            var_args: completers::none,
        }
    }

    const fn positional(completers: &'static [Completer]) -> Self {
        Self {
            positional_args: completers,
            var_args: completers::none,
        }
    }

    const fn all(completer: Completer) -> Self {
        Self {
            positional_args: &[],
            var_args: completer,
        }
    }

    fn for_argument_number(&self, n: usize) -> &Completer {
        match self.positional_args.get(n) {
            Some(completer) => completer,
            _ => &self.var_args,
        }
    }
}

fn exit(cx: &mut compositor::Context, args: Args, event: PromptEvent) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    if doc!(cx.editor).is_modified() {
        write_impl(
            cx,
            args.first(),
            WriteOptions {
                force: false,
                auto_format: !args.has_flag(WRITE_NO_FORMAT_FLAG.name),
            },
        )?;
    }
    cx.block_try_flush_writes()?;
    quit(cx, Args::default(), event)
}

fn force_exit(cx: &mut compositor::Context, args: Args, event: PromptEvent) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    if doc!(cx.editor).is_modified() {
        write_impl(
            cx,
            args.first(),
            WriteOptions {
                force: true,
                auto_format: !args.has_flag(WRITE_NO_FORMAT_FLAG.name),
            },
        )?;
    }
    cx.block_try_flush_writes()?;
    quit(cx, Args::default(), event)
}

fn quit(cx: &mut compositor::Context, _args: Args, event: PromptEvent) -> anyhow::Result<()> {
    log::debug!("quitting...");

    if event != PromptEvent::Validate {
        return Ok(());
    }

    // last view and we have unsaved changes
    if cx.editor.tree.views().count() == 1 {
        buffers_remaining_impl(cx.editor)?
    }

    cx.block_try_flush_writes()?;
    cx.editor.close(view!(cx.editor).id);

    Ok(())
}

fn force_quit(cx: &mut compositor::Context, _args: Args, event: PromptEvent) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    cx.block_try_flush_writes()?;
    cx.editor.close(view!(cx.editor).id);

    Ok(())
}

fn open(cx: &mut compositor::Context, args: Args, event: PromptEvent) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    open_impl(cx, args, Action::Replace)
}

fn open_impl(cx: &mut compositor::Context, args: Args, action: Action) -> anyhow::Result<()> {
    for arg in args {
        let (path, pos) = crate::args::parse_file(&arg);
        let path = helix_stdx::path::expand_tilde(path);
        // If the path is a directory, open a file picker on that directory and update the status
        // message
        if let Ok(true) = std::fs::canonicalize(&path).map(|p| p.is_dir()) {
            let callback = async move {
                let call: job::Callback = job::Callback::EditorCompositor(Box::new(
                    move |editor: &mut Editor, compositor: &mut Compositor| {
                        let picker =
                            ui::file_picker(editor, path.into_owned()).with_default_action(action);
                        compositor.push(Box::new(overlaid(picker)));
                    },
                ));
                Ok(call)
            };
            cx.jobs.callback(callback);
        } else {
            // Otherwise, just open the file
            let _ = cx.editor.open(&path, action)?;
            let (view, doc) = current!(cx.editor);
            let pos = Selection::point(pos_at_coords(doc.text().slice(..), pos, true));
            doc.set_selection(view.id, pos);
            // does not affect opening a buffer without pos
            align_view(doc, view, Align::Center);
        }
    }
    Ok(())
}

fn buffer_close_by_ids_impl(
    cx: &mut compositor::Context,
    doc_ids: &[DocumentId],
    force: bool,
) -> anyhow::Result<()> {
    cx.block_try_flush_writes()?;

    let (modified_ids, modified_names): (Vec<_>, Vec<_>) = doc_ids
        .iter()
        .filter_map(|&doc_id| {
            if let Err(CloseError::BufferModified(name)) = cx.editor.close_document(doc_id, force) {
                Some((doc_id, name))
            } else {
                None
            }
        })
        .unzip();

    if let Some(first) = modified_ids.first() {
        let current = doc!(cx.editor);
        // If the current document is unmodified, and there are modified
        // documents, switch focus to the first modified doc.
        if !modified_ids.contains(&current.id()) {
            cx.editor.switch(*first, Action::Replace);
        }
        bail!(
            "{} unsaved buffer{} remaining: {:?}",
            modified_names.len(),
            if modified_names.len() == 1 { "" } else { "s" },
            modified_names,
        );
    }

    Ok(())
}

fn buffer_gather_paths_impl(editor: &mut Editor, args: Args) -> Vec<DocumentId> {
    // No arguments implies current document
    if args.is_empty() {
        let doc_id = view!(editor).doc;
        return vec![doc_id];
    }

    let mut nonexistent_buffers = vec![];
    let mut document_ids = vec![];
    for arg in args {
        let doc_id = editor.documents().find_map(|doc| {
            let arg_path = Some(Path::new(arg.as_ref()));
            if doc.path().map(|p| p.as_path()) == arg_path || doc.relative_path() == arg_path {
                Some(doc.id())
            } else {
                None
            }
        });

        match doc_id {
            Some(doc_id) => document_ids.push(doc_id),
            None => nonexistent_buffers.push(format!("'{}'", arg)),
        }
    }

    if !nonexistent_buffers.is_empty() {
        editor.set_error(format!(
            "cannot close non-existent buffers: {}",
            nonexistent_buffers.join(", ")
        ));
    }

    document_ids
}

fn buffer_close(
    cx: &mut compositor::Context,
    args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    let document_ids = buffer_gather_paths_impl(cx.editor, args);
    buffer_close_by_ids_impl(cx, &document_ids, false)
}

fn force_buffer_close(
    cx: &mut compositor::Context,
    args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    let document_ids = buffer_gather_paths_impl(cx.editor, args);
    buffer_close_by_ids_impl(cx, &document_ids, true)
}

fn buffer_gather_others_impl(editor: &mut Editor, skip_visible: bool) -> Vec<DocumentId> {
    if skip_visible {
        let visible_document_ids = editor
            .tree
            .views()
            .map(|view| &view.0.doc)
            .collect::<HashSet<_>>();
        editor
            .documents()
            .map(|doc| doc.id())
            .filter(|doc_id| !visible_document_ids.contains(doc_id))
            .collect()
    } else {
        let current_document = &doc!(editor).id();
        editor
            .documents()
            .map(|doc| doc.id())
            .filter(|doc_id| doc_id != current_document)
            .collect()
    }
}

fn buffer_close_others(
    cx: &mut compositor::Context,
    args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    let document_ids = buffer_gather_others_impl(cx.editor, args.has_flag("skip-visible"));
    buffer_close_by_ids_impl(cx, &document_ids, false)
}

fn force_buffer_close_others(
    cx: &mut compositor::Context,
    args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    let document_ids = buffer_gather_others_impl(cx.editor, args.has_flag("skip-visible"));
    buffer_close_by_ids_impl(cx, &document_ids, true)
}

fn buffer_gather_all_impl(editor: &mut Editor) -> Vec<DocumentId> {
    editor.documents().map(|doc| doc.id()).collect()
}

fn buffer_close_all(
    cx: &mut compositor::Context,
    _args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    let document_ids = buffer_gather_all_impl(cx.editor);
    buffer_close_by_ids_impl(cx, &document_ids, false)
}

fn force_buffer_close_all(
    cx: &mut compositor::Context,
    _args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    let document_ids = buffer_gather_all_impl(cx.editor);
    buffer_close_by_ids_impl(cx, &document_ids, true)
}

fn buffer_next(
    cx: &mut compositor::Context,
    _args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    goto_buffer(cx.editor, Direction::Forward, 1);
    Ok(())
}

fn buffer_previous(
    cx: &mut compositor::Context,
    _args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    goto_buffer(cx.editor, Direction::Backward, 1);
    Ok(())
}

fn write_impl(
    cx: &mut compositor::Context,
    path: Option<&str>,
    options: WriteOptions,
) -> anyhow::Result<()> {
    let config = cx.editor.config();
    let jobs = &mut cx.jobs;
    let (view, doc) = current!(cx.editor);

    if doc.trim_trailing_whitespace() {
        trim_trailing_whitespace(doc, view.id);
    }
    if config.trim_final_newlines {
        trim_final_newlines(doc, view.id);
    }
    if doc.insert_final_newline() {
        insert_final_newline(doc, view.id);
    }

    // Save an undo checkpoint for any outstanding changes.
    doc.append_changes_to_history(view);

    let (view, doc) = current_ref!(cx.editor);
    let fmt = if config.auto_format && options.auto_format {
        doc.auto_format(cx.editor).map(|fmt| {
            let callback = make_format_callback(
                doc.id(),
                doc.version(),
                view.id,
                fmt,
                Some((path.map(Into::into), options.force)),
            );

            jobs.add(Job::with_callback(callback).wait_before_exiting());
        })
    } else {
        None
    };

    if fmt.is_none() {
        let id = doc.id();
        cx.editor.save(id, path, options.force)?;
    }

    Ok(())
}

/// Trim all whitespace preceding line-endings in a document.
fn trim_trailing_whitespace(doc: &mut Document, view_id: ViewId) {
    let text = doc.text();
    let mut pos = 0;
    let transaction = Transaction::delete(
        text,
        text.lines().filter_map(|line| {
            let line_end_len_chars = line_ending::get_line_ending(&line)
                .map(|le| le.len_chars())
                .unwrap_or_default();
            // Char after the last non-whitespace character or the beginning of the line if the
            // line is all whitespace:
            let first_trailing_whitespace =
                pos + line.last_non_whitespace_char().map_or(0, |idx| idx + 1);
            pos += line.len_chars();
            // Char before the line ending character(s), or the final char in the text if there
            // is no line-ending on this line:
            let line_end = pos - line_end_len_chars;
            if first_trailing_whitespace != line_end {
                Some((first_trailing_whitespace, line_end))
            } else {
                None
            }
        }),
    );
    doc.apply(&transaction, view_id);
}

/// Trim any extra line-endings after the final line-ending.
fn trim_final_newlines(doc: &mut Document, view_id: ViewId) {
    let rope = doc.text();
    let mut text = rope.slice(..);
    let mut total_char_len = 0;
    let mut final_char_len = 0;
    while let Some(line_ending) = line_ending::get_line_ending(&text) {
        total_char_len += line_ending.len_chars();
        final_char_len = line_ending.len_chars();
        text = text.slice(..text.len_chars() - line_ending.len_chars());
    }
    let chars_to_delete = total_char_len - final_char_len;
    if chars_to_delete != 0 {
        let transaction = Transaction::delete(
            rope,
            [(rope.len_chars() - chars_to_delete, rope.len_chars())].into_iter(),
        );
        doc.apply(&transaction, view_id);
    }
}

/// Ensure that the document is terminated with a line ending.
fn insert_final_newline(doc: &mut Document, view_id: ViewId) {
    let text = doc.text();
    if text.len_chars() > 0 && line_ending::get_line_ending(&text.slice(..)).is_none() {
        let eof = Selection::point(text.len_chars());
        let insert = Transaction::insert(text, &eof, doc.line_ending.as_str().into());
        doc.apply(&insert, view_id);
    }
}

#[derive(Debug, Clone, Copy)]
pub struct WriteOptions {
    pub force: bool,
    pub auto_format: bool,
}

fn write(cx: &mut compositor::Context, args: Args, event: PromptEvent) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    write_impl(
        cx,
        args.first(),
        WriteOptions {
            force: false,
            auto_format: !args.has_flag(WRITE_NO_FORMAT_FLAG.name),
        },
    )
}

fn force_write(cx: &mut compositor::Context, args: Args, event: PromptEvent) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    write_impl(
        cx,
        args.first(),
        WriteOptions {
            force: true,
            auto_format: !args.has_flag(WRITE_NO_FORMAT_FLAG.name),
        },
    )
}

fn write_buffer_close(
    cx: &mut compositor::Context,
    args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    write_impl(
        cx,
        args.first(),
        WriteOptions {
            force: false,
            auto_format: !args.has_flag(WRITE_NO_FORMAT_FLAG.name),
        },
    )?;

    let document_ids = buffer_gather_paths_impl(cx.editor, args);
    buffer_close_by_ids_impl(cx, &document_ids, false)
}

fn force_write_buffer_close(
    cx: &mut compositor::Context,
    args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    write_impl(
        cx,
        args.first(),
        WriteOptions {
            force: true,
            auto_format: !args.has_flag(WRITE_NO_FORMAT_FLAG.name),
        },
    )?;

    let document_ids = buffer_gather_paths_impl(cx.editor, args);
    buffer_close_by_ids_impl(cx, &document_ids, false)
}

fn new_file(cx: &mut compositor::Context, _args: Args, event: PromptEvent) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    cx.editor.new_file(Action::Replace);

    Ok(())
}

fn open_agent_context(cx: &mut compositor::Context) -> anyhow::Result<()> {
    let snapshot = crate::agent::context::current_snapshot_pretty(cx.editor)?;
    open_agent_json_scratch(cx, snapshot)
}

fn open_agent_session(cx: &mut compositor::Context) -> anyhow::Result<()> {
    let session = crate::agent::session::new_session_pretty(cx.editor)?;
    open_agent_json_scratch(cx, session)
}

fn open_agent_acp_handshake(cx: &mut compositor::Context) -> anyhow::Result<()> {
    let messages = crate::agent::acp::session_handshake_pretty(cx.editor)?;
    open_agent_json_scratch(cx, messages)
}

fn open_agent_launch_config(cx: &mut compositor::Context) -> anyhow::Result<()> {
    let config = cx.editor.config().agent.launch_config()?;
    open_agent_json_scratch(cx, serde_json::to_string_pretty(&config)?)
}

fn start_agent(cx: &mut compositor::Context) -> anyhow::Result<()> {
    let launch_config = cx.editor.config().agent.launch_config()?;
    let handshake = crate::agent::acp::session_handshake(cx.editor)?;
    let agent_name = launch_config.name.clone();

    cx.jobs.callback(async move {
        crate::agent::runtime::start(launch_config, handshake).await?;
        Ok(job::Callback::Editor(Box::new(move |editor| {
            editor.set_status(format!("Agent '{agent_name}' started"));
        })))
    });

    Ok(())
}

fn stop_agent(cx: &mut compositor::Context) {
    cx.jobs.callback(async move {
        let stopped = crate::agent::runtime::stop().await?;
        Ok(job::Callback::Editor(Box::new(move |editor| {
            if let Some(name) = stopped {
                editor.set_status(format!("Agent '{name}' stopped"));
            } else {
                editor.set_status("No agent is running");
            }
        })))
    });
}

fn cancel_agent(cx: &mut compositor::Context) {
    crate::agent::runtime::cancel_all();
    crate::agent::runtime::clear_latest_patch();
    close_agent_patch_buffers(cx.editor);
    let cancelled = cancel_agent_pending_turns(cx.editor);

    cx.jobs.callback(async move {
        let _ = crate::agent::runtime::stop().await;
        Ok(job::Callback::Editor(Box::new(move |editor| {
            if cancelled == 0 {
                editor.set_status("Agent cancellation requested");
            } else {
                editor.set_status(format!("Agent cancelled {cancelled} pending turn(s)"));
            }
        })))
    });
}

fn chat_agent(cx: &mut compositor::Context) {
    cx.jobs.callback(async move {
        Ok(job::Callback::EditorCompositor(Box::new(
            move |editor, compositor| {
                let mut prompt = ui::Prompt::new(
                    "agent: ".into(),
                    None,
                    |_editor, _input| Vec::new(),
                    move |cx, input, event| {
                        if event != PromptEvent::Validate {
                            return;
                        }

                        let input = input.trim();
                        if input.is_empty() {
                            cx.editor.set_error("agent prompt requires text");
                            return;
                        }

                        if let Err(err) = prompt_agent_turn(
                            cx,
                            input.to_string(),
                            crate::agent::runtime::AgentTranscriptKind::Chat,
                            false,
                        ) {
                            cx.editor.set_error(err.to_string());
                        }
                    },
                );
                prompt.recalculate_completion(editor);
                compositor.push(Box::new(prompt));
            },
        )))
    });
}

fn explain_agent(cx: &mut compositor::Context) -> anyhow::Result<()> {
    prompt_agent_turn(
        cx,
        "Explain this selected code.".to_string(),
        crate::agent::runtime::AgentTranscriptKind::Explain,
        false,
    )
}

fn fix_agent(cx: &mut compositor::Context) -> anyhow::Result<()> {
    prompt_agent_turn(
        cx,
        "Find and fix the bug or problem in this selected code.".to_string(),
        crate::agent::runtime::AgentTranscriptKind::Fix,
        false,
    )
}

fn refactor_agent(cx: &mut compositor::Context) -> anyhow::Result<()> {
    prompt_agent_turn(
        cx,
        "Refactor this selected code cleanly.".to_string(),
        crate::agent::runtime::AgentTranscriptKind::Refactor,
        false,
    )
}

fn edit_agent(cx: &mut compositor::Context) -> anyhow::Result<()> {
    prompt_agent_turn(
        cx,
        "Edit this selected code. Return a git-apply compatible unified diff patch with file paths and enough surrounding context."
            .to_string(),
        crate::agent::runtime::AgentTranscriptKind::Edit,
        true,
    )
}

fn open_agent_panel(cx: &mut compositor::Context, status: &'static str) {
    let position = agent_panel_position(cx.editor);
    let action = agent_panel_action(position);
    let (doc_id, created) = agent_transcript_doc_id(cx.editor, action);
    let view_id = prepare_agent_transcript_doc(cx.editor, doc_id, action);
    if created {
        place_agent_panel(cx.editor, position);
    }
    resize_agent_panel(cx.editor, view_id);
    render_agent_transcript_editor(cx.editor, doc_id, view_id);
    cx.editor.set_status(status);
}

fn agent_panel_position(editor: &Editor) -> AgentPanelPosition {
    match AGENT_PANEL_POSITION_OVERRIDE.load(Ordering::Relaxed) {
        1 => AgentPanelPosition::Left,
        2 => AgentPanelPosition::Right,
        3 => AgentPanelPosition::Top,
        4 => AgentPanelPosition::Bottom,
        _ => editor.config().agent.panel_position,
    }
}

fn set_agent_panel_position_override(position: AgentPanelPosition) {
    let value = match position {
        AgentPanelPosition::Left => 1,
        AgentPanelPosition::Right => 2,
        AgentPanelPosition::Top => 3,
        AgentPanelPosition::Bottom => 4,
    };
    AGENT_PANEL_POSITION_OVERRIDE.store(value, Ordering::Relaxed);
}

fn clear_agent_panel(cx: &mut compositor::Context) {
    let Some(doc_id) = crate::agent::runtime::transcript_doc_id()
        .filter(|doc_id| cx.editor.document(*doc_id).is_some())
    else {
        cx.editor.set_status("No agent transcript to clear");
        return;
    };

    let view_id = prepare_agent_transcript_doc(cx.editor, doc_id, Action::Replace);
    resize_agent_panel(cx.editor, view_id);
    crate::agent::runtime::clear_transcript_turns();
    render_agent_transcript_editor(cx.editor, doc_id, view_id);
    cx.editor.set_status("Agent transcript cleared");
}

fn goto_agent_turn(cx: &mut compositor::Context, next: bool) {
    let Some(doc_id) = crate::agent::runtime::transcript_doc_id()
        .filter(|doc_id| cx.editor.document(*doc_id).is_some())
    else {
        cx.editor.set_error("No agent transcript is available");
        return;
    };

    let scrolloff = cx.editor.config().scrolloff;
    let view_id = prepare_agent_transcript_doc(cx.editor, doc_id, Action::Replace);
    resize_agent_panel(cx.editor, view_id);
    let doc = doc_mut!(cx.editor, &doc_id);
    let text = doc.text().slice(..).to_string();
    let cursor = doc
        .selection(view_id)
        .primary()
        .cursor(doc.text().slice(..));
    let cursor_byte = doc.text().char_to_byte(cursor);
    let target_byte = if next {
        text.match_indices("**You:**")
            .map(|(index, _)| index)
            .find(|index| *index > cursor_byte)
    } else {
        text[..cursor_byte].rfind("**You:**")
    };

    let Some(target_byte) = target_byte else {
        cx.editor.set_status("No more agent turns");
        return;
    };

    let target = doc.text().byte_to_char(target_byte);
    doc.set_selection(view_id, Selection::point(target));
    let view = view_mut!(cx.editor, view_id);
    view.ensure_cursor_in_view(doc, scrolloff);
}

fn open_agent_patch(cx: &mut compositor::Context) -> anyhow::Result<()> {
    let Some(proposal) = crate::agent::runtime::latest_patch() else {
        cx.editor.set_error("No agent patch proposal is available");
        return Ok(());
    };
    if !is_unified_diff(&proposal.patch) {
        cx.editor
            .set_error("Latest agent response is not an applicable patch");
        return Ok(());
    }

    open_agent_named_scratch_editor(
        cx.editor,
        agent_patch_preview(&proposal),
        "diff",
        "[agent patch]",
        true,
    )
}

fn apply_agent_patch(cx: &mut compositor::Context) {
    let Some(proposal) = crate::agent::runtime::latest_patch() else {
        cx.editor.set_error("No agent patch proposal is available");
        return;
    };
    if !is_unified_diff(&proposal.patch) {
        cx.editor
            .set_error("Latest agent response is not an applicable patch");
        return;
    }
    let apply_cwd = PathBuf::from(proposal.cwd.clone());
    let patch = proposal.patch.clone();

    cx.jobs.callback(async move {
        Ok(job::Callback::EditorCompositor(Box::new(
            move |_editor, compositor| {
                let patch = patch.clone();
                let apply_cwd = apply_cwd.clone();
                let prompt = ui::Prompt::new(
                    "apply latest agent patch? [y/N] ".into(),
                    None,
                    |_editor, _input| Vec::new(),
                    move |cx, input, event| {
                        if event != PromptEvent::Validate {
                            return;
                        }

                        match input.trim().to_ascii_lowercase().as_str() {
                            "y" | "yes" => {
                                let apply_cwd = apply_cwd.clone();
                                let original_patch = patch.clone();
                                let normalized_patch =
                                    normalize_patch_paths_for_apply(&patch, &apply_cwd);
                                cx.editor.set_status("Applying agent patch...");
                                cx.jobs.callback(async move {
                                    let result = apply_agent_patch_with_git(
                                        normalized_patch.clone(),
                                        apply_cwd.clone(),
                                    )
                                    .await;
                                    Ok(job::Callback::Editor(Box::new(move |editor| {
                                        match result {
                                            Ok(()) => {
                                                close_agent_patch_buffers(editor);
                                                let (reloaded, skipped) =
                                                    reload_unmodified_file_documents(editor);
                                                if skipped == 0 {
                                                    editor.set_status(format!(
                                                        "Agent patch applied; reloaded {reloaded} buffer(s)"
                                                    ));
                                                } else {
                                                    editor.set_status(format!(
                                                        "Agent patch applied; reloaded {reloaded} buffer(s), skipped {skipped} modified buffer(s)"
                                                    ));
                                                }
                                            }
                                            Err(error) => {
                                                let diagnostics = agent_patch_apply_diagnostics(
                                                    &error,
                                                    &apply_cwd,
                                                    &original_patch,
                                                    &normalized_patch,
                                                );
                                                if let Err(open_error) = open_agent_scratch_editor(
                                                    editor,
                                                    diagnostics,
                                                    "markdown",
                                                    "[agent apply diagnostics]",
                                                ) {
                                                    editor.set_error(format!(
                                                        "Agent patch apply failed: {error}; also failed to open diagnostics: {open_error}"
                                                    ));
                                                } else {
                                                    editor.set_error(
                                                        "Agent patch apply failed; opened diagnostics",
                                                    );
                                                }
                                            }
                                        }
                                    })))
                                });
                            }
                            _ => cx.editor.set_status("Agent patch apply cancelled"),
                        }
                    },
                );
                compositor.push(Box::new(prompt));
            },
        )))
    });
}

fn agent_patch_preview(proposal: &crate::agent::runtime::AgentPatchProposal) -> String {
    format!(
        "# agent patch\n# turn: {}\n# cwd: {}\n# source: {}\n\n{}",
        proposal.request_id,
        proposal.cwd,
        proposal.source_path.as_deref().unwrap_or("<unknown>"),
        proposal.patch.trim()
    )
}

fn agent_patch_apply_diagnostics(
    error: &anyhow::Error,
    cwd: &Path,
    original_patch: &str,
    normalized_patch: &str,
) -> String {
    let normalized_section = if original_patch.trim() == normalized_patch.trim() {
        "No path normalization was applied.".to_string()
    } else {
        format!("```diff\n{}\n```", normalized_patch.trim())
    };

    format!(
        "# Agent Patch Apply Failed\n\n{}\n\n## Apply Cwd\n\n`{}`\n\n## Original Agent Patch\n\n```diff\n{}\n```\n\n## Normalized Patch Sent To Git\n\n{}\n",
        error,
        cwd.display(),
        original_patch.trim(),
        normalized_section,
    )
}

async fn apply_agent_patch_with_git(patch: String, cwd: PathBuf) -> anyhow::Result<()> {
    run_git_apply(&patch, &cwd, true).await?;
    run_git_apply(&patch, &cwd, false).await
}

fn normalize_patch_paths_for_apply(patch: &str, cwd: &Path) -> String {
    patch
        .lines()
        .map(|line| {
            if let Some(rest) = line.strip_prefix("diff --git ") {
                let paths = rest.split_whitespace().collect::<Vec<_>>();
                if paths.len() == 2 {
                    return format!(
                        "diff --git {} {}",
                        normalize_patch_path_token(paths[0], cwd),
                        normalize_patch_path_token(paths[1], cwd)
                    );
                }
            }

            if line.starts_with("--- ") || line.starts_with("+++ ") {
                return normalize_patch_file_header(line, cwd);
            }

            line.to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn normalize_patch_file_header(line: &str, cwd: &Path) -> String {
    let (prefix, rest) = line.split_at(4);
    let split_at = rest
        .char_indices()
        .find_map(|(index, char)| char.is_whitespace().then_some(index));
    let Some(split_at) = split_at else {
        return format!("{prefix}{}", normalize_patch_path_token(rest, cwd));
    };
    let (path, suffix) = rest.split_at(split_at);
    format!(
        "{prefix}{}{}",
        normalize_patch_path_token(path, cwd),
        suffix
    )
}

fn normalize_patch_path_token(token: &str, cwd: &Path) -> String {
    if token == "/dev/null" {
        return token.to_string();
    }

    let (prefix, path) = match token
        .strip_prefix("a/")
        .map(|path| ("a/", path))
        .or_else(|| token.strip_prefix("b/").map(|path| ("b/", path)))
    {
        Some((prefix, path)) => (prefix, path),
        None => ("", token),
    };

    let relative = normalize_patch_relative_path(path, cwd);
    format!("{prefix}{}", relative.display())
}

fn normalize_patch_relative_path(path: &str, cwd: &Path) -> PathBuf {
    let path = Path::new(path);
    if path.is_absolute() {
        return path
            .strip_prefix(cwd)
            .map(Path::to_path_buf)
            .unwrap_or_else(|_| path.to_path_buf());
    }

    if cwd.join(path).exists()
        || cwd
            .join(path.parent().unwrap_or_else(|| Path::new("")))
            .exists()
    {
        return path.to_path_buf();
    }

    let components = path.components().collect::<Vec<_>>();
    for index in 1..components.len() {
        let candidate = components[index..].iter().collect::<PathBuf>();
        let candidate_parent = candidate.parent().unwrap_or_else(|| Path::new(""));
        if cwd.join(&candidate).exists() || cwd.join(candidate_parent).exists() {
            return candidate;
        }
    }

    path.to_path_buf()
}

async fn run_git_apply(patch: &str, cwd: &PathBuf, check: bool) -> anyhow::Result<()> {
    use std::process::Stdio;
    use tokio::io::AsyncWriteExt;
    use tokio::process::Command;

    let mut command = Command::new("git");
    command
        .current_dir(cwd)
        .arg("apply")
        .arg("--whitespace=nowarn");
    if check {
        command.arg("--check");
    }
    let mut child = command
        .arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    let mut stdin = child.stdin.take().expect("git apply stdin is piped");
    stdin
        .write_all(format!("{}\n", patch.trim_end()).as_bytes())
        .await?;
    drop(stdin);

    let output = child.wait_with_output().await?;
    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    let mode = if check { "check" } else { "apply" };
    anyhow::bail!(
        "git apply {mode} failed in {}: {}",
        cwd.display(),
        stderr.trim()
    );
}

fn reload_unmodified_file_documents(editor: &mut Editor) -> (usize, usize) {
    let scrolloff = editor.config().scrolloff;
    let current_view_id = view!(editor).id;
    let mut skipped = 0;

    let docs_view_ids: Vec<(DocumentId, Vec<ViewId>)> = editor
        .documents_mut()
        .filter_map(|doc| {
            doc.path().as_ref()?;
            if doc.is_modified() {
                skipped += 1;
                return None;
            }

            let mut view_ids: Vec<_> = doc.selections().keys().cloned().collect();
            if view_ids.is_empty() {
                doc.ensure_view_init(current_view_id);
                view_ids.push(current_view_id);
            }

            Some((doc.id(), view_ids))
        })
        .collect();

    let mut reloaded = 0;
    for (doc_id, view_ids) in docs_view_ids {
        let doc = doc_mut!(editor, &doc_id);
        let view = view_mut!(editor, view_ids[0]);
        view.sync_changes(doc);

        if doc.reload(view, &editor.diff_providers).is_err() {
            skipped += 1;
            continue;
        }

        reloaded += 1;
        if let Some(path) = doc.path() {
            editor
                .language_servers
                .file_event_handler
                .file_changed(path.clone());
        }

        for view_id in view_ids {
            let view = view_mut!(editor, view_id);
            if view.doc.eq(&doc_id) {
                view.ensure_cursor_in_view(doc, scrolloff);
            }
        }
    }

    (reloaded, skipped)
}

fn close_agent_patch_buffers(editor: &mut Editor) {
    let patch_doc_ids = editor
        .documents()
        .filter(|doc| doc.path().is_none() && doc.display_name() == "[agent patch]")
        .map(|doc| doc.id())
        .collect::<Vec<_>>();

    for doc_id in patch_doc_ids {
        let _ = editor.close_document(doc_id, true);
    }
}

fn agent_panel_action(position: AgentPanelPosition) -> Action {
    match position {
        AgentPanelPosition::Left | AgentPanelPosition::Right => Action::VerticalSplit,
        AgentPanelPosition::Top | AgentPanelPosition::Bottom => Action::HorizontalSplit,
    }
}

fn place_agent_panel(editor: &mut Editor, position: AgentPanelPosition) {
    match position {
        AgentPanelPosition::Left => {
            editor.swap_split_in_direction(helix_view::tree::Direction::Left)
        }
        AgentPanelPosition::Top => editor.swap_split_in_direction(helix_view::tree::Direction::Up),
        AgentPanelPosition::Right | AgentPanelPosition::Bottom => {}
    }
}

fn resize_agent_panel(editor: &mut Editor, view_id: helix_view::ViewId) {
    let panel_size = agent_panel_size(editor);
    editor.tree.resize_view_to_percent(view_id, panel_size);
}

fn agent_panel_size(editor: &Editor) -> u16 {
    match AGENT_PANEL_SIZE_OVERRIDE.load(Ordering::Relaxed) {
        0 => editor.config().agent.panel_size,
        size => size,
    }
}

fn resize_agent_panel_command(cx: &mut compositor::Context, args: Args) -> anyhow::Result<()> {
    let size_arg = args
        .get(1)
        .map(str::trim)
        .filter(|arg| !arg.is_empty())
        .context("agent resize requires a percentage, +delta, or -delta")?;

    let current_size = agent_panel_size(cx.editor);
    let new_size = if let Some(delta) = size_arg.strip_prefix('+') {
        current_size.saturating_add(delta.parse::<u16>()?)
    } else if let Some(delta) = size_arg.strip_prefix('-') {
        current_size.saturating_sub(delta.parse::<u16>()?)
    } else {
        size_arg.parse::<u16>()?
    }
    .clamp(5, 95);

    let Some(doc_id) = crate::agent::runtime::transcript_doc_id()
        .filter(|doc_id| cx.editor.document(*doc_id).is_some())
    else {
        cx.editor.set_error("No agent panel is available");
        return Ok(());
    };

    let view_id = agent_transcript_view_id(cx.editor, doc_id)
        .unwrap_or_else(|| prepare_agent_transcript_doc(cx.editor, doc_id, Action::Replace));
    AGENT_PANEL_SIZE_OVERRIDE.store(new_size, Ordering::Relaxed);
    resize_agent_panel(cx.editor, view_id);
    cx.editor
        .set_status(format!("Agent panel resized to {new_size}%"));

    Ok(())
}

fn position_agent_panel_command(cx: &mut compositor::Context, args: Args) -> anyhow::Result<()> {
    let position_arg = args
        .get(1)
        .map(str::trim)
        .filter(|arg| !arg.is_empty())
        .context("agent position requires left, right, top, or bottom")?;
    let position = parse_agent_panel_position(position_arg)?;
    set_agent_panel_position_override(position);

    let Some(doc_id) = crate::agent::runtime::transcript_doc_id()
        .filter(|doc_id| cx.editor.document(*doc_id).is_some())
    else {
        cx.editor
            .set_status(format!("Agent panel position set to {position_arg}"));
        return Ok(());
    };

    move_agent_panel(cx.editor, doc_id, position);
    cx.editor
        .set_status(format!("Agent panel moved to {position_arg}"));
    Ok(())
}

fn parse_agent_panel_position(position: &str) -> anyhow::Result<AgentPanelPosition> {
    match position {
        "left" | "l" => Ok(AgentPanelPosition::Left),
        "right" | "r" => Ok(AgentPanelPosition::Right),
        "top" | "t" => Ok(AgentPanelPosition::Top),
        "bottom" | "b" => Ok(AgentPanelPosition::Bottom),
        _ => anyhow::bail!("agent position must be left, right, top, or bottom"),
    }
}

fn move_agent_panel(
    editor: &mut Editor,
    doc_id: helix_view::DocumentId,
    position: AgentPanelPosition,
) {
    if let Some(view_id) = agent_transcript_view_id(editor, doc_id) {
        let previous_doc_id = editor.tree.try_get(view_id).and_then(|view| {
            view.docs_access_history
                .iter()
                .rev()
                .copied()
                .find(|prev_doc_id| {
                    *prev_doc_id != doc_id && editor.document(*prev_doc_id).is_some()
                })
        });

        if let Some(previous_doc_id) = previous_doc_id {
            editor.tree.focus = view_id;
            editor.switch(previous_doc_id, Action::Replace);
        } else if editor.tree.views().count() > 1 {
            editor.close(view_id);
        } else {
            resize_agent_panel(editor, view_id);
            render_agent_transcript_editor(editor, doc_id, view_id);
            return;
        }
    }

    let action = agent_panel_action(position);
    editor.switch(doc_id, action);
    let view_id = view!(editor).id;
    let doc = doc_mut!(editor, &doc_id);
    doc.ensure_view_init(view_id);
    place_agent_panel(editor, position);
    resize_agent_panel(editor, view_id);
    render_agent_transcript_editor(editor, doc_id, view_id);
}

fn agent_prompt_with_formatting(
    kind: crate::agent::runtime::AgentTranscriptKind,
    prompt: &str,
    language_id: Option<&str>,
) -> String {
    let language = language_id.unwrap_or("text");
    let format_rules = match kind {
        crate::agent::runtime::AgentTranscriptKind::Edit => {
            "Formatting rules:\n- Return only a git-apply compatible unified diff.\n- Do not wrap the diff in prose unless explicitly asked.\n- Use file paths that are relative to the workspace root."
        }
        crate::agent::runtime::AgentTranscriptKind::Explain => {
            "Formatting rules:\n- Use concise prose.\n- When referencing code, use fenced code blocks with the language id.\n- Prefer short snippets over repeating the whole selection."
        }
        crate::agent::runtime::AgentTranscriptKind::Fix
        | crate::agent::runtime::AgentTranscriptKind::Refactor => {
            "Formatting rules:\n- Explain the issue or refactor briefly.\n- Put code examples in fenced code blocks with the language id.\n- If proposing a concrete edit, use a fenced diff block."
        }
        crate::agent::runtime::AgentTranscriptKind::Chat => {
            "Formatting rules:\n- Use markdown.\n- Put code in fenced code blocks with the language id when applicable."
        }
    };

    format!("{prompt}\n\n{format_rules}\n- Current language id: `{language}`.")
}

fn visible_agent_prompt(
    snapshot: &crate::agent::context::EditorSnapshot,
    kind: crate::agent::runtime::AgentTranscriptKind,
    prompt: &str,
) -> String {
    let base = match kind {
        crate::agent::runtime::AgentTranscriptKind::Chat => prompt.trim().to_string(),
        crate::agent::runtime::AgentTranscriptKind::Explain => "Explain".to_string(),
        crate::agent::runtime::AgentTranscriptKind::Fix => "Fix".to_string(),
        crate::agent::runtime::AgentTranscriptKind::Refactor => "Refactor".to_string(),
        crate::agent::runtime::AgentTranscriptKind::Edit => "Edit".to_string(),
    };

    crate::agent::context::prompt_with_primary_selection_snapshot(snapshot, &base)
}

fn prompt_agent_turn(
    cx: &mut compositor::Context,
    prompt: String,
    kind: crate::agent::runtime::AgentTranscriptKind,
    store_patch: bool,
) -> anyhow::Result<()> {
    let launch_config = cx.editor.config().agent.launch_config()?;
    let agent_name = launch_config.name.clone();
    let handshake = crate::agent::acp::session_handshake(cx.editor)?;
    let snapshot = crate::agent::context::current_snapshot(cx.editor);
    let transcript_prompt = visible_agent_prompt(&snapshot, kind, prompt.trim());
    let prompt =
        agent_prompt_with_formatting(kind, &prompt, snapshot.active_file.language_id.as_deref());
    let prompt = crate::agent::context::prompt_with_editor_context_snapshot(&snapshot, &prompt);
    let patch_cwd = snapshot.workspace_root.clone();
    let patch_source_path = snapshot.active_file.path.clone();
    let cancel_generation = crate::agent::runtime::cancel_generation();
    let meta = serde_json::json!({
        "helix": {
            "context": snapshot,
        }
    });
    if !store_patch {
        crate::agent::runtime::clear_latest_patch();
        close_agent_patch_buffers(cx.editor);
    }
    cx.editor.set_status("Agent is thinking...");
    let pending_range =
        append_agent_pending_transcript_editor(cx.editor, kind, &transcript_prompt)?;

    cx.jobs.callback(async move {
        let result = async {
            let started = crate::agent::runtime::ensure_started(launch_config, handshake).await?;
            let status = if started {
                format!("Agent started; request sent to {agent_name}...")
            } else {
                format!("Agent request sent to {agent_name}...")
            };
            helix_event::status::report(helix_event::status::StatusMessage {
                severity: helix_event::status::Severity::Info,
                message: status.into(),
            })
            .await;
            let stream_pending_range = pending_range;
            let turn = crate::agent::runtime::send_prompt_turn_streaming(
                prompt,
                Some(meta),
                move |message| {
                    if crate::agent::runtime::is_cancelled(cancel_generation) {
                        return;
                    }

                    match agent_message_update_markdown_chunk(message) {
                        Ok(Some(AgentStreamingUpdate::Response(chunk))) => {
                            crate::job::dispatch_blocking(move |editor, _compositor| {
                                if let Err(err) = append_agent_streaming_transcript_editor(
                                    editor,
                                    stream_pending_range,
                                    chunk,
                                ) {
                                    editor.set_error(err.to_string());
                                }
                            });
                        }
                        Ok(Some(AgentStreamingUpdate::Status(status))) => {
                            crate::job::dispatch_blocking(move |editor, _compositor| {
                                if let Err(err) = update_agent_streaming_status_editor(
                                    editor,
                                    stream_pending_range,
                                    status,
                                ) {
                                    editor.set_error(err.to_string());
                                }
                            });
                        }
                        Ok(None) => {}
                        Err(err) => {
                            let message = err.to_string();
                            crate::job::dispatch_blocking(move |editor, _compositor| {
                                editor.set_error(message);
                            });
                        }
                    }
                },
            )
            .await?;
            if crate::agent::runtime::is_cancelled(cancel_generation) {
                let _ = crate::agent::runtime::stop().await;
                return Ok(None);
            }
            let contents = agent_turn_response_markdown(&turn)?;
            let patch_stored = if store_patch {
                if let Some(patch) = extract_agent_patch(&contents) {
                    crate::agent::runtime::set_latest_patch(
                        crate::agent::runtime::AgentPatchProposal {
                            patch,
                            cwd: patch_cwd,
                            source_path: patch_source_path,
                            request_id: turn.request_id,
                        },
                    );
                    true
                } else {
                    crate::agent::runtime::clear_latest_patch();
                    false
                }
            } else {
                false
            };
            let status_suffix = if store_patch && !patch_stored {
                "; no applicable patch found"
            } else {
                ""
            };
            let status = format!("Agent turn #{}{status_suffix}", turn.request_id);
            let status = if status_suffix.is_empty() {
                format!("{status} complete")
            } else {
                status
            }
            .to_string();
            anyhow::Ok(Some((contents, status)))
        }
        .await;

        Ok(job::Callback::Editor(Box::new(
            move |editor| match result {
                Ok(Some((contents, status))) => {
                    if let Err(err) =
                        replace_agent_pending_transcript_editor(editor, pending_range, contents)
                    {
                        editor.set_error(err.to_string());
                    } else {
                        let (_reloaded, skipped) = reload_unmodified_file_documents(editor);
                        if skipped > 0 {
                            editor.set_status(format!(
                                "{status}; skipped {skipped} modified buffer(s)"
                            ));
                            return;
                        }
                        editor.set_status(status);
                    }
                }
                Ok(None) => {
                    editor.set_status("Ignored cancelled agent response");
                }
                Err(err) => {
                    crate::agent::runtime::clear_latest_patch();
                    let message = format!("Agent turn failed:\n\n```text\n{err:#}\n```");
                    if let Err(render_err) =
                        fail_agent_pending_transcript_editor(editor, pending_range, message)
                    {
                        editor.set_error(render_err.to_string());
                    } else {
                        editor.set_error("Agent turn failed");
                    }
                }
            },
        )))
    });

    Ok(())
}

fn show_agent_status(cx: &mut compositor::Context) {
    cx.editor.set_status(agent_status_message());
}

fn agent_status_message() -> String {
    match crate::agent::runtime::status() {
        crate::agent::runtime::AgentRuntimeStatus::Running {
            name,
            session_id,
            next_request_id,
        } => {
            let session = session_id.as_deref().unwrap_or("<pending>");
            format!("Agent '{name}' is running, session {session}, next request #{next_request_id}")
        }
        crate::agent::runtime::AgentRuntimeStatus::Busy(status) => {
            let session = status.session_id.as_deref().unwrap_or("<pending>");
            let request = status
                .request_id
                .map(|request_id| format!(", request #{request_id}"))
                .unwrap_or_default();
            format!(
                "Agent '{}' is busy, session {session}{request}",
                status.name
            )
        }
        crate::agent::runtime::AgentRuntimeStatus::Stopped => "No agent is running".to_string(),
    }
}

fn open_agent_json_scratch(cx: &mut compositor::Context, contents: String) -> anyhow::Result<()> {
    open_agent_json_scratch_editor(cx.editor, contents)
}

fn open_agent_json_scratch_editor(editor: &mut Editor, contents: String) -> anyhow::Result<()> {
    open_agent_scratch_editor(editor, contents, "json", "[agent json]")
}

#[derive(Debug, Clone, Copy)]
struct AgentPendingTurn {
    doc_id: helix_view::DocumentId,
    id: u64,
}

fn append_agent_pending_transcript_editor(
    editor: &mut Editor,
    kind: crate::agent::runtime::AgentTranscriptKind,
    prompt: &str,
) -> anyhow::Result<AgentPendingTurn> {
    let position = agent_panel_position(editor);
    let action = agent_panel_action(position);
    let (doc_id, created) = agent_transcript_doc_id(editor, action);
    let view_id = prepare_agent_transcript_doc(editor, doc_id, action);
    if created {
        place_agent_panel(editor, position);
    }
    resize_agent_panel(editor, view_id);
    let pending_id = AGENT_PENDING_TURN_ID.fetch_add(1, Ordering::Relaxed);
    crate::agent::runtime::append_transcript_turn(pending_id, kind, prompt.trim().to_string());
    render_agent_transcript_editor(editor, doc_id, view_id);
    move_agent_transcript_cursor_to_end(editor, doc_id, view_id);

    Ok(AgentPendingTurn {
        doc_id,
        id: pending_id,
    })
}

fn replace_agent_pending_transcript_editor(
    editor: &mut Editor,
    pending_turn: AgentPendingTurn,
    contents: String,
) -> anyhow::Result<()> {
    if editor.document(pending_turn.doc_id).is_none() {
        return Ok(());
    }

    let position = agent_panel_position(editor);
    let action = agent_panel_action(position);
    let view_id = prepare_agent_transcript_doc(editor, pending_turn.doc_id, action);
    resize_agent_panel(editor, view_id);
    crate::agent::runtime::complete_transcript_turn(pending_turn.id, contents.trim().to_string());
    render_agent_transcript_editor(editor, pending_turn.doc_id, view_id);
    let doc = doc_mut!(editor, &pending_turn.doc_id);
    let cursor = doc.text().len_chars();
    doc.set_selection(view_id, Selection::point(cursor));
    let scrolloff = editor.config().scrolloff;
    let (view, doc) = current!(editor);
    view.ensure_cursor_in_view(doc, scrolloff);

    Ok(())
}

fn append_agent_streaming_transcript_editor(
    editor: &mut Editor,
    pending_turn: AgentPendingTurn,
    chunk: String,
) -> anyhow::Result<()> {
    crate::agent::runtime::append_transcript_turn_response(pending_turn.id, chunk);

    if editor.document(pending_turn.doc_id).is_none() {
        return Ok(());
    }

    let Some(view_id) = agent_transcript_view_id(editor, pending_turn.doc_id) else {
        return Ok(());
    };

    render_agent_transcript_editor(editor, pending_turn.doc_id, view_id);
    resize_agent_panel(editor, view_id);
    move_agent_transcript_cursor_to_end(editor, pending_turn.doc_id, view_id);

    Ok(())
}

fn update_agent_streaming_status_editor(
    editor: &mut Editor,
    pending_turn: AgentPendingTurn,
    status: String,
) -> anyhow::Result<()> {
    crate::agent::runtime::update_transcript_turn_status(pending_turn.id, status);

    if editor.document(pending_turn.doc_id).is_none() {
        return Ok(());
    }

    let Some(view_id) = agent_transcript_view_id(editor, pending_turn.doc_id) else {
        return Ok(());
    };

    render_agent_transcript_editor(editor, pending_turn.doc_id, view_id);
    resize_agent_panel(editor, view_id);
    move_agent_transcript_cursor_to_end(editor, pending_turn.doc_id, view_id);

    Ok(())
}

fn fail_agent_pending_transcript_editor(
    editor: &mut Editor,
    pending_turn: AgentPendingTurn,
    contents: String,
) -> anyhow::Result<()> {
    if editor.document(pending_turn.doc_id).is_none() {
        return Ok(());
    }

    let position = agent_panel_position(editor);
    let action = agent_panel_action(position);
    let view_id = prepare_agent_transcript_doc(editor, pending_turn.doc_id, action);
    resize_agent_panel(editor, view_id);
    crate::agent::runtime::fail_transcript_turn(pending_turn.id, contents);
    render_agent_transcript_editor(editor, pending_turn.doc_id, view_id);
    let doc = doc_mut!(editor, &pending_turn.doc_id);
    let cursor = doc.text().len_chars();
    doc.set_selection(view_id, Selection::point(cursor));
    let scrolloff = editor.config().scrolloff;
    let (view, doc) = current!(editor);
    view.ensure_cursor_in_view(doc, scrolloff);

    Ok(())
}

fn cancel_agent_pending_turns(editor: &mut Editor) -> usize {
    let Some(doc_id) = crate::agent::runtime::transcript_doc_id()
        .filter(|doc_id| editor.document(*doc_id).is_some())
    else {
        return 0;
    };

    let cancelled = crate::agent::runtime::cancel_pending_transcript_turns();
    if cancelled > 0 {
        let view_id = prepare_agent_transcript_doc(editor, doc_id, Action::Replace);
        resize_agent_panel(editor, view_id);
        render_agent_transcript_editor(editor, doc_id, view_id);
    }

    cancelled
}

fn render_agent_transcript_editor(
    editor: &mut Editor,
    doc_id: helix_view::DocumentId,
    view_id: helix_view::ViewId,
) {
    let loader = editor.syn_loader.load();
    let contents = crate::agent::runtime::render_transcript();
    let doc = doc_mut!(editor, &doc_id);
    doc.readonly = false;
    let transaction = Transaction::change(
        doc.text(),
        [(0, doc.text().len_chars(), Some(contents.into()))].into_iter(),
    );
    doc.apply(&transaction, view_id);
    doc.set_language_by_language_id("markdown", &loader).ok();
    doc.reset_modified();
    doc.readonly = true;
}

fn move_agent_transcript_cursor_to_end(
    editor: &mut Editor,
    doc_id: helix_view::DocumentId,
    view_id: helix_view::ViewId,
) {
    let scrolloff = editor.config().scrolloff;
    let doc = doc_mut!(editor, &doc_id);
    let cursor = doc.text().len_chars();
    doc.set_selection(view_id, Selection::point(cursor));
    let view = view_mut!(editor, view_id);
    view.ensure_cursor_in_view(doc, scrolloff);
}

fn agent_transcript_doc_id(editor: &mut Editor, action: Action) -> (helix_view::DocumentId, bool) {
    if let Some(doc_id) = crate::agent::runtime::transcript_doc_id()
        .filter(|doc_id| editor.document(*doc_id).is_some())
    {
        return (doc_id, false);
    }

    let doc_id = editor.new_file(action);
    mark_agent_document(doc_mut!(editor, &doc_id), "[agent]");
    crate::agent::runtime::set_transcript_doc_id(doc_id);
    (doc_id, true)
}

fn prepare_agent_transcript_doc(
    editor: &mut Editor,
    doc_id: helix_view::DocumentId,
    action: Action,
) -> helix_view::ViewId {
    if let Some(view_id) = focus_existing_agent_transcript_doc(editor, doc_id) {
        return view_id;
    }

    editor.switch(doc_id, action);
    let view_id = view!(editor).id;
    let doc = doc_mut!(editor, &doc_id);
    doc.ensure_view_init(view_id);
    view_id
}

fn focus_existing_agent_transcript_doc(
    editor: &mut Editor,
    doc_id: helix_view::DocumentId,
) -> Option<helix_view::ViewId> {
    let transcript_views: Vec<_> = editor
        .tree
        .views()
        .filter(|(view, _)| view.doc == doc_id)
        .map(|(view, focused)| (view.id, focused, view.docs_access_history.clone()))
        .collect();

    if transcript_views.is_empty() {
        return None;
    }

    let keep_view_id = transcript_views
        .iter()
        .find(|(_, _, history)| history.is_empty())
        .or_else(|| transcript_views.iter().find(|(_, focused, _)| *focused))
        .map(|(view_id, _, _)| *view_id)
        .unwrap_or(transcript_views[0].0);

    for (view_id, _, history) in transcript_views {
        if view_id == keep_view_id {
            continue;
        }

        if let Some(prev_doc_id) =
            history.iter().rev().copied().find(|prev_doc_id| {
                *prev_doc_id != doc_id && editor.document(*prev_doc_id).is_some()
            })
        {
            editor.tree.focus = view_id;
            editor.switch(prev_doc_id, Action::Replace);
        } else if editor.tree.views().count() > 1 {
            editor.close(view_id);
        }
    }

    editor.tree.focus = keep_view_id;
    let doc = doc_mut!(editor, &doc_id);
    doc.ensure_view_init(keep_view_id);
    Some(keep_view_id)
}

fn agent_transcript_view_id(
    editor: &Editor,
    doc_id: helix_view::DocumentId,
) -> Option<helix_view::ViewId> {
    editor
        .tree
        .views()
        .find(|(view, _)| view.doc == doc_id)
        .map(|(view, _)| view.id)
}

fn open_agent_scratch_editor(
    editor: &mut Editor,
    contents: String,
    language_id: &str,
    display_name: &str,
) -> anyhow::Result<()> {
    open_agent_named_scratch_editor(editor, contents, language_id, display_name, false)
}

fn open_agent_named_scratch_editor(
    editor: &mut Editor,
    contents: String,
    language_id: &str,
    display_name: &str,
    reuse_existing: bool,
) -> anyhow::Result<()> {
    if reuse_existing {
        if let Some(doc_id) = agent_named_scratch_doc_id(editor, display_name) {
            editor.switch(doc_id, Action::Replace);
            let view_id = view!(editor).id;
            replace_agent_scratch_contents(editor, doc_id, view_id, contents, language_id);
            return Ok(());
        }
    }

    let doc_id = editor.new_file(Action::HorizontalSplit);
    let view_id = view!(editor).id;
    mark_agent_document(doc_mut!(editor, &doc_id), display_name);
    replace_agent_scratch_contents(editor, doc_id, view_id, contents, language_id);

    Ok(())
}

fn replace_agent_scratch_contents(
    editor: &mut Editor,
    doc_id: helix_view::DocumentId,
    view_id: helix_view::ViewId,
    contents: String,
    language_id: &str,
) {
    let loader = editor.syn_loader.load();
    let doc = doc_mut!(editor, &doc_id);
    doc.readonly = false;
    doc.ensure_view_init(view_id);
    let delete = Transaction::delete(doc.text(), [(0, doc.text().len_chars())].into_iter());
    doc.apply(&delete, view_id);
    let selection = Selection::point(0);
    let transaction = Transaction::insert(doc.text(), &selection, contents.into());
    doc.apply(&transaction, view_id);
    doc.set_selection(view_id, Selection::point(0));
    doc.set_language_by_language_id(language_id, &loader).ok();
    doc.reset_modified();
    doc.readonly = true;
}

fn agent_named_scratch_doc_id(
    editor: &Editor,
    display_name: &str,
) -> Option<helix_view::DocumentId> {
    editor
        .documents()
        .find(|doc| doc.path().is_none() && doc.display_name() == display_name)
        .map(|doc| doc.id())
}

fn mark_agent_document(doc: &mut helix_view::Document, display_name: &str) {
    doc.set_display_name(display_name);
    doc.reset_modified();
    doc.readonly = true;
}

fn agent_turn_response_markdown(turn: &crate::agent::runtime::AgentTurn) -> anyhow::Result<String> {
    let mut response = String::new();

    for message in &turn.messages {
        let value = serde_json::to_value(message)?;
        let Some(method) = value.get("method").and_then(Value::as_str) else {
            continue;
        };

        if method != "session/update" {
            continue;
        }

        let Some(update) = value.get("params").and_then(|params| params.get("update")) else {
            continue;
        };

        let update_kind = update
            .get("sessionUpdate")
            .and_then(Value::as_str)
            .unwrap_or("update");
        let Some(text) = update
            .get("content")
            .and_then(|content| content.get("text"))
            .and_then(Value::as_str)
        else {
            continue;
        };

        match update_kind {
            "agent_message_chunk" => response.push_str(text),
            "agent_status" => {}
            "agent_thought_chunk" => {
                writeln!(response, "\n\n> {text}")?;
            }
            "user_message_chunk" => {}
            _ => {
                writeln!(response, "\n\n`{update_kind}`: {text}")?;
            }
        }
    }

    if response.trim().is_empty() {
        return Ok(serde_json::to_string_pretty(turn)?);
    }

    Ok(normalize_agent_markdown_fence_languages(response.trim()))
}

enum AgentStreamingUpdate {
    Response(String),
    Status(String),
}

fn agent_message_update_markdown_chunk(
    message: &crate::agent::acp::JsonRpcMessage,
) -> anyhow::Result<Option<AgentStreamingUpdate>> {
    let value = serde_json::to_value(message)?;
    let Some(method) = value.get("method").and_then(Value::as_str) else {
        return Ok(None);
    };

    if method != "session/update" {
        return Ok(None);
    }

    let Some(update) = value.get("params").and_then(|params| params.get("update")) else {
        return Ok(None);
    };

    let update_kind = update
        .get("sessionUpdate")
        .and_then(Value::as_str)
        .unwrap_or("update");
    let Some(text) = update
        .get("content")
        .and_then(|content| content.get("text"))
        .and_then(Value::as_str)
    else {
        return Ok(None);
    };

    match update_kind {
        "agent_message_chunk" => Ok(Some(AgentStreamingUpdate::Response(text.to_string()))),
        "agent_status" => Ok(Some(AgentStreamingUpdate::Status(text.to_string()))),
        "agent_thought_chunk" => Ok(Some(AgentStreamingUpdate::Response(format!(
            "\n\n> {text}"
        )))),
        "user_message_chunk" => Ok(None),
        _ => Ok(Some(AgentStreamingUpdate::Response(format!(
            "\n\n`{update_kind}`: {text}"
        )))),
    }
}

fn normalize_agent_markdown_fence_languages(markdown: &str) -> String {
    let mut normalized = String::with_capacity(markdown.len());
    let mut in_fence = false;

    for line in markdown.split_inclusive('\n') {
        let (line, line_ending) = line
            .strip_suffix('\n')
            .map_or((line, ""), |line| (line, "\n"));

        if let Some((rewritten, toggles_fence)) =
            normalize_agent_markdown_fence_line(line, in_fence)
        {
            normalized.push_str(&rewritten);
            if toggles_fence {
                in_fence = !in_fence;
            }
        } else {
            normalized.push_str(line);
        }
        normalized.push_str(line_ending);
    }

    normalized
}

fn normalize_agent_markdown_fence_line(line: &str, in_fence: bool) -> Option<(String, bool)> {
    let trimmed = line.trim_start();
    let rest = trimmed.strip_prefix("```")?;

    if rest.starts_with('`') {
        return None;
    }

    if in_fence {
        return rest.trim().is_empty().then(|| (line.to_string(), true));
    }

    let Some(language) = rest
        .split_whitespace()
        .next()
        .and_then(normalize_agent_markdown_fence_language)
    else {
        return Some((line.to_string(), true));
    };

    let prefix_len = line.len() - trimmed.len();
    Some((format!("{}```{language}", &line[..prefix_len]), true))
}

fn normalize_agent_markdown_fence_language(language: &str) -> Option<String> {
    let language = language
        .strip_prefix("{.")
        .and_then(|language| language.split('}').next())
        .unwrap_or(language);
    let language = language
        .strip_prefix("language-")
        .unwrap_or(language)
        .trim_matches(|ch: char| {
            !(ch.is_ascii_alphanumeric() || matches!(ch, '#' | '+' | '-' | '_' | '.'))
        });

    match language {
        "" => None,
        "golang" => Some("go".to_string()),
        "shell" | "sh" => Some("bash".to_string()),
        "plaintext" | "plain" => Some("text".to_string()),
        "patch" => Some("diff".to_string()),
        _ => Some(language.to_string()),
    }
}

#[cfg(test)]
mod agent_markdown_tests {
    use super::normalize_agent_markdown_fence_languages;

    #[test]
    fn normalizes_noisy_code_fence_languages() {
        let input = "Before\n\n```go!\nprintln(\"x\")\n```\n\n```shell session\nls\n```";
        let expected = "Before\n\n```go\nprintln(\"x\")\n```\n\n```bash\nls\n```";

        assert_eq!(normalize_agent_markdown_fence_languages(input), expected);
    }

    #[test]
    fn preserves_unlabeled_code_fences() {
        let input = "```\n```go!\n```";

        assert_eq!(normalize_agent_markdown_fence_languages(input), input);
    }
}

fn extract_agent_patch(response: &str) -> Option<String> {
    let mut in_fence = false;
    let mut fence_is_diff = false;
    let mut patch = String::new();

    for line in response.lines() {
        let trimmed = line.trim_start();
        if let Some(info) = trimmed.strip_prefix("```") {
            if in_fence {
                if fence_is_diff {
                    return normalize_agent_patch(&patch);
                }
                in_fence = false;
                fence_is_diff = false;
                continue;
            }

            in_fence = true;
            fence_is_diff = matches!(info.split_whitespace().next(), Some("diff" | "patch"));
            continue;
        }

        if in_fence && fence_is_diff {
            patch.push_str(line);
            patch.push('\n');
        }
    }

    normalize_agent_patch(response)
}

fn normalize_agent_patch(response: &str) -> Option<String> {
    let lines = response.lines().collect::<Vec<_>>();
    let start = lines
        .iter()
        .position(|line| is_diff_start_line(line.trim_start()))?;
    let mut patch_lines = Vec::new();
    let mut saw_hunk = false;

    for line in &lines[start..] {
        if is_patch_header_line(line.trim_start()) {
            saw_hunk |= line.trim_start().starts_with("@@ ");
            patch_lines.push(*line);
            continue;
        }

        if saw_hunk && is_hunk_body_line(line) {
            patch_lines.push(*line);
            continue;
        }

        break;
    }

    let patch = patch_lines.join("\n");
    if is_unified_diff(&patch) {
        Some(patch.trim().to_string())
    } else {
        None
    }
}

fn is_diff_start_line(line: &str) -> bool {
    line.starts_with("diff --git ") || line.starts_with("--- ")
}

fn is_patch_header_line(line: &str) -> bool {
    is_diff_start_line(line)
        || line.starts_with("index ")
        || line.starts_with("new file mode ")
        || line.starts_with("deleted file mode ")
        || line.starts_with("old mode ")
        || line.starts_with("new mode ")
        || line.starts_with("similarity index ")
        || line.starts_with("dissimilarity index ")
        || line.starts_with("rename from ")
        || line.starts_with("rename to ")
        || line.starts_with("copy from ")
        || line.starts_with("copy to ")
        || line.starts_with("+++ ")
        || line.starts_with("@@ ")
}

fn is_hunk_body_line(line: &str) -> bool {
    line.starts_with(' ')
        || line.starts_with('+')
        || line.starts_with('-')
        || line.starts_with('\\')
}

fn is_unified_diff(patch: &str) -> bool {
    let mut has_old_path = false;
    let mut has_new_path = false;
    let mut has_hunk = false;

    for line in patch.lines().map(str::trim_start) {
        if line.starts_with("--- ") {
            has_old_path = true;
        } else if line.starts_with("+++ ") {
            has_new_path = true;
        } else if line.starts_with("@@ ") {
            has_hunk = true;
        }
    }

    has_old_path && has_new_path && has_hunk
}

fn agent(cx: &mut compositor::Context, args: Args, event: PromptEvent) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    match args.first() {
        None | Some("context") => open_agent_context(cx),
        Some("new") => open_agent_session(cx),
        Some("acp") => open_agent_acp_handshake(cx),
        Some("launch-config") => open_agent_launch_config(cx),
        Some("start") => start_agent(cx),
        Some("cancel") => {
            cancel_agent(cx);
            Ok(())
        }
        Some("stop") => {
            stop_agent(cx);
            Ok(())
        }
        Some("chat") => {
            chat_agent(cx);
            Ok(())
        }
        Some("explain") => explain_agent(cx),
        Some("fix") => fix_agent(cx),
        Some("refactor") => refactor_agent(cx),
        Some("edit") => edit_agent(cx),
        Some("patch") => open_agent_patch(cx),
        Some("apply") => {
            apply_agent_patch(cx);
            Ok(())
        }
        Some("panel") => {
            open_agent_panel(cx, "Agent panel focused");
            Ok(())
        }
        Some("restore") => {
            open_agent_panel(cx, "Agent panel restored");
            Ok(())
        }
        Some("next") => {
            goto_agent_turn(cx, true);
            Ok(())
        }
        Some("prev") => {
            goto_agent_turn(cx, false);
            Ok(())
        }
        Some("resize") => resize_agent_panel_command(cx, args),
        Some("position") => position_agent_panel_command(cx, args),
        Some("clear") => {
            clear_agent_panel(cx);
            Ok(())
        }
        Some("status") => {
            show_agent_status(cx);
            Ok(())
        }
        Some("ask") => {
            let prompt = args
                .get(1)
                .map(str::trim)
                .filter(|prompt| !prompt.is_empty())
                .context("agent ask requires a prompt")?;
            prompt_agent_turn(
                cx,
                prompt.to_string(),
                crate::agent::runtime::AgentTranscriptKind::Chat,
                false,
            )
        }
        Some(command) => bail!("unknown agent command: {command}"),
    }
}

fn complete_agent_subcommand(_editor: &Editor, input: &str) -> Vec<ui::prompt::Completion> {
    fuzzy_match(input, AGENT_SUBCOMMANDS.iter().copied(), false)
        .into_iter()
        .map(|(subcommand, _)| ((0..), subcommand.into()))
        .collect()
}

fn agent_context(
    cx: &mut compositor::Context,
    _args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    open_agent_context(cx)
}

fn agent_new(cx: &mut compositor::Context, _args: Args, event: PromptEvent) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    open_agent_session(cx)
}

fn format(cx: &mut compositor::Context, _args: Args, event: PromptEvent) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    let (view, doc) = current_ref!(cx.editor);
    let format = doc.format(cx.editor).context(
        "A formatter isn't available, and no language server provides formatting capabilities",
    )?;
    let callback = make_format_callback(doc.id(), doc.version(), view.id, format, None);
    cx.jobs.callback(callback);

    Ok(())
}

fn set_indent_style(
    cx: &mut compositor::Context,
    args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    use IndentStyle::*;

    // If no argument, report current indent style.
    if args.is_empty() {
        let style = doc!(cx.editor).indent_style;
        cx.editor.set_status(match style {
            Tabs => "tabs".to_owned(),
            Spaces(1) => "1 space".to_owned(),
            Spaces(n) => format!("{} spaces", n),
        });
        return Ok(());
    }

    // Attempt to parse argument as an indent style.
    let style = match args.first() {
        Some(arg) if "tabs".starts_with(&arg.to_lowercase()) => Some(Tabs),
        Some("0") => Some(Tabs),
        Some(arg) => arg
            .parse::<u8>()
            .ok()
            .filter(|n| (1..=MAX_INDENT).contains(n))
            .map(Spaces),
        _ => None,
    };

    let style = style.context("invalid indent style")?;
    let doc = doc_mut!(cx.editor);
    doc.indent_style = style;

    Ok(())
}

/// Sets or reports the current document's line ending setting.
fn set_line_ending(
    cx: &mut compositor::Context,
    args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    use LineEnding::*;

    // If no argument, report current line ending setting.
    if args.is_empty() {
        let line_ending = doc!(cx.editor).line_ending;
        cx.editor.set_status(match line_ending {
            Crlf => "crlf",
            LF => "line feed",
            #[cfg(feature = "unicode-lines")]
            FF => "form feed",
            #[cfg(feature = "unicode-lines")]
            CR => "carriage return",
            #[cfg(feature = "unicode-lines")]
            Nel => "next line",

            // These should never be a document's default line ending.
            #[cfg(feature = "unicode-lines")]
            VT | LS | PS => "error",
        });

        return Ok(());
    }

    let arg = args
        .first()
        .context("argument missing")?
        .to_ascii_lowercase();

    // Attempt to parse argument as a line ending.
    let line_ending = match arg {
        arg if arg.starts_with("crlf") => Crlf,
        arg if arg.starts_with("lf") => LF,
        #[cfg(feature = "unicode-lines")]
        arg if arg.starts_with("cr") => CR,
        #[cfg(feature = "unicode-lines")]
        arg if arg.starts_with("ff") => FF,
        #[cfg(feature = "unicode-lines")]
        arg if arg.starts_with("nel") => Nel,
        _ => bail!("invalid line ending"),
    };
    let (view, doc) = current!(cx.editor);
    doc.line_ending = line_ending;

    let mut pos = 0;
    let transaction = Transaction::change(
        doc.text(),
        doc.text().lines().filter_map(|line| {
            pos += line.len_chars();
            match helix_core::line_ending::get_line_ending(&line) {
                Some(ending) if ending != line_ending => {
                    let start = pos - ending.len_chars();
                    let end = pos;
                    Some((start, end, Some(line_ending.as_str().into())))
                }
                _ => None,
            }
        }),
    );
    doc.apply(&transaction, view.id);
    doc.append_changes_to_history(view);

    Ok(())
}
fn earlier(cx: &mut compositor::Context, args: Args, event: PromptEvent) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    let uk = args.join(" ").parse::<UndoKind>().map_err(|s| anyhow!(s))?;

    let (view, doc) = current!(cx.editor);
    let success = doc.earlier(view, uk);
    if !success {
        cx.editor.set_status("Already at oldest change");
    }

    Ok(())
}

fn later(cx: &mut compositor::Context, args: Args, event: PromptEvent) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    let uk = args.join(" ").parse::<UndoKind>().map_err(|s| anyhow!(s))?;
    let (view, doc) = current!(cx.editor);
    let success = doc.later(view, uk);
    if !success {
        cx.editor.set_status("Already at newest change");
    }

    Ok(())
}

fn write_quit(cx: &mut compositor::Context, args: Args, event: PromptEvent) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    write_impl(
        cx,
        args.first(),
        WriteOptions {
            force: false,
            auto_format: !args.has_flag(WRITE_NO_FORMAT_FLAG.name),
        },
    )?;
    cx.block_try_flush_writes()?;
    quit(cx, Args::default(), event)
}

fn force_write_quit(
    cx: &mut compositor::Context,
    args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    write_impl(
        cx,
        args.first(),
        WriteOptions {
            force: true,
            auto_format: !args.has_flag(WRITE_NO_FORMAT_FLAG.name),
        },
    )?;
    cx.block_try_flush_writes()?;
    force_quit(cx, Args::default(), event)
}

/// Results in an error if there are modified buffers remaining and sets editor
/// error, otherwise returns `Ok(())`. If the current document is unmodified,
/// and there are modified documents, switches focus to one of them.
pub(super) fn buffers_remaining_impl(editor: &mut Editor) -> anyhow::Result<()> {
    let modified_ids: Vec<_> = editor
        .documents()
        .filter(|doc| doc.is_modified())
        .map(|doc| doc.id())
        .collect();

    if let Some(first) = modified_ids.first() {
        let current = doc!(editor);
        // If the current document is unmodified, and there are modified
        // documents, switch focus to the first modified doc.
        if !modified_ids.contains(&current.id()) {
            editor.switch(*first, Action::Replace);
        }

        let modified_names: Vec<_> = modified_ids
            .iter()
            .map(|doc_id| doc!(editor, doc_id).display_name())
            .collect();

        bail!(
            "{} unsaved buffer{} remaining: {:?}",
            modified_names.len(),
            if modified_names.len() == 1 { "" } else { "s" },
            modified_names,
        );
    }
    Ok(())
}

#[derive(Debug, Clone, Copy)]
pub struct WriteAllOptions {
    pub force: bool,
    pub write_scratch: bool,
    pub auto_format: bool,
}

pub fn write_all_impl(
    cx: &mut compositor::Context,
    options: WriteAllOptions,
) -> anyhow::Result<()> {
    let mut errors: Vec<&'static str> = Vec::new();
    let config = cx.editor.config();
    let jobs = &mut cx.jobs;
    let saves: Vec<_> = cx
        .editor
        .documents
        .keys()
        .cloned()
        .collect::<Vec<_>>()
        .into_iter()
        .filter_map(|id| {
            let doc = doc!(cx.editor, &id);
            if !doc.is_modified() {
                return None;
            }
            if doc.path().is_none() {
                if options.write_scratch {
                    errors.push("cannot write a buffer without a filename");
                }
                return None;
            }

            // Look for a view to apply the formatting change to.
            let target_view = cx.editor.get_synced_view_id(doc.id());
            Some((id, target_view))
        })
        .collect();

    for (doc_id, target_view) in saves {
        let doc = doc_mut!(cx.editor, &doc_id);
        let view = view_mut!(cx.editor, target_view);

        if doc.trim_trailing_whitespace() {
            trim_trailing_whitespace(doc, target_view);
        }
        if config.trim_final_newlines {
            trim_final_newlines(doc, target_view);
        }
        if doc.insert_final_newline() {
            insert_final_newline(doc, target_view);
        }

        // Save an undo checkpoint for any outstanding changes.
        doc.append_changes_to_history(view);

        let fmt = if options.auto_format && config.auto_format {
            let doc = doc!(cx.editor, &doc_id);
            doc.auto_format(cx.editor).map(|fmt| {
                let callback = make_format_callback(
                    doc_id,
                    doc.version(),
                    target_view,
                    fmt,
                    Some((None, options.force)),
                );
                jobs.add(Job::with_callback(callback).wait_before_exiting());
            })
        } else {
            None
        };

        if fmt.is_none() {
            cx.editor.save::<PathBuf>(doc_id, None, options.force)?;
        }
    }

    if !errors.is_empty() && !options.force {
        bail!("{:?}", errors);
    }

    Ok(())
}

fn write_all(cx: &mut compositor::Context, args: Args, event: PromptEvent) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    write_all_impl(
        cx,
        WriteAllOptions {
            force: false,
            write_scratch: true,
            auto_format: !args.has_flag(WRITE_NO_FORMAT_FLAG.name),
        },
    )
}

fn force_write_all(
    cx: &mut compositor::Context,
    args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    write_all_impl(
        cx,
        WriteAllOptions {
            force: true,
            write_scratch: true,
            auto_format: !args.has_flag(WRITE_NO_FORMAT_FLAG.name),
        },
    )
}

fn write_all_quit(
    cx: &mut compositor::Context,
    args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }
    write_all_impl(
        cx,
        WriteAllOptions {
            force: false,
            write_scratch: true,
            auto_format: !args.has_flag(WRITE_NO_FORMAT_FLAG.name),
        },
    )?;
    quit_all_impl(cx, false)
}

fn force_write_all_quit(
    cx: &mut compositor::Context,
    args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }
    let _ = write_all_impl(
        cx,
        WriteAllOptions {
            force: true,
            write_scratch: true,
            auto_format: !args.has_flag(WRITE_NO_FORMAT_FLAG.name),
        },
    );
    quit_all_impl(cx, true)
}

fn quit_all_impl(cx: &mut compositor::Context, force: bool) -> anyhow::Result<()> {
    cx.block_try_flush_writes()?;
    if !force {
        buffers_remaining_impl(cx.editor)?;
    }

    // close all views
    let views: Vec<_> = cx.editor.tree.views().map(|(view, _)| view.id).collect();
    for view_id in views {
        cx.editor.close(view_id);
    }

    Ok(())
}

fn quit_all(cx: &mut compositor::Context, _args: Args, event: PromptEvent) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    quit_all_impl(cx, false)
}

fn force_quit_all(
    cx: &mut compositor::Context,
    _args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    quit_all_impl(cx, true)
}

fn cquit(cx: &mut compositor::Context, args: Args, event: PromptEvent) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    let exit_code = args
        .first()
        .and_then(|code| code.parse::<i32>().ok())
        .unwrap_or(1);

    cx.editor.exit_code = exit_code;
    quit_all_impl(cx, false)
}

fn force_cquit(cx: &mut compositor::Context, args: Args, event: PromptEvent) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    let exit_code = args
        .first()
        .and_then(|code| code.parse::<i32>().ok())
        .unwrap_or(1);
    cx.editor.exit_code = exit_code;

    quit_all_impl(cx, true)
}

fn theme(cx: &mut compositor::Context, args: Args, event: PromptEvent) -> anyhow::Result<()> {
    let true_color = cx.editor.config.load().true_color || crate::true_color();
    match event {
        PromptEvent::Abort => {
            cx.editor.unset_theme_preview()?;
        }
        PromptEvent::Update => {
            if args.is_empty() {
                // Ensures that a preview theme gets cleaned up if the user backspaces until the prompt is empty.
                cx.editor.unset_theme_preview()?;
            } else if let Some(theme_name) = args.first() {
                if let Ok(theme) = cx.editor.theme_loader.load(theme_name) {
                    if !(true_color || theme.is_16_color()) {
                        bail!("Unsupported theme: theme requires true color support");
                    }
                    cx.editor.set_theme_preview(theme)?;
                };
            };
        }
        PromptEvent::Validate => {
            if let Some(theme_name) = args.first() {
                let theme = cx
                    .editor
                    .theme_loader
                    .load(theme_name)
                    .map_err(|err| anyhow::anyhow!("Could not load theme: {}", err))?;
                if !(true_color || theme.is_16_color()) {
                    bail!("Unsupported theme: theme requires true color support");
                }
                cx.editor.set_theme(theme)?;
            } else {
                let name = cx.editor.theme.name().to_string();

                cx.editor.set_status(name);
            }
        }
    };

    Ok(())
}

fn yank_main_selection_to_clipboard(
    cx: &mut compositor::Context,
    _args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    yank_main_selection_to_register(cx.editor, '+');
    Ok(())
}

fn yank_joined(cx: &mut compositor::Context, args: Args, event: PromptEvent) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    let doc = doc!(cx.editor);
    let default_sep = Cow::Borrowed(doc.line_ending.as_str());
    let separator = args.first().unwrap_or(&default_sep);
    let register = cx
        .editor
        .selected_register
        .unwrap_or(cx.editor.config().default_yank_register);
    yank_joined_impl(cx.editor, separator, register);
    Ok(())
}

fn yank_joined_to_clipboard(
    cx: &mut compositor::Context,
    args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    let doc = doc!(cx.editor);
    let default_sep = Cow::Borrowed(doc.line_ending.as_str());
    let separator = args.first().unwrap_or(&default_sep);
    yank_joined_impl(cx.editor, separator, '+');
    Ok(())
}

fn yank_main_selection_to_primary_clipboard(
    cx: &mut compositor::Context,
    _args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    yank_main_selection_to_register(cx.editor, '*');
    Ok(())
}

fn yank_joined_to_primary_clipboard(
    cx: &mut compositor::Context,
    args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    let doc = doc!(cx.editor);
    let default_sep = Cow::Borrowed(doc.line_ending.as_str());
    let separator = args.first().unwrap_or(&default_sep);
    yank_joined_impl(cx.editor, separator, '*');
    Ok(())
}

fn paste_clipboard_after(
    cx: &mut compositor::Context,
    _args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    paste(cx.editor, '+', Paste::After, 1);
    Ok(())
}

fn paste_clipboard_before(
    cx: &mut compositor::Context,
    _args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    paste(cx.editor, '+', Paste::Before, 1);
    Ok(())
}

fn paste_primary_clipboard_after(
    cx: &mut compositor::Context,
    _args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    paste(cx.editor, '*', Paste::After, 1);
    Ok(())
}

fn paste_primary_clipboard_before(
    cx: &mut compositor::Context,
    _args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    paste(cx.editor, '*', Paste::Before, 1);
    Ok(())
}

fn replace_selections_with_clipboard(
    cx: &mut compositor::Context,
    _args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    replace_selections_with_register(cx.editor, '+', 1);
    Ok(())
}

fn replace_selections_with_primary_clipboard(
    cx: &mut compositor::Context,
    _args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    replace_selections_with_register(cx.editor, '*', 1);
    Ok(())
}

fn show_clipboard_provider(
    cx: &mut compositor::Context,
    _args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    cx.editor
        .set_status(cx.editor.registers.clipboard_provider_name());
    Ok(())
}

/// Helper function to parse the first argument as a directory
#[inline]
fn parse_first_arg_as_dir(args: &Args, last_cwd: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    match args.first().map(AsRef::as_ref) {
        Some("-") => last_cwd.ok_or_else(|| anyhow!("No previous working directory")),
        Some(path) => Ok(helix_stdx::path::expand_tilde(Path::new(path)).into_owned()),
        None => Ok(home_dir()?),
    }
}

/// Helper function to apply a directory change for an already-parsed Path ref
#[inline]
fn apply_directory_change(cx: &mut compositor::Context, dir: &Path) -> anyhow::Result<()> {
    cx.editor.set_cwd(dir).map_err(|err| {
        anyhow!(
            "Could not change working directory to '{}': {err}",
            dir.display()
        )
    })?;

    cx.editor.set_status(format!(
        "Current working directory is now {}",
        helix_stdx::env::current_working_dir().display()
    ));

    Ok(())
}

fn change_current_directory(
    cx: &mut compositor::Context,
    args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    let dir = parse_first_arg_as_dir(&args, cx.editor.get_last_cwd().map(|p| p.to_path_buf()))?;

    apply_directory_change(cx, &dir)
}

fn show_directory_stack(
    cx: &mut compositor::Context,
    _args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    let serialized_stack = cx
        .editor
        .dir_stack
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join(" ");

    if !serialized_stack.is_empty() {
        cx.editor.set_status(serialized_stack);
    } else {
        cx.editor.set_error("Stack is empty");
    }

    Ok(())
}

fn push_directory(
    cx: &mut compositor::Context,
    args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    // avoid an unbounded directory stack and reallocs for perf
    if cx.editor.dir_stack.len() == cx.editor.dir_stack.capacity() {
        cx.editor.dir_stack.pop_back();
    }

    cx.editor
        .dir_stack
        .push_front(helix_stdx::env::current_working_dir());

    change_current_directory(cx, args, event)
}

fn pop_directory(
    cx: &mut compositor::Context,
    _args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    if let Some(dir) = cx.editor.dir_stack.pop_front() {
        apply_directory_change(cx, &dir)?;
    } else {
        cx.editor.set_error("Stack is empty");
    }

    Ok(())
}

fn show_current_directory(
    cx: &mut compositor::Context,
    _args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    let cwd = helix_stdx::env::current_working_dir();
    let message = format!("Current working directory is {}", cwd.display());

    if cwd.exists() {
        cx.editor.set_status(message);
    } else {
        cx.editor.set_error(format!("{} (deleted)", message));
    }
    Ok(())
}

/// Sets the [`Document`]'s encoding..
fn set_encoding(
    cx: &mut compositor::Context,
    args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    let doc = doc_mut!(cx.editor);
    if let Some(label) = args.first() {
        doc.set_encoding(label)
    } else {
        let encoding = doc.encoding().name().to_owned();
        cx.editor.set_status(encoding);
        Ok(())
    }
}

/// Shows info about the character under the primary cursor.
fn get_character_info(
    cx: &mut compositor::Context,
    _args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    let (view, doc) = current_ref!(cx.editor);
    let text = doc.text().slice(..);

    let grapheme_start = doc.selection(view.id).primary().cursor(text);
    let grapheme_end = graphemes::next_grapheme_boundary(text, grapheme_start);

    if grapheme_start == grapheme_end {
        return Ok(());
    }

    let grapheme = text.slice(grapheme_start..grapheme_end).to_string();
    let encoding = doc.encoding();

    let printable = grapheme.chars().fold(String::new(), |mut s, c| {
        match c {
            '\0' => s.push_str("\\0"),
            '\t' => s.push_str("\\t"),
            '\n' => s.push_str("\\n"),
            '\r' => s.push_str("\\r"),
            _ => s.push(c),
        }

        s
    });

    // Convert to Unicode codepoints if in UTF-8
    let unicode = if encoding == encoding::UTF_8 {
        let mut unicode = " (".to_owned();

        for (i, char) in grapheme.chars().enumerate() {
            if i != 0 {
                unicode.push(' ');
            }

            unicode.push_str("U+");

            let codepoint: u32 = if char.is_ascii() {
                char.into()
            } else {
                // Not ascii means it will be multi-byte, so strip out the extra
                // bits that encode the length & mark continuation bytes

                let s = String::from(char);
                let bytes = s.as_bytes();

                // First byte starts with 2-4 ones then a zero, so strip those off
                let first = bytes[0];
                let codepoint = first & (0xFF >> (first.leading_ones() + 1));
                let mut codepoint = u32::from(codepoint);

                // Following bytes start with 10
                for byte in bytes.iter().skip(1) {
                    codepoint <<= 6;
                    codepoint += u32::from(*byte) & 0x3F;
                }

                codepoint
            };

            write!(unicode, "{codepoint:0>4x}").unwrap();
        }

        unicode.push(')');
        unicode
    } else {
        String::new()
    };

    // Give the decimal value for ascii characters
    let dec = if encoding.is_ascii_compatible() && grapheme.len() == 1 {
        format!(" Dec {}", grapheme.as_bytes()[0])
    } else {
        String::new()
    };

    let hex = {
        let mut encoder = encoding.new_encoder();
        let max_encoded_len = encoder
            .max_buffer_length_from_utf8_without_replacement(grapheme.len())
            .unwrap();
        let mut bytes = Vec::with_capacity(max_encoded_len);
        let mut current_byte = 0;
        let mut hex = String::new();

        for (i, char) in grapheme.chars().enumerate() {
            if i != 0 {
                hex.push_str(" +");
            }

            let (result, _input_bytes_read) = encoder.encode_from_utf8_to_vec_without_replacement(
                &char.to_string(),
                &mut bytes,
                true,
            );

            if let encoding::EncoderResult::Unmappable(char) = result {
                bail!("{char:?} cannot be mapped to {}", encoding.name());
            }

            for byte in &bytes[current_byte..] {
                write!(hex, " {byte:0>2x}").unwrap();
            }

            current_byte = bytes.len();
        }

        hex
    };

    cx.editor
        .set_status(format!("\"{printable}\"{unicode}{dec} Hex{hex}"));

    Ok(())
}

/// Reload the [`Document`] from its source file.
fn reload(cx: &mut compositor::Context, _args: Args, event: PromptEvent) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    let scrolloff = cx.editor.config().scrolloff;
    let (view, doc) = current!(cx.editor);
    doc.reload(view, &cx.editor.diff_providers).map(|_| {
        view.ensure_cursor_in_view(doc, scrolloff);
    })?;
    if let Some(path) = doc.path() {
        cx.editor
            .language_servers
            .file_event_handler
            .file_changed(path.clone());
    }
    Ok(())
}

fn reload_all(cx: &mut compositor::Context, _args: Args, event: PromptEvent) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    let scrolloff = cx.editor.config().scrolloff;
    let view_id = view!(cx.editor).id;

    let docs_view_ids: Vec<(DocumentId, Vec<ViewId>)> = cx
        .editor
        .documents_mut()
        .map(|doc| {
            let mut view_ids: Vec<_> = doc.selections().keys().cloned().collect();

            if view_ids.is_empty() {
                doc.ensure_view_init(view_id);
                view_ids.push(view_id);
            };

            (doc.id(), view_ids)
        })
        .collect();

    for (doc_id, view_ids) in docs_view_ids {
        let doc = doc_mut!(cx.editor, &doc_id);

        // Every doc is guaranteed to have at least 1 view at this point.
        let view = view_mut!(cx.editor, view_ids[0]);

        // Ensure that the view is synced with the document's history.
        view.sync_changes(doc);

        if let Err(error) = doc.reload(view, &cx.editor.diff_providers) {
            cx.editor.set_error(format!("{}", error));
            continue;
        }

        if let Some(path) = doc.path() {
            cx.editor
                .language_servers
                .file_event_handler
                .file_changed(path.clone());
        }

        for view_id in view_ids {
            let view = view_mut!(cx.editor, view_id);
            if view.doc.eq(&doc_id) {
                view.ensure_cursor_in_view(doc, scrolloff);
            }
        }
    }

    Ok(())
}

/// Update the [`Document`] if it has been modified.
fn update(cx: &mut compositor::Context, args: Args, event: PromptEvent) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    let (_view, doc) = current!(cx.editor);
    if doc.is_modified() {
        write_impl(
            cx,
            None,
            WriteOptions {
                force: false,
                auto_format: !args.has_flag(WRITE_NO_FORMAT_FLAG.name),
            },
        )
    } else {
        Ok(())
    }
}

fn lsp_workspace_command(
    cx: &mut compositor::Context,
    args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    let doc = doc!(cx.editor);
    let ls_id_commands = doc
        .language_servers_with_feature(LanguageServerFeature::WorkspaceCommand)
        .flat_map(|ls| {
            ls.capabilities()
                .execute_command_provider
                .iter()
                .flat_map(|options| options.commands.iter())
                .map(|command| (ls.id(), command))
        });

    if args.is_empty() {
        let commands = ls_id_commands
            .map(|(ls_id, command)| {
                (
                    ls_id,
                    helix_lsp::lsp::Command {
                        title: command.clone(),
                        command: command.clone(),
                        arguments: None,
                    },
                )
            })
            .collect::<Vec<_>>();
        let callback = async move {
            let call: job::Callback = Callback::EditorCompositor(Box::new(
                move |_editor: &mut Editor, compositor: &mut Compositor| {
                    let columns = [ui::PickerColumn::new(
                        "title",
                        |(_ls_id, command): &(_, helix_lsp::lsp::Command), _| {
                            command.title.as_str().into()
                        },
                    )];
                    let picker = ui::Picker::new(
                        columns,
                        0,
                        commands,
                        (),
                        move |cx, (ls_id, command), _action| {
                            cx.editor.execute_lsp_command(command.clone(), *ls_id);
                        },
                    );
                    compositor.push(Box::new(overlaid(picker)))
                },
            ));
            Ok(call)
        };
        cx.jobs.callback(callback);
    } else {
        let command = args[0].to_string();
        let matches: Vec<_> = ls_id_commands
            .filter(|(_ls_id, c)| *c == &command)
            .collect();

        match matches.as_slice() {
            [(ls_id, _command)] => {
                let arguments = args
                    .get(1)
                    .map(|rest| {
                        serde_json::Deserializer::from_str(rest)
                            .into_iter()
                            .collect::<Result<Vec<Value>, _>>()
                            .map_err(|err| anyhow!("failed to parse arguments: {err}"))
                    })
                    .transpose()?
                    .filter(|args| !args.is_empty());

                cx.editor.execute_lsp_command(
                    helix_lsp::lsp::Command {
                        title: command.clone(),
                        arguments,
                        command,
                    },
                    *ls_id,
                );
            }
            [] => {
                cx.editor.set_status(format!(
                    "`{command}` is not supported for any language server"
                ));
            }
            _ => {
                cx.editor.set_status(format!(
                    "`{command}` supported by multiple language servers"
                ));
            }
        }
    }
    Ok(())
}

fn lsp_restart(cx: &mut compositor::Context, args: Args, event: PromptEvent) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    let editor_config = cx.editor.config.load();
    let doc = doc!(cx.editor);
    let config = doc
        .language_config()
        .context("LSP not defined for the current document")?;

    let language_servers: Vec<_> = config
        .language_servers
        .iter()
        .map(|ls| ls.name.as_str())
        .collect();
    let language_servers = if args.is_empty() {
        language_servers
    } else {
        let (valid, invalid): (Vec<_>, Vec<_>) = args
            .iter()
            .map(|arg| arg.as_ref())
            .partition(|name| language_servers.contains(name));
        if !invalid.is_empty() {
            let s = if invalid.len() == 1 { "" } else { "s" };
            bail!("Unknown language server{s}: {}", invalid.join(", "));
        }
        valid
    };

    let mut errors = Vec::new();
    for server in language_servers.iter() {
        match cx
            .editor
            .language_servers
            .restart_server(
                server,
                config,
                doc.path(),
                &editor_config.workspace_lsp_roots,
                editor_config.lsp.snippets,
            )
            .transpose()
        {
            // Ignore the executable-not-found error unless the server was explicitly requested
            // in the arguments.
            Err(helix_lsp::Error::ExecutableNotFound(_))
                if !args.iter().any(|arg| arg == server) => {}
            Err(err) => errors.push(err.to_string()),
            _ => (),
        }
    }

    // This collect is needed because refresh_language_server would need to re-borrow editor.
    let document_ids_to_refresh: Vec<DocumentId> = cx
        .editor
        .documents()
        .filter_map(|doc| match doc.language_config() {
            Some(config)
                if config.language_servers.iter().any(|ls| {
                    language_servers
                        .iter()
                        .any(|restarted_ls| restarted_ls == &ls.name)
                }) =>
            {
                Some(doc.id())
            }
            _ => None,
        })
        .collect();

    for document_id in document_ids_to_refresh {
        cx.editor.refresh_language_servers(document_id);
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(anyhow::anyhow!(
            "Error restarting language servers: {}",
            errors.join(", ")
        ))
    }
}

fn lsp_stop(cx: &mut compositor::Context, args: Args, event: PromptEvent) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }
    let doc = doc!(cx.editor);

    let language_servers: Vec<_> = doc
        .language_servers()
        .map(|ls| ls.name().to_string())
        .collect();
    let language_servers = if args.is_empty() {
        language_servers
    } else {
        let (valid, invalid): (Vec<_>, Vec<_>) = args
            .iter()
            .map(|arg| arg.to_string())
            .partition(|name| language_servers.contains(name));
        if !invalid.is_empty() {
            let s = if invalid.len() == 1 { "" } else { "s" };
            bail!("Unknown language server{s}: {}", invalid.join(", "));
        }
        valid
    };

    for ls_name in &language_servers {
        cx.editor.language_servers.stop(ls_name);

        for doc in cx.editor.documents_mut() {
            if let Some(client) = doc.remove_language_server_by_name(ls_name) {
                doc.clear_diagnostics_for_language_server(client.id());
                doc.reset_all_inlay_hints();
                doc.inlay_hints_oudated = true;
            }
        }
    }

    Ok(())
}

fn tree_sitter_scopes(
    cx: &mut compositor::Context,
    _args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    let (view, doc) = current!(cx.editor);
    let text = doc.text().slice(..);

    let pos = doc.selection(view.id).primary().cursor(text);
    let scopes = indent::get_scopes(doc.syntax(), text, pos);

    let contents = format!("```json\n{:?}\n````", scopes);

    let callback = async move {
        let call: job::Callback = Callback::EditorCompositor(Box::new(
            move |editor: &mut Editor, compositor: &mut Compositor| {
                let contents = ui::Markdown::new(contents, editor.syn_loader.clone());
                let popup = Popup::new("hover", contents).auto_close(true);
                compositor.replace_or_push("hover", popup);
            },
        ));
        Ok(call)
    };

    cx.jobs.callback(callback);

    Ok(())
}

fn tree_sitter_highlight_name(
    cx: &mut compositor::Context,
    _args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    let (view, doc) = current_ref!(cx.editor);
    let Some(syntax) = doc.syntax() else {
        return Ok(());
    };
    let text = doc.text().slice(..);
    let cursor = doc.selection(view.id).primary().cursor(text);
    let byte = text.char_to_byte(cursor) as u32;
    // Query the same range as the one used in syntax highlighting.
    let range = {
        // Calculate viewport byte ranges:
        let row = text.char_to_line(doc.view_offset(view.id).anchor.min(text.len_chars()));
        // Saturating subs to make it inclusive zero indexing.
        let last_line = text.len_lines().saturating_sub(1);
        let height = view.inner_area(doc).height;
        let last_visible_line = (row + height as usize).saturating_sub(1).min(last_line);
        let start = text.line_to_byte(row.min(last_line)) as u32;
        let end = text.line_to_byte(last_visible_line + 1) as u32;

        start..end
    };

    let loader = cx.editor.syn_loader.load();
    let mut highlighter = syntax.highlighter(text, &loader, range);
    let mut highlights = Vec::new();

    while highlighter.next_event_offset() <= byte {
        let (event, new_highlights) = highlighter.advance();
        if event == helix_core::syntax::HighlightEvent::Refresh {
            highlights.clear();
        }
        highlights.extend(new_highlights);
    }

    let content = highlights
        .into_iter()
        .fold(String::new(), |mut acc, highlight| {
            if !acc.is_empty() {
                acc.push_str(", ");
            }
            acc.push_str(cx.editor.theme.scope(highlight));
            acc
        });

    let callback = async move {
        let call: job::Callback = Callback::EditorCompositor(Box::new(
            move |editor: &mut Editor, compositor: &mut Compositor| {
                let content = ui::Markdown::new(content, editor.syn_loader.clone());
                let popup = Popup::new("hover", content).auto_close(true);
                compositor.replace_or_push("hover", popup);
            },
        ));
        Ok(call)
    };

    cx.jobs.callback(callback);

    Ok(())
}

fn tree_sitter_layers(
    cx: &mut compositor::Context,
    _args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    let (view, doc) = current_ref!(cx.editor);
    let Some(syntax) = doc.syntax() else {
        bail!("Syntax information is not available");
    };

    let loader: &helix_core::syntax::Loader = &cx.editor.syn_loader.load();
    let text = doc.text().slice(..);
    let cursor = doc.selection(view.id).primary().cursor(text);
    let byte = text.char_to_byte(cursor) as u32;
    let languages =
        syntax
            .layers_for_byte_range(byte, byte)
            .fold(String::new(), |mut acc, layer| {
                if !acc.is_empty() {
                    acc.push_str(", ");
                }
                acc.push_str(
                    &loader
                        .language(syntax.layer(layer).language)
                        .config()
                        .language_id,
                );
                acc
            });

    let callback = async move {
        let call: job::Callback = Callback::EditorCompositor(Box::new(
            move |editor: &mut Editor, compositor: &mut Compositor| {
                let content = ui::Markdown::new(languages, editor.syn_loader.clone());
                let popup = Popup::new("hover", content).auto_close(true);
                compositor.replace_or_push("hover", popup);
            },
        ));
        Ok(call)
    };

    cx.jobs.callback(callback);

    Ok(())
}

fn vsplit(cx: &mut compositor::Context, args: Args, event: PromptEvent) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    if args.is_empty() {
        split(cx.editor, Action::VerticalSplit);
    } else {
        open_impl(cx, args, Action::VerticalSplit)?;
    }

    Ok(())
}

fn hsplit(cx: &mut compositor::Context, args: Args, event: PromptEvent) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    if args.is_empty() {
        split(cx.editor, Action::HorizontalSplit);
    } else {
        open_impl(cx, args, Action::HorizontalSplit)?;
    }

    Ok(())
}

fn vsplit_new(cx: &mut compositor::Context, _args: Args, event: PromptEvent) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    cx.editor.new_file(Action::VerticalSplit);

    Ok(())
}

fn hsplit_new(cx: &mut compositor::Context, _args: Args, event: PromptEvent) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    cx.editor.new_file(Action::HorizontalSplit);

    Ok(())
}

fn debug_eval(cx: &mut compositor::Context, args: Args, event: PromptEvent) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    if let Some(debugger) = cx.editor.debug_adapters.get_active_client() {
        let (frame, thread_id) = match (debugger.active_frame, debugger.thread_id) {
            (Some(frame), Some(thread_id)) => (frame, thread_id),
            _ => {
                bail!("Cannot find current stack frame to access variables")
            }
        };

        // TODO: support no frame_id

        let frame_id = debugger.stack_frames[&thread_id][frame].id;
        let response = helix_lsp::block_on(debugger.eval(args.join(" "), Some(frame_id)))?;
        cx.editor.set_status(response.result);
    }
    Ok(())
}

fn debug_start(cx: &mut compositor::Context, args: Args, event: PromptEvent) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    let mut args: Vec<_> = args.into_iter().collect();
    let name = match args.len() {
        0 => None,
        _ => Some(args.remove(0)),
    };
    dap_start_impl(cx, name.as_deref(), None, Some(args))
}

fn debug_remote(
    cx: &mut compositor::Context,
    args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    let mut args: Vec<_> = args.into_iter().collect();
    let address = match args.len() {
        0 => None,
        _ => Some(args.remove(0).parse()?),
    };
    let name = match args.len() {
        0 => None,
        _ => Some(args.remove(0)),
    };
    dap_start_impl(cx, name.as_deref(), address, Some(args))
}

fn tutor(cx: &mut compositor::Context, _args: Args, event: PromptEvent) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    let path = helix_loader::runtime_file(Path::new("tutor"));
    cx.editor.open(&path, Action::Replace)?;
    // Unset path to prevent accidentally saving to the original tutor file.
    doc_mut!(cx.editor).set_path(None);
    Ok(())
}

fn abort_goto_line_number_preview(cx: &mut compositor::Context) {
    if let Some(last_selection) = cx.editor.last_selection.take() {
        let scrolloff = cx.editor.config().scrolloff;

        let (view, doc) = current!(cx.editor);
        doc.set_selection(view.id, last_selection);
        view.ensure_cursor_in_view(doc, scrolloff);
    }
}

fn update_goto_line_number_preview(cx: &mut compositor::Context, args: Args) -> anyhow::Result<()> {
    cx.editor.last_selection.get_or_insert_with(|| {
        let (view, doc) = current!(cx.editor);
        doc.selection(view.id).clone()
    });

    let scrolloff = cx.editor.config().scrolloff;
    let line = args[0].parse::<usize>()?;
    goto_line_without_jumplist(
        cx.editor,
        NonZeroUsize::new(line),
        if cx.editor.mode == Mode::Select {
            Movement::Extend
        } else {
            Movement::Move
        },
    );

    let (view, doc) = current!(cx.editor);
    view.ensure_cursor_in_view(doc, scrolloff);

    Ok(())
}

pub(super) fn goto_line_number(
    cx: &mut compositor::Context,
    args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    match event {
        PromptEvent::Abort => abort_goto_line_number_preview(cx),
        PromptEvent::Validate => {
            // If we are invoked directly via a keybinding, Validate is
            // sent without any prior Update events. Ensure the cursor
            // is moved to the appropriate location.
            update_goto_line_number_preview(cx, args)?;

            let last_selection = cx
                .editor
                .last_selection
                .take()
                .expect("update_goto_line_number_preview should always set last_selection");

            let (view, doc) = current!(cx.editor);
            view.jumps.push((doc.id(), last_selection));
        }

        // When a user hits backspace and there are no numbers left,
        // we can bring them back to their original selection. If they
        // begin typing numbers again, we'll start a new preview session.
        PromptEvent::Update if args.is_empty() => abort_goto_line_number_preview(cx),
        PromptEvent::Update => update_goto_line_number_preview(cx, args)?,
    }

    Ok(())
}

// Fetch the current value of a config option and output as status.
fn get_option(cx: &mut compositor::Context, args: Args, event: PromptEvent) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    let key = &args[0].to_lowercase();
    let key_error = || anyhow::anyhow!("Unknown key `{}`", key);

    let config = serde_json::json!(cx.editor.config().deref());
    let pointer = format!("/{}", key.replace('.', "/"));
    let value = config.pointer(&pointer).ok_or_else(key_error)?;

    cx.editor.set_status(value.to_string());
    Ok(())
}

/// Change config at runtime. Access nested values by dot syntax, for
/// example to disable smart case search, use `:set search.smart-case false`.
fn set_option(cx: &mut compositor::Context, args: Args, event: PromptEvent) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    let (key, arg) = (&args[0].to_lowercase(), args[1].trim());

    let key_error = || anyhow::anyhow!("Unknown key `{}`", key);
    let field_error = |_| anyhow::anyhow!("Could not parse field `{}`", arg);

    let mut config = serde_json::json!(&cx.editor.config().deref());
    let pointer = format!("/{}", key.replace('.', "/"));
    let value = config.pointer_mut(&pointer).ok_or_else(key_error)?;

    *value = if value.is_string() {
        // JSON strings require quotes, so we can't .parse() directly
        Value::String(arg.to_string())
    } else {
        arg.parse().map_err(field_error)?
    };
    let config = serde_json::from_value(config).map_err(field_error)?;

    cx.editor
        .config_events
        .0
        .send(ConfigEvent::Update(config))?;
    Ok(())
}

/// Toggle boolean config option at runtime. Access nested values by dot
/// syntax, for example to toggle smart case search, use `:toggle search.smart-
/// case`.
fn toggle_option(
    cx: &mut compositor::Context,
    args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    let key = &args[0].to_lowercase();

    let key_error = || anyhow::anyhow!("Unknown key `{}`", key);

    let mut config = serde_json::json!(&cx.editor.config().deref());
    let pointer = format!("/{}", key.replace('.', "/"));
    let value = config.pointer_mut(&pointer).ok_or_else(key_error)?;

    *value = match value {
        Value::Bool(ref value) => {
            ensure!(
                args.len() == 1,
                "Bad arguments. For boolean configurations use: `:toggle {key}`"
            );
            Value::Bool(!value)
        }
        Value::String(ref value) => {
            ensure!(
                args.len() == 2,
                "Bad arguments. For string configurations use: `:toggle {key} val1 val2 ...`",
            );
            // For string values, parse the input according to normal command line rules.
            let values: Vec<_> = command_line::Tokenizer::new(&args[1], true)
                .map(|res| res.map(|token| token.content))
                .collect::<Result<_, _>>()
                .map_err(|err| anyhow!("failed to parse values: {err}"))?;

            Value::String(
                values
                    .iter()
                    .skip_while(|e| *e != value)
                    .nth(1)
                    .map(AsRef::as_ref)
                    .unwrap_or_else(|| &values[0])
                    .to_string(),
            )
        }
        Value::Null => bail!("Configuration {key} cannot be toggled"),
        Value::Number(_) | Value::Array(_) | Value::Object(_) => {
            ensure!(
                args.len() == 2,
                "Bad arguments. For {kind} configurations use: `:toggle {key} val1 val2 ...`",
                kind = match value {
                    Value::Number(_) => "number",
                    Value::Array(_) => "array",
                    Value::Object(_) => "object",
                    _ => unreachable!(),
                }
            );
            // For numbers, arrays and objects, parse each argument with
            // `serde_json::StreamDeserializer`.
            let values: Vec<Value> = serde_json::Deserializer::from_str(&args[1])
                .into_iter()
                .collect::<Result<_, _>>()
                .map_err(|err| anyhow!("failed to parse value: {err}"))?;

            if let Some(wrongly_typed_value) = values
                .iter()
                .find(|v| std::mem::discriminant(*v) != std::mem::discriminant(&*value))
            {
                bail!("value '{wrongly_typed_value}' has a different type than '{value}'");
            }

            values
                .iter()
                .skip_while(|e| *e != value)
                .nth(1)
                .unwrap_or(&values[0])
                .clone()
        }
    };

    let status = format!("'{key}' is now set to {value}");
    let config = serde_json::from_value(config)
        .map_err(|err| anyhow::anyhow!("Failed to parse config: {err}"))?;

    cx.editor
        .config_events
        .0
        .send(ConfigEvent::Update(config))?;
    cx.editor.set_status(status);
    Ok(())
}

/// Change the language of the current buffer at runtime.
fn language(cx: &mut compositor::Context, args: Args, event: PromptEvent) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    if args.is_empty() {
        let doc = doc!(cx.editor);
        let language = &doc.language_name().unwrap_or(DEFAULT_LANGUAGE_NAME);
        cx.editor.set_status(language.to_string());
        return Ok(());
    }

    let doc = doc_mut!(cx.editor);

    let loader = cx.editor.syn_loader.load();
    if &args[0] == DEFAULT_LANGUAGE_NAME {
        doc.set_language(None, &loader)
    } else {
        doc.set_language_by_language_id(&args[0], &loader)?;
    }
    doc.detect_indent_and_line_ending();

    let id = doc.id();
    cx.editor.refresh_language_servers(id);
    let doc = doc_mut!(cx.editor);
    let diagnostics =
        Editor::doc_diagnostics(&cx.editor.language_servers, &cx.editor.diagnostics, doc);
    doc.replace_diagnostics(diagnostics, &[], None);
    Ok(())
}

fn sort(cx: &mut compositor::Context, args: Args, event: PromptEvent) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    let scrolloff = cx.editor.config().scrolloff;
    let (view, doc) = current!(cx.editor);
    let text = doc.text().slice(..);

    let selection = doc.selection(view.id);

    if selection.len() == 1 {
        bail!("Sorting requires multiple selections. Hint: split selection first");
    }

    let mut fragments: Vec<_> = selection
        .slices(text)
        .map(|fragment| fragment.chunks().collect())
        .collect();

    fragments.sort_by(
        match (args.has_flag("insensitive"), args.has_flag("reverse")) {
            (true, true) => |a: &Tendril, b: &Tendril| b.to_lowercase().cmp(&a.to_lowercase()),
            (true, false) => |a: &Tendril, b: &Tendril| a.to_lowercase().cmp(&b.to_lowercase()),
            (false, true) => |a: &Tendril, b: &Tendril| b.cmp(a),
            (false, false) => |a: &Tendril, b: &Tendril| a.cmp(b),
        },
    );

    let transaction = Transaction::change(
        doc.text(),
        selection
            .into_iter()
            .zip(fragments)
            .map(|(s, fragment)| (s.from(), s.to(), Some(fragment))),
    );

    doc.apply(&transaction, view.id);
    doc.append_changes_to_history(view);
    view.ensure_cursor_in_view(doc, scrolloff);

    Ok(())
}

fn reflow(cx: &mut compositor::Context, args: Args, event: PromptEvent) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    let scrolloff = cx.editor.config().scrolloff;
    let (view, doc) = current!(cx.editor);

    // Find the text_width by checking the following sources in order:
    //   - The passed argument in `args`
    //   - The configured text-width for this language in languages.toml
    //   - The configured text-width in the config.toml
    let text_width: usize = args
        .first()
        .map(|num| num.parse::<usize>())
        .transpose()?
        .unwrap_or_else(|| doc.text_width());

    let rope = doc.text();

    let selection = doc.selection(view.id);
    let transaction = Transaction::change_by_selection(rope, selection, |range| {
        let fragment = range.fragment(rope.slice(..));
        let reflowed_text = helix_core::wrap::reflow_hard_wrap(&fragment, text_width);

        (range.from(), range.to(), Some(reflowed_text))
    });

    doc.apply(&transaction, view.id);
    doc.append_changes_to_history(view);
    view.ensure_cursor_in_view(doc, scrolloff);

    Ok(())
}

fn tree_sitter_subtree(
    cx: &mut compositor::Context,
    _args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    let (view, doc) = current!(cx.editor);

    if let Some(syntax) = doc.syntax() {
        let primary_selection = doc.selection(view.id).primary();
        let text = doc.text();
        let from = text.char_to_byte(primary_selection.from()) as u32;
        let to = text.char_to_byte(primary_selection.to()) as u32;
        if let Some(selected_node) = syntax.descendant_for_byte_range(from, to) {
            let mut contents = String::from("```tsq\n");
            helix_core::syntax::pretty_print_tree(&mut contents, selected_node)?;
            contents.push_str("\n```");

            let callback = async move {
                let call: job::Callback = Callback::EditorCompositor(Box::new(
                    move |editor: &mut Editor, compositor: &mut Compositor| {
                        let contents = ui::Markdown::new(contents, editor.syn_loader.clone());
                        let popup = Popup::new("hover", contents).auto_close(true);
                        compositor.replace_or_push("hover", popup);
                    },
                ));
                Ok(call)
            };

            cx.jobs.callback(callback);
        }
    }

    Ok(())
}

fn open_config(
    cx: &mut compositor::Context,
    _args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    cx.editor
        .open(&helix_loader::config_file(), Action::Replace)?;
    Ok(())
}

fn open_workspace_config(
    cx: &mut compositor::Context,
    _args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    cx.editor
        .open(&helix_loader::workspace_config_file(), Action::Replace)?;
    Ok(())
}

fn open_log(cx: &mut compositor::Context, _args: Args, event: PromptEvent) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    cx.editor.open(&helix_loader::log_file(), Action::Replace)?;
    Ok(())
}

fn refresh_config(
    cx: &mut compositor::Context,
    _args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    cx.editor.config_events.0.send(ConfigEvent::Refresh)?;
    Ok(())
}

fn append_output(
    cx: &mut compositor::Context,
    args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    shell(cx, &args.join(" "), &ShellBehavior::Append);
    Ok(())
}

fn insert_output(
    cx: &mut compositor::Context,
    args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    shell(cx, &args.join(" "), &ShellBehavior::Insert);
    Ok(())
}

fn pipe_to(cx: &mut compositor::Context, args: Args, event: PromptEvent) -> anyhow::Result<()> {
    pipe_impl(cx, args, event, &ShellBehavior::Ignore)
}

fn pipe(cx: &mut compositor::Context, args: Args, event: PromptEvent) -> anyhow::Result<()> {
    pipe_impl(cx, args, event, &ShellBehavior::Replace)
}

fn pipe_impl(
    cx: &mut compositor::Context,
    args: Args,
    event: PromptEvent,
    behavior: &ShellBehavior,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    shell(cx, &args.join(" "), behavior);
    Ok(())
}

fn run_shell_command(
    cx: &mut compositor::Context,
    args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    let shell = cx.editor.config().shell.clone();
    let args = args.join(" ");

    let callback = async move {
        let output = shell_impl_async(&shell, &args, None).await?;
        let call: job::Callback = Callback::EditorCompositor(Box::new(
            move |editor: &mut Editor, compositor: &mut Compositor| {
                if !output.trim().is_empty() {
                    let contents = ui::Markdown::new(
                        format!("```sh\n{}\n```", output.trim_end()),
                        editor.syn_loader.clone(),
                    );
                    let popup = Popup::new("shell", contents).position(Some(
                        helix_core::Position::new(editor.cursor().0.unwrap_or_default().row, 2),
                    ));
                    compositor.replace_or_push("shell", popup);
                }
                editor.set_status("Command run");
            },
        ));
        Ok(call)
    };
    cx.jobs.callback(callback);

    Ok(())
}

fn reset_diff_change(
    cx: &mut compositor::Context,
    _args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    let editor = &mut cx.editor;
    let scrolloff = editor.config().scrolloff;

    let (view, doc) = current!(editor);
    let Some(handle) = doc.diff_handle() else {
        bail!("Diff is not available in the current buffer")
    };

    let diff = handle.load();
    let doc_text = doc.text().slice(..);
    let diff_base = diff.diff_base();
    let mut changes = 0;

    let transaction = Transaction::change(
        doc.text(),
        diff.hunks_intersecting_line_ranges(doc.selection(view.id).line_ranges(doc_text))
            .map(|hunk| {
                changes += 1;
                let start = diff_base.line_to_char(hunk.before.start as usize);
                let end = diff_base.line_to_char(hunk.before.end as usize);
                let text: Tendril = diff_base.slice(start..end).chunks().collect();
                (
                    doc_text.line_to_char(hunk.after.start as usize),
                    doc_text.line_to_char(hunk.after.end as usize),
                    (!text.is_empty()).then_some(text),
                )
            }),
    );
    if changes == 0 {
        bail!("There are no changes under any selection");
    }

    drop(diff); // make borrow check happy
    doc.apply(&transaction, view.id);
    doc.append_changes_to_history(view);
    view.ensure_cursor_in_view(doc, scrolloff);
    cx.editor.set_status(format!(
        "Reset {changes} change{}",
        if changes == 1 { "" } else { "s" }
    ));
    Ok(())
}

fn clear_register(
    cx: &mut compositor::Context,
    args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    if args.is_empty() {
        cx.editor.registers.clear();
        cx.editor.set_status("All registers cleared");
        return Ok(());
    }

    ensure!(
        args[0].chars().count() == 1,
        format!("Invalid register {}", &args[0])
    );
    let register = args[0].chars().next().unwrap_or_default();
    if cx.editor.registers.remove(register) {
        cx.editor
            .set_status(format!("Register {} cleared", register));
    } else {
        cx.editor
            .set_error(format!("Register {} not found", register));
    }
    Ok(())
}

fn set_register(
    cx: &mut compositor::Context,
    args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    ensure!(
        args[0].chars().count() == 1,
        format!("Invalid register {}", &args[0])
    );

    let register = args[0].chars().next().unwrap_or_default();
    cx.editor.registers.write(register, vec![args[1].into()])
}

fn redraw(cx: &mut compositor::Context, _args: Args, event: PromptEvent) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    let callback = Box::pin(async move {
        let call: job::Callback =
            job::Callback::EditorCompositor(Box::new(|_editor, compositor| {
                compositor.need_full_redraw();
            }));

        Ok(call)
    });

    cx.jobs.callback(callback);

    Ok(())
}

#[derive(Debug, Clone, Copy)]
pub struct MoveBufferOptions {
    pub force: bool,
}

fn move_buffer(cx: &mut compositor::Context, args: Args, event: PromptEvent) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    let new_path: PathBuf = args.first().unwrap().into();
    move_buffer_impl(cx, new_path, MoveBufferOptions { force: false })
}

fn force_move_buffer(
    cx: &mut compositor::Context,
    args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    let new_path: PathBuf = args.first().unwrap().into();
    move_buffer_impl(cx, new_path, MoveBufferOptions { force: true })
}

fn move_buffer_impl(
    cx: &mut compositor::Context,
    new_path: PathBuf,
    options: MoveBufferOptions,
) -> anyhow::Result<()> {
    let doc = doc!(cx.editor);
    let old_path = doc
        .path()
        .context("Scratch buffer cannot be moved. Use :write instead")?
        .clone();

    // if new_path is a directory, append the original file name
    // to move the file into that directory.
    let new_path = old_path
        .file_name()
        .filter(|_| new_path.is_dir())
        .map(|old_file_name| new_path.join(old_file_name))
        .unwrap_or(new_path);

    if old_path.exists() {
        if let Some(parent) = new_path.parent() {
            if !parent.exists() {
                if options.force {
                    std::fs::DirBuilder::new().recursive(true).create(parent)?;
                } else {
                    bail!(
                        "can't move file, parent directory does not exist (use :mv! to create it)"
                    )
                }
            }
        }
    }

    if let Err(err) = cx.editor.move_path(&old_path, new_path.as_ref()) {
        bail!("Could not move file: {err}");
    }
    Ok(())
}

fn yank_diagnostic(
    cx: &mut compositor::Context,
    args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    let reg = match args.first() {
        Some(s) => {
            ensure!(s.chars().count() == 1, format!("Invalid register {s}"));
            s.chars().next().unwrap()
        }
        None => '+',
    };

    let (view, doc) = current_ref!(cx.editor);
    let primary = doc.selection(view.id).primary();

    // Look only for diagnostics that intersect with the primary selection
    let diag: Vec<_> = doc
        .diagnostics()
        .iter()
        .filter(|d| primary.overlaps(&helix_core::Range::new(d.range.start, d.range.end)))
        .map(|d| d.message.clone())
        .collect();
    let n = diag.len();
    if n == 0 {
        bail!("No diagnostics under primary selection");
    }

    cx.editor.registers.write(reg, diag)?;
    cx.editor.set_status(format!(
        "Yanked {n} diagnostic{} to register {reg}",
        if n == 1 { "" } else { "s" }
    ));
    Ok(())
}

fn read(cx: &mut compositor::Context, args: Args, event: PromptEvent) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    let scrolloff = cx.editor.config().scrolloff;
    let (view, doc) = current!(cx.editor);

    let filename = args.first().unwrap();
    let path = helix_stdx::path::expand_tilde(PathBuf::from(filename.to_string()));

    ensure!(
        path.exists() && path.is_file(),
        "path is not a file: {:?}",
        path
    );

    let file = std::fs::File::open(path).map_err(|err| anyhow!("error opening file: {}", err))?;
    let mut reader = BufReader::new(file);
    let (contents, _, _) = read_to_string(&mut reader, Some(doc.encoding()))
        .map_err(|err| anyhow!("error reading file: {}", err))?;
    let contents = Tendril::from(contents);
    let selection = doc.selection(view.id);
    let transaction = Transaction::insert(doc.text(), selection, contents);
    doc.apply(&transaction, view.id);
    doc.append_changes_to_history(view);
    view.ensure_cursor_in_view(doc, scrolloff);

    Ok(())
}

fn echo(cx: &mut compositor::Context, args: Args, event: PromptEvent) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    let output = args.into_iter().fold(String::new(), |mut acc, arg| {
        if !acc.is_empty() {
            acc.push(' ');
        }
        acc.push_str(&arg);
        acc
    });
    cx.editor.set_status(output);

    Ok(())
}

fn noop(_cx: &mut compositor::Context, _args: Args, _event: PromptEvent) -> anyhow::Result<()> {
    Ok(())
}

/// This command accepts a single boolean --skip-visible flag and no positionals.
const BUFFER_CLOSE_OTHERS_SIGNATURE: Signature = Signature {
    positionals: (0, Some(0)),
    flags: &[Flag {
        name: "skip-visible",
        alias: Some('s'),
        doc: "don't close buffers that are visible",
        ..Flag::DEFAULT
    }],
    ..Signature::DEFAULT
};

// TODO: SHELL_SIGNATURE should specify var args for arguments, so that just completers::filename can be used,
// but Signature does not yet allow for var args.

/// This command handles all of its input as-is with no quoting or flags.
pub const SHELL_SIGNATURE: Signature = Signature {
    positionals: (1, Some(2)),
    raw_after: Some(1),
    ..Signature::DEFAULT
};

pub const SHELL_COMPLETER: CommandCompleter = CommandCompleter::positional(&[
    // Command name
    completers::program,
    // Shell argument(s)
    completers::repeating_filenames,
]);

const WRITE_NO_FORMAT_FLAG: Flag = Flag {
    name: "no-format",
    doc: "skip auto-formatting",
    ..Flag::DEFAULT
};

pub const TYPABLE_COMMAND_LIST: &[TypableCommand] = &[
    TypableCommand {
        name: "exit",
        aliases: &["x", "xit"],
        doc: "Write changes to disk if the buffer is modified and then quit. Accepts an optional path (:exit some/path.txt).",
        fun: exit,
        completer: CommandCompleter::positional(&[completers::filename]),
        signature: Signature {
            positionals: (0, Some(1)),
            flags: &[WRITE_NO_FORMAT_FLAG],
            ..Signature::DEFAULT
        },
    },
    TypableCommand {
        name: "exit!",
        aliases: &["x!", "xit!"],
        doc: "Force write changes to disk, creating necessary subdirectories, if the buffer is modified and then quit. Accepts an optional path (:exit! some/path.txt).",
        fun: force_exit,
        completer: CommandCompleter::positional(&[completers::filename]),
        signature: Signature {
            positionals: (0, Some(1)),
            flags: &[WRITE_NO_FORMAT_FLAG],
            ..Signature::DEFAULT
        },
    },
    TypableCommand {
        name: "quit",
        aliases: &["q"],
        doc: "Close the current view.",
        fun: quit,
        completer: CommandCompleter::none(),
        signature: Signature {
            positionals: (0, Some(0)),
            ..Signature::DEFAULT
        },
    },
    TypableCommand {
        name: "quit!",
        aliases: &["q!"],
        doc: "Force close the current view, ignoring unsaved changes.",
        fun: force_quit,
        completer: CommandCompleter::none(),
        signature: Signature {
            positionals: (0, Some(0)),
            ..Signature::DEFAULT
        },
    },
    TypableCommand {
        name: "open",
        aliases: &["o", "edit", "e"],
        doc: "Open a file from disk into the current view.",
        fun: open,
        completer: CommandCompleter::all(completers::filename),
        signature: Signature {
            positionals: (1, None),
            ..Signature::DEFAULT
        },
    },
    TypableCommand {
        name: "buffer-close",
        aliases: &["bc", "bclose"],
        doc: "Close the current buffer.",
        fun: buffer_close,
        completer: CommandCompleter::all(completers::buffer),
        signature: Signature {
            positionals: (0, None),
            ..Signature::DEFAULT
        },
    },
    TypableCommand {
        name: "buffer-close!",
        aliases: &["bc!", "bclose!"],
        doc: "Close the current buffer forcefully, ignoring unsaved changes.",
        fun: force_buffer_close,
        completer: CommandCompleter::all(completers::buffer),
        signature: Signature {
            positionals: (0, None),
            ..Signature::DEFAULT
        },
    },
    TypableCommand {
        name: "buffer-close-others",
        aliases: &["bco", "bcloseother"],
        doc: "Close all buffers but the currently focused one.",
        fun: buffer_close_others,
        completer: CommandCompleter::none(),
        signature: BUFFER_CLOSE_OTHERS_SIGNATURE,
    },
    TypableCommand {
        name: "buffer-close-others!",
        aliases: &["bco!", "bcloseother!"],
        doc: "Force close all buffers but the currently focused one.",
        fun: force_buffer_close_others,
        completer: CommandCompleter::none(),
        signature: BUFFER_CLOSE_OTHERS_SIGNATURE,
    },
    TypableCommand {
        name: "buffer-close-all",
        aliases: &["bca", "bcloseall"],
        doc: "Close all buffers without quitting.",
        fun: buffer_close_all,
        completer: CommandCompleter::none(),
        signature: Signature {
            positionals: (0, Some(0)),
            ..Signature::DEFAULT
        },
    },
    TypableCommand {
        name: "buffer-close-all!",
        aliases: &["bca!", "bcloseall!"],
        doc: "Force close all buffers ignoring unsaved changes without quitting.",
        fun: force_buffer_close_all,
        completer: CommandCompleter::none(),
        signature: Signature {
            positionals: (0, Some(0)),
            ..Signature::DEFAULT
        },
    },
    TypableCommand {
        name: "buffer-next",
        aliases: &["bn", "bnext"],
        doc: "Goto next buffer.",
        fun: buffer_next,
        completer: CommandCompleter::none(),
        signature: Signature {
            positionals: (0, Some(0)),
            ..Signature::DEFAULT
        },
    },
    TypableCommand {
        name: "buffer-previous",
        aliases: &["bp", "bprev"],
        doc: "Goto previous buffer.",
        fun: buffer_previous,
        completer: CommandCompleter::none(),
        signature: Signature {
            positionals: (0, Some(0)),
            ..Signature::DEFAULT
        },
    },
    TypableCommand {
        name: "write",
        aliases: &["w"],
        doc: "Write changes to disk. Accepts an optional path (:write some/path.txt)",
        fun: write,
        completer: CommandCompleter::positional(&[completers::filename]),
        signature: Signature {
            positionals: (0, Some(1)),
            flags: &[WRITE_NO_FORMAT_FLAG],
            ..Signature::DEFAULT
        },
    },
    TypableCommand {
        name: "write!",
        aliases: &["w!"],
        doc: "Force write changes to disk creating necessary subdirectories. Accepts an optional path (:write! some/path.txt)",
        fun: force_write,
        completer: CommandCompleter::positional(&[completers::filename]),
        signature: Signature {
            positionals: (0, Some(1)),
            flags: &[WRITE_NO_FORMAT_FLAG],
            ..Signature::DEFAULT
        },
    },
    TypableCommand {
        name: "write-buffer-close",
        aliases: &["wbc"],
        doc: "Write changes to disk and closes the buffer. Accepts an optional path (:write-buffer-close some/path.txt)",
        fun: write_buffer_close,
        completer: CommandCompleter::positional(&[completers::filename]),
        signature: Signature {
            positionals: (0, Some(1)),
            flags: &[WRITE_NO_FORMAT_FLAG],
            ..Signature::DEFAULT
        },
    },
    TypableCommand {
        name: "write-buffer-close!",
        aliases: &["wbc!"],
        doc: "Force write changes to disk creating necessary subdirectories and closes the buffer. Accepts an optional path (:write-buffer-close! some/path.txt)",
        fun: force_write_buffer_close,
        completer: CommandCompleter::positional(&[completers::filename]),
        signature: Signature {
            positionals: (0, Some(1)),
            flags: &[WRITE_NO_FORMAT_FLAG],
            ..Signature::DEFAULT
        },
    },
    TypableCommand {
        name: "new",
        aliases: &["n"],
        doc: "Create a new scratch buffer.",
        fun: new_file,
        completer: CommandCompleter::none(),
        signature: Signature {
            positionals: (0, Some(0)),
            ..Signature::DEFAULT
        },
    },
    TypableCommand {
        name: "agent-context",
        aliases: &[],
        doc: "Open a scratch buffer with the current agent context snapshot.",
        fun: agent_context,
        completer: CommandCompleter::none(),
        signature: Signature {
            positionals: (0, Some(0)),
            ..Signature::DEFAULT
        },
    },
    TypableCommand {
        name: "agent",
        aliases: &[],
        doc: "Run an agent command. With no subcommand, opens the current context snapshot.",
        fun: agent,
        completer: CommandCompleter::positional(&[complete_agent_subcommand]),
        signature: Signature {
            positionals: (0, None),
            raw_after: Some(1),
            ..Signature::DEFAULT
        },
    },
    TypableCommand {
        name: "agent-new",
        aliases: &[],
        doc: "Open a new agent session scratch buffer.",
        fun: agent_new,
        completer: CommandCompleter::none(),
        signature: Signature {
            positionals: (0, Some(0)),
            ..Signature::DEFAULT
        },
    },
    TypableCommand {
        name: "format",
        aliases: &["fmt"],
        doc: "Format the file using an external formatter or language server.",
        fun: format,
        completer: CommandCompleter::none(),
        signature: Signature {
            positionals: (0, Some(0)),
            ..Signature::DEFAULT
        },
    },
    TypableCommand {
        name: "indent-style",
        aliases: &[],
        doc: "Set the indentation style for editing. ('t' for tabs or 1-16 for number of spaces.)",
        fun: set_indent_style,
        completer: CommandCompleter::none(),
        signature: Signature {
            positionals: (0, Some(1)),
            ..Signature::DEFAULT
        },
    },
    TypableCommand {
        name: "line-ending",
        aliases: &[],
        #[cfg(not(feature = "unicode-lines"))]
        doc: "Set the document's default line ending. Options: crlf, lf.",
        #[cfg(feature = "unicode-lines")]
        doc: "Set the document's default line ending. Options: crlf, lf, cr, ff, nel.",
        fun: set_line_ending,
        completer: CommandCompleter::none(),
        signature: Signature {
            positionals: (0, Some(1)),
            ..Signature::DEFAULT
        },
    },
    TypableCommand {
        name: "earlier",
        aliases: &["ear"],
        doc: "Jump back to an earlier point in edit history. Accepts a number of steps or a time span.",
        fun: earlier,
        completer: CommandCompleter::none(),
        signature: Signature {
            positionals: (0, Some(1)),
            ..Signature::DEFAULT
        },
    },
    TypableCommand {
        name: "later",
        aliases: &["lat"],
        doc: "Jump to a later point in edit history. Accepts a number of steps or a time span.",
        fun: later,
        completer: CommandCompleter::none(),
        signature: Signature {
            positionals: (0, Some(1)),
            ..Signature::DEFAULT
        },
    },
    TypableCommand {
        name: "write-quit",
        aliases: &["wq"],
        doc: "Write changes to disk and close the current view. Accepts an optional path (:wq some/path.txt)",
        fun: write_quit,
        completer: CommandCompleter::positional(&[completers::filename]),
        signature: Signature {
            positionals: (0, Some(1)),
            flags: &[WRITE_NO_FORMAT_FLAG],
            ..Signature::DEFAULT
        },
    },
    TypableCommand {
        name: "write-quit!",
        aliases: &["wq!"],
        doc: "Write changes to disk and close the current view forcefully. Accepts an optional path (:wq! some/path.txt)",
        fun: force_write_quit,
        completer: CommandCompleter::positional(&[completers::filename]),
        signature: Signature {
            positionals: (0, Some(1)),
            flags: &[WRITE_NO_FORMAT_FLAG],
            ..Signature::DEFAULT
        },
    },
    TypableCommand {
        name: "write-all",
        aliases: &["wa"],
        doc: "Write changes from all buffers to disk.",
        fun: write_all,
        completer: CommandCompleter::none(),
        signature: Signature {
            positionals: (0, Some(0)),
            flags: &[WRITE_NO_FORMAT_FLAG],
            ..Signature::DEFAULT
        },
    },
    TypableCommand {
        name: "write-all!",
        aliases: &["wa!"],
        doc: "Forcefully write changes from all buffers to disk creating necessary subdirectories.",
        fun: force_write_all,
        completer: CommandCompleter::none(),
        signature: Signature {
            positionals: (0, Some(0)),
            flags: &[WRITE_NO_FORMAT_FLAG],
            ..Signature::DEFAULT
        },
    },
    TypableCommand {
        name: "write-quit-all",
        aliases: &["wqa", "xa"],
        doc: "Write changes from all buffers to disk and close all views.",
        fun: write_all_quit,
        completer: CommandCompleter::none(),
        signature: Signature {
            positionals: (0, Some(0)),
            flags: &[WRITE_NO_FORMAT_FLAG],
            ..Signature::DEFAULT
        },
    },
    TypableCommand {
        name: "write-quit-all!",
        aliases: &["wqa!", "xa!"],
        doc: "Forcefully write changes from all buffers to disk, creating necessary subdirectories, and close all views (ignoring unsaved changes).",
        fun: force_write_all_quit,
        completer: CommandCompleter::none(),
        signature: Signature {
            positionals: (0, Some(0)),
            flags: &[WRITE_NO_FORMAT_FLAG],
            ..Signature::DEFAULT
        },
    },
    TypableCommand {
        name: "quit-all",
        aliases: &["qa"],
        doc: "Close all views.",
        fun: quit_all,
        completer: CommandCompleter::none(),
        signature: Signature {
            positionals: (0, Some(0)),
            ..Signature::DEFAULT
        },
    },
    TypableCommand {
        name: "quit-all!",
        aliases: &["qa!"],
        doc: "Force close all views ignoring unsaved changes.",
        fun: force_quit_all,
        completer: CommandCompleter::none(),
        signature: Signature {
            positionals: (0, Some(0)),
            ..Signature::DEFAULT
        },
    },
    TypableCommand {
        name: "cquit",
        aliases: &["cq"],
        doc: "Quit with exit code (default 1). Accepts an optional integer exit code (:cq 2).",
        fun: cquit,
        completer: CommandCompleter::none(),
        signature: Signature {
            positionals: (0, Some(1)),
            ..Signature::DEFAULT
        },
    },
    TypableCommand {
        name: "cquit!",
        aliases: &["cq!"],
        doc: "Force quit with exit code (default 1) ignoring unsaved changes. Accepts an optional integer exit code (:cq! 2).",
        fun: force_cquit,
        completer: CommandCompleter::none(),
        signature: Signature {
            positionals: (0, Some(1)),
            ..Signature::DEFAULT
        },
    },
    TypableCommand {
        name: "theme",
        aliases: &[],
        doc: "Change the editor theme (show current theme if no name specified).",
        fun: theme,
        completer: CommandCompleter::positional(&[completers::theme]),
        signature: Signature {
            positionals: (0, Some(1)),
            ..Signature::DEFAULT
        },
    },
    TypableCommand {
        name: "yank-join",
        aliases: &[],
        doc: "Yank joined selections. A separator can be provided as first argument. Default value is newline.",
        fun: yank_joined,
        completer: CommandCompleter::none(),
        signature: Signature {
            positionals: (0, Some(1)),
            ..Signature::DEFAULT
        },
    },
    TypableCommand {
        name: "clipboard-yank",
        aliases: &[],
        doc: "Yank main selection into system clipboard.",
        fun: yank_main_selection_to_clipboard,
        completer: CommandCompleter::none(),
        signature: Signature {
            positionals: (0, Some(0)),
            ..Signature::DEFAULT
        },
    },
    TypableCommand {
        name: "clipboard-yank-join",
        aliases: &[],
        doc: "Yank joined selections into system clipboard. A separator can be provided as first argument. Default value is newline.", // FIXME: current UI can't display long doc.
        fun: yank_joined_to_clipboard,
        completer: CommandCompleter::none(),
        signature: Signature {
            positionals: (0, Some(1)),
            ..Signature::DEFAULT
        },
    },
    TypableCommand {
        name: "primary-clipboard-yank",
        aliases: &[],
        doc: "Yank main selection into system primary clipboard.",
        fun: yank_main_selection_to_primary_clipboard,
        completer: CommandCompleter::none(),
        signature: Signature {
            positionals: (0, Some(0)),
            ..Signature::DEFAULT
        },
    },
    TypableCommand {
        name: "primary-clipboard-yank-join",
        aliases: &[],
        doc: "Yank joined selections into system primary clipboard. A separator can be provided as first argument. Default value is newline.", // FIXME: current UI can't display long doc.
        fun: yank_joined_to_primary_clipboard,
        completer: CommandCompleter::none(),
        signature: Signature {
            positionals: (0, Some(1)),
            ..Signature::DEFAULT
        },
    },
    TypableCommand {
        name: "clipboard-paste-after",
        aliases: &[],
        doc: "Paste system clipboard after selections.",
        fun: paste_clipboard_after,
        completer: CommandCompleter::none(),
        signature: Signature {
            positionals: (0, Some(0)),
            ..Signature::DEFAULT
        },
    },
    TypableCommand {
        name: "clipboard-paste-before",
        aliases: &[],
        doc: "Paste system clipboard before selections.",
        fun: paste_clipboard_before,
        completer: CommandCompleter::none(),
        signature: Signature {
            positionals: (0, Some(0)),
            ..Signature::DEFAULT
        },
    },
    TypableCommand {
        name: "clipboard-paste-replace",
        aliases: &[],
        doc: "Replace selections with content of system clipboard.",
        fun: replace_selections_with_clipboard,
        completer: CommandCompleter::none(),
        signature: Signature {
            positionals: (0, Some(0)),
            ..Signature::DEFAULT
        },
    },
    TypableCommand {
        name: "primary-clipboard-paste-after",
        aliases: &[],
        doc: "Paste primary clipboard after selections.",
        fun: paste_primary_clipboard_after,
        completer: CommandCompleter::none(),
        signature: Signature {
            positionals: (0, Some(0)),
            ..Signature::DEFAULT
        },
    },
    TypableCommand {
        name: "primary-clipboard-paste-before",
        aliases: &[],
        doc: "Paste primary clipboard before selections.",
        fun: paste_primary_clipboard_before,
        completer: CommandCompleter::none(),
        signature: Signature {
            positionals: (0, Some(0)),
            ..Signature::DEFAULT
        },
    },
    TypableCommand {
        name: "primary-clipboard-paste-replace",
        aliases: &[],
        doc: "Replace selections with content of system primary clipboard.",
        fun: replace_selections_with_primary_clipboard,
        completer: CommandCompleter::none(),
        signature: Signature {
            positionals: (0, Some(0)),
            ..Signature::DEFAULT
        },
    },
    TypableCommand {
        name: "show-clipboard-provider",
        aliases: &[],
        doc: "Show clipboard provider name in status bar.",
        fun: show_clipboard_provider,
        completer: CommandCompleter::none(),
        signature: Signature {
            positionals: (0, Some(0)),
            ..Signature::DEFAULT
        },
    },
    TypableCommand {
        name: "change-current-directory",
        aliases: &["cd"],
        doc: "Change the current working directory.",
        fun: change_current_directory,
        completer: CommandCompleter::positional(&[completers::directory]),
        signature: Signature {
            positionals: (1, Some(1)),
            ..Signature::DEFAULT
        },
    },
    TypableCommand {
        name: "show-directory-stack",
        aliases: &[],
        doc: "Show the directory stack as a <space> delimited string.",
        fun: show_directory_stack,
        completer: CommandCompleter::none(),
        signature: Signature {
            positionals: (0, Some(0)),
            ..Signature::DEFAULT
        },
    },
    TypableCommand {
        name: "push-directory",
        aliases: &["pushd"],
        doc: "Save and then change the current directory.",
        fun: push_directory,
        completer: CommandCompleter::positional(&[completers::directory]),
        signature: Signature {
            positionals: (1, Some(1)),
            ..Signature::DEFAULT
        },
    },
    TypableCommand {
        name: "pop-directory",
        aliases: &["popd"],
        doc: "Remove the top entry from the directory stack, and cd to the new top directory..",
        fun: pop_directory,
        completer: CommandCompleter::none(),
        signature: Signature {
            positionals: (0, Some(0)),
            ..Signature::DEFAULT
        },
    },
    TypableCommand {
        name: "show-directory",
        aliases: &["pwd"],
        doc: "Show the current working directory.",
        fun: show_current_directory,
        completer: CommandCompleter::none(),
        signature: Signature {
            positionals: (0, Some(0)),
            ..Signature::DEFAULT
        },
    },
    TypableCommand {
        name: "encoding",
        aliases: &[],
        doc: "Set encoding. Based on `https://encoding.spec.whatwg.org`.",
        fun: set_encoding,
        completer: CommandCompleter::none(),
        signature: Signature {
            positionals: (0, Some(1)),
            ..Signature::DEFAULT
        },
    },
    TypableCommand {
        name: "character-info",
        aliases: &["char"],
        doc: "Get info about the character under the primary cursor.",
        fun: get_character_info,
        completer: CommandCompleter::none(),
        signature: Signature {
            positionals: (0, Some(0)),
            ..Signature::DEFAULT
        },
    },
    TypableCommand {
        name: "reload",
        aliases: &["rl"],
        doc: "Discard changes and reload from the source file.",
        fun: reload,
        completer: CommandCompleter::none(),
        signature: Signature {
            positionals: (0, Some(0)),
            ..Signature::DEFAULT
        },
    },
    TypableCommand {
        name: "reload-all",
        aliases: &["rla"],
        doc: "Discard changes and reload all documents from the source files.",
        fun: reload_all,
        completer: CommandCompleter::none(),
        signature: Signature {
            positionals: (0, Some(0)),
            ..Signature::DEFAULT
        },
    },
    TypableCommand {
        name: "update",
        aliases: &["u"],
        doc: "Write changes only if the file has been modified.",
        fun: update,
        completer: CommandCompleter::none(),
        signature: Signature {
            positionals: (0, Some(0)),
            flags: &[WRITE_NO_FORMAT_FLAG],
            ..Signature::DEFAULT
        },
    },
    TypableCommand {
        name: "lsp-workspace-command",
        aliases: &[],
        doc: "Open workspace command picker",
        fun: lsp_workspace_command,
        completer: CommandCompleter::positional(&[completers::lsp_workspace_command]),
        signature: Signature {
            positionals: (0, None),
            raw_after: Some(1),
            ..Signature::DEFAULT
        },
    },
    TypableCommand {
        name: "lsp-restart",
        aliases: &[],
        doc: "Restarts the given language servers, or all language servers that are used by the current file if no arguments are supplied",
        fun: lsp_restart,
        completer: CommandCompleter::all(completers::configured_language_servers),
        signature: Signature {
            positionals: (0, None),
            ..Signature::DEFAULT
        },
    },
    TypableCommand {
        name: "lsp-stop",
        aliases: &[],
        doc: "Stops the given language servers, or all language servers that are used by the current file if no arguments are supplied",
        fun: lsp_stop,
        completer: CommandCompleter::all(completers::active_language_servers),
        signature: Signature {
            positionals: (0, None),
            ..Signature::DEFAULT
        },
    },
    TypableCommand {
        name: "tree-sitter-scopes",
        aliases: &[],
        doc: "Display tree sitter scopes, primarily for theming and development.",
        fun: tree_sitter_scopes,
        completer: CommandCompleter::none(),
        signature: Signature {
            positionals: (0, Some(0)),
            ..Signature::DEFAULT
        },
    },
    TypableCommand {
        name: "tree-sitter-highlight-name",
        aliases: &[],
        doc: "Display name of tree-sitter highlight scope under the cursor.",
        fun: tree_sitter_highlight_name,
        completer: CommandCompleter::none(),
        signature: Signature {
            positionals: (0, Some(0)),
            ..Signature::DEFAULT
        },
    },
    TypableCommand {
        name: "tree-sitter-layers",
        aliases: &[],
        doc: "Display language names of tree-sitter injection layers under the cursor.",
        fun: tree_sitter_layers,
        completer: CommandCompleter::none(),
        signature: Signature {
            positionals: (0, Some(0)),
            ..Signature::DEFAULT
        },
    },
    TypableCommand {
        name: "debug-start",
        aliases: &["dbg"],
        doc: "Start a debug session from a given template with given parameters.",
        fun: debug_start,
        completer: CommandCompleter::none(),
        signature: Signature {
            positionals: (0, None),
            ..Signature::DEFAULT
        },
    },
    TypableCommand {
        name: "debug-remote",
        aliases: &["dbg-tcp"],
        doc: "Connect to a debug adapter by TCP address and start a debugging session from a given template with given parameters.",
        fun: debug_remote,
        completer: CommandCompleter::none(),
        signature: Signature {
            positionals: (0, None),
            ..Signature::DEFAULT
        },
    },
    TypableCommand {
        name: "debug-eval",
        aliases: &[],
        doc: "Evaluate expression in current debug context.",
        fun: debug_eval,
        completer: CommandCompleter::none(),
        signature: Signature {
            positionals: (1, Some(1)),
            ..Signature::DEFAULT
        },
    },
    TypableCommand {
        name: "vsplit",
        aliases: &["vs"],
        doc: "Open the file in a vertical split.",
        fun: vsplit,
        completer: CommandCompleter::all(completers::filename),
        signature: Signature {
            positionals: (0, None),
            ..Signature::DEFAULT
        },
    },
    TypableCommand {
        name: "vsplit-new",
        aliases: &["vnew"],
        doc: "Open a scratch buffer in a vertical split.",
        fun: vsplit_new,
        completer: CommandCompleter::none(),
        signature: Signature {
            positionals: (0, Some(0)),
            ..Signature::DEFAULT
        },
    },
    TypableCommand {
        name: "hsplit",
        aliases: &["hs", "sp"],
        doc: "Open the file in a horizontal split.",
        fun: hsplit,
        completer: CommandCompleter::all(completers::filename),
        signature: Signature {
            positionals: (0, None),
            ..Signature::DEFAULT
        },
    },
    TypableCommand {
        name: "hsplit-new",
        aliases: &["hnew"],
        doc: "Open a scratch buffer in a horizontal split.",
        fun: hsplit_new,
        completer: CommandCompleter::none(),
        signature: Signature {
            positionals: (0, Some(0)),
            ..Signature::DEFAULT
        },
    },
    TypableCommand {
        name: "tutor",
        aliases: &[],
        doc: "Open the tutorial.",
        fun: tutor,
        completer: CommandCompleter::none(),
        signature: Signature {
            positionals: (0, Some(0)),
            ..Signature::DEFAULT
        },
    },
    TypableCommand {
        name: "goto",
        aliases: &["g"],
        doc: "Goto line number.",
        fun: goto_line_number,
        completer: CommandCompleter::none(),
        signature: Signature {
            positionals: (1, Some(1)),
            ..Signature::DEFAULT
        },
    },
    TypableCommand {
        name: "set-language",
        aliases: &["lang"],
        doc: "Set the language of current buffer (show current language if no value specified).",
        fun: language,
        completer: CommandCompleter::positional(&[completers::language]),
        signature: Signature {
            positionals: (0, Some(1)),
            ..Signature::DEFAULT
        },
    },
    TypableCommand {
        name: "set-option",
        aliases: &["set"],
        doc: "Set a config option at runtime.\nFor example to disable smart case search, use `:set search.smart-case false`.",
        fun: set_option,
        // TODO: Add support for completion of the options value(s), when appropriate.
        completer: CommandCompleter::positional(&[completers::setting]),
        signature: Signature {
            positionals: (2, Some(2)),
            raw_after: Some(1),
            ..Signature::DEFAULT
        },
    },
    TypableCommand {
        name: "toggle-option",
        aliases: &["toggle"],
        doc: "Toggle a config option at runtime.\nFor example to toggle smart case search, use `:toggle search.smart-case`.",
        fun: toggle_option,
        completer: CommandCompleter::positional(&[completers::setting]),
        signature: Signature {
            positionals: (1, None),
            raw_after: Some(1),
            ..Signature::DEFAULT
        },
    },
    TypableCommand {
        name: "get-option",
        aliases: &["get"],
        doc: "Get the current value of a config option.",
        fun: get_option,
        completer: CommandCompleter::positional(&[completers::setting]),
        signature: Signature {
            positionals: (1, Some(1)),
            ..Signature::DEFAULT
        },
    },
    TypableCommand {
        name: "sort",
        aliases: &[],
        doc: "Sort ranges in selection.",
        fun: sort,
        completer: CommandCompleter::none(),
        signature: Signature {
            positionals: (0, Some(0)),
            flags: &[
                Flag {
                    name: "insensitive",
                    alias: Some('i'),
                    doc: "sort the ranges case-insensitively",
                    ..Flag::DEFAULT
                },
                Flag {
                    name: "reverse",
                    alias: Some('r'),
                    doc: "sort ranges in reverse order",
                    ..Flag::DEFAULT
                },
            ],
            ..Signature::DEFAULT
        },
    },
    TypableCommand {
        name: "reflow",
        aliases: &[],
        doc: "Hard-wrap the current selection of lines to a given width.",
        fun: reflow,
        completer: CommandCompleter::none(),
        signature: Signature {
            positionals: (0, Some(1)),
            ..Signature::DEFAULT
        },
    },
    TypableCommand {
        name: "tree-sitter-subtree",
        aliases: &["ts-subtree"],
        doc: "Display the smallest tree-sitter subtree that spans the primary selection, primarily for debugging queries.",
        fun: tree_sitter_subtree,
        completer: CommandCompleter::none(),
        signature: Signature {
            positionals: (0, Some(0)),
            ..Signature::DEFAULT
        },
    },
    TypableCommand {
        name: "config-reload",
        aliases: &[],
        doc: "Refresh user config.",
        fun: refresh_config,
        completer: CommandCompleter::none(),
        signature: Signature {
            positionals: (0, Some(0)),
            ..Signature::DEFAULT
        },
    },
    TypableCommand {
        name: "config-open",
        aliases: &[],
        doc: "Open the user config.toml file.",
        fun: open_config,
        completer: CommandCompleter::none(),
        signature: Signature {
            positionals: (0, Some(0)),
            ..Signature::DEFAULT
        },
    },
    TypableCommand {
        name: "config-open-workspace",
        aliases: &[],
        doc: "Open the workspace config.toml file.",
        fun: open_workspace_config,
        completer: CommandCompleter::none(),
        signature: Signature {
            positionals: (0, Some(0)),
            ..Signature::DEFAULT
        },
    },
    TypableCommand {
        name: "log-open",
        aliases: &[],
        doc: "Open the DoomHelix log file.",
        fun: open_log,
        completer: CommandCompleter::none(),
        signature: Signature {
            positionals: (0, Some(0)),
            ..Signature::DEFAULT
        },
    },
    TypableCommand {
        name: "insert-output",
        aliases: &[],
        doc: "Run shell command, inserting output before each selection.",
        fun: insert_output,
        completer: SHELL_COMPLETER,
        signature: SHELL_SIGNATURE,
    },
    TypableCommand {
        name: "append-output",
        aliases: &[],
        doc: "Run shell command, appending output after each selection.",
        fun: append_output,
        completer: SHELL_COMPLETER,
        signature: SHELL_SIGNATURE,
    },
    TypableCommand {
        name: "pipe",
        aliases: &["|"],
        doc: "Pipe each selection to the shell command.",
        fun: pipe,
        completer: SHELL_COMPLETER,
        signature: SHELL_SIGNATURE,
    },
    TypableCommand {
        name: "pipe-to",
        aliases: &[],
        doc: "Pipe each selection to the shell command, ignoring output.",
        fun: pipe_to,
        completer: SHELL_COMPLETER,
        signature: SHELL_SIGNATURE,
    },
    TypableCommand {
        name: "run-shell-command",
        aliases: &["sh", "!"],
        doc: "Run a shell command",
        fun: run_shell_command,
        completer: SHELL_COMPLETER,
        signature: SHELL_SIGNATURE,
    },
    TypableCommand {
        name: "reset-diff-change",
        aliases: &["diffget", "diffg"],
        doc: "Reset the diff change at the cursor position.",
        fun: reset_diff_change,
        completer: CommandCompleter::none(),
        signature: Signature {
            positionals: (0, Some(0)),
            ..Signature::DEFAULT
        },
    },
    TypableCommand {
        name: "clear-register",
        aliases: &[],
        doc: "Clear given register. If no argument is provided, clear all registers.",
        fun: clear_register,
        completer: CommandCompleter::all(completers::register),
        signature: Signature {
            positionals: (0, Some(1)),
            ..Signature::DEFAULT
        },
    },
    TypableCommand {
        name: "set-register",
        aliases: &[],
        doc: "Set contents of the given register.",
        fun: set_register,
        completer: CommandCompleter::positional(&[completers::register, completers::none]),
        signature: Signature {
            positionals: (2, Some(2)),
            raw_after: Some(1),
            ..Signature::DEFAULT
        },
    },
    TypableCommand {
        name: "redraw",
        aliases: &[],
        doc: "Clear and re-render the whole UI",
        fun: redraw,
        completer: CommandCompleter::none(),
        signature: Signature {
            positionals: (0, Some(0)),
            ..Signature::DEFAULT
        },
    },
    TypableCommand {
        name: "move",
        aliases: &["mv"],
        doc: "Move the current buffer and its corresponding file to a different path",
        fun: move_buffer,
        completer: CommandCompleter::positional(&[completers::filename]),
        signature: Signature {
            positionals: (1, Some(1)),
            ..Signature::DEFAULT
        },
    },
    TypableCommand {
        name: "move!",
        aliases: &["mv!"],
        doc: "Move the current buffer and its corresponding file to a different path creating necessary subdirectories",
        fun: force_move_buffer,
        completer: CommandCompleter::positional(&[completers::filename]),
        signature: Signature {
            positionals: (1, Some(1)),
            ..Signature::DEFAULT
        },
    },
    TypableCommand {
        name: "yank-diagnostic",
        aliases: &[],
        doc: "Yank diagnostic(s) under primary cursor to register, or clipboard by default",
        fun: yank_diagnostic,
        completer: CommandCompleter::all(completers::register),
        signature: Signature {
            positionals: (0, Some(1)),
            ..Signature::DEFAULT
        },
    },
    TypableCommand {
        name: "read",
        aliases: &["r"],
        doc: "Load a file into buffer",
        fun: read,
        completer: CommandCompleter::positional(&[completers::filename]),
        signature: Signature {
            positionals: (1, Some(1)),
            ..Signature::DEFAULT
        },
    },
    TypableCommand {
        name: "echo",
        aliases: &[],
        doc: "Prints the given arguments to the statusline.",
        fun: echo,
        completer: CommandCompleter::none(),
        signature: Signature {
            positionals: (1, None),
            ..Signature::DEFAULT
        },
    },
    TypableCommand {
        name: "noop",
        aliases: &[],
        doc: "Does nothing.",
        fun: noop,
        completer: CommandCompleter::none(),
        signature: Signature {
            positionals: (0, None),
            ..Signature::DEFAULT
        },
    },
    TypableCommand {
        name: "workspace-trust",
        aliases: &[],
        doc: "Add current workspace to the list of trusted workspaces.",
        fun: trust_workspace,
        completer: CommandCompleter::none(),
        signature: Signature { positionals: (0, None), ..Signature::DEFAULT },
    },
    TypableCommand {
        name: "workspace-untrust",
        aliases: &[],
        doc: "Remove current workspace from the list of trusted workspaces.",
        fun: untrust_workspace,
        completer: CommandCompleter::none(),
        signature: Signature { positionals: (0, None), ..Signature::DEFAULT },
    }
];

pub static TYPABLE_COMMAND_MAP: Lazy<HashMap<&'static str, &'static TypableCommand>> =
    Lazy::new(|| {
        TYPABLE_COMMAND_LIST
            .iter()
            .flat_map(|cmd| {
                std::iter::once((cmd.name, cmd))
                    .chain(cmd.aliases.iter().map(move |&alias| (alias, cmd)))
            })
            .collect()
    });

fn execute_command_line(
    cx: &mut compositor::Context,
    input: &str,
    event: PromptEvent,
) -> anyhow::Result<()> {
    let (command, rest, _) = command_line::split(input);
    if command.is_empty() {
        return Ok(());
    }

    // If command is numeric, interpret as line number and go there.
    if command.parse::<usize>().is_ok() && rest.trim().is_empty() {
        let cmd = TYPABLE_COMMAND_MAP.get("goto").unwrap();
        return execute_command(cx, cmd, command, event);
    }

    match typed::TYPABLE_COMMAND_MAP.get(command) {
        Some(cmd) => execute_command(cx, cmd, rest, event),
        None if event == PromptEvent::Validate => Err(anyhow!("no such command: '{command}'")),
        None => Ok(()),
    }
}

pub(super) fn execute_command(
    cx: &mut compositor::Context,
    cmd: &TypableCommand,
    args: &str,
    event: PromptEvent,
) -> anyhow::Result<()> {
    let args = if event == PromptEvent::Validate {
        Args::parse(args, cmd.signature, true, |token| {
            expansion::expand(cx.editor, token).map_err(|err| err.into())
        })
        .map_err(|err| anyhow!("'{}': {err}", cmd.name))?
    } else {
        Args::parse(args, cmd.signature, false, |token| Ok(token.content))
            .expect("arg parsing cannot fail when validation is turned off")
    };

    (cmd.fun)(cx, args, event).map_err(|err| anyhow!("'{}': {err}", cmd.name))
}

#[allow(clippy::unnecessary_unwrap)]
pub(super) fn command_mode(cx: &mut Context) {
    let mut prompt = Prompt::new(
        ":".into(),
        Some(':'),
        complete_command_line,
        move |cx: &mut compositor::Context, input: &str, event: PromptEvent| {
            if let Err(err) = execute_command_line(cx, input, event) {
                cx.editor.set_error(err.to_string());
            }
        },
    );
    prompt.doc_fn = Box::new(command_line_doc);

    // Calculate initial completion
    prompt.recalculate_completion(cx.editor);
    cx.push_layer(Box::new(prompt));
}

fn command_line_doc(input: &str) -> Option<Cow<'_, str>> {
    let (command, _, _) = command_line::split(input);
    let command = TYPABLE_COMMAND_MAP.get(command)?;

    if command.aliases.is_empty() && command.signature.flags.is_empty() {
        return Some(Cow::Borrowed(command.doc));
    }

    let mut doc = command.doc.to_string();

    if !command.aliases.is_empty() {
        write!(doc, "\nAliases: {}", command.aliases.join(", ")).unwrap();
    }

    if !command.signature.flags.is_empty() {
        const ARG_PLACEHOLDER: &str = " <arg>";

        fn flag_len(flag: &Flag) -> usize {
            let name_len = flag.name.len();
            let alias_len = if let Some(alias) = flag.alias {
                "/-".len() + alias.len_utf8()
            } else {
                0
            };
            let arg_len = if flag.completions.is_some() {
                ARG_PLACEHOLDER.len()
            } else {
                0
            };
            name_len + alias_len + arg_len
        }

        doc.push_str("\nFlags:");

        let max_flag_len = command.signature.flags.iter().map(flag_len).max().unwrap();

        for flag in command.signature.flags {
            let mut buf = [0u8; 4];
            let this_flag_len = flag_len(flag);
            write!(
                doc,
                "\n  --{flag_text}{spacer:spacing$}  {doc}",
                doc = flag.doc,
                // `fmt::Arguments` does not respect width controls so we must place the spacers
                // explicitly:
                spacer = "",
                spacing = max_flag_len - this_flag_len,
                flag_text = format_args!(
                    "{}{}{}{}",
                    flag.name,
                    // Ideally this would be written as a `format_args!` too but the borrow
                    // checker is not yet smart enough.
                    if flag.alias.is_some() { "/-" } else { "" },
                    if let Some(alias) = flag.alias {
                        alias.encode_utf8(&mut buf)
                    } else {
                        ""
                    },
                    if flag.completions.is_some() {
                        ARG_PLACEHOLDER
                    } else {
                        ""
                    }
                ),
            )
            .unwrap();
        }
    }

    Some(Cow::Owned(doc))
}

fn complete_command_line(editor: &Editor, input: &str) -> Vec<ui::prompt::Completion> {
    let (command, rest, complete_command) = command_line::split(input);

    if complete_command {
        fuzzy_match(
            input,
            TYPABLE_COMMAND_LIST.iter().map(|command| command.name),
            false,
        )
        .into_iter()
        .map(|(name, _)| (0.., name.into()))
        .collect()
    } else {
        TYPABLE_COMMAND_MAP
            .get(command)
            .map_or_else(Vec::new, |cmd| {
                if command == "agent" {
                    let args_offset = command.len() + 1;
                    let completions = complete_agent_command_args(rest, args_offset);
                    if !completions.is_empty() {
                        return completions;
                    }
                }

                {
                    let args_offset = command.len() + 1;
                    complete_command_args(editor, cmd.signature, &cmd.completer, rest, args_offset)
                }
            })
    }
}

fn complete_agent_command_args(input: &str, offset: usize) -> Vec<ui::prompt::Completion> {
    let leading = input.len() - input.trim_start().len();
    let input = &input[leading..];
    let base_offset = offset + leading;

    let Some(first_space) = input.find(char::is_whitespace) else {
        return complete_agent_values(AGENT_SUBCOMMANDS, input, base_offset);
    };

    let subcommand = &input[..first_space];
    let rest = &input[first_space..];
    let value_leading = rest.len() - rest.trim_start().len();
    let value = &rest[value_leading..];
    if value.contains(char::is_whitespace) {
        return Vec::new();
    }

    let value_offset = base_offset + first_space + value_leading;
    match subcommand {
        "position" => {
            complete_agent_values(&["left", "right", "top", "bottom"], value, value_offset)
        }
        "resize" => complete_agent_values(&["30", "40", "+5", "-5"], value, value_offset),
        _ => Vec::new(),
    }
}

fn complete_agent_values(
    values: &'static [&'static str],
    input: &str,
    offset: usize,
) -> Vec<ui::prompt::Completion> {
    fuzzy_match(input, values.iter().copied(), false)
        .into_iter()
        .map(|(value, _)| ((offset).., value.into()))
        .collect()
}

pub fn complete_command_args(
    editor: &Editor,
    signature: Signature,
    completer: &CommandCompleter,
    input: &str,
    offset: usize,
) -> Vec<ui::prompt::Completion> {
    use command_line::{CompletionState, ExpansionKind, Tokenizer};

    // TODO: completion should depend on the location of the cursor instead of the end of the
    // string. This refactor is left for the future but the below completion code should respect
    // the cursor position if it becomes a parameter.
    let cursor = input.len();
    let prefix = &input[..cursor];
    let mut tokenizer = Tokenizer::new(prefix, false);
    let mut args = Args::new(signature, false);
    let mut final_token = None;
    let mut is_last_token = true;

    while let Some(token) = args
        .read_token(&mut tokenizer)
        .expect("arg parsing cannot fail when validation is turned off")
    {
        final_token = Some(token.clone());
        args.push(token.content)
            .expect("arg parsing cannot fail when validation is turned off");
        if tokenizer.pos() >= cursor {
            is_last_token = false;
        }
    }

    // Use a fake final token when the input is not terminated with a token. This simulates an
    // empty argument, causing completion on an empty value whenever you type space/tab. For
    // example if you say `":open README.md "` (with that trailing space) you should see the
    // files in the current dir - completing `""` rather than completions for `"README.md"` or
    // `"README.md "`.
    let token = if is_last_token {
        let token = Token::empty_at(prefix.len());
        args.push(token.content.clone()).unwrap();
        token
    } else {
        final_token.unwrap()
    };

    // Don't complete on closed tokens, for example after writing a closing double quote.
    if token.is_terminated {
        return Vec::new();
    }

    match token.kind {
        TokenKind::Unquoted | TokenKind::Quoted(_) => {
            match args.completion_state() {
                CompletionState::Positional => {
                    // If the completion state is positional there must be at least one positional
                    // in `args`.
                    let n = args
                        .len()
                        .checked_sub(1)
                        .expect("completion state to be positional");
                    let completer = completer.for_argument_number(n);

                    completer(editor, &token.content)
                        .into_iter()
                        .map(|(range, span)| quote_completion(&token, range, span, offset))
                        .collect()
                }
                CompletionState::Flag(_) => fuzzy_match(
                    token.content.trim_start_matches('-'),
                    signature.flags.iter().map(|flag| flag.name),
                    false,
                )
                .into_iter()
                .map(|(name, _)| ((offset + token.content_start).., format!("--{name}").into()))
                .collect(),
                CompletionState::FlagArgument(flag) => fuzzy_match(
                    &token.content,
                    flag.completions
                        .expect("flags in FlagArgument always have completions"),
                    false,
                )
                .into_iter()
                .map(|(value, _)| ((offset + token.content_start).., (*value).into()))
                .collect(),
            }
        }
        TokenKind::Expand | TokenKind::Expansion(ExpansionKind::Shell) => {
            // See the comment about the checked sub expect above.
            let arg_completer = matches!(args.completion_state(), CompletionState::Positional)
                .then(|| {
                    let n = args
                        .len()
                        .checked_sub(1)
                        .expect("completion state to be positional");
                    completer.for_argument_number(n)
                });
            complete_expand(editor, &token, arg_completer, offset + token.content_start)
        }
        TokenKind::Expansion(ExpansionKind::Variable) => {
            complete_variable_expansion(&token.content, offset + token.content_start)
        }
        TokenKind::Expansion(ExpansionKind::Unicode) => Vec::new(),
        TokenKind::Expansion(ExpansionKind::Register) => {
            complete_register_expansion(editor, &token.content, offset + token.content_start)
        }
        TokenKind::ExpansionKind => {
            complete_expansion_kind(&token.content, offset + token.content_start)
        }
    }
}

/// Replace the content and optionally update the range of a positional's completion to account
/// for quoting.
///
/// This is used to handle completions of file or directory names for example. When completing a
/// file with a space, tab or percent character in the name, the space should be escaped by
/// quoting the entire token. If the token being completed is already quoted, any quotes within
/// the completion text should be escaped by doubling them.
fn quote_completion<'a>(
    token: &Token,
    range: ops::RangeFrom<usize>,
    mut span: Span<'a>,
    offset: usize,
) -> (ops::RangeFrom<usize>, Span<'a>) {
    fn replace<'a>(text: Cow<'a, str>, from: char, to: &str) -> Cow<'a, str> {
        if text.contains(from) {
            Cow::Owned(text.replace(from, to))
        } else {
            text
        }
    }

    match token.kind {
        TokenKind::Unquoted if span.content.contains([' ', '\t', '%']) => {
            span.content = Cow::Owned(format!(
                "'{}{}'",
                // Escape any inner single quotes by doubling them.
                replace(token.content[..range.start].into(), '\'', "''"),
                replace(span.content, '\'', "''")
            ));
            // Ignore `range.start` here since we're replacing the entire token. We used
            // `range.start` above to emulate the replacement that using `range.start` would have
            // done.
            ((offset + token.content_start).., span)
        }
        TokenKind::Quoted(quote) => {
            span.content = replace(span.content, quote.char(), quote.escape());
            ((range.start + offset + token.content_start).., span)
        }
        TokenKind::Expand => {
            // NOTE: `token.content_start` is already accounted for in `offset` for `Expand`
            // tokens.
            span.content = replace(span.content, '"', "\"\"");
            ((range.start + offset).., span)
        }
        _ => ((range.start + offset + token.content_start).., span),
    }
}

fn complete_expand(
    editor: &Editor,
    token: &Token,
    completer: Option<&Completer>,
    offset: usize,
) -> Vec<ui::prompt::Completion> {
    use command_line::{ExpansionKind, Tokenizer};

    let mut start = 0;

    // If the expand token contains expansions, complete those.
    while let Some(idx) = token.content[start..].find('%') {
        let idx = start + idx;
        if token.content.as_bytes().get(idx + '%'.len_utf8()).copied() == Some(b'%') {
            // Two percents together are skipped.
            start = idx + ('%'.len_utf8() * 2);
        } else {
            let mut tokenizer = Tokenizer::new(&token.content[idx..], false);
            let token = tokenizer
                .parse_percent_token()
                .map(|token| token.expect("arg parser cannot fail when validation is disabled"));
            start = idx + tokenizer.pos();

            // Like closing quote characters in `complete_command_args` above, don't provide
            // completions if the token is already terminated. This also skips expansions
            // which have already been fully written, for example
            // `"%{cursor_line}:%{cursor_col` should complete `cursor_column` instead of
            // `cursor_line`.
            let Some(token) = token.filter(|t| !t.is_terminated) else {
                continue;
            };

            let local_offset = offset + idx + token.content_start;
            match token.kind {
                TokenKind::Expansion(ExpansionKind::Variable) => {
                    return complete_variable_expansion(&token.content, local_offset);
                }
                TokenKind::Expansion(ExpansionKind::Shell) => {
                    return complete_expand(editor, &token, None, local_offset);
                }
                TokenKind::ExpansionKind => {
                    return complete_expansion_kind(&token.content, local_offset);
                }
                _ => continue,
            }
        }
    }

    match completer {
        // If no expansions were found and an argument is being completed,
        Some(completer) if start == 0 => completer(editor, &token.content)
            .into_iter()
            .map(|(range, span)| quote_completion(token, range, span, offset))
            .collect(),
        _ => Vec::new(),
    }
}

fn complete_variable_expansion(content: &str, offset: usize) -> Vec<ui::prompt::Completion> {
    use expansion::Variable;

    fuzzy_match(
        content,
        Variable::VARIANTS.iter().map(Variable::as_str),
        false,
    )
    .into_iter()
    .map(|(name, _)| (offset.., (*name).into()))
    .collect()
}

fn complete_register_expansion(
    editor: &Editor,
    content: &str,
    offset: usize,
) -> Vec<ui::prompt::Completion> {
    let register_names: Vec<String> = editor
        .registers
        .iter_preview()
        .map(|(ch, _)| ch.to_string())
        .collect();
    fuzzy_match(content, register_names, false)
        .into_iter()
        .map(|(name, _)| (offset.., name.to_string().into()))
        .collect()
}

fn complete_expansion_kind(content: &str, offset: usize) -> Vec<ui::prompt::Completion> {
    use command_line::ExpansionKind;

    fuzzy_match(
        content,
        // Skip `ExpansionKind::Variable` since its kind string is empty.
        ExpansionKind::VARIANTS
            .iter()
            .skip(1)
            .map(ExpansionKind::as_str),
        false,
    )
    .into_iter()
    .map(|(name, _)| (offset.., (*name).into()))
    .collect()
}

fn trust_workspace(
    cx: &mut compositor::Context,
    args: Args<'_>,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    helix_loader::workspace_trust::WorkspaceTrust::load(false).trust_workspace();

    cx.editor.config_events.0.send(ConfigEvent::Refresh)?;
    // HACK
    lsp_restart(cx, args, event)
}

fn untrust_workspace(
    _cx: &mut compositor::Context,
    _args: Args<'_>,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    helix_loader::workspace_trust::WorkspaceTrust::load(false).untrust_workspace();
    Ok(())
}
