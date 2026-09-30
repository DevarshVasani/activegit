//! Phase 3: status panel state machine.
//!
//! Pure logic: selection, stage toggle, commit modal. Rendering lives in
//! `ui.rs`; the event loop lives in `main.rs`.

use crossterm::event::KeyCode;
use git_tui_core::branch::BranchInfo;
use git_tui_core::diff::FileDiff;
use git_tui_core::error::GitError;
use git_tui_core::jobqueue::{AsyncJob, AsyncResult, JobQueue};
use git_tui_core::llm::LlmConfig;
use git_tui_core::log::CommitInfo;
use git_tui_core::stash::StashEntry;
use git_tui_core::status::{FileState, RepoStatus, StatusEntry};
use git_tui_core::sync::SyncStatus;
use std::cell::Cell;

use crate::config::{Config, KeyBindings, Theme};
use crate::fuzzy;
use crate::ui::{cursor_line_text, diff_rows, visible_file_rows, wrap_draft, DiffRow, FileRow};

/// Labels for the LLM setup form rows: provider, model, API key, base URL.
pub const LLM_FIELD_LABELS: [&str; 4] = [
    "Provider (openai|openrouter|ollama|anthropic|gemini|custom)",
    "Model",
    "API key (empty = use env var)",
    "Base URL (optional, custom only)",
];

/// Input mode: normal list navigation, a fullscreen diff overlay, or a
/// text-input modal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Normal,
    FullDiff,
    Committing,
    NewBranch,
    StashPush,
    FindFile,
    OpenProject,
    ConfirmInit,
    /// Push with no upstream yet: the draft names the remote to push `-u` to.
    SetUpstream,
    /// Publish a repo with no remotes: the draft is the new `origin` URL.
    SetRemote,
    /// In-TUI LLM provider setup (`A` in the file list): provider, model,
    /// API key, and optional base URL. Enter saves to the config file.
    LlmSettings,
}

/// Which panel receives navigation keys. Everything is vertical: Tab cycles
/// the rail panels plus the right-side diff preview; Enter opens the
/// selected file fullscreen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Status,
    Branches,
    Log,
    Stash,
    Diff,
}

/// UI state. Owns the [`JobQueue`]; git state itself lives on the worker
/// thread and is mirrored here as snapshots (status, diff, branches).
pub struct App {
    queue: JobQueue,
    keys: KeyBindings,
    theme: Theme,
    /// LLM provider config for Shift+A commit generation.
    llm: LlmConfig,
    /// A GenerateCommitMessage job is in flight ("generating…" in the
    /// commit modal). Set on Shift+A, cleared when the message or an
    /// error arrives via `poll`.
    generating: bool,
    /// One-line success confirmation (e.g. "LLM settings saved"), shown
    /// above the footer until the next keypress. Like `error` but green.
    notice: Option<String>,
    /// Where the config file lives (for the LLM setup form to persist).
    /// `None` in tests: saving still updates the session, minus the file.
    config_path: Option<std::path::PathBuf>,
    /// LLM setup form buffers: provider, model, api_key, base_url.
    llm_form: [String; 4],
    /// Saved cursor (chars) per form field while switching rows.
    llm_cursors: [usize; 4],
    /// Highlighted form row.
    llm_selected: usize,
    /// Short repo name for the status panel (workdir basename).
    repo_name: String,
    status: Option<RepoStatus>,
    /// Browsable tree: changed files in status order, then clean tracked
    /// files alphabetically. Selection indexes into this, not `status.files`.
    file_list: Vec<StatusEntry>,
    selected: usize,
    mode: Mode,
    focus: Focus,
    draft: String,
    /// Cursor inside `draft` as a char index (commit/branch/stash modals,
    /// the finder query, the jump-to-path line). Byte math is derived on
    /// demand so direct `draft` writes elsewhere can't break it: readers
    /// clamp to the current char count.
    draft_cursor: usize,
    /// Cursor inside the fuzzy file finder (`Mode::FindFile`).
    finder_selected: usize,
    /// Where the finder returns on Enter/Esc: the mode it was opened
    /// from (`Normal` or `FullDiff`), so `/` works fullscreen too.
    finder_return: Mode,
    /// Where the push/publish modals return on Enter/Esc (`Normal` or
    /// `FullDiff`), so `P` works fullscreen too.
    sync_return: Mode,
    error: Option<String>,
    quit: bool,
    diff: Option<FileDiff>,
    /// Side-by-side rows paired from `diff`, computed once when the diff
    /// arrives. Frames and scroll steps read this instead of rerunning the
    /// word diffs over the whole diff every time.
    diff_rows: Vec<DiffRow>,
    /// (path, staged) the loaded/loading diff belongs to.
    diff_for: Option<(String, bool)>,
    /// The loaded diff is a whole-file view (clean file), not a real diff.
    diff_whole_file: bool,
    /// The whole-file view came from the empty-diff fallback (a listed file
    /// whose diff has no hunks, e.g. a mode-only change). Reset whenever the
    /// selected path changes.
    fallback_whole_file: bool,
    hunk: usize,
    /// Line cursor: index into `diff_rows` (both the fullscreen view and
    /// the right-side preview). `j/k`/`↑`/`↓` move it one row, `J`/`K`
    /// jump by hunk, `PgUp`/`PgDn` move it by 10. Nvim-style: the cursor
    /// walks inside the viewport first and the view only scrolls at the
    /// edge (see `ensure_cursor_visible`); transitions (open/close,
    /// reload) top-pin via `snap_scroll_to_cursor` instead.
    cursor: usize,
    /// Nvim-style block column: char index into the cursor row's logical
    /// text (`h`/`l` move it, clamped to the line; row moves keep it so
    /// it behaves like vim's sticky column). Rendered as one reversed
    /// cell in `ui.rs`.
    cursor_col: usize,
    diff_scroll: u16,
    /// Last rendered inner heights of the fullscreen diff and the
    /// right-side preview (recorded by the renderer, which only gets
    /// `&App` — same pattern as `draft_wrap_width`). Drives
    /// `ensure_cursor_visible`. Zero until the first frame, which reads
    /// as height 1 (top-pin) so the cursor is never stranded.
    full_view_h: Cell<usize>,
    prev_view_h: Cell<usize>,
    /// Nvim-style visual selection anchor (`v` charwise, `V` linewise).
    /// The live end is the line cursor; `y` yanks, `Esc` cancels.
    /// Cleared whenever the diff reloads.
    visual: Option<Visual>,
    branches: Option<Vec<BranchInfo>>,
    branch_selected: usize,
    log: Option<Vec<CommitInfo>>,
    log_scroll: u16,
    stash: Option<Vec<StashEntry>>,
    stash_selected: usize,
    /// Upstream tracking state (upstream ref, ahead/behind, remotes).
    /// Loaded at startup and after every mutation; drives `P` behavior.
    sync: Option<SyncStatus>,
    /// A push/pull/publish job is in flight ("pushing…", …). Shown in the
    /// status panel until it succeeds or fails.
    syncing: Option<String>,
    // List scroll offsets for the left-rail panels. `Cell` so the renderer
    // (which only gets `&App`) can follow the selection without a `&mut`.
    files_scroll: Cell<usize>,
    branch_scroll: Cell<usize>,
    stash_scroll: Cell<usize>,
    /// Content width of the commit box as last rendered. The renderer
    /// records it so ↑/↓ move by the same soft-wrapped rows the user sees.
    draft_wrap_width: Cell<usize>,
    /// Collapsed directory prefixes in the files tree (no trailing slash).
    collapsed: std::collections::HashSet<String>,
    /// Open (expanded) folder header the files cursor sits on, if any.
    /// `selected` then holds the folder's first file (its diff previews).
    /// Collapsed headers don't use this: their hidden anchor file does.
    dir_cursor: Option<String>,
    /// Directory picker for opening projects (`Mode::OpenProject`).
    open_browser: Option<OpenBrowser>,
    /// Rendered Markdown preview toggle (`m` for `.md` files).
    markdown_preview: bool,
    /// (path, staged) the loaded/loading Markdown text belongs to.
    md_for: Option<(String, bool)>,
    /// Full new-version text for the Markdown preview.
    md_text: Option<String>,
    /// A mutation just landed; the [5] diff preview (and Markdown) waits
    /// for the fresh status before reloading. Reloading the old
    /// `diff_for` immediately (e.g. stash push/pop) flashes stale content
    /// — a whole-file view instead of hunks or vice versa — and costs an
    /// extra worker roundtrip.
    pending_diff_reload: bool,
}

/// Nvim-style visual selection: charwise (`v`) or linewise (`V`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VisualMode {
    Charwise,
    Linewise,
}

/// Normalized visual selection `((start_row, start_col), (end_row,
/// end_col), linewise)`; see `App::visual_selection`.
pub(crate) type VisualSel = ((usize, usize), (usize, usize), bool);

/// Visual anchor: the fixed end of the selection. The live end is the
/// line cursor (`cursor`, `cursor_col`); see `App::visual_selection`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Visual {
    pub(crate) anchor_row: usize,
    pub(crate) anchor_col: usize,
    pub(crate) mode: VisualMode,
}

/// One subdirectory row in the project browser.
#[derive(Debug, Clone)]
pub struct DirEntry {
    pub name: String,
    /// The directory itself contains `.git` (repo root): badged `[repo]`.
    pub is_repo_root: bool,
}

/// Which row of the browser list is highlighted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserRow {
    /// `.`: open the browsed folder itself.
    Current,
    /// `..`: go up to the parent.
    Parent,
    /// A subdirectory: opens as a project when it is a repo, otherwise
    /// Enter descends into it for browsing.
    Dir(usize),
}

/// Directory picker behind `Mode::OpenProject`. Row 0 is always `.`
/// (open this folder), row 1 is `..` (parent), then the visible
/// subdirectories (all of them, or the type-to-filter matches).
#[derive(Debug, Clone)]
pub struct OpenBrowser {
    pub cwd: std::path::PathBuf,
    pub entries: Vec<DirEntry>,
    /// Subset of `entries` shown: everything without a filter, the
    /// case-insensitive substring matches with one.
    pub view: Vec<DirEntry>,
    pub selected: usize,
    pub error: Option<String>,
    /// Jump-to-path line (`tab`): typing a path instead of browsing.
    pub editing_path: bool,
    /// Type-to-filter query: any typed char narrows this folder's list,
    /// so there is no separate "start searching" step.
    pub filter: String,
}

impl OpenBrowser {
    /// Rows in the list: `.` + `..` + one per visible subdirectory.
    pub fn row_count(&self) -> usize {
        self.view.len() + 2
    }

    pub fn row(&self, index: usize) -> BrowserRow {
        if index == 0 {
            BrowserRow::Current
        } else if index == 1 {
            BrowserRow::Parent
        } else {
            BrowserRow::Dir(index - 2)
        }
    }

    pub fn selected_row(&self) -> BrowserRow {
        self.row(self.selected.min(self.row_count().saturating_sub(1)))
    }

    /// Absolute path the highlighted row points at (`.` and `..`
    /// included; `..` of `/` resolves to `/` itself).
    pub fn selected_path(&self) -> std::path::PathBuf {
        match self.selected_row() {
            BrowserRow::Current => self.cwd.clone(),
            BrowserRow::Parent => self
                .cwd
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| self.cwd.clone()),
            BrowserRow::Dir(i) => self
                .view
                .get(i)
                .map(|e| self.cwd.join(&e.name))
                .unwrap_or_else(|| self.cwd.clone()),
        }
    }

    /// Start substring-filtering the folder list (any typed char lands
    /// here; there is no separate search mode to enter).
    pub fn push_filter_char(&mut self, c: char) {
        self.filter.push(c);
        self.apply_filter();
    }

    pub fn pop_filter_char(&mut self) {
        self.filter.pop();
        self.apply_filter();
    }

    /// Drop the query and show the full list again.
    pub fn clear_filter(&mut self) {
        self.filter.clear();
        self.apply_filter();
    }

    /// Rebuild the visible rows from `entries` + `filter`. `.`/`..` always
    /// stay; a fresh filter jumps the highlight to the first match.
    fn apply_filter(&mut self) {
        if self.filter.is_empty() {
            self.view = self.entries.clone();
            return;
        }
        let q = self.filter.to_lowercase();
        self.view = self
            .entries
            .iter()
            .filter(|e| e.name.to_lowercase().contains(&q))
            .cloned()
            .collect();
        if !self.view.is_empty() {
            // First match sits at row 2 (after `.` and `..`).
            self.selected = 2;
        }
        self.selected = self.selected.min(self.row_count().saturating_sub(1));
    }

    pub fn move_cursor(&mut self, delta: isize) {
        let n = self.row_count();
        if n == 0 {
            return;
        }
        let cur = self.selected.min(n - 1) as isize;
        self.selected = (cur + delta).clamp(0, n as isize - 1) as usize;
    }

    /// Re-read `cwd` from disk (subdirectories only, sorted). Keeps the
    /// old listing when the directory cannot be read. Moving folders
    /// resets the cursor and drops any active filter.
    pub fn rescan(&mut self) {
        match read_subdirs(&self.cwd) {
            Ok(entries) => {
                self.entries = entries;
                self.error = None;
            }
            Err(e) => {
                self.entries = Vec::new();
                self.error = Some(e);
            }
        }
        self.filter.clear();
        self.view = self.entries.clone();
        self.selected = self.selected.min(self.row_count().saturating_sub(1));
    }

    /// Move the browser to `dir`, rescanning only when it reads cleanly.
    /// Returns false (and keeps the old folder) on unreadable targets.
    pub fn goto(&mut self, dir: std::path::PathBuf) -> bool {
        match read_subdirs(&dir) {
            Ok(entries) => {
                self.cwd = dir;
                self.entries = entries;
                self.view = self.entries.clone();
                self.selected = 0;
                self.error = None;
                self.filter.clear();
                true
            }
            Err(e) => {
                self.error = Some(e);
                false
            }
        }
    }
}

/// Subdirectories of `dir`, sorted by name. Files are hidden: only
/// folders can become projects.
fn read_subdirs(dir: &std::path::Path) -> Result<Vec<DirEntry>, String> {
    let rd = std::fs::read_dir(dir).map_err(|e| format!("cannot list {}: {e}", dir.display()))?;
    let mut out = Vec::new();
    for entry in rd {
        let entry = entry.map_err(|e| format!("cannot list {}: {e}", dir.display()))?;
        let ft = entry
            .file_type()
            .map_err(|e| format!("cannot list {}: {e}", dir.display()))?;
        // Follow symlinked dirs so linked projects stay browsable.
        let is_dir = ft.is_dir() || (ft.is_symlink() && entry.path().is_dir());
        if !is_dir {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        let is_repo_root = path.join(".git").exists();
        out.push(DirEntry { name, is_repo_root });
    }
    out.sort_by_key(|e| e.name.to_lowercase());
    Ok(out)
}

/// Split an upstream shorthand (`origin/main`, `origin/feature/x`) into
/// its remote and branch. Remote names cannot contain `/`, so the first
/// slash is the boundary; a bare name with no slash pushes to `origin`.
fn split_upstream(upstream: &str) -> (&str, &str) {
    match upstream.find('/') {
        Some(i) => (&upstream[..i], &upstream[i + 1..]),
        None => ("origin", upstream),
    }
}

/// Cumulative ancestor prefixes: "a/b/c/f" -> ["a", "a/b", "a/b/c"].
fn ancestors_of(path: &str) -> Vec<String> {
    let mut out = Vec::new();
    for (i, b) in path.bytes().enumerate() {
        if b == b'/' {
            out.push(path[..i].to_string());
        }
    }
    out
}

impl App {
    #[cfg(test)]
    pub fn new(queue: JobQueue) -> Self {
        Self::new_with_config(queue, Config::default())
    }

    pub fn new_with_config(queue: JobQueue, config: Config) -> Self {
        let mut app = Self {
            queue,
            keys: config.keys,
            theme: config.theme,
            llm: config.llm,
            generating: false,
            notice: None,
            config_path: None,
            llm_form: Default::default(),
            llm_cursors: [0; 4],
            llm_selected: 0,
            repo_name: "repo".into(),
            status: None,
            file_list: Vec::new(),
            selected: 0,
            mode: Mode::Normal,
            focus: Focus::Status,
            draft: String::new(),
            draft_cursor: 0,
            finder_selected: 0,
            finder_return: Mode::Normal,
            sync_return: Mode::Normal,
            error: None,
            quit: false,
            diff: None,
            diff_rows: Vec::new(),
            diff_for: None,
            diff_whole_file: false,
            fallback_whole_file: false,
            hunk: 0,
            cursor: 0,
            cursor_col: 0,
            diff_scroll: 0,
            full_view_h: Cell::new(0),
            prev_view_h: Cell::new(0),
            visual: None,
            branches: None,
            branch_selected: 0,
            log: None,
            log_scroll: 0,
            stash: None,
            stash_selected: 0,
            sync: None,
            syncing: None,
            files_scroll: Cell::new(0),
            branch_scroll: Cell::new(0),
            stash_scroll: Cell::new(0),
            // Default matches the commit modal's usual inner width (60 − 2
            // borders) until the renderer records the real one.
            draft_wrap_width: Cell::new(58),
            collapsed: Default::default(),
            dir_cursor: None,
            open_browser: None,
            markdown_preview: false,
            md_for: None,
            md_text: None,
            pending_diff_reload: false,
        };
        app.refresh();
        app.preload_panels();
        app
    }

    pub fn repo_name(&self) -> &str {
        &self.repo_name
    }

    pub fn set_repo_name(&mut self, name: String) {
        self.repo_name = name;
    }

    /// Open the project browser (`o`): pick a directory, Enter opens it.
    /// Starts at the current project's root so siblings are one step away.
    pub fn begin_open_project(&mut self, start: std::path::PathBuf) {
        self.mode = Mode::OpenProject;
        self.draft.clear();
        self.error = None;
        let mut browser = OpenBrowser {
            cwd: start,
            entries: Vec::new(),
            view: Vec::new(),
            // Row 0 is `.` (the folder itself); row 1 is `..`, then dirs.
            selected: 0,
            error: None,
            editing_path: false,
            filter: String::new(),
        };
        browser.rescan();
        self.open_browser = Some(browser);
    }

    pub fn open_browser(&self) -> Option<&OpenBrowser> {
        self.open_browser.as_ref()
    }

    pub fn open_browser_mut(&mut self) -> Option<&mut OpenBrowser> {
        self.open_browser.as_mut()
    }

    pub fn push_draft_char(&mut self, c: char) {
        self.insert_draft_char(c);
    }

    pub fn pop_draft_char(&mut self) {
        self.delete_draft_before();
    }

    pub fn clear_draft(&mut self) {
        self.draft.clear();
        self.draft_cursor = 0;
    }

    /// Cursor position in `draft` (chars from the start), clamped so stale
    /// values after direct `draft` writes stay valid.
    pub fn draft_cursor(&self) -> usize {
        self.draft_cursor.min(self.draft.chars().count())
    }

    fn draft_byte_index(&self) -> usize {
        self.draft
            .char_indices()
            .nth(self.draft_cursor())
            .map(|(i, _)| i)
            .unwrap_or(self.draft.len())
    }

    /// Insert one char at the cursor (commit box, finder, jump line).
    pub fn insert_draft_char(&mut self, c: char) {
        let byte = self.draft_byte_index();
        self.draft.insert(byte, c);
        self.draft_cursor = self.draft_cursor() + 1;
    }

    /// Backspace: delete the char before the cursor.
    pub fn delete_draft_before(&mut self) {
        let cursor = self.draft_cursor();
        if cursor == 0 {
            return;
        }
        let byte = self.draft_byte_index();
        let prev = self.draft[..byte]
            .chars()
            .next_back()
            .map(|c| c.len_utf8())
            .unwrap_or(0);
        self.draft.drain(byte - prev..byte);
        self.draft_cursor = cursor - 1;
    }

    /// Delete key: delete the char under the cursor.
    pub fn delete_draft_after(&mut self) {
        let byte = self.draft_byte_index();
        if byte >= self.draft.len() {
            return;
        }
        let len = self.draft[byte..]
            .chars()
            .next()
            .map(|c| c.len_utf8())
            .unwrap_or(0);
        self.draft.drain(byte..byte + len);
        self.draft_cursor = self.draft_cursor();
    }

    pub fn move_draft_left(&mut self) {
        self.draft_cursor = self.draft_cursor().saturating_sub(1);
    }

    pub fn move_draft_right(&mut self) {
        self.draft_cursor = (self.draft_cursor() + 1).min(self.draft.chars().count());
    }

    pub fn move_draft_home(&mut self) {
        self.draft_cursor = 0;
    }

    pub fn move_draft_end(&mut self) {
        self.draft_cursor = self.draft.chars().count();
    }

    /// Up in the commit box: one visual (soft-wrapped) row up, preserving
    /// the display column when the target row is long enough.
    pub fn move_draft_up_line(&mut self) {
        self.move_draft_visual_row(-1);
    }

    /// Down in the commit box: one visual (soft-wrapped) row down, then
    /// the very end past the last row.
    pub fn move_draft_down_line(&mut self) {
        self.move_draft_visual_row(1);
    }

    /// Move the draft cursor by `delta` visual rows using the same wrap
    /// the renderer drew (`draft_wrap_width`). The display column is
    /// preserved; past the first/last row clamps to document start/end.
    fn move_draft_visual_row(&mut self, delta: i32) {
        use unicode_width::UnicodeWidthChar;
        let width = self.draft_wrap_width.get().max(1);
        let cursor = self.draft_cursor();
        let wrap = wrap_draft(&self.draft, cursor, width);
        if wrap.bounds.is_empty() {
            return;
        }
        let target = wrap.cursor_row as i32 + delta;
        if target < 0 {
            self.draft_cursor = 0;
            return;
        }
        let target = target as usize;
        if target >= wrap.bounds.len() {
            self.draft_cursor = self.draft.chars().count();
            return;
        }
        let (rs, re) = wrap.bounds[target];
        let chars: Vec<char> = self.draft.chars().collect();
        let mut col = 0usize;
        let mut idx = rs;
        while idx < re && idx < chars.len() {
            let w = chars[idx].width().unwrap_or(0);
            if col + w > wrap.cursor_col {
                break;
            }
            col += w;
            idx += 1;
        }
        self.draft_cursor = idx;
    }

    /// Recorded by the commit-box renderer each frame.
    pub(crate) fn set_draft_wrap_width(&self, width: usize) {
        self.draft_wrap_width.set(width.max(1));
    }

    /// Cancel the open-project flow entirely (Esc in the browser).
    pub fn cancel_open_project(&mut self) {
        self.mode = Mode::Normal;
        self.draft.clear();
        self.open_browser = None;
    }

    /// Move from the browser to the `git init` confirm step, rewriting the
    /// draft to the directory that would be initialized.
    pub fn confirm_init_prompt(&mut self, dir: String) {
        self.mode = Mode::ConfirmInit;
        self.draft = dir;
    }

    /// Close the open-project flow after a successful open/switch.
    pub fn finish_open_project(&mut self) {
        self.mode = Mode::Normal;
        self.draft.clear();
        self.open_browser = None;
    }

    /// Back out of the init-confirm step to the browser (keeps the listing).
    pub fn back_to_open_project(&mut self) {
        self.mode = Mode::OpenProject;
    }

    pub fn set_error(&mut self, msg: String) {
        self.error = Some(msg);
    }

    pub fn set_browser_error(&mut self, msg: String) {
        if let Some(b) = self.open_browser.as_mut() {
            b.error = Some(msg);
        } else {
            self.error = Some(msg);
        }
    }

    pub(crate) fn files_scroll(&self) -> usize {
        self.files_scroll.get()
    }

    pub(crate) fn set_files_scroll(&self, off: usize) {
        self.files_scroll.set(off);
    }

    pub(crate) fn branch_scroll(&self) -> usize {
        self.branch_scroll.get()
    }

    pub(crate) fn set_branch_scroll(&self, off: usize) {
        self.branch_scroll.set(off);
    }

    pub(crate) fn stash_scroll(&self) -> usize {
        self.stash_scroll.get()
    }

    pub(crate) fn set_stash_scroll(&self, off: usize) {
        self.stash_scroll.set(off);
    }

    pub(crate) fn is_collapsed(&self, dir: &str) -> bool {
        self.collapsed.contains(dir)
    }

    /// Open folder header under the files cursor. `None` on a file or a
    /// collapsed header, and when the stored folder went stale (collapsed,
    /// folded away, or emptied by a refresh).
    fn cursor_dir_is(&self, dir: &str) -> bool {
        let prefix = format!("{dir}/");
        !self.collapsed.contains(dir)
            && !self.is_hidden_path(dir)
            && self.file_list.iter().any(|f| f.path.starts_with(&prefix))
    }

    pub(crate) fn cursor_dir(&self) -> Option<&str> {
        let dir = self.dir_cursor.as_deref()?;
        self.cursor_dir_is(dir).then_some(dir)
    }

    pub(crate) fn set_collapsed(&mut self, dir: &str, value: bool) {
        if value {
            self.collapsed.insert(dir.to_string());
        } else {
            self.collapsed.remove(dir);
        }
    }

    pub fn status(&self) -> Option<&RepoStatus> {
        self.status.as_ref()
    }

    pub fn selected(&self) -> usize {
        self.selected
    }

    pub fn selected_file(&self) -> Option<&StatusEntry> {
        self.file_list
            .get(self.selected.min(self.file_count().saturating_sub(1)))
    }

    /// One browsable tree entry by index (for the file finder).
    pub fn file_entry(&self, index: usize) -> Option<&StatusEntry> {
        self.file_list.get(index)
    }

    /// Every browsable file: changed first, then clean tracked files.
    /// The files panel renders this same list so the highlight always
    /// tracks the cursor (see `render_files_panel`).
    pub fn file_list(&self) -> &[StatusEntry] {
        &self.file_list
    }

    pub fn has_files(&self) -> bool {
        !self.file_list.is_empty()
    }

    /// Rebuild [`Self::file_list`] from a fresh status: changed files keep
    /// status order, then clean tracked files alphabetically.
    fn rebuild_file_list(&mut self) {
        let Some(st) = self.status.as_ref() else {
            self.file_list = Vec::new();
            self.selected = 0;
            return;
        };
        let mut list = st.files.clone();
        let changed: std::collections::HashSet<&str> =
            st.files.iter().map(|e| e.path.as_str()).collect();
        // tracked_files arrives sorted, so filtering preserves that order.
        list.extend(
            st.tracked_files
                .iter()
                .filter(|p| !changed.contains(p.as_str()))
                .map(|p| StatusEntry {
                    path: p.clone(),
                    state: FileState::Clean,
                }),
        );
        self.file_list = list;
        self.selected = self.selected.min(self.file_count().saturating_sub(1));
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn focus(&self) -> Focus {
        self.focus
    }

    pub fn theme(&self) -> Theme {
        self.theme
    }

    /// Quit regardless of bindings (Ctrl-C safety hatch in the event loop).
    pub fn request_quit(&mut self) {
        self.quit = true;
    }

    pub fn diff(&self) -> Option<&FileDiff> {
        self.diff.as_ref()
    }

    /// Side-by-side rows for the loaded diff, paired once on arrival.
    /// Empty when no diff is loaded.
    pub(crate) fn diff_rows(&self) -> &[DiffRow] {
        &self.diff_rows
    }

    /// Replace the loaded diff and re-pair its side-by-side rows.
    /// All `self.diff` writes go through here so the cache never stales.
    fn set_diff(&mut self, diff: Option<FileDiff>) {
        self.diff_rows = diff.as_ref().map(diff_rows).unwrap_or_default();
        self.diff = diff;
        self.cursor = self.cursor.min(self.diff_rows.len().saturating_sub(1));
        // A fresh diff shifts rows: drop any visual selection with it.
        self.visual = None;
    }

    /// Whether the loaded diff shows staged (`Some(true)`) or unstaged
    /// (`Some(false)`) changes; `None` when no diff is loaded.
    pub fn diff_viewing_staged(&self) -> Option<bool> {
        self.diff_for.as_ref().map(|(_, staged)| *staged)
    }

    /// The loaded diff is a whole-file view (clean file has no diff).
    pub fn diff_whole_file(&self) -> bool {
        self.diff_whole_file
    }

    /// Full new-version Markdown text, if loaded for the current target.
    pub fn markdown_text(&self) -> Option<&str> {
        self.md_text.as_deref()
    }

    /// Whether the preview should show for the selected file: toggled on
    /// and the selected path is Markdown.
    pub fn show_markdown_preview(&self) -> bool {
        self.markdown_preview
            && self
                .selected_file()
                .is_some_and(|f| crate::markdown::is_markdown_path(&f.path))
    }

    /// `m`: toggle rendered Markdown preview for `.md` files.
    pub fn toggle_markdown_preview(&mut self) {
        let Some(file) = self.selected_file().cloned() else {
            return;
        };
        if !crate::markdown::is_markdown_path(&file.path) {
            self.error =
                Some("markdown preview is only for .md files (press enter for diff)".into());
            return;
        }
        self.markdown_preview = !self.markdown_preview;
        self.diff_scroll = 0;
        // The Markdown view has no diff rows: drop any selection with it.
        self.visual = None;
        if !self.markdown_preview {
            self.snap_scroll_to_cursor();
        }
        if self.markdown_preview {
            self.maybe_load_markdown();
        }
    }

    /// Request full new-version text for the selected Markdown file,
    /// unless it is already loaded/loading.
    fn maybe_load_markdown(&mut self) {
        if !self.show_markdown_preview() {
            return;
        }
        let target = self.diff_target();
        if target == self.md_for && self.md_text.is_some() {
            return;
        }
        // Only (re)submit when the target changed or nothing is cached.
        if target == self.md_for {
            return;
        }
        self.md_for = target.clone();
        self.md_text = None;
        if let Some((path, staged)) = target {
            if let Err(e) = self.queue.submit(AsyncJob::LoadMarkdown { path, staged }) {
                self.error = Some(e.to_string());
            }
        }
    }

    pub fn hunk(&self) -> usize {
        self.hunk
    }

    /// Line-cursor row into `diff_rows` (clamped; 0 when no diff loaded).
    pub fn cursor_row(&self) -> usize {
        self.cursor.min(self.diff_rows.len().saturating_sub(1))
    }

    /// Nvim-style block column: char index into the cursor row's text.
    pub fn cursor_col(&self) -> usize {
        self.cursor_col
    }

    /// Active visual selection, if any.
    pub fn visual(&self) -> Option<Visual> {
        self.visual
    }

    /// Normalized selection `((start_row, start_col), (end_row, end_col),
    /// linewise)` from the anchor and the live cursor, clamped to the
    /// loaded rows. `None` when visual mode is off or no diff is loaded.
    pub(crate) fn visual_selection(&self) -> Option<VisualSel> {
        let v = self.visual?;
        if self.diff_rows.is_empty() {
            return None;
        }
        let max = self.diff_rows.len() - 1;
        let (ar, ac) = (v.anchor_row.min(max), v.anchor_col);
        let (cr, cc) = (self.cursor.min(max), self.cursor_col);
        let ((r1, c1), (r2, c2)) = if (ar, ac) <= (cr, cc) {
            ((ar, ac), (cr, cc))
        } else {
            ((cr, cc), (ar, ac))
        };
        Some(((r1, c1), (r2, c2), v.mode == VisualMode::Linewise))
    }

    /// Recorded by the fullscreen renderer each frame.
    pub(crate) fn set_full_view_h(&self, h: usize) {
        self.full_view_h.set(h);
    }

    /// Recorded by the preview renderer each frame.
    pub(crate) fn set_prev_view_h(&self, h: usize) {
        self.prev_view_h.set(h);
    }

    #[cfg(test)]
    pub(crate) fn set_view_h_for_test(&mut self, full: usize, prev: usize) {
        self.full_view_h.set(full);
        self.prev_view_h.set(prev);
    }

    pub fn diff_scroll(&self) -> u16 {
        self.diff_scroll
    }

    pub fn branches(&self) -> Option<&[BranchInfo]> {
        self.branches.as_deref()
    }

    pub fn branch_selected(&self) -> usize {
        self.branch_selected
    }

    pub fn log(&self) -> Option<&[CommitInfo]> {
        self.log.as_deref()
    }

    pub fn log_scroll(&self) -> u16 {
        self.log_scroll
    }

    pub fn stash(&self) -> Option<&[StashEntry]> {
        self.stash.as_deref()
    }

    pub fn stash_selected(&self) -> usize {
        self.stash_selected
    }

    pub fn draft(&self) -> &str {
        &self.draft
    }

    /// File-list indices matching the finder query, best first. Empty
    /// query lists every file in tree order.
    pub fn finder_matches(&self) -> Vec<usize> {
        let query = self.draft.as_str();
        if query.is_empty() {
            return (0..self.file_list.len()).collect();
        }
        let paths: Vec<&str> = self.file_list.iter().map(|e| e.path.as_str()).collect();
        fuzzy::rank(query, &paths)
            .into_iter()
            .map(|(i, _)| i)
            .collect()
    }

    /// Clamped cursor into [`Self::finder_matches`] for the renderer.
    pub fn finder_cursor(&self) -> usize {
        self.finder_selected
            .min(self.finder_matches().len().saturating_sub(1))
    }

    /// Mode the open finder returns to on Enter/Esc.
    pub fn finder_return(&self) -> Mode {
        self.finder_return
    }

    /// Upstream tracking state, if loaded yet.
    pub fn sync(&self) -> Option<&SyncStatus> {
        self.sync.as_ref()
    }

    /// In-flight push/pull/publish label ("pushing…"), if any.
    pub fn syncing(&self) -> Option<&str> {
        self.syncing.as_deref()
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub fn should_quit(&self) -> bool {
        self.quit
    }

    /// Whether an LLM commit-message job is in flight (commit modal shows
    /// "generating…").
    pub fn is_generating(&self) -> bool {
        self.generating
    }

    #[allow(dead_code)]
    pub fn llm_config(&self) -> &LlmConfig {
        &self.llm
    }

    /// Config file location for persisting the LLM setup form.
    pub fn set_config_path(&mut self, path: Option<std::path::PathBuf>) {
        self.config_path = path;
    }

    /// Success confirmation line (cleared on the next keypress).
    pub fn notice(&self) -> Option<&str> {
        self.notice.as_deref()
    }

    /// Highlighted row of the LLM setup form.
    pub fn llm_selected(&self) -> usize {
        self.llm_selected.min(LLM_FIELD_LABELS.len() - 1)
    }

    /// Display value of form row `i`: the live draft for the selected row,
    /// the stashed buffer otherwise.
    pub fn llm_field_value(&self, i: usize) -> &str {
        if i == self.llm_selected() {
            &self.draft
        } else {
            self.llm_form.get(i).map(String::as_str).unwrap_or("")
        }
    }

    /// `A` in the file list: open the LLM setup form prefilled from the
    /// current `[llm]` config (or its defaults).
    pub fn begin_llm_settings(&mut self) {
        self.mode = Mode::LlmSettings;
        self.error = None;
        self.notice = None;
        self.llm_form = [
            self.llm.provider.clone(),
            self.llm.model.clone(),
            self.llm.api_key.clone(),
            self.llm.base_url.clone().unwrap_or_default(),
        ];
        self.llm_cursors = [0; 4];
        self.llm_selected = 0;
        self.draft = self.llm_form[0].clone();
        self.move_draft_end();
        self.llm_cursors[0] = self.draft_cursor();
    }

    /// Stash the live draft/cursor into the selected form row.
    fn stash_llm_field(&mut self) {
        let i = self.llm_selected();
        self.llm_form[i] = self.draft.clone();
        self.llm_cursors[i] = self.draft_cursor();
    }

    /// Move the form highlight, stashing/loading the row buffers.
    fn move_llm_selection(&mut self, delta: isize) {
        let n = LLM_FIELD_LABELS.len();
        self.stash_llm_field();
        let cur = self.llm_selected.min(n - 1) as isize;
        self.llm_selected = (cur + delta).clamp(0, n as isize - 1) as usize;
        let i = self.llm_selected;
        self.draft = self.llm_form[i].clone();
        self.draft_cursor = self.llm_cursors[i].min(self.draft.chars().count());
    }

    /// Back out without saving.
    pub fn cancel_llm_settings(&mut self) {
        self.mode = Mode::Normal;
        self.draft.clear();
        self.draft_cursor = 0;
    }

    /// Enter in the setup form: validate, apply to the session, and persist
    /// `[llm]` to the config file. Stays open with an error on bad input.
    pub fn save_llm_settings(&mut self) {
        self.stash_llm_field();
        let provider = self.llm_form[0].trim().to_lowercase();
        if !git_tui_core::llm::PROVIDERS.contains(&provider.as_str()) {
            self.error = Some(format!(
                "unknown provider {provider:?} (expected one of: {})",
                git_tui_core::llm::PROVIDERS.join(", ")
            ));
            return;
        }
        let model = self.llm_form[1].trim().to_string();
        self.llm = LlmConfig {
            provider,
            model: if model.is_empty() {
                LlmConfig::default().model
            } else {
                model
            },
            api_key: self.llm_form[2].trim().to_string(),
            base_url: {
                let u = self.llm_form[3].trim().to_string();
                if u.is_empty() {
                    None
                } else {
                    Some(u)
                }
            },
        };
        if let Some(path) = self.config_path.clone() {
            if let Err(e) = Config::save_llm_to_path(&path, &self.llm) {
                self.error = Some(e.to_string());
                return;
            }
            self.notice = Some(format!("LLM settings saved to {}", path.display()));
        } else {
            self.notice = Some("LLM settings updated for this session".into());
        }
        self.mode = Mode::Normal;
        self.draft.clear();
        self.draft_cursor = 0;
        self.error = None;
    }

    /// Keys inside the LLM setup form. Tab/Up/Down switch rows; the draft
    /// line edits the selected row; Enter saves; Esc cancels.
    fn on_key_llm_settings(&mut self, key: KeyCode) {
        match key {
            KeyCode::Char(c) => self.insert_draft_char(c),
            KeyCode::Backspace => self.delete_draft_before(),
            KeyCode::Delete => self.delete_draft_after(),
            KeyCode::Left => self.move_draft_left(),
            KeyCode::Right => self.move_draft_right(),
            KeyCode::Home => self.move_draft_home(),
            KeyCode::End => self.move_draft_end(),
            KeyCode::Up => self.move_llm_selection(-1),
            KeyCode::Down | KeyCode::Tab => self.move_llm_selection(1),
            KeyCode::BackTab => self.move_llm_selection(-1),
            KeyCode::Enter => self.save_llm_settings(),
            KeyCode::Esc => self.cancel_llm_settings(),
            _ => {}
        }
    }

    /// Shift+A in the commit box: reference every staged file (index vs
    /// HEAD) and ask the LLM provider for a Conventional-Commits message.
    /// The result arrives via `poll` as `GeneratedMessage` and replaces
    /// the draft (cursor jumps to the end for quick editing).
    pub fn begin_generate_commit_message(&mut self) {
        if self.mode != Mode::Committing || self.generating {
            return;
        }
        self.generating = true;
        self.error = None;
        if let Err(e) = self.queue.submit(AsyncJob::GenerateCommitMessage {
            llm: self.llm.clone(),
        }) {
            self.generating = false;
            self.error = Some(e.to_string());
        }
    }

    #[cfg(test)]
    pub(crate) fn set_status_for_test(&mut self, st: git_tui_core::status::RepoStatus) {
        self.status = Some(st);
        self.selected = 0;
        self.rebuild_file_list();
    }

    #[cfg(test)]
    pub(crate) fn set_diff_for_test(&mut self, diff: git_tui_core::diff::FileDiff, staged: bool) {
        self.diff_for = Some((diff.path.clone(), staged));
        self.diff_whole_file = false;
        self.set_diff(Some(diff));
        self.hunk = 0;
        self.cursor = 0;
        self.cursor_col = 0;
        self.diff_scroll = 0;
    }

    #[cfg(test)]
    pub(crate) fn set_whole_file_for_test(&mut self, whole: bool) {
        self.diff_whole_file = whole;
    }

    #[cfg(test)]
    pub(crate) fn set_branches_for_test(&mut self, branches: Vec<BranchInfo>) {
        self.branches = Some(branches);
        self.branch_selected = 0;
    }

    #[cfg(test)]
    pub(crate) fn set_log_for_test(&mut self, entries: Vec<CommitInfo>) {
        self.log = Some(entries);
        self.log_scroll = 0;
    }

    #[cfg(test)]
    pub(crate) fn set_stash_for_test(&mut self, entries: Vec<StashEntry>) {
        self.stash = Some(entries);
        self.stash_selected = 0;
    }

    fn file_count(&self) -> usize {
        self.file_list.len()
    }

    fn refresh(&mut self) {
        if let Err(e) = self.queue.submit(AsyncJob::RefreshStatus) {
            self.error = Some(e.to_string());
        }
    }

    /// Eagerly load every rail panel at startup so branches, log, and
    /// stash render data immediately instead of waiting for first focus.
    /// The channel is unbounded and reads are idempotent, so submitting
    /// up front is cheap; `maybe_load_*` stays as a backstop in case a
    /// submit here ever fails.
    fn preload_panels(&mut self) {
        let jobs = [
            AsyncJob::ListBranches,
            AsyncJob::ListLog {
                limit: Self::LOG_LIMIT,
            },
            AsyncJob::ListStash,
            AsyncJob::LoadSync,
        ];
        for job in jobs {
            if let Err(e) = self.queue.submit(job) {
                self.error = Some(e.to_string());
                return;
            }
        }
    }

    /// Dispatch a keypress (event loop calls this; then [`App::poll`]).
    /// Test/legacy path with no modifiers: `A` is treated as Shift+A so
    /// tests can drive generation with a single KeyCode. The binary uses
    /// [`Self::on_key_with_modifiers`].
    #[allow(dead_code)]
    pub fn on_key(&mut self, key: KeyCode) {
        // Test/legacy path: no modifiers known. `A` is treated as Shift+A
        // so tests can drive generation with a single KeyCode.
        let shift = matches!(key, KeyCode::Char('A'));
        self.on_key_with_modifiers(key, shift);
    }

    /// Modifier-aware dispatch (the event loop calls this). Only Shift+A
    /// generates a message in the commit box, so a literal `A` (caps lock
    /// or otherwise without Shift) still types normally.
    pub fn on_key_with_modifiers(&mut self, key: KeyCode, shift_held: bool) {
        // A new action dismisses the previous error/notice; async failures
        // from this action arrive later via `poll` and replace them.
        self.error = None;
        self.notice = None;
        if self.mode == Mode::Committing && matches!(key, KeyCode::Char('a' | 'A')) && shift_held {
            self.begin_generate_commit_message();
            return;
        }
        // Shift+Right in the file list jumps to the right-side diff
        // preview (plain Right expands folders, so it must stay put).
        if self.mode == Mode::Normal
            && self.focus == Focus::Status
            && key == KeyCode::Right
            && shift_held
        {
            self.focus = Focus::Diff;
            return;
        }
        self.on_key_inner(key);
    }

    fn on_key_inner(&mut self, key: KeyCode) {
        if self.mode == Mode::LlmSettings {
            // Shift+A must not leak generation in here: the form owns every
            // key until Enter saves or Esc cancels.
            self.on_key_llm_settings(key);
            return;
        }
        if self.mode == Mode::FullDiff {
            self.on_key_full_diff(key);
            return;
        }
        if self.mode == Mode::FindFile {
            match key {
                KeyCode::Up => self.finder_move(-1),
                KeyCode::Down => self.finder_move(1),
                KeyCode::Left => self.move_draft_left(),
                KeyCode::Right => self.move_draft_right(),
                KeyCode::Char(c) => {
                    self.insert_draft_char(c);
                    self.finder_selected = 0;
                }
                KeyCode::Backspace => {
                    self.delete_draft_before();
                    self.finder_selected = 0;
                }
                KeyCode::Delete => {
                    self.delete_draft_after();
                    self.finder_selected = 0;
                }
                KeyCode::Home => self.move_draft_home(),
                KeyCode::End => self.move_draft_end(),
                KeyCode::Enter => {
                    self.expand_selected();
                    self.submit_finder();
                }
                KeyCode::Esc => {
                    self.mode = self.finder_return;
                    self.draft.clear();
                }
                _ => {}
            }
            return;
        }
        if self.mode != Mode::Normal {
            match key {
                KeyCode::Char(c) => self.insert_draft_char(c),
                KeyCode::Backspace => self.delete_draft_before(),
                KeyCode::Delete => self.delete_draft_after(),
                KeyCode::Left => self.move_draft_left(),
                KeyCode::Right => self.move_draft_right(),
                KeyCode::Up if self.mode == Mode::Committing => self.move_draft_up_line(),
                KeyCode::Down if self.mode == Mode::Committing => self.move_draft_down_line(),
                KeyCode::Home => self.move_draft_home(),
                KeyCode::End => self.move_draft_end(),
                KeyCode::Enter => match self.mode {
                    Mode::Committing => self.submit_commit(),
                    Mode::NewBranch => self.submit_new_branch(),
                    Mode::StashPush => self.submit_stash_push(),
                    Mode::SetUpstream => self.submit_push_upstream(),
                    Mode::SetRemote => self.submit_publish(),
                    // FullDiff and FindFile return before reaching here.
                    // OpenProject/ConfirmInit submit through `Workspace`
                    // (it owns all projects), so they are no-ops here.
                    // LlmSettings is routed to `on_key_llm_settings` above.
                    Mode::Normal
                    | Mode::FullDiff
                    | Mode::FindFile
                    | Mode::OpenProject
                    | Mode::ConfirmInit
                    | Mode::LlmSettings => {}
                },
                KeyCode::Esc => {
                    self.mode = match self.mode {
                        Mode::SetUpstream | Mode::SetRemote => self.sync_return,
                        _ => Mode::Normal,
                    };
                    self.draft.clear();
                }
                _ => {}
            }
            return;
        }
        // Binding dispatch (first match wins; modal keys above stay fixed).
        // Cloned: small vecs, and it keeps the borrow checker happy while
        // the arms below take &mut self.
        // Esc with an active selection only leaves visual mode.
        if key == KeyCode::Esc && self.visual.is_some() {
            self.visual = None;
            return;
        }
        let k = self.keys.clone();
        if k.focus_next.contains(&key) {
            // Tab cycles the left rail only (Status -> Branches -> Log ->
            // Stash -> Status). The right-side Diff preview is reached via
            // `5` / Shift+Right, never via Tab; Tab from Diff drops back
            // to the file list.
            self.focus = match self.focus {
                Focus::Status => Focus::Branches,
                Focus::Branches => Focus::Log,
                Focus::Log => Focus::Stash,
                Focus::Stash => Focus::Status,
                Focus::Diff => Focus::Status,
            };
            self.maybe_load_branches();
            self.maybe_load_log();
            self.maybe_load_stash();
        } else if self.focus == Focus::Status && key == KeyCode::Left {
            self.toggle_folder();
        } else if self.focus == Focus::Status && key == KeyCode::Right {
            self.expand_at_cursor();
        } else if k.focus_status.contains(&key) {
            self.focus = Focus::Status;
        } else if k.focus_branches.contains(&key) {
            self.focus = Focus::Branches;
            self.maybe_load_branches();
        } else if k.focus_log.contains(&key) {
            self.focus = Focus::Log;
            self.maybe_load_log();
        } else if k.focus_stash.contains(&key) {
            self.focus = Focus::Stash;
            self.maybe_load_stash();
        } else if k.focus_diff.contains(&key) {
            self.focus = Focus::Diff;
        } else if k.nav_down.contains(&key) {
            self.move_down();
        } else if k.nav_up.contains(&key) {
            self.move_up();
        } else if key == KeyCode::Char('h') && self.focus == Focus::Diff {
            self.move_column(-1);
        } else if key == KeyCode::Char('l') && self.focus == Focus::Diff {
            self.move_column(1);
        } else if (key == KeyCode::Char('0') || key == KeyCode::Home) && self.focus == Focus::Diff {
            self.column_home();
        } else if key == KeyCode::End && self.focus == Focus::Diff {
            self.column_end();
        } else if key == KeyCode::Char('v') && self.focus == Focus::Diff {
            self.begin_visual(VisualMode::Charwise);
        } else if key == KeyCode::Char('V') && self.focus == Focus::Diff {
            self.begin_visual(VisualMode::Linewise);
        } else if key == KeyCode::Char('y') && self.focus == Focus::Diff {
            self.begin_yank();
        } else if k.checkout.contains(&key) && self.focus == Focus::Branches {
            self.checkout_selected_branch();
        } else if k.stash_pop.contains(&key) && self.focus == Focus::Stash {
            self.pop_selected_stash();
        } else if key == KeyCode::Enter && matches!(self.focus, Focus::Status | Focus::Diff) {
            // Enter on a file opens it fullscreen (checkout/pop own Enter
            // in their own panels, so no conflict). Works from the file
            // list and from the focused diff preview on the right.
            self.open_full_diff();
        } else if k.stage.contains(&key) {
            if self.focus == Focus::Status {
                self.toggle_stage();
            }
        } else if k.discard.contains(&key) {
            if self.focus == Focus::Status {
                self.discard_selected();
            }
        } else if k.branch_new.contains(&key) && self.focus == Focus::Branches {
            self.mode = Mode::NewBranch;
            self.draft.clear();
        } else if k.stash_push.contains(&key) && self.focus == Focus::Stash {
            self.mode = Mode::StashPush;
            self.draft.clear();
        } else if k.branch_delete.contains(&key) && self.focus == Focus::Branches {
            self.delete_selected_branch();
        } else if k.stash_drop.contains(&key) && self.focus == Focus::Stash {
            self.drop_selected_stash();
        } else if k.scroll_up.contains(&key) {
            if self.focus == Focus::Diff && !self.show_markdown_preview() {
                self.move_cursor(-10);
            } else {
                self.scroll_diff_by(-10);
            }
        } else if k.scroll_down.contains(&key) {
            if self.focus == Focus::Diff && !self.show_markdown_preview() {
                self.move_cursor(10);
            } else {
                self.scroll_diff_by(10);
            }
        } else if k.commit.contains(&key) {
            self.mode = Mode::Committing;
            self.draft.clear();
        } else if k.toggle_markdown_preview.contains(&key) {
            self.toggle_markdown_preview();
        } else if k.llm_settings.contains(&key) {
            self.begin_llm_settings();
        } else if k.find_files.contains(&key) {
            self.open_finder();
        } else if k.refresh.contains(&key) {
            self.refresh();
        } else if k.sync_pull.contains(&key) {
            self.start_pull();
        } else if k.sync_push.contains(&key) {
            self.start_push();
        } else if k.quit.contains(&key) {
            self.quit = true;
        }
    }

    /// Keys inside the fullscreen diff overlay. `j/k`/`↑`/`↓` move the
    /// line cursor, `h/l`/`←`/`→` move the nvim-style block column,
    /// `0`/`Home`/`End` jump it, `J`/`K` jump by hunk, `v`/`V` select,
    /// `y` yanks, `/` finds another file without leaving fullscreen;
    /// Esc leaves visual mode first, then closes back to the file list.
    fn on_key_full_diff(&mut self, key: KeyCode) {
        let k = self.keys.clone();
        if key == KeyCode::Esc {
            if self.visual.is_some() {
                self.visual = None;
            } else {
                self.mode = Mode::Normal;
                self.snap_scroll_to_cursor();
            }
        } else if k.toggle_markdown_preview.contains(&key) {
            self.toggle_markdown_preview();
        } else if k.find_files.contains(&key) {
            self.open_finder();
        } else if key == KeyCode::Up {
            // Arrows move the line cursor so long single-hunk diffs stay
            // viewable line by line; `J`/`K` below jump by hunk.
            self.move_cursor_or_scroll(-1);
        } else if key == KeyCode::Down || k.nav_down.contains(&key) {
            self.move_cursor_or_scroll(1);
        } else if k.nav_up.contains(&key) {
            self.move_cursor_or_scroll(-1);
        } else if key == KeyCode::Char('h') || key == KeyCode::Left {
            self.move_column_or_scroll(-1);
        } else if key == KeyCode::Char('l') || key == KeyCode::Right {
            self.move_column_or_scroll(1);
        } else if key == KeyCode::Char('0') || key == KeyCode::Home {
            self.column_home();
        } else if key == KeyCode::End {
            self.column_end();
        } else if key == KeyCode::Char('v') {
            self.begin_visual(VisualMode::Charwise);
        } else if key == KeyCode::Char('V') {
            self.begin_visual(VisualMode::Linewise);
        } else if key == KeyCode::Char('y') {
            self.begin_yank();
        } else if k.stage.contains(&key) {
            self.stage_selected_hunk();
        } else if k.discard.contains(&key) {
            self.discard_loaded_file();
        } else if k.scroll_up.contains(&key) {
            self.move_cursor_or_scroll(-10);
        } else if k.scroll_down.contains(&key) {
            self.move_cursor_or_scroll(10);
        } else if key == KeyCode::Char('J') && !self.show_markdown_preview() {
            self.select_hunk(self.hunk.saturating_add(1));
        } else if key == KeyCode::Char('K') && !self.show_markdown_preview() {
            self.select_hunk(self.hunk.saturating_sub(1));
        } else if k.sync_pull.contains(&key) {
            self.start_pull();
        } else if k.sync_push.contains(&key) {
            self.start_push();
        } else if k.quit.contains(&key) {
            self.quit = true;
        }
    }

    /// Enter on a file: open its diff fullscreen.
    fn open_full_diff(&mut self) {
        if self.selected_file().is_some() {
            self.mode = Mode::FullDiff;
            self.snap_scroll_to_cursor();
        }
    }

    fn move_down(&mut self) {
        match self.focus {
            Focus::Status => self.step_tree(true),
            Focus::Branches => {
                self.branch_selected = self
                    .branch_selected
                    .saturating_add(1)
                    .min(self.branch_count().saturating_sub(1));
            }
            // Read-only log: navigation scrolls.
            Focus::Log => {
                self.scroll_log_by(1);
            }
            Focus::Stash => {
                self.stash_selected = self
                    .stash_selected
                    .saturating_add(1)
                    .min(self.stash_count().saturating_sub(1));
            }
            // Right-side preview: navigation moves the line cursor
            // (PgUp/PgDn below page by 10 regardless of focus).
            Focus::Diff => {
                self.move_cursor_or_scroll(1);
            }
        }
    }

    fn move_up(&mut self) {
        match self.focus {
            Focus::Status => self.step_tree(false),
            Focus::Branches => {
                self.branch_selected = self.branch_selected.saturating_sub(1);
            }
            Focus::Log => {
                self.scroll_log_by(-1);
            }
            Focus::Stash => {
                self.stash_selected = self.stash_selected.saturating_sub(1);
            }
            Focus::Diff => {
                self.move_cursor_or_scroll(-1);
            }
        }
    }

    /// Whether `path` sits under a collapsed dir (i.e. its tree rows are
    /// folded away; only the `▶` header renders).
    fn is_hidden_path(&self, path: &str) -> bool {
        ancestors_of(path)
            .into_iter()
            .any(|dir| self.collapsed.contains(dir.as_str()))
    }

    pub(crate) fn is_hidden_index(&self, index: usize) -> bool {
        self.file_list
            .get(index)
            .is_some_and(|f| self.is_hidden_path(&f.path))
    }

    /// Tree navigation: one row at a time through exactly the rows the
    /// files panel shows (`visible_file_rows`), so every folder header,
    /// open (`▼`) or collapsed (`▶`), is a cursor stop for Space (stage
    /// the whole folder) and Left/Right (fold). Clamps at both ends.
    ///
    /// - File row: select it.
    /// - Open header: `dir_cursor` marks it; `selected` becomes the
    ///   folder's first file, so the preview shows something useful.
    /// - Collapsed header: `selected` becomes a hidden anchor inside it
    ///   (the first file going down, the last going up) and the header
    ///   takes the highlight.
    fn step_tree(&mut self, down: bool) {
        enum Target {
            File(usize),
            Dir(String),
        }
        let target = {
            let rows = visible_file_rows(&self.file_list, |d| self.collapsed.contains(d));
            let Some(pos) = self.tree_cursor_row(&rows) else {
                return;
            };
            let next = if down {
                pos.checked_add(1)
            } else {
                pos.checked_sub(1)
            };
            match next.and_then(|i| rows.get(i)) {
                Some(FileRow::File { index, .. }) => Target::File(*index),
                Some(FileRow::Dir { path, .. }) => Target::Dir(path.to_string()),
                None => return,
            }
        };
        match target {
            Target::File(index) => {
                self.selected = index;
                self.dir_cursor = None;
            }
            Target::Dir(dir) if self.collapsed.contains(&dir) => {
                if let Some(anchor) = self.folder_anchor(&dir, down) {
                    self.selected = anchor;
                }
                self.dir_cursor = None;
            }
            Target::Dir(dir) => {
                // An open header previews the folder's first file.
                let prefix = format!("{dir}/");
                if let Some(first) = self
                    .file_list
                    .iter()
                    .position(|f| f.path.starts_with(&prefix))
                {
                    self.selected = first;
                }
                self.dir_cursor = Some(dir);
            }
        }
        self.maybe_load_diff();
    }

    /// Index in `rows` of the cursor: the open header it sits on, the
    /// collapsed header hiding the selected file, or the file's own row.
    fn tree_cursor_row(&self, rows: &[FileRow]) -> Option<usize> {
        let dir_row = |dir: &str| {
            rows.iter()
                .position(|r| matches!(r, FileRow::Dir { path, .. } if *path == dir))
        };
        if let Some(dir) = self.cursor_dir() {
            return dir_row(dir);
        }
        let file = self.file_list.get(self.selected)?;
        if let Some(header) = self.collapsed_dir_for(&file.path) {
            return dir_row(&header);
        }
        rows.iter()
            .position(|r| matches!(r, FileRow::File { index, .. } if *index == self.selected))
    }

    /// File to select for folder `dir`. Going down (`first`): the first
    /// file under it after the cursor, else its first file. Going up: the
    /// last file under it before the cursor, else its last file.
    fn folder_anchor(&self, dir: &str, first: bool) -> Option<usize> {
        let prefix = format!("{dir}/");
        let under: Vec<usize> = (0..self.file_list.len())
            .filter(|&i| self.file_list[i].path.starts_with(&prefix))
            .collect();
        if first {
            under
                .iter()
                .copied()
                .find(|&i| i > self.selected)
                .or_else(|| under.first().copied())
        } else {
            under
                .iter()
                .copied()
                .rev()
                .find(|&i| i < self.selected)
                .or_else(|| under.last().copied())
        }
    }

    /// Toggle collapse on the deepest collapsed-capable ancestor of the
    /// selected file (or expand when that ancestor is collapsed). Only
    /// the deepest directory flips so sibling subtrees stay visible.
    fn toggle_folder(&mut self) {
        // On an open header: fold that folder. The cursor stays on its
        // (now `▶`) header via the hidden anchor, like any collapse.
        if let Some(dir) = self.dir_cursor.take() {
            if self.cursor_dir_is(&dir) {
                self.set_collapsed(&dir, true);
                return;
            }
        }
        let Some(file) = self.selected_file() else {
            return;
        };
        let dirs: Vec<String> = ancestors_of(&file.path);
        let Some(deepest) = dirs.last().cloned() else {
            self.error = Some("nothing to collapse: file has no parent folder".into());
            return;
        };
        let collapsed = !self.collapsed.contains(&deepest);
        self.set_collapsed(&deepest, collapsed);
        // Stay put when collapsing under the cursor: the cursor becomes a
        // hidden anchor and the `▶` header takes the highlight, so Space
        // still stages the whole dir and Right still expands it. The next
        // Up/Down escapes to the nearest visible file (skipped both ways),
        // so hidden files never render while navigating.
    }

    /// Right on a file: expand every collapsed ancestor so the file is
    /// visible (no-op already visible).
    fn expand_selected(&mut self) {
        let Some(file) = self.selected_file() else {
            return;
        };
        let path = file.path.clone();
        for i in 0..path.len() {
            if path.as_bytes()[i] == b'/' {
                self.set_collapsed(&path[..i], false);
            }
        }
    }

    /// Right on the file list: open the fold at the cursor.
    ///
    /// - Hidden anchor (just collapsed, or a finder jump): expand its own
    ///   collapsed ancestors, like `expand_selected`.
    /// - Visible file: expand the folded region immediately below the
    ///   cursor, else the one immediately above. This is the way back in
    ///   after Up/Down skipped away from a `▶` header, since the highlight
    ///   never rests on hidden files while navigating.
    fn expand_at_cursor(&mut self) {
        if self.cursor_dir().is_some() {
            // Already open; don't unfold a neighbouring folder instead.
            return;
        }
        let Some(file) = self.selected_file().cloned() else {
            return;
        };
        if self.is_hidden_path(&file.path) {
            self.expand_selected();
            return;
        }
        if self.selected + 1 < self.file_count() && self.is_hidden_index(self.selected + 1) {
            let path = self.file_list[self.selected + 1].path.clone();
            if let Some(dir) = self.collapsed_dir_for(&path) {
                self.set_collapsed(&dir, false);
                return;
            }
        }
        if self.selected > 0 && self.is_hidden_index(self.selected - 1) {
            let path = self.file_list[self.selected - 1].path.clone();
            if let Some(dir) = self.collapsed_dir_for(&path) {
                self.set_collapsed(&dir, false);
            }
        }
    }

    fn hunk_count(&self) -> usize {
        self.diff.as_ref().map(|d| d.hunks.len()).unwrap_or(0)
    }

    fn select_hunk(&mut self, index: usize) {
        let clamped = index.min(self.hunk_count().saturating_sub(1));
        self.hunk = clamped;
        self.cursor = self.hunk_start_row(clamped) as usize;
        self.cursor = self.cursor.min(self.diff_rows.len().saturating_sub(1));
        // Snap the selected hunk to the top of the view.
        self.diff_scroll = self.hunk_start_row(clamped);
    }

    /// Move the line cursor by `delta` rows, clamped to the loaded rows.
    /// The hunk follows the cursor (nearest header at or above it) so
    /// `space` stages the hunk under the cursor; the view follows at the
    /// edge only (`ensure_cursor_visible`), nvim-style. No-op with no
    /// diff or in Markdown preview (which owns plain scrolling instead).
    fn move_cursor(&mut self, delta: isize) {
        if self.diff_rows.is_empty() || self.show_markdown_preview() {
            return;
        }
        let max = self.diff_rows.len().saturating_sub(1) as isize;
        let cur = (self.cursor as isize).clamp(0, max);
        self.cursor = (cur + delta).clamp(0, max) as usize;
        self.sync_hunk_to_cursor();
        self.ensure_cursor_visible();
    }

    /// Nvim-style viewport follow: the cursor walks freely inside the
    /// visible window and the view scrolls only once it would leave it
    /// (top edge pins, bottom edge advances minimally). Fullscreen counts
    /// side-by-side rows, the preview unified lines (a del/add pair keeps
    /// both of its lines visible). Unknown height (no frame rendered yet)
    /// reads as 1, i.e. top-pin, so the cursor is never stranded.
    fn ensure_cursor_visible(&mut self) {
        if self.show_markdown_preview() || self.diff_rows.is_empty() {
            return;
        }
        if self.mode == Mode::FullDiff {
            let h = self.full_view_h.get().max(1);
            let scroll = self.diff_scroll as usize;
            if self.cursor < scroll {
                self.diff_scroll = self.cursor.min(u16::MAX as usize) as u16;
            } else if self.cursor + 1 > scroll + h {
                self.diff_scroll = (self.cursor + 1 - h).min(u16::MAX as usize) as u16;
            }
        } else {
            let h = self.prev_view_h.get().max(1);
            let c = self.cursor_row();
            let start = crate::ui::rows_unified_len(&self.diff_rows[..c]);
            let end = start + crate::ui::unified_row_count(&self.diff_rows[c]);
            let scroll = self.diff_scroll as usize;
            if start < scroll {
                self.diff_scroll = start.min(u16::MAX as usize) as u16;
            } else if end > scroll + h {
                self.diff_scroll = end.saturating_sub(h).min(u16::MAX as usize) as u16;
            }
        }
    }

    /// Hunk under the cursor: nearest hunk header at or above it.
    fn sync_hunk_to_cursor(&mut self) {
        let mut hunk = 0;
        for (i, row) in self.diff_rows.iter().enumerate() {
            if i > self.cursor {
                break;
            }
            if let DiffRow::Header { index } = row {
                hunk = *index;
            }
        }
        self.hunk = hunk.min(self.hunk_count().saturating_sub(1));
    }

    /// Keep the cursor visible by top-pinning the view on its row:
    /// side-by-side rows fullscreen, unified lines in the preview.
    fn snap_scroll_to_cursor(&mut self) {
        if self.show_markdown_preview() {
            return;
        }
        if self.mode == Mode::FullDiff {
            self.diff_scroll = (self.cursor as u16).min(self.diff_full_max());
        } else {
            let off = crate::ui::rows_unified_len(
                &self.diff_rows[..self.cursor.min(self.diff_rows.len())],
            );
            self.diff_scroll = (off as u16).min(self.diff_preview_max());
        }
    }

    /// Line-cursor step, or plain scroll when the Markdown preview owns
    /// the view (it has no diff rows to point at).
    fn move_cursor_or_scroll(&mut self, delta: isize) {
        if self.show_markdown_preview() {
            self.scroll_diff_by(delta);
        } else {
            self.move_cursor(delta);
        }
    }

    /// Nvim-style `h`/`l`: move the block column within the cursor row's
    /// text, clamped to its last char (like `$`). Row moves never touch
    /// it, so it sticks across short/long lines like vim's column.
    /// No-op with no diff or in Markdown preview.
    fn move_column(&mut self, delta: isize) {
        if self.show_markdown_preview() {
            return;
        }
        let max = self.cursor_text_len().saturating_sub(1) as isize;
        // No text under the block (blank side): nowhere to move.
        if self.cursor_text_len() == 0 {
            return;
        }
        let cur = (self.cursor_col as isize).clamp(0, max);
        self.cursor_col = (cur + delta).clamp(0, max) as usize;
    }

    /// Char count of the text under the block cursor (0 with no diff).
    fn cursor_text_len(&self) -> usize {
        match (self.diff_rows.get(self.cursor_row()), self.diff.as_ref()) {
            (Some(row), Some(diff)) => cursor_line_text(diff, row).chars().count(),
            _ => 0,
        }
    }

    /// Nvim-style `0`/`Home`: block to the line start.
    fn column_home(&mut self) {
        if self.show_markdown_preview() {
            return;
        }
        if self.diff_rows.get(self.cursor_row()).is_some() {
            self.cursor_col = 0;
        }
    }

    /// Nvim-style `End`: block to the line's last char.
    fn column_end(&mut self) {
        if self.show_markdown_preview() {
            return;
        }
        self.cursor_col = self.cursor_text_len().saturating_sub(1);
    }

    /// Block-column step, or plain scroll when the Markdown preview owns
    /// the view.
    fn move_column_or_scroll(&mut self, delta: isize) {
        if self.show_markdown_preview() {
            self.scroll_diff_by(delta);
        } else {
            self.move_column(delta);
        }
    }

    /// `v`/`V`: enter visual mode anchoring at the cursor (charwise /
    /// linewise). Same key again leaves it; the other key flips the mode
    /// keeping the anchor, like nvim. Motions extend the selection since
    /// its live end is the line cursor.
    fn begin_visual(&mut self, mode: VisualMode) {
        if self.diff_rows.is_empty() || self.show_markdown_preview() {
            return;
        }
        match self.visual {
            Some(v) if v.mode == mode => self.visual = None,
            Some(mut v) => {
                v.mode = mode;
                self.visual = Some(v);
            }
            None => {
                self.visual = Some(Visual {
                    anchor_row: self.cursor_row(),
                    anchor_col: self.cursor_col,
                    mode,
                })
            }
        }
    }

    /// Char slice `[from..=to]` without cutting UTF-8 boundaries.
    fn slice_chars(text: &str, from: usize, to_incl: usize) -> String {
        if from > to_incl {
            return String::new();
        }
        text.chars().skip(from).take(to_incl - from + 1).collect()
    }

    /// Text the next yank would take: the selection (linewise whole rows
    /// with trailing newlines, charwise endpoint slices joined by `\n`),
    /// or the cursor line when visual mode is off. `None` when empty.
    fn yank_text(&self) -> Option<String> {
        let diff = self.diff.as_ref()?;
        if self.diff_rows.is_empty() {
            return None;
        }
        let text = match self.visual_selection() {
            None => {
                let line = cursor_line_text(diff, &self.diff_rows[self.cursor_row()]);
                if line.is_empty() {
                    return None;
                }
                format!("{line}\n")
            }
            Some(((r1, c1), (r2, c2), linewise)) => {
                if linewise {
                    let mut s = String::new();
                    for r in r1..=r2 {
                        s.push_str(&cursor_line_text(diff, &self.diff_rows[r]));
                        s.push('\n');
                    }
                    s
                } else if r1 == r2 {
                    Self::slice_chars(&cursor_line_text(diff, &self.diff_rows[r1]), c1, c2)
                } else {
                    let mut s = String::new();
                    let first = cursor_line_text(diff, &self.diff_rows[r1]);
                    s.push_str(&first.chars().skip(c1).collect::<String>());
                    for r in r1 + 1..r2 {
                        s.push('\n');
                        s.push_str(&cursor_line_text(diff, &self.diff_rows[r]));
                    }
                    s.push('\n');
                    s.push_str(&Self::slice_chars(
                        &cursor_line_text(diff, &self.diff_rows[r2]),
                        0,
                        c2,
                    ));
                    s
                }
            }
        };
        if text.is_empty() {
            None
        } else {
            Some(text)
        }
    }

    /// `y`: yank the selection (or the cursor line) to the system
    /// clipboard via OSC 52 and leave visual mode. Empty selections
    /// report instead of clobbering the clipboard.
    fn begin_yank(&mut self) {
        let Some(text) = self.yank_text() else {
            self.error = Some("nothing to yank".into());
            return;
        };
        self.visual = None;
        if text.ends_with('\n') {
            let lines = text.lines().count();
            self.notice = Some(format!(
                "yanked {} line{}",
                lines,
                if lines == 1 { "" } else { "s" }
            ));
        } else {
            self.notice = Some(format!("yanked {} chars", text.chars().count()));
        }
        if !yank_to_clipboard(&text) {
            self.error = Some("yanked, but the terminal refused the clipboard (OSC 52)".into());
        }
    }

    /// Rendered row offset where hunk `index` starts (its header row in the
    /// side-by-side rows cached from the loaded diff).
    fn hunk_start_row(&self, index: usize) -> u16 {
        crate::ui::rows_hunk_start(&self.diff_rows, index)
    }
    /// Last valid preview scroll offset (unified lines), so scrolling the
    /// [5] tab stops on the last line instead of running into blank space.
    fn diff_preview_max(&self) -> u16 {
        crate::ui::rows_unified_len(&self.diff_rows)
            .saturating_sub(1)
            .min(u16::MAX as usize) as u16
    }

    /// Last valid fullscreen offset (side-by-side rows).
    fn diff_full_max(&self) -> u16 {
        self.diff_rows
            .len()
            .saturating_sub(1)
            .min(u16::MAX as usize) as u16
    }

    /// Scroll the diff by `delta` lines, clamped to the content of the
    /// currently shown view (unified preview in Normal, side-by-side rows
    /// in FullDiff). Markdown preview lines depend on the render width,
    /// so they use a generous cap and the renderer clamps the slice.
    fn scroll_diff_by(&mut self, delta: isize) {
        let max = if self.show_markdown_preview() {
            self.md_text
                .as_ref()
                .map(|t| (t.lines().count() * 4 + 16).min(u16::MAX as usize) as u16)
                .unwrap_or(u16::MAX)
        } else if self.mode == Mode::FullDiff {
            self.diff_full_max()
        } else {
            self.diff_preview_max()
        };
        let cur = self.diff_scroll as isize;
        self.diff_scroll = (cur + delta).clamp(0, max as isize) as u16;
    }

    /// Scroll the log by `delta` lines, clamped to the loaded entries.
    fn scroll_log_by(&mut self, delta: isize) {
        let max = self
            .log
            .as_ref()
            .map(|l| l.len().saturating_sub(1))
            .unwrap_or(0)
            .min(u16::MAX as usize) as isize;
        let cur = self.log_scroll as isize;
        self.log_scroll = (cur + delta).clamp(0, max) as u16;
    }

    /// Space: stage unless already fully staged (then unstage). When the
    /// highlight sits on a directory header, open (`▼`, `dir_cursor`) or
    /// collapsed (the selected file is hidden inside it), the whole
    /// directory is staged/unstaged so its files are ready to commit
    /// together.
    fn toggle_stage(&mut self) {
        if let Some(dir) = self.cursor_dir().map(str::to_string) {
            self.toggle_stage_dir(&dir);
            return;
        }
        let Some(file) = self.selected_file().cloned() else {
            return;
        };
        if let Some(dir) = self.collapsed_dir_for(&file.path) {
            self.toggle_stage_dir(&dir);
        } else {
            self.toggle_stage_file(&file);
        }
    }

    /// Shallowest collapsed ancestor of `path` (the header the UI
    /// highlights when this file is hidden), or `None` when visible.
    /// Mirrors the highlight fallback in `ui.rs`.
    fn collapsed_dir_for(&self, path: &str) -> Option<String> {
        ancestors_of(path)
            .into_iter()
            .find(|dir| self.collapsed.contains(dir.as_str()))
    }

    /// Space on one visible file: stage unless already fully staged.
    fn toggle_stage_file(&mut self, file: &StatusEntry) {
        if file.state == FileState::Conflicted {
            self.error = Some(format!(
                "conflicted: resolve markers in {} first",
                file.path
            ));
            return;
        }
        if file.state == FileState::Clean {
            self.error = Some(format!("{} is unchanged — nothing to stage", file.path));
            return;
        }
        let job = if file.state == FileState::Staged {
            AsyncJob::UnstageFile {
                path: file.path.clone(),
            }
        } else {
            AsyncJob::StageFile {
                path: file.path.clone(),
            }
        };
        if let Err(e) = self.queue.submit(job) {
            self.error = Some(e.to_string());
        }
    }

    /// Space on a directory header: stage every stageable file
    /// beneath it, or unstage them all when every one is already staged.
    /// Conflicted files abort the whole directory like the single-file
    /// case; clean files are skipped silently.
    fn toggle_stage_dir(&mut self, dir: &str) {
        let prefix = format!("{dir}/");
        let under: Vec<(String, FileState)> = self
            .file_list
            .iter()
            .filter(|e| e.path.starts_with(&prefix))
            .map(|e| (e.path.clone(), e.state))
            .collect();
        if let Some((path, _)) = under.iter().find(|(_, s)| *s == FileState::Conflicted) {
            self.error = Some(format!("conflicted: resolve markers in {path} first"));
            return;
        }
        let to_stage: Vec<String> = under
            .iter()
            .filter(|(_, s)| !matches!(s, FileState::Clean | FileState::Staged))
            .map(|(p, _)| p.clone())
            .collect();
        if !to_stage.is_empty() {
            for path in to_stage {
                if let Err(e) = self.queue.submit(AsyncJob::StageFile { path }) {
                    self.error = Some(e.to_string());
                    return;
                }
            }
            return;
        }
        let staged: Vec<String> = under
            .iter()
            .filter(|(_, s)| *s == FileState::Staged)
            .map(|(p, _)| p.clone())
            .collect();
        if !staged.is_empty() {
            for path in staged {
                if let Err(e) = self.queue.submit(AsyncJob::UnstageFile { path }) {
                    self.error = Some(e.to_string());
                    return;
                }
            }
            return;
        }
        self.error = Some(format!("nothing to stage under {dir}/"));
    }

    /// `d`: discard all changes in the selected file (staged and unstaged),
    /// restoring it to HEAD; untracked files are deleted. When the
    /// highlight sits on a directory header, open (`▼`, `dir_cursor`) or
    /// collapsed (the selected file is hidden inside it), every changed
    /// file beneath it is discarded so the folder is clean again.
    fn discard_selected(&mut self) {
        if let Some(dir) = self.cursor_dir().map(str::to_string) {
            self.discard_dir(&dir);
            return;
        }
        let Some(file) = self.selected_file().cloned() else {
            return;
        };
        if let Some(dir) = self.collapsed_dir_for(&file.path) {
            self.discard_dir(&dir);
        } else {
            self.discard_file_entry(&file);
        }
    }

    /// `d` on one visible file: discard its changes (or delete it when
    /// untracked / staged-new).
    fn discard_file_entry(&mut self, file: &StatusEntry) {
        if file.state == FileState::Conflicted {
            self.error = Some(format!(
                "conflicted: resolve markers in {} first",
                file.path
            ));
            return;
        }
        if file.state == FileState::Clean {
            self.error = Some(format!("{} is unchanged — nothing to discard", file.path));
            return;
        }
        if let Err(e) = self.queue.submit(AsyncJob::DiscardFile {
            path: file.path.clone(),
        }) {
            self.error = Some(e.to_string());
        }
    }

    /// `d` on a directory header: discard every changed file beneath it.
    /// Conflicted files abort the whole directory like the single-file
    /// case; clean files are skipped silently.
    fn discard_dir(&mut self, dir: &str) {
        let prefix = format!("{dir}/");
        let under: Vec<(String, FileState)> = self
            .file_list
            .iter()
            .filter(|e| e.path.starts_with(&prefix))
            .map(|e| (e.path.clone(), e.state))
            .collect();
        if let Some((path, _)) = under.iter().find(|(_, s)| *s == FileState::Conflicted) {
            self.error = Some(format!("conflicted: resolve markers in {path} first"));
            return;
        }
        let dirty: Vec<String> = under
            .iter()
            .filter(|(_, s)| !matches!(s, FileState::Clean))
            .map(|(p, _)| p.clone())
            .collect();
        if dirty.is_empty() {
            self.error = Some(format!("nothing to discard under {dir}/"));
            return;
        }
        for path in dirty {
            if let Err(e) = self.queue.submit(AsyncJob::DiscardFile { path }) {
                self.error = Some(e.to_string());
                return;
            }
        }
    }

    /// `d` in the fullscreen diff: discard the loaded file's changes.
    fn discard_loaded_file(&mut self) {
        let Some((path, _)) = self.diff_for.clone() else {
            self.error = Some("no diff loaded".into());
            return;
        };
        // The file list knows the state (clean vs changed), including
        // mode-only changes shown via the whole-file fallback.
        if let Some(file) = self.file_list.iter().find(|f| f.path == path).cloned() {
            self.discard_file_entry(&file);
        } else if let Err(e) = self.queue.submit(AsyncJob::DiscardFile { path }) {
            self.error = Some(e.to_string());
        }
    }

    fn submit_commit(&mut self) {
        if self.generating {
            self.notice = Some("still generating a message…".into());
            return;
        }
        if self.draft.trim().is_empty() {
            self.notice = Some("type a commit message first".into());
            return;
        }
        let job = AsyncJob::Commit {
            message: self.draft.clone(),
        };
        match self.queue.submit(job) {
            Ok(()) => {
                self.mode = Mode::Normal;
                self.draft.clear();
            }
            Err(e) => self.error = Some(e.to_string()),
        }
    }

    /// Stage the selected hunk of the loaded (unstaged) diff.
    fn stage_selected_hunk(&mut self) {
        let Some((path, staged)) = self.diff_for.clone() else {
            self.error = Some("no diff loaded".into());
            return;
        };
        if staged {
            self.error = Some(
                "hunk is already staged — switch to status (1) and space toggles the file".into(),
            );
            return;
        }
        let Some(diff) = self.diff.as_ref() else {
            self.error = Some("no diff loaded".into());
            return;
        };
        // Whole-file views (clean files) have no changes to stage.
        let has_changes = diff.hunks.get(self.hunk).is_some_and(|h| {
            h.lines.iter().any(|l| {
                l.kind == git_tui_core::diff::LineKind::Add
                    || l.kind == git_tui_core::diff::LineKind::Del
            })
        });
        if !has_changes {
            self.error = Some("nothing to stage in this hunk".into());
            return;
        }
        let job = AsyncJob::StageHunk {
            path,
            hunk_index: self.hunk,
        };
        if let Err(e) = self.queue.submit(job) {
            self.error = Some(e.to_string());
        }
    }

    /// Which diff belongs to the selected file: fully staged files show the
    /// staged diff, clean files show the whole file, everything else the
    /// unstaged one.
    fn diff_target(&self) -> Option<(String, bool)> {
        let file = self.selected_file()?;
        Some((file.path.clone(), file.state == FileState::Staged))
    }

    /// Whether the selected file needs a whole-file view (clean).
    fn selected_is_clean(&self) -> bool {
        self.selected_file()
            .is_some_and(|f| f.state == FileState::Clean)
    }
    /// Request the selected file's diff unless it is already loaded/loading.
    /// Automatic: files with no changes load the whole workdir file (their
    /// diff would be empty); changed files load the staged/unstaged diff.
    /// A listed file whose diff arrives empty (e.g. a mode-only change)
    /// falls back to the whole file instead of showing "(no changes)".
    fn maybe_load_diff(&mut self) {
        let target = self.diff_target();
        // A new path drops the empty-diff fallback; it applies per file.
        let path_changed =
            target.as_ref().map(|(p, _)| p) != self.diff_for.as_ref().map(|(p, _)| p);
        if path_changed {
            self.fallback_whole_file = false;
        }
        let whole = self.selected_is_clean() || self.fallback_whole_file;
        if target == self.diff_for && whole == self.diff_whole_file {
            return;
        }
        self.diff_for = target.clone();
        // Stale view: show loading until the fresh diff arrives.
        self.set_diff(None);
        self.hunk = 0;
        self.cursor = 0;
        self.cursor_col = 0;
        self.diff_scroll = 0;
        self.diff_whole_file = whole;
        if let Some((path, staged)) = target {
            let job = if whole {
                AsyncJob::LoadFile { path }
            } else {
                AsyncJob::LoadDiff { path, staged }
            };
            if let Err(e) = self.queue.submit(job) {
                self.error = Some(e.to_string());
            }
        }
    }

    /// Reload the [5] diff preview from the *fresh* status after a
    /// mutation (stash push/pop, stage, commit, …). It recomputes the
    /// target (path, staged flag, whole-file vs diff) instead of
    /// re-requesting a stale `diff_for`, so a stash that cleans the tree
    /// shows the whole file and a pop shows the restored hunks — no flash
    /// of wrong content, no wasted worker roundtrip.
    fn force_reload_diff_for_fresh_status(&mut self) {
        let target = self.diff_target();
        let path_changed =
            target.as_ref().map(|(p, _)| p) != self.diff_for.as_ref().map(|(p, _)| p);
        if path_changed {
            self.fallback_whole_file = false;
        }
        let whole = self.selected_is_clean() || self.fallback_whole_file;
        self.diff_for = target.clone();
        // Stale view: show loading until the fresh diff arrives.
        self.set_diff(None);
        self.hunk = 0;
        self.cursor = 0;
        self.cursor_col = 0;
        self.diff_scroll = 0;
        self.diff_whole_file = whole;
        if let Some((path, staged)) = target {
            let job = if whole {
                AsyncJob::LoadFile { path }
            } else {
                AsyncJob::LoadDiff { path, staged }
            };
            if let Err(e) = self.queue.submit(job) {
                self.error = Some(e.to_string());
            }
        }
        // Markdown follows the same fresh target.
        self.md_for = None;
        self.md_text = None;
        self.maybe_load_markdown();
    }

    fn branch_count(&self) -> usize {
        self.branches.as_ref().map(|b| b.len()).unwrap_or(0)
    }

    fn selected_branch(&self) -> Option<BranchInfo> {
        self.branches
            .as_ref()?
            .get(
                self.branch_selected
                    .min(self.branch_count().saturating_sub(1)),
            )
            .cloned()
    }

    /// Request the branch list unless already loaded/loading.
    fn maybe_load_branches(&mut self) {
        if self.focus == Focus::Branches && self.branches.is_none() {
            if let Err(e) = self.queue.submit(AsyncJob::ListBranches) {
                self.error = Some(e.to_string());
            }
        }
    }

    fn reload_branches(&mut self) {
        if self.branches.is_some() {
            // Forget the snapshot so the fresh list replaces it wholesale.
            self.branches = None;
            if let Err(e) = self.queue.submit(AsyncJob::ListBranches) {
                self.error = Some(e.to_string());
            }
        }
    }

    /// History depth for the log panel.
    const LOG_LIMIT: usize = 100;

    /// Request history unless already loaded/loading.
    fn maybe_load_log(&mut self) {
        if self.focus == Focus::Log && self.log.is_none() {
            if let Err(e) = self.queue.submit(AsyncJob::ListLog {
                limit: Self::LOG_LIMIT,
            }) {
                self.error = Some(e.to_string());
            }
        }
    }

    fn reload_log(&mut self) {
        if self.log.is_some() {
            self.log = None;
            if let Err(e) = self.queue.submit(AsyncJob::ListLog {
                limit: Self::LOG_LIMIT,
            }) {
                self.error = Some(e.to_string());
            }
        }
    }

    fn submit_new_branch(&mut self) {
        if self.draft.trim().is_empty() {
            return;
        }
        let job = AsyncJob::CreateBranch {
            name: self.draft.clone(),
        };
        match self.queue.submit(job) {
            Ok(()) => {
                self.mode = Mode::Normal;
                self.draft.clear();
            }
            Err(e) => self.error = Some(e.to_string()),
        }
    }

    fn checkout_selected_branch(&mut self) {
        let Some(branch) = self.selected_branch() else {
            return;
        };
        if let Err(e) = self
            .queue
            .submit(AsyncJob::CheckoutBranch { name: branch.name })
        {
            self.error = Some(e.to_string());
        }
    }

    fn delete_selected_branch(&mut self) {
        let Some(branch) = self.selected_branch() else {
            return;
        };
        if let Err(e) = self
            .queue
            .submit(AsyncJob::DeleteBranch { name: branch.name })
        {
            self.error = Some(e.to_string());
        }
    }

    fn stash_count(&self) -> usize {
        self.stash.as_ref().map(|s| s.len()).unwrap_or(0)
    }

    fn selected_stash(&self) -> Option<StashEntry> {
        self.stash
            .as_ref()?
            .get(
                self.stash_selected
                    .min(self.stash_count().saturating_sub(1)),
            )
            .cloned()
    }

    /// Request the stash list unless already loaded/loading.
    fn maybe_load_stash(&mut self) {
        if self.focus == Focus::Stash && self.stash.is_none() {
            if let Err(e) = self.queue.submit(AsyncJob::ListStash) {
                self.error = Some(e.to_string());
            }
        }
    }

    fn reload_stash(&mut self) {
        if self.stash.is_some() {
            self.stash = None;
            if let Err(e) = self.queue.submit(AsyncJob::ListStash) {
                self.error = Some(e.to_string());
            }
        }
    }

    /// `/`: open the fuzzy file finder on the browsable tree. Works
    /// from `Normal` and from `FullDiff`; Enter/Esc return there.
    fn open_finder(&mut self) {
        self.finder_return = self.mode;
        self.mode = Mode::FindFile;
        self.draft.clear();
        self.finder_selected = self.selected.min(self.file_count().saturating_sub(1));
    }

    fn finder_move(&mut self, delta: isize) {
        let n = self.finder_matches().len();
        if n == 0 {
            self.finder_selected = 0;
            return;
        }
        let cur = self.finder_selected.min(n - 1) as isize;
        self.finder_selected = (cur + delta).clamp(0, n as isize - 1) as usize;
    }

    /// Enter in the finder: jump the file cursor to the chosen match,
    /// expanding any collapsed ancestors so the match is actually visible,
    /// and return where the finder was opened from (staying fullscreen
    /// when opened fullscreen, with the new file's diff loading).
    fn submit_finder(&mut self) {
        let matches = self.finder_matches();
        let Some(&index) = matches.get(self.finder_cursor()) else {
            return;
        };
        self.selected = index;
        self.dir_cursor = None;
        self.expand_selected();
        self.focus = Focus::Status;
        self.mode = self.finder_return;
        self.draft.clear();
        self.finder_selected = 0;
        self.maybe_load_diff();
    }

    fn submit_stash_push(&mut self) {
        if self.draft.trim().is_empty() {
            return;
        }
        let job = AsyncJob::StashPush {
            message: self.draft.clone(),
        };
        match self.queue.submit(job) {
            Ok(()) => {
                self.mode = Mode::Normal;
                self.draft.clear();
            }
            Err(e) => self.error = Some(e.to_string()),
        }
    }

    fn pop_selected_stash(&mut self) {
        let Some(entry) = self.selected_stash() else {
            return;
        };
        if let Err(e) = self.queue.submit(AsyncJob::StashPop { index: entry.index }) {
            self.error = Some(e.to_string());
        }
    }

    fn drop_selected_stash(&mut self) {
        let Some(entry) = self.selected_stash() else {
            return;
        };
        if let Err(e) = self
            .queue
            .submit(AsyncJob::StashDrop { index: entry.index })
        {
            self.error = Some(e.to_string());
        }
    }

    /// `p`, lazygit-style: pull the current branch (`git pull`, honoring
    /// the user's pull.rebase/pull.ff config). Runs on the worker thread;
    /// the status panel shows "pulling…" until it lands or fails.
    fn start_pull(&mut self) {
        self.syncing = Some("pulling…".into());
        if let Err(e) = self.queue.submit(AsyncJob::Pull) {
            self.syncing = None;
            self.error = Some(e.to_string());
        }
    }

    /// `P`, lazygit-style: push the current branch. With an upstream it
    /// pushes straight away; without one it asks for the remote (`-u`);
    /// with no remotes at all it asks for the `origin` URL (publish).
    fn start_push(&mut self) {
        let Some(branch) = self.status.as_ref().map(|st| st.branch.clone()) else {
            self.error = Some("still loading — try again in a moment".into());
            return;
        };
        let Some(sync) = self.sync.clone() else {
            self.reload_sync();
            self.error = Some("loading remotes — press P again in a moment".into());
            return;
        };
        self.sync_return = self.mode;
        if sync.remotes.is_empty() {
            self.mode = Mode::SetRemote;
            self.draft.clear();
            self.draft_cursor = 0;
        } else if let Some(upstream) = sync.upstream {
            let (remote, _) = split_upstream(&upstream);
            self.syncing = Some("pushing…".into());
            if let Err(e) = self.queue.submit(AsyncJob::Push {
                remote: remote.to_string(),
                branch,
                set_upstream: false,
            }) {
                self.syncing = None;
                self.error = Some(e.to_string());
            }
        } else {
            self.mode = Mode::SetUpstream;
            self.draft = "origin".to_string();
            self.move_draft_end();
        }
    }

    /// Enter in `SetUpstream`: push `-u <remote> <branch>`.
    fn submit_push_upstream(&mut self) {
        let remote = self.draft.trim().to_string();
        if remote.is_empty() {
            return;
        }
        let Some(branch) = self.status.as_ref().map(|st| st.branch.clone()) else {
            self.error = Some("still loading — try again in a moment".into());
            return;
        };
        match self.queue.submit(AsyncJob::Push {
            remote,
            branch,
            set_upstream: true,
        }) {
            Ok(()) => {
                self.mode = self.sync_return;
                self.draft.clear();
                self.draft_cursor = 0;
                self.syncing = Some("pushing…".into());
            }
            Err(e) => self.error = Some(e.to_string()),
        }
    }

    /// Enter in `SetRemote`: point `origin` at the typed URL and push `-u`.
    /// The "not yet on GitHub" flow: create the empty repo on the host,
    /// paste its URL here, and the branch is published.
    fn submit_publish(&mut self) {
        let url = self.draft.trim().to_string();
        if url.is_empty() {
            return;
        }
        let Some(branch) = self.status.as_ref().map(|st| st.branch.clone()) else {
            self.error = Some("still loading — try again in a moment".into());
            return;
        };
        match self.queue.submit(AsyncJob::Publish {
            remote: "origin".to_string(),
            url,
            branch,
        }) {
            Ok(()) => {
                self.mode = self.sync_return;
                self.draft.clear();
                self.draft_cursor = 0;
                self.syncing = Some("publishing…".into());
            }
            Err(e) => self.error = Some(e.to_string()),
        }
    }

    /// Refresh upstream tracking state (cheap local reads, no network).
    fn reload_sync(&mut self) {
        if let Err(e) = self.queue.submit(AsyncJob::LoadSync) {
            self.error = Some(e.to_string());
        }
    }

    /// Drain finished jobs without blocking (event loop calls this every
    /// frame). A [`AsyncResult::MutationDone`] invalidates the snapshot, so
    /// it refreshes status and reloads the focused diff rather than patching
    /// either incrementally.
    pub fn poll(&mut self) {
        while let Some(result) = self.queue.try_recv() {
            self.apply(result);
        }
        // After a mutation the [5] preview waits for the fresh status
        // (see `pending_diff_reload`): loading the old target now would
        // flash stale content and waste a worker roundtrip.
        if !self.pending_diff_reload {
            self.maybe_load_diff();
            self.maybe_load_markdown();
        }
    }

    fn apply(&mut self, result: AsyncResult) {
        match result {
            AsyncResult::Status(st) => {
                self.status = Some(st);
                self.rebuild_file_list();
                self.error = None;
                if self.pending_diff_reload {
                    self.pending_diff_reload = false;
                    self.force_reload_diff_for_fresh_status();
                }
            }
            AsyncResult::Diff(d) => {
                // Drop overtaken loads: only the latest target counts
                // (jobs run FIFO, so a newer LoadDiff may follow).
                if self.diff_for.as_ref().is_some_and(|(p, _)| *p == d.path) {
                    if d.hunks.is_empty() && !d.binary && !self.diff_whole_file {
                        // Listed but no content diff (e.g. mode-only
                        // change): show the whole file automatically.
                        // Binary files skip this: their bytes are never
                        // drawn, the diff pane shows a notice instead.
                        self.fallback_whole_file = true;
                        self.set_diff(None);
                        self.hunk = 0;
                        self.diff_scroll = 0;
                        self.diff_whole_file = true;
                        let job = AsyncJob::LoadFile { path: d.path };
                        if let Err(e) = self.queue.submit(job) {
                            self.error = Some(e.to_string());
                        }
                    } else {
                        self.set_diff(Some(d));
                        self.sync_hunk_to_cursor();
                        if self.show_markdown_preview() {
                            // The fresh content may be shorter: keep the
                            // offset inside the new content.
                            self.diff_scroll = self.diff_scroll.min(self.diff_preview_max());
                        } else {
                            self.snap_scroll_to_cursor();
                        }
                    }
                }
            }
            AsyncResult::Markdown { path, staged, text } => {
                if self
                    .md_for
                    .as_ref()
                    .is_some_and(|(p, s)| *p == path && *s == staged)
                {
                    self.md_text = Some(text);
                    self.diff_scroll = 0;
                }
            }
            AsyncResult::Branches(b) => {
                self.branch_selected = self.branch_selected.min(b.len().saturating_sub(1));
                self.branches = Some(b);
            }
            AsyncResult::Log(entries) => {
                self.log_scroll = self.log_scroll.min(entries.len().saturating_sub(1) as u16);
                self.log = Some(entries);
            }
            AsyncResult::Stash(entries) => {
                self.stash_selected = self.stash_selected.min(entries.len().saturating_sub(1));
                self.stash = Some(entries);
            }
            AsyncResult::SyncStatus(st) => {
                self.sync = Some(st);
            }
            AsyncResult::GeneratedMessage(msg) => {
                self.generating = false;
                // The user may have Esc'd while the network call was in
                // flight: only fill the open commit box.
                if self.mode == Mode::Committing {
                    self.draft = msg;
                    self.move_draft_end();
                    self.error = None;
                }
            }
            AsyncResult::MutationDone => {
                self.syncing = None;
                self.refresh();
                // Defer the [5] preview (+ Markdown) until the fresh
                // status lands: stash push/pop changes the target itself
                // (clean <-> dirty, whole-file <-> hunks). Show loading
                // now so stale hunks never linger.
                self.pending_diff_reload = true;
                self.set_diff(None);
                self.hunk = 0;
                self.cursor = 0;
                self.cursor_col = 0;
                self.diff_scroll = 0;
                self.md_text = None;
                self.reload_branches();
                self.reload_log();
                self.reload_stash();
                self.reload_sync();
            }
            AsyncResult::Error(e) => {
                self.syncing = None;
                self.generating = false;
                // A failed mutation leaves status unchanged: drop the
                // deferred reload so the preview recovers via the normal
                // `maybe_load_diff` path on the next poll.
                self.pending_diff_reload = false;
                // EmptyCommit on its own doesn't say how to fix it.
                self.error = Some(match e {
                    GitError::EmptyCommit => {
                        "nothing to commit: press space on a file to stage it, then c to commit"
                            .into()
                    }
                    _ => e.to_string(),
                });
            }
        }
    }
}

/// Push `text` to the system clipboard with an OSC 52 escape. Terminal-
/// native (kitty, foot, alacritty, wezterm, tmux passthrough, SSH), no
/// windowing libraries needed. Returns whether the write succeeded.
fn yank_to_clipboard(text: &str) -> bool {
    use std::io::Write;
    let encoded = base64_encode(text.as_bytes());
    let mut out = std::io::stdout().lock();
    write!(out, "\x1b]52;c;{encoded}\x07")
        .and_then(|_| out.flush())
        .is_ok()
}

/// Minimal standard base64 (no external crate): 3 bytes -> 4 chars.
fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((n >> 18) & 63) as usize] as char);
        out.push(TABLE[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            TABLE[((n >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    struct Fixture {
        _dir: tempfile::TempDir,
        app: App,
    }

    fn init_repo() -> (tempfile::TempDir, git2::Repository) {
        let dir = tempfile::TempDir::new().unwrap();
        let repo = git2::Repository::init(dir.path()).unwrap();
        repo.set_head("refs/heads/main").unwrap();
        let mut cfg = repo.config().unwrap();
        cfg.set_str("user.name", "Test User").unwrap();
        cfg.set_str("user.email", "test@example.com").unwrap();
        (dir, repo)
    }

    fn commit_file(repo: &git2::Repository, path: &str, contents: &str, msg: &str) {
        let full = repo.workdir().unwrap().join(path);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&full, contents).unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(std::path::Path::new(path)).unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let sig = repo.signature().unwrap();
        let parents: Vec<git2::Commit> = match repo.head() {
            Ok(h) => vec![h.peel_to_commit().unwrap()],
            Err(_) => vec![],
        };
        let refs: Vec<&git2::Commit> = parents.iter().collect();
        repo.commit(Some("HEAD"), &sig, &sig, msg, &tree, &refs)
            .unwrap();
    }

    /// Settle until `pred` holds on the latest status (drives `poll`).
    fn wait_for(app: &mut App, mut pred: impl FnMut(&RepoStatus) -> bool) -> RepoStatus {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            app.poll();
            if let Some(st) = app.status() {
                if pred(st) {
                    return st.clone();
                }
            }
            assert!(
                Instant::now() < deadline,
                "timed out; last: {:?}",
                app.status()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn wait_for_error(app: &mut App) -> String {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            app.poll();
            if let Some(e) = app.error() {
                return e.to_string();
            }
            assert!(Instant::now() < deadline, "timed out waiting for error");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Repo with each named file committed then dirtied (all Unstaged).
    fn harness(names: &[&str]) -> Fixture {
        let (dir, repo) = init_repo();
        for name in names {
            commit_file(&repo, name, "base\n", "init");
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(repo.workdir().unwrap().join(name))
                .unwrap();
            f.write_all(b"dirty\n").unwrap();
        }
        let path = repo.workdir().unwrap().to_path_buf();
        drop(repo);
        let mut fx = Fixture {
            _dir: dir,
            app: App::new(JobQueue::spawn(path).unwrap()),
        };
        let n = names.len();
        wait_for(&mut fx.app, |st| st.files.len() == n);
        fx
    }

    #[test]
    fn left_collapses_only_deepest_parent() {
        let mut fx = harness(&["src/a.rs", "src/nested/b.rs", "z.txt"]);
        fx.app.on_key(KeyCode::Char('j'));
        assert_eq!(fx.app.selected_file().unwrap().path, "src/nested/b.rs");
        fx.app.on_key(KeyCode::Left);
        assert!(
            fx.app.is_collapsed("src/nested"),
            "deepest parent should collapse"
        );
        assert!(
            !fx.app.is_collapsed("src"),
            "sibling subtree must stay expanded"
        );
    }

    #[test]
    fn folders_collapse_expand_and_skip_hidden_files() {
        let mut fx = harness(&["src/a.rs", "src/nested/b.rs", "z.txt"]);
        // Left collapses src/; the cursor stays as a hidden anchor so the
        // `▶ src/` header takes the highlight (cursor is on the folder).
        fx.app.on_key(KeyCode::Left);
        assert!(fx.app.is_collapsed("src"));
        // Down escapes the anchor to the next visible file.
        fx.app.on_key(KeyCode::Down);
        assert_eq!(
            fx.app.selected_file().unwrap().path,
            "z.txt",
            "collapsed src/ must be skipped when moving down"
        );
        // Up from below steps onto the folded header (cursor back on the
        // folder), never showing its hidden files as rows.
        fx.app.on_key(KeyCode::Up);
        assert_eq!(
            fx.app.selected_file().unwrap().path,
            "src/nested/b.rs",
            "moving up must stop on the collapsed header's anchor"
        );
        assert!(
            fx.app.is_hidden_index(fx.app.selected()),
            "anchor must be hidden so the header takes the highlight"
        );
        // Right on the header expands it back.
        fx.app.on_key(KeyCode::Right);
        assert!(
            !fx.app.is_collapsed("src"),
            "Right on the header must expand it"
        );
        assert_eq!(
            fx.app.selected_file().unwrap().path,
            "src/nested/b.rs",
            "expand restores the anchored file"
        );
        fx.app.on_key(KeyCode::Left);
        assert!(
            fx.app.is_collapsed("src/nested"),
            "Left collapses the deepest parent"
        );
        fx.app.on_key(KeyCode::Right);
        assert!(
            !fx.app.is_collapsed("src/nested"),
            "Right on the hidden anchor expands it back"
        );
        assert_eq!(
            fx.app.selected_file().unwrap().path,
            "src/nested/b.rs",
            "expand + collapse + expand restores visibility"
        );
        fx.app.on_key(KeyCode::Enter);
        assert_eq!(fx.app.mode(), Mode::FullDiff);
        fx.app.on_key(KeyCode::Esc);
        fx.app.on_key(KeyCode::Char('j'));
        assert_eq!(fx.app.selected_file().unwrap().path, "z.txt");
    }

    #[test]
    fn finder_reveals_a_file_inside_collapsed_ancestors() {
        let mut fx = harness(&["src/a.rs", "src/nested/b.rs"]);
        // Collapse src/ (both files fold away; the cursor stays anchored).
        fx.app.on_key(KeyCode::Left);
        assert!(fx.app.is_collapsed("src"));
        fx.app.on_key(KeyCode::Char('/'));
        for c in "b.rs".chars() {
            fx.app.on_key(KeyCode::Char(c));
        }
        fx.app.on_key(KeyCode::Enter);
        assert_eq!(fx.app.selected_file().unwrap().path, "src/nested/b.rs");
        assert!(
            !fx.app.is_collapsed("src"),
            "jumping to a match must expand its ancestors"
        );
        fx.app.on_key(KeyCode::Down);
        assert_eq!(
            fx.app.selected_file().unwrap().path,
            "src/nested/b.rs",
            "at the end of the list Down clamps"
        );
    }

    #[test]
    fn up_escapes_hidden_anchor_to_nearest_visible() {
        let mut fx = harness(&["src/a.rs", "src/nested/b.rs", "z.txt"]);
        // Move onto b.rs, then collapse its deepest parent: the cursor is
        // now a hidden anchor on the `▶ nested/` header.
        fx.app.on_key(KeyCode::Char('j'));
        assert_eq!(fx.app.selected_file().unwrap().path, "src/nested/b.rs");
        fx.app.on_key(KeyCode::Left);
        assert!(fx.app.is_collapsed("src/nested"));
        // Up escapes to the nearest visible file above, not into hiding.
        fx.app.on_key(KeyCode::Up);
        assert_eq!(fx.app.selected_file().unwrap().path, "src/a.rs");
        // Down jumps back over the folded region to z.txt.
        fx.app.on_key(KeyCode::Down);
        fx.app.on_key(KeyCode::Down);
        assert_eq!(fx.app.selected_file().unwrap().path, "z.txt");
    }

    #[test]
    fn selection_moves_with_jk_and_clamps() {
        let mut fx = harness(&["a.txt", "b.txt", "c.txt"]);
        assert_eq!(fx.app.selected(), 0);
        fx.app.on_key(KeyCode::Char('j'));
        fx.app.on_key(KeyCode::Char('j'));
        assert_eq!(fx.app.selected(), 2);
        fx.app.on_key(KeyCode::Char('j'));
        assert_eq!(fx.app.selected(), 2, "clamps at end");
        fx.app.on_key(KeyCode::Char('k'));
        assert_eq!(fx.app.selected(), 1);
        fx.app.on_key(KeyCode::Char('k'));
        fx.app.on_key(KeyCode::Char('k'));
        assert_eq!(fx.app.selected(), 0, "clamps at start");
    }

    #[test]
    fn space_on_unstaged_stages_the_file() {
        let mut fx = harness(&["a.txt"]);
        fx.app.on_key(KeyCode::Char(' '));
        let st = wait_for(&mut fx.app, |st| {
            st.files
                .iter()
                .any(|e| e.path == "a.txt" && e.state == FileState::Staged)
        });
        let entry = st.files.iter().find(|e| e.path == "a.txt").unwrap();
        assert_eq!(entry.state, FileState::Staged);
    }

    #[test]
    fn space_on_staged_unstages_the_file() {
        let mut fx = harness(&["a.txt"]);
        fx.app.on_key(KeyCode::Char(' '));
        wait_for(&mut fx.app, |st| {
            st.files
                .iter()
                .any(|e| e.path == "a.txt" && e.state == FileState::Staged)
        });
        fx.app.on_key(KeyCode::Char(' '));
        let st = wait_for(&mut fx.app, |st| {
            st.files
                .iter()
                .any(|e| e.path == "a.txt" && e.state == FileState::Unstaged)
        });
        let entry = st.files.iter().find(|e| e.path == "a.txt").unwrap();
        assert_eq!(entry.state, FileState::Unstaged);
    }

    #[test]
    fn space_on_collapsed_dir_stages_everything_under_it() {
        let mut fx = harness(&["src/a.rs", "src/nested/b.rs", "z.txt"]);
        // Collapse src/; the cursor stays on hidden src/a.rs, so the
        // header takes the highlight and Space acts on the whole dir.
        fx.app.on_key(KeyCode::Left);
        assert_eq!(fx.app.selected_file().unwrap().path, "src/a.rs");
        fx.app.on_key(KeyCode::Char(' '));
        let st = wait_for(&mut fx.app, |st| {
            let under: Vec<_> = st
                .files
                .iter()
                .filter(|e| e.path.starts_with("src/"))
                .collect();
            under.len() == 2 && under.iter().all(|e| e.state == FileState::Staged)
        });
        let outside = st.files.iter().find(|e| e.path == "z.txt").unwrap();
        assert_eq!(
            outside.state,
            FileState::Unstaged,
            "files outside the dir must be left alone"
        );
    }

    #[test]
    fn cursor_stops_on_open_folder_headers_both_ways() {
        // Rows: ▼ src/, a.rs, ▼ nested/, b.rs, c.rs, z.txt
        let mut fx = harness(&["src/a.rs", "src/nested/b.rs", "src/nested/c.rs", "z.txt"]);
        assert_eq!(fx.app.selected_file().unwrap().path, "src/a.rs");
        assert_eq!(fx.app.cursor_dir(), None);
        // Down from a.rs stops on the open `nested/` header, previewing
        // its first file.
        fx.app.on_key(KeyCode::Down);
        assert_eq!(fx.app.cursor_dir(), Some("src/nested"));
        assert_eq!(fx.app.selected_file().unwrap().path, "src/nested/b.rs");
        // Down again enters the folder.
        fx.app.on_key(KeyCode::Down);
        assert_eq!(fx.app.cursor_dir(), None);
        assert_eq!(fx.app.selected_file().unwrap().path, "src/nested/b.rs");
        // Up goes back onto the header, then a.rs, then the top `src/`.
        fx.app.on_key(KeyCode::Up);
        assert_eq!(fx.app.cursor_dir(), Some("src/nested"));
        fx.app.on_key(KeyCode::Up);
        assert_eq!(fx.app.cursor_dir(), None);
        assert_eq!(fx.app.selected_file().unwrap().path, "src/a.rs");
        fx.app.on_key(KeyCode::Up);
        assert_eq!(fx.app.cursor_dir(), Some("src"));
        assert_eq!(fx.app.selected_file().unwrap().path, "src/a.rs");
        // Clamps at the top.
        fx.app.on_key(KeyCode::Up);
        assert_eq!(fx.app.cursor_dir(), Some("src"));
    }

    #[test]
    fn space_on_open_folder_header_stages_only_that_folder() {
        let mut fx = harness(&["src/a.rs", "src/nested/b.rs", "src/nested/c.rs", "z.txt"]);
        fx.app.on_key(KeyCode::Down);
        assert_eq!(fx.app.cursor_dir(), Some("src/nested"));
        fx.app.on_key(KeyCode::Char(' '));
        let st = wait_for(&mut fx.app, |st| {
            st.files
                .iter()
                .filter(|e| e.path.starts_with("src/nested/"))
                .all(|e| e.state == FileState::Staged)
        });
        for outside in ["src/a.rs", "z.txt"] {
            let e = st.files.iter().find(|e| e.path == outside).unwrap();
            assert_eq!(e.state, FileState::Unstaged, "{outside} must be left alone");
        }
        // The folder stays open and the cursor stays on its header, so a
        // second Space unstages it again.
        assert!(!fx.app.is_collapsed("src/nested"));
        assert_eq!(fx.app.cursor_dir(), Some("src/nested"));
        fx.app.on_key(KeyCode::Char(' '));
        wait_for(&mut fx.app, |st| {
            st.files
                .iter()
                .filter(|e| e.path.starts_with("src/nested/"))
                .all(|e| e.state == FileState::Unstaged)
        });
    }

    #[test]
    fn left_on_open_header_folds_it_and_right_leaves_it_open() {
        let mut fx = harness(&["src/a.rs", "src/nested/b.rs", "z.txt"]);
        fx.app.on_key(KeyCode::Up);
        assert_eq!(fx.app.cursor_dir(), Some("src"));
        // Right on an already-open header changes nothing.
        fx.app.on_key(KeyCode::Right);
        assert!(!fx.app.is_collapsed("src") && !fx.app.is_collapsed("src/nested"));
        assert_eq!(fx.app.cursor_dir(), Some("src"));
        // Left folds exactly that folder (not the file's deepest parent);
        // the cursor stays on the now-collapsed header.
        fx.app.on_key(KeyCode::Left);
        assert!(fx.app.is_collapsed("src"));
        assert!(!fx.app.is_collapsed("src/nested"));
        assert!(fx.app.is_hidden_index(fx.app.selected()));
        // Right re-opens it.
        fx.app.on_key(KeyCode::Right);
        assert!(!fx.app.is_collapsed("src"));
    }

    #[test]
    fn space_on_collapsed_dir_unstages_when_everything_staged() {
        let mut fx = harness(&["src/a.rs", "src/nested/b.rs"]);
        fx.app.on_key(KeyCode::Left);
        fx.app.on_key(KeyCode::Char(' '));
        wait_for(&mut fx.app, |st| {
            st.files.len() == 2 && st.files.iter().all(|e| e.state == FileState::Staged)
        });
        // Everything under src/ is staged, so Space unstages the whole dir.
        fx.app.on_key(KeyCode::Char(' '));
        let st = wait_for(&mut fx.app, |st| {
            st.files.len() == 2 && st.files.iter().all(|e| e.state == FileState::Unstaged)
        });
        assert!(
            st.files.iter().all(|e| e.state == FileState::Unstaged),
            "got: {:?}",
            st.files
        );
    }

    #[test]
    fn space_on_collapsed_dir_with_conflict_reports_error() {
        let mut fx = harness(&[]);
        fx.app.status = Some(RepoStatus {
            branch: "main".into(),
            head_summary: "x".into(),
            files: vec![
                StatusEntry {
                    path: "src/a.rs".into(),
                    state: FileState::Unstaged,
                },
                StatusEntry {
                    path: "src/b.rs".into(),
                    state: FileState::Conflicted,
                },
            ],
            tracked_files: vec!["src/a.rs".into(), "src/b.rs".into()],
        });
        fx.app.rebuild_file_list();
        fx.app.set_collapsed("src", true);
        fx.app.on_key(KeyCode::Char(' '));
        let err = fx.app.error().expect("expected conflict error");
        assert!(err.contains("conflicted"), "got: {err}");
        assert!(err.contains("src/b.rs"), "got: {err}");
    }

    #[test]
    fn space_on_collapsed_dir_with_nothing_to_stage_reports_error() {
        let mut fx = harness(&[]);
        fx.app.status = Some(RepoStatus {
            branch: "main".into(),
            head_summary: "x".into(),
            files: vec![],
            tracked_files: vec!["src/a.rs".into()],
        });
        fx.app.rebuild_file_list();
        fx.app.set_collapsed("src", true);
        fx.app.on_key(KeyCode::Char(' '));
        let err = fx.app.error().expect("expected nothing-to-stage error");
        assert!(err.contains("under src/"), "got: {err}");
    }

    #[test]
    fn space_with_no_files_does_nothing() {
        let mut fx = harness(&[]);
        fx.app.on_key(KeyCode::Char(' '));
        assert!(fx.app.error().is_none());
        assert_eq!(fx.app.selected(), 0);
    }

    #[test]
    fn d_on_unstaged_discards_the_file() {
        let mut fx = harness(&["a.txt"]);
        fx.app.on_key(KeyCode::Char('d'));
        let st = wait_for(&mut fx.app, |st| st.files.iter().all(|e| e.path != "a.txt"));
        assert!(
            st.files.iter().all(|e| e.path != "a.txt"),
            "got: {:?}",
            st.files
        );
    }

    #[test]
    fn d_on_staged_discards_index_and_workdir_changes() {
        let mut fx = harness(&["a.txt"]);
        fx.app.on_key(KeyCode::Char(' '));
        wait_for(&mut fx.app, |st| {
            st.files
                .iter()
                .any(|e| e.path == "a.txt" && e.state == FileState::Staged)
        });
        fx.app.on_key(KeyCode::Char('d'));
        let st = wait_for(&mut fx.app, |st| st.files.iter().all(|e| e.path != "a.txt"));
        assert!(
            st.files.iter().all(|e| e.path != "a.txt"),
            "got: {:?}",
            st.files
        );
    }

    #[test]
    fn d_on_untracked_deletes_the_file() {
        let (dir, repo) = init_repo();
        commit_file(&repo, "a.txt", "base\n", "init");
        std::fs::write(repo.workdir().unwrap().join("new.txt"), "new\n").unwrap();
        let path = repo.workdir().unwrap().to_path_buf();
        drop(repo);
        let mut fx = Fixture {
            _dir: dir,
            app: App::new(JobQueue::spawn(path).unwrap()),
        };
        wait_for(&mut fx.app, |st| {
            st.files
                .iter()
                .any(|e| e.path == "new.txt" && e.state == FileState::Untracked)
        });
        // Changed files sort before clean ones, so the untracked file is
        // already selected.
        assert_eq!(fx.app.selected_file().unwrap().path, "new.txt");
        fx.app.on_key(KeyCode::Char('d'));
        wait_for(&mut fx.app, |st| {
            st.files.iter().all(|e| e.path != "new.txt")
        });
        assert!(!fx._dir.path().join("new.txt").exists());
    }

    #[test]
    fn d_on_clean_file_reports_error() {
        let mut fx = harness(&[]);
        fx.app.status = Some(RepoStatus {
            branch: "main".into(),
            head_summary: "x".into(),
            files: vec![],
            tracked_files: vec!["a.txt".into()],
        });
        fx.app.rebuild_file_list();
        assert_eq!(fx.app.selected_file().unwrap().path, "a.txt");
        fx.app.on_key(KeyCode::Char('d'));
        let err = fx.app.error().expect("expected nothing-to-discard error");
        assert!(err.contains("nothing to discard"), "got: {err}");
    }

    #[test]
    fn d_on_collapsed_dir_discards_everything_under_it() {
        let mut fx = harness(&["src/a.rs", "src/nested/b.rs", "z.txt"]);
        // Collapse src/; the cursor stays on hidden src/a.rs, so the
        // header takes the highlight and `d` acts on the whole dir.
        fx.app.on_key(KeyCode::Left);
        assert_eq!(fx.app.selected_file().unwrap().path, "src/a.rs");
        fx.app.on_key(KeyCode::Char('d'));
        let st = wait_for(&mut fx.app, |st| {
            st.files.iter().all(|e| !e.path.starts_with("src/"))
        });
        let outside = st.files.iter().find(|e| e.path == "z.txt").unwrap();
        assert_eq!(
            outside.state,
            FileState::Unstaged,
            "files outside the dir must be left alone"
        );
    }

    #[test]
    fn d_in_fullscreen_discards_the_loaded_file() {
        let mut fx = harness(&["a.txt"]);
        fx.app.on_key(KeyCode::Enter);
        assert_eq!(fx.app.mode(), Mode::FullDiff);
        fx.app.on_key(KeyCode::Char('d'));
        wait_for(&mut fx.app, |st| st.files.iter().all(|e| e.path != "a.txt"));
    }

    #[test]
    fn enter_with_no_files_does_not_open_fullscreen() {
        let mut fx = harness(&[]);
        fx.app.on_key(KeyCode::Enter);
        assert_eq!(fx.app.mode(), Mode::Normal);
    }

    #[test]
    fn space_on_conflicted_reports_error() {
        let mut fx = harness(&[]);
        fx.app.status = Some(RepoStatus {
            branch: "main".into(),
            head_summary: "x".into(),
            files: vec![StatusEntry {
                path: "a.txt".into(),
                state: FileState::Conflicted,
            }],
            tracked_files: vec!["a.txt".into()],
        });
        fx.app.rebuild_file_list();
        fx.app.on_key(KeyCode::Char(' '));
        assert!(fx.app.error().is_some(), "expected conflict error");
    }

    #[test]
    fn c_opens_modal_esc_cancels() {
        let mut fx = harness(&["a.txt"]);
        assert_eq!(fx.app.mode(), Mode::Normal);
        fx.app.on_key(KeyCode::Char('c'));
        assert_eq!(fx.app.mode(), Mode::Committing);
        fx.app.on_key(KeyCode::Char('x'));
        assert_eq!(fx.app.draft(), "x");
        fx.app.on_key(KeyCode::Esc);
        assert_eq!(fx.app.mode(), Mode::Normal);
        assert_eq!(fx.app.draft(), "");
    }

    #[test]
    fn enter_in_modal_commits_and_returns_to_normal() {
        let mut fx = harness(&["a.txt"]);
        fx.app.on_key(KeyCode::Char(' '));
        wait_for(&mut fx.app, |st| {
            st.files
                .iter()
                .any(|e| e.path == "a.txt" && e.state == FileState::Staged)
        });
        fx.app.on_key(KeyCode::Char('c'));
        for c in "my commit".chars() {
            fx.app.on_key(KeyCode::Char(c));
        }
        fx.app.on_key(KeyCode::Enter);
        assert_eq!(fx.app.mode(), Mode::Normal);
        let st = wait_for(&mut fx.app, |st| st.files.is_empty());
        assert_eq!(st.head_summary, "my commit");
    }

    #[test]
    fn enter_with_empty_message_does_not_submit() {
        let mut fx = harness(&["a.txt"]);
        fx.app.on_key(KeyCode::Char('c'));
        fx.app.on_key(KeyCode::Enter);
        assert_eq!(fx.app.mode(), Mode::Committing, "stays in modal");
    }

    #[test]
    fn commit_message_edits_mid_text_with_cursor() {
        let mut fx = harness(&["a.txt"]);
        fx.app.on_key(KeyCode::Char('c'));
        for c in "hello".chars() {
            fx.app.on_key(KeyCode::Char(c));
        }
        assert_eq!(fx.app.draft_cursor(), 5);
        fx.app.on_key(KeyCode::Left);
        fx.app.on_key(KeyCode::Left);
        assert_eq!(fx.app.draft_cursor(), 3);
        // Backspace deletes before the cursor: "hello" -> "helo".
        fx.app.on_key(KeyCode::Backspace);
        assert_eq!(fx.app.draft(), "helo");
        assert_eq!(fx.app.draft_cursor(), 2);
        // Typing inserts at the cursor: "helo" -> "heXlo".
        fx.app.on_key(KeyCode::Char('X'));
        assert_eq!(fx.app.draft(), "heXlo");
        assert_eq!(fx.app.draft_cursor(), 3);
        // Delete removes under the cursor: "heXlo" -> "heXo".
        fx.app.on_key(KeyCode::Delete);
        assert_eq!(fx.app.draft(), "heXo");
        // Home/End jump; typing at the front and back works.
        fx.app.on_key(KeyCode::Home);
        fx.app.on_key(KeyCode::Char('!'));
        assert_eq!(fx.app.draft(), "!heXo");
        fx.app.on_key(KeyCode::End);
        fx.app.on_key(KeyCode::Char('?'));
        assert_eq!(fx.app.draft(), "!heXo?");
        assert_eq!(fx.app.draft_cursor(), 6);
    }

    #[test]
    fn commit_cursor_clamps_at_both_ends() {
        let mut fx = harness(&["a.txt"]);
        fx.app.on_key(KeyCode::Char('c'));
        fx.app.on_key(KeyCode::Char('a'));
        fx.app.on_key(KeyCode::Left);
        fx.app.on_key(KeyCode::Left);
        assert_eq!(fx.app.draft_cursor(), 0, "clamps at start");
        fx.app.on_key(KeyCode::Backspace);
        assert_eq!(fx.app.draft(), "a", "nothing to delete at start");
        fx.app.on_key(KeyCode::Right);
        fx.app.on_key(KeyCode::Right);
        assert_eq!(fx.app.draft_cursor(), 1, "clamps at end");
        fx.app.on_key(KeyCode::Delete);
        assert_eq!(fx.app.draft(), "a", "nothing to delete at end");
    }

    #[test]
    fn commit_box_up_down_moves_between_visual_rows() {
        let mut fx = harness(&["a.txt"]);
        fx.app.on_key(KeyCode::Char('c'));
        for c in "subject".chars() {
            fx.app.on_key(KeyCode::Char(c));
        }
        fx.app.push_draft_char('\n');
        for c in "body".chars() {
            fx.app.on_key(KeyCode::Char(c));
        }
        assert_eq!(fx.app.draft(), "subject\nbody");
        assert_eq!(fx.app.draft_cursor(), 12);
        // Up keeps the display column (end of "body" → col 4 in "subject").
        fx.app.on_key(KeyCode::Up);
        assert_eq!(fx.app.draft_cursor(), 4);
        // Up past the first row clamps to the document start.
        fx.app.on_key(KeyCode::Up);
        assert_eq!(fx.app.draft_cursor(), 0);
        // Down preserves the column into the next row.
        fx.app.on_key(KeyCode::Down);
        assert_eq!(fx.app.draft_cursor(), 8);
        // Down past the last row jumps to the very end.
        fx.app.on_key(KeyCode::Down);
        assert_eq!(fx.app.draft_cursor(), 12);
        // Enter still commits a multi-line message.
        fx.app.on_key(KeyCode::Enter);
        assert_eq!(fx.app.mode(), Mode::Normal);
    }

    #[test]
    fn commit_box_up_down_follows_soft_wrapped_rows() {
        let mut fx = harness(&["a.txt"]);
        // Narrow wrap so a single hard line becomes several visual rows.
        fx.app.set_draft_wrap_width(5);
        fx.app.on_key(KeyCode::Char('c'));
        for c in "hello world".chars() {
            fx.app.on_key(KeyCode::Char(c));
        }
        assert_eq!(fx.app.draft_cursor(), 11);
        // Rows are "hello" / " worl" / "d": end-of-doc col is 1 ("d"), so
        // up lands at col 1 of the previous row (index 6, after the space).
        fx.app.on_key(KeyCode::Up);
        assert_eq!(
            fx.app.draft_cursor(),
            6,
            "same display col on prev visual row"
        );
        fx.app.on_key(KeyCode::Down);
        assert_eq!(fx.app.draft_cursor(), 11, "back to end on the last row");
        fx.app.on_key(KeyCode::Down);
        assert_eq!(fx.app.draft_cursor(), 11, "clamps at document end");
        // From the document start, down walks visual rows without jumping
        // to the end of the hard line.
        fx.app.on_key(KeyCode::Home);
        assert_eq!(fx.app.draft_cursor(), 0);
        fx.app.on_key(KeyCode::Down);
        assert_eq!(fx.app.draft_cursor(), 5, "start of second visual row");
    }

    #[test]
    fn empty_commit_shows_notice_and_stays_open() {
        let mut fx = harness(&["a.txt"]);
        fx.app.on_key(KeyCode::Char('c'));
        fx.app.on_key(KeyCode::Enter);
        assert_eq!(fx.app.mode(), Mode::Committing, "box stays open");
        let notice = fx.app.notice().unwrap_or("");
        assert!(
            notice.contains("type a commit message"),
            "expected empty-draft notice, got: {notice:?}"
        );
        assert!(fx.app.error().is_none());
    }

    #[test]
    fn enter_while_generating_is_ignored_with_notice() {
        let mut fx = harness(&["a.txt"]);
        fx.app.on_key(KeyCode::Char('c'));
        fx.app.generating = true;
        for c in "wip".chars() {
            fx.app.on_key(KeyCode::Char(c));
        }
        fx.app.on_key(KeyCode::Enter);
        assert_eq!(
            fx.app.mode(),
            Mode::Committing,
            "must not commit mid-generation"
        );
        assert_eq!(fx.app.draft(), "wip");
        let notice = fx.app.notice().unwrap_or("");
        assert!(notice.contains("generating"), "got: {notice:?}");
    }

    #[test]
    fn failing_job_surfaces_error_display() {
        let mut fx = harness(&["a.txt"]);
        // Commit with nothing staged -> EmptyCommit error via worker.
        fx.app.on_key(KeyCode::Char('c'));
        for c in "nothing".chars() {
            fx.app.on_key(KeyCode::Char(c));
        }
        fx.app.on_key(KeyCode::Enter);
        let err = wait_for_error(&mut fx.app);
        assert!(err.contains("nothing to commit"), "got: {err}");
    }

    #[test]
    fn empty_commit_error_tells_user_to_stage_first_and_clears_on_next_action() {
        let mut fx = harness(&["a.txt"]);
        fx.app.on_key(KeyCode::Char('c'));
        for c in "oops".chars() {
            fx.app.on_key(KeyCode::Char(c));
        }
        fx.app.on_key(KeyCode::Enter);
        let err = wait_for_error(&mut fx.app);
        assert!(err.contains("nothing to commit"), "got: {err}");
        assert!(err.contains("space"), "should hint at staging, got: {err}");
        // A new action dismisses the stale error.
        fx.app.on_key(KeyCode::Char('j'));
        assert!(fx.app.error().is_none());
    }

    #[test]
    fn right_arrow_no_longer_leaves_the_file_list_enter_opens_fullscreen() {
        let mut fx = harness(&["a.txt"]);
        assert_eq!(fx.app.focus(), Focus::Status);
        // Vertical-only navigation: Right does nothing now.
        fx.app.on_key(KeyCode::Right);
        assert_eq!(fx.app.focus(), Focus::Status);
        assert_eq!(fx.app.mode(), Mode::Normal);
        // Enter opens the file fullscreen; Esc closes back.
        fx.app.on_key(KeyCode::Enter);
        assert_eq!(fx.app.mode(), Mode::FullDiff);
        fx.app.on_key(KeyCode::Esc);
        assert_eq!(fx.app.mode(), Mode::Normal);
        // Left still jumps back to the file list.
        fx.app.on_key(KeyCode::Char('2'));
        assert_eq!(fx.app.focus(), Focus::Branches);
        fx.app.on_key(KeyCode::Left);
        assert_eq!(fx.app.focus(), Focus::Status);
    }

    #[test]
    fn up_down_arrows_scroll_diff_line_by_line_without_moving_hunk() {
        let mut fx = two_hunk_fixture();
        fx.app.on_key(KeyCode::Enter);
        assert_eq!(fx.app.mode(), Mode::FullDiff);
        assert_eq!(fx.app.diff_scroll(), 0);
        fx.app.on_key(KeyCode::Down);
        assert_eq!(fx.app.diff_scroll(), 1);
        assert_eq!(
            fx.app.hunk(),
            0,
            "arrow scroll must not move hunk selection"
        );
        fx.app.on_key(KeyCode::Down);
        assert_eq!(fx.app.diff_scroll(), 2);
        fx.app.on_key(KeyCode::Up);
        assert_eq!(fx.app.diff_scroll(), 1);
        // J/K jump by hunk (and snap scroll to the hunk top).
        fx.app.on_key(KeyCode::Char('J'));
        assert_eq!(fx.app.hunk(), 1);
        assert!(fx.app.diff_scroll() > 1);
    }

    /// Repo with `a.txt` dirtied and `b.txt` left clean.
    fn clean_fixture() -> (tempfile::TempDir, App) {
        let (dir, repo) = init_repo();
        commit_file(&repo, "a.txt", "a\n", "init");
        commit_file(&repo, "b.txt", "b\n", "init");
        {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(repo.workdir().unwrap().join("a.txt"))
                .unwrap();
            f.write_all(b"dirty\n").unwrap();
        }
        let path = repo.workdir().unwrap().to_path_buf();
        drop(repo);
        let app = App::new(JobQueue::spawn(path).unwrap());
        (dir, app)
    }

    /// Settle until the browsable tree holds `n` entries.
    fn wait_for_files(app: &mut App, n: usize) -> Vec<StatusEntry> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            app.poll();
            if app.file_list().len() == n {
                return app.file_list().to_vec();
            }
            assert!(
                Instant::now() < deadline,
                "timed out; last: {:?}",
                app.file_list()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn clean_tracked_files_join_the_tree_after_changed_ones() {
        let (_dir, mut app) = clean_fixture();
        let list = wait_for_files(&mut app, 2);
        assert_eq!(list[0].path, "a.txt");
        assert_eq!(list[0].state, FileState::Unstaged);
        assert_eq!(list[1].path, "b.txt");
        assert_eq!(list[1].state, FileState::Clean);
    }

    #[test]
    fn binary_file_gets_notice_instead_of_whole_file_bytes() {
        let dir = tempfile::TempDir::new().unwrap();
        git2::Repository::init(dir.path()).unwrap();
        std::fs::write(dir.path().join("img.png"), b"\x89PNG\r\n\x1a\n\0\0\x1b[2J").unwrap();
        let mut app = App::new(JobQueue::spawn(dir.path()).unwrap());
        wait_for(&mut app, |st| st.files.iter().any(|e| e.path == "img.png"));
        let deadline = Instant::now() + Duration::from_secs(10);
        while !app.diff().is_some_and(|d| d.path == "img.png") {
            assert!(Instant::now() < deadline, "timed out waiting for diff");
            app.poll();
            std::thread::sleep(Duration::from_millis(10));
        }
        // Let any (wrong) whole-file fallback job run and land.
        for _ in 0..30 {
            app.poll();
            std::thread::sleep(Duration::from_millis(10));
        }
        let d = app.diff().expect("diff stays loaded");
        assert!(d.binary, "binary flag lost: {d:?}");
        assert!(d.hunks.is_empty(), "binary bytes leaked into hunks");
        assert!(
            !app.diff_whole_file(),
            "binary must not fall back to whole-file view"
        );
    }

    #[test]
    fn enter_on_clean_file_loads_whole_file() {
        use git_tui_core::diff::LineKind;
        let (_dir, mut app) = clean_fixture();
        wait_for_files(&mut app, 2);
        fx_select(&mut app);
        let d = wait_for_diff(&mut app, "b.txt");
        assert!(app.diff_whole_file());
        let texts: Vec<&str> = d
            .hunks
            .iter()
            .flat_map(|h| h.lines.iter())
            .map(|l| l.text.as_str())
            .collect();
        assert_eq!(texts, vec!["b"]);
        assert!(d
            .hunks
            .iter()
            .flat_map(|h| h.lines.iter())
            .all(|l| l.kind == LineKind::Context));
    }

    /// Move selection to the clean `b.txt` (index 1 in the clean fixture).
    fn fx_select(app: &mut App) {
        app.on_key(KeyCode::Char('j'));
        assert_eq!(app.selected_file().unwrap().path, "b.txt");
    }

    #[test]
    fn space_on_clean_file_reports_error() {
        let (_dir, mut app) = clean_fixture();
        wait_for_files(&mut app, 2);
        fx_select(&mut app);
        app.on_key(KeyCode::Char(' '));
        let err = app.error().unwrap_or("").to_string();
        assert!(err.contains("unchanged"), "got: {err}");
    }

    #[test]
    fn space_on_whole_file_hunk_errors_instead_of_staging() {
        let (_dir, mut app) = clean_fixture();
        wait_for_files(&mut app, 2);
        fx_select(&mut app);
        app.on_key(KeyCode::Enter);
        assert_eq!(app.mode(), Mode::FullDiff);
        wait_for_diff(&mut app, "b.txt");
        app.on_key(KeyCode::Char(' '));
        let err = app.error().unwrap_or("").to_string();
        assert!(err.contains("nothing to stage"), "got: {err}");
    }

    #[test]
    fn enter_on_changed_file_shows_diff_automatically() {
        let mut fx = harness(&["a.txt"]);
        // Changed file: the loaded view is the diff, not the whole file.
        let d = wait_for_diff(&mut fx.app, "a.txt");
        assert!(!fx.app.diff_whole_file());
        assert!(!d.hunks.is_empty());
        // Fullscreen shows that same diff.
        fx.app.on_key(KeyCode::Enter);
        assert_eq!(fx.app.mode(), Mode::FullDiff);
        assert!(!fx.app.diff_whole_file());
        assert_eq!(fx.app.diff().unwrap().path, "a.txt");
    }

    #[test]
    fn enter_on_clean_file_shows_whole_file_automatically() {
        use git_tui_core::diff::LineKind;
        let (_dir, mut app) = clean_fixture();
        wait_for_files(&mut app, 2);
        fx_select(&mut app);
        // No changes: whole workdir file loads on its own.
        let d = wait_for_diff(&mut app, "b.txt");
        assert!(app.diff_whole_file());
        assert!(d
            .hunks
            .iter()
            .flat_map(|h| h.lines.iter())
            .all(|l| { l.kind == LineKind::Context }));
        // Fullscreen shows that same whole-file view.
        app.on_key(KeyCode::Enter);
        assert_eq!(app.mode(), Mode::FullDiff);
        assert!(app.diff_whole_file());
        assert_eq!(app.diff().unwrap().path, "b.txt");
    }

    #[test]
    fn empty_diff_falls_back_to_whole_file_automatically() {
        use std::os::unix::fs::PermissionsExt;
        let (dir, repo) = init_repo();
        commit_file(&repo, "a.txt", "hello\nworld\n", "init");
        // Mode-only change: status lists the file, but the content diff
        // has zero hunks — the viewer must show the whole file, not
        // "(no changes)".
        std::fs::set_permissions(
            repo.workdir().unwrap().join("a.txt"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        let path = repo.workdir().unwrap().to_path_buf();
        drop(repo);
        let mut fx = Fixture {
            _dir: dir,
            app: App::new(JobQueue::spawn(path).unwrap()),
        };
        wait_for(&mut fx.app, |st| st.files.iter().any(|e| e.path == "a.txt"));
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            fx.app.poll();
            if fx.app.diff_whole_file() {
                if let Some(d) = fx.app.diff() {
                    if d.path == "a.txt" && !d.hunks.is_empty() {
                        break;
                    }
                }
            }
            assert!(
                Instant::now() < deadline,
                "whole-file fallback never loaded; whole={} diff={:?}",
                fx.app.diff_whole_file(),
                fx.app.diff()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let texts: Vec<&str> = fx
            .app
            .diff()
            .unwrap()
            .hunks
            .iter()
            .flat_map(|h| h.lines.iter())
            .map(|l| l.text.as_str())
            .collect();
        assert_eq!(texts, vec!["hello", "world"]);
    }

    #[test]
    fn q_in_modal_types_literal_and_quits_after_esc() {
        let mut fx = harness(&["a.txt"]);
        fx.app.on_key(KeyCode::Char('c'));
        fx.app.on_key(KeyCode::Char('q'));
        assert!(!fx.app.should_quit());
        assert_eq!(fx.app.draft(), "q");
        fx.app.on_key(KeyCode::Char('Q'));
        assert!(!fx.app.should_quit(), "Q in modal must type, not quit");
        assert_eq!(fx.app.draft(), "qQ");
        fx.app.on_key(KeyCode::Esc);
        // Lowercase `q` no longer quits the app (workspace closes the
        // project instead); uppercase `Q` quits everything.
        fx.app.on_key(KeyCode::Char('q'));
        assert!(!fx.app.should_quit());
        fx.app.on_key(KeyCode::Char('Q'));
        assert!(fx.app.should_quit());
    }

    /// Repo with an `origin` remote (bare, on disk) that nothing has been
    /// pushed to yet: pressing `P` must offer to set the upstream.
    fn sync_harness() -> (tempfile::TempDir, tempfile::TempDir, App) {
        let (dir, repo) = init_repo();
        commit_file(&repo, "a.txt", "a\n", "init");
        let origin_dir = tempfile::TempDir::new().unwrap();
        let bare = origin_dir.path().join("origin.git");
        git2::Repository::init_bare(&bare).unwrap();
        repo.remote("origin", bare.to_str().unwrap()).unwrap();
        let path = repo.workdir().unwrap().to_path_buf();
        drop(repo);
        let app = App::new(JobQueue::spawn(path).unwrap());
        (dir, origin_dir, app)
    }

    /// Settle until upstream tracking state arrives (preloaded at startup).
    fn wait_for_sync(app: &mut App) -> git_tui_core::sync::SyncStatus {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            app.poll();
            if let Some(st) = app.sync() {
                return st.clone();
            }
            assert!(Instant::now() < deadline, "timed out waiting for sync");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn p_pull_failure_surfaces_error_and_clears_syncing() {
        // Harness repos have no remote (and dirty files, so `git pull`
        // fails one way or another depending on the user's pull config).
        // What matters: the failure lands in the error line and the
        // "pulling…" indicator clears instead of sticking forever.
        let mut fx = harness(&["a.txt"]);
        fx.app.on_key(KeyCode::Char('p'));
        assert_eq!(fx.app.syncing(), Some("pulling…"));
        let err = wait_for_error(&mut fx.app);
        assert!(!err.is_empty(), "expected a pull failure message");
        assert!(fx.app.syncing().is_none());
    }

    #[test]
    fn push_without_remotes_opens_publish_modal_and_enter_publishes() {
        let (dir, repo) = init_repo();
        commit_file(&repo, "a.txt", "a\n", "init");
        let origin_dir = tempfile::TempDir::new().unwrap();
        let bare = origin_dir.path().join("origin.git");
        git2::Repository::init_bare(&bare).unwrap();
        let path = repo.workdir().unwrap().to_path_buf();
        drop(repo);
        let mut app = App::new(JobQueue::spawn(path).unwrap());
        let st = wait_for_sync(&mut app);
        assert!(st.remotes.is_empty());
        app.on_key(KeyCode::Char('P'));
        assert_eq!(app.mode(), Mode::SetRemote);
        for c in bare.to_str().unwrap().chars() {
            app.on_key(KeyCode::Char(c));
        }
        app.on_key(KeyCode::Enter);
        assert_eq!(app.mode(), Mode::Normal);
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            app.poll();
            if app.sync().is_some_and(|s| s.upstream.is_some()) {
                break;
            }
            assert!(Instant::now() < deadline, "publish never recorded upstream");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(app.sync().unwrap().upstream.as_deref(), Some("origin/main"));
        let remote = git2::Repository::open_bare(&bare).unwrap();
        assert!(remote.find_reference("refs/heads/main").is_ok());
        drop(dir);
    }

    #[test]
    fn push_without_upstream_opens_set_upstream_modal() {
        let (_dir, _origin, mut app) = sync_harness();
        wait_for_sync(&mut app);
        app.on_key(KeyCode::Char('P'));
        assert_eq!(app.mode(), Mode::SetUpstream);
        assert_eq!(app.draft(), "origin");
        app.on_key(KeyCode::Enter);
        assert_eq!(app.mode(), Mode::Normal);
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            app.poll();
            if app.sync().is_some_and(|s| s.upstream.is_some()) {
                break;
            }
            assert!(Instant::now() < deadline, "push -u never recorded upstream");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(app.sync().unwrap().upstream.as_deref(), Some("origin/main"));
    }

    #[test]
    fn push_with_upstream_pushes_directly_without_modal() {
        let (_dir, _origin, mut app) = sync_harness();
        wait_for_sync(&mut app);
        // Establish the upstream first.
        app.on_key(KeyCode::Char('P'));
        assert_eq!(app.mode(), Mode::SetUpstream);
        app.on_key(KeyCode::Enter);
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            app.poll();
            if app.sync().is_some_and(|s| s.upstream.is_some()) {
                break;
            }
            assert!(Instant::now() < deadline, "push -u never recorded upstream");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(app.syncing().is_none());
        // A fresh `P` now pushes straight away, no modal.
        app.on_key(KeyCode::Char('P'));
        assert_eq!(app.mode(), Mode::Normal);
        assert_eq!(app.syncing(), Some("pushing…"));
    }

    #[test]
    fn esc_in_sync_modal_returns_without_syncing() {
        let (_dir, _origin, mut app) = sync_harness();
        wait_for_sync(&mut app);
        app.on_key(KeyCode::Char('P'));
        assert_eq!(app.mode(), Mode::SetUpstream);
        app.on_key(KeyCode::Esc);
        assert_eq!(app.mode(), Mode::Normal);
        assert!(app.syncing().is_none());
        assert!(app.sync().unwrap().upstream.is_none());
    }

    #[test]
    fn sync_modal_from_fullscreen_returns_to_fullscreen() {
        let mut fx = harness(&["a.txt"]);
        fx.app.on_key(KeyCode::Enter);
        assert_eq!(fx.app.mode(), Mode::FullDiff);
        // Harness repos have no remotes: `P` opens the publish modal.
        fx.app.on_key(KeyCode::Char('P'));
        assert_eq!(fx.app.mode(), Mode::SetRemote);
        fx.app.on_key(KeyCode::Esc);
        assert_eq!(fx.app.mode(), Mode::FullDiff);
    }

    /// Two-hunk fixture: 40 lines, changes at line 5 and line 35.
    fn two_hunk_fixture() -> Fixture {
        let (dir, repo) = init_repo();
        let base = (1..=40).map(|i| format!("line {i}\n")).collect::<String>();
        commit_file(&repo, "a.txt", &base, "init");
        let dirty = base.replacen("line 5\n", "line 5 CHANGED\n", 1).replacen(
            "line 35\n",
            "line 35 CHANGED\n",
            1,
        );
        std::fs::write(repo.workdir().unwrap().join("a.txt"), &dirty).unwrap();
        let path = repo.workdir().unwrap().to_path_buf();
        drop(repo);
        let mut fx = Fixture {
            _dir: dir,
            app: App::new(JobQueue::spawn(path).unwrap()),
        };
        wait_for(&mut fx.app, |st| st.files.iter().any(|e| e.path == "a.txt"));
        wait_for_diff(&mut fx.app, "a.txt");
        fx
    }

    fn wait_for_diff(app: &mut App, path: &str) -> git_tui_core::diff::FileDiff {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            app.poll();
            if let Some(d) = app.diff() {
                if d.path == path && !d.hunks.is_empty() {
                    return d.clone();
                }
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for diff of {path}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn force_reload_recomputes_target_from_fresh_status() {
        // Stash push cleans the tree (whole-file view); pop dirties it
        // again (hunks). The [5] preview must follow the fresh status,
        // not re-request the stale `diff_for`.
        use git_tui_core::status::{RepoStatus, StatusEntry};
        let mut fx = harness(&["a.txt"]);
        wait_for_diff(&mut fx.app, "a.txt");
        // Fresh status: tree clean -> whole-file view for the same path.
        fx.app.status = Some(RepoStatus {
            branch: "main".into(),
            head_summary: "x".into(),
            files: vec![],
            tracked_files: vec!["a.txt".into()],
        });
        fx.app.rebuild_file_list();
        fx.app.force_reload_diff_for_fresh_status();
        assert_eq!(
            fx.app.diff_for,
            Some(("a.txt".to_string(), false)),
            "target must be recomputed, got {:?}",
            fx.app.diff_for
        );
        assert!(
            fx.app.diff_whole_file(),
            "clean tree must show the whole file, not hunks"
        );
        // Fresh status: dirty again -> real diff with hunks.
        fx.app.status = Some(RepoStatus {
            branch: "main".into(),
            head_summary: "x".into(),
            files: vec![StatusEntry {
                path: "a.txt".into(),
                state: FileState::Unstaged,
            }],
            tracked_files: vec!["a.txt".into()],
        });
        fx.app.rebuild_file_list();
        fx.app.force_reload_diff_for_fresh_status();
        assert!(
            !fx.app.diff_whole_file(),
            "dirty tree must show the diff, not the whole file"
        );
    }

    #[test]
    fn enter_esc_then_walk_files_in_both_directions() {        let mut fx = harness(&["a.txt", "b.txt", "c.txt"]);
        fx.app.on_key(KeyCode::Enter);
        assert_eq!(fx.app.mode(), Mode::FullDiff);
        fx.app.on_key(KeyCode::Esc);
        assert_eq!(fx.app.mode(), Mode::Normal);
        for expected in ["b.txt", "c.txt"] {
            fx.app.on_key(KeyCode::Char('j'));
            assert_eq!(
                fx.app.selected_file().unwrap().path,
                expected,
                "down-walk broke after Esc"
            );
        }
        for expected in ["b.txt", "a.txt"] {
            fx.app.on_key(KeyCode::Char('k'));
            assert_eq!(
                fx.app.selected_file().unwrap().path,
                expected,
                "up-walk broke after Esc"
            );
        }
        // The preview must follow the cursor again too.
        let d = wait_for_diff(&mut fx.app, "a.txt");
        assert_eq!(d.path, "a.txt");
    }

    #[test]
    fn slash_opens_finder_and_esc_cancels() {
        let mut fx = harness(&["a.txt"]);
        fx.app.on_key(KeyCode::Char('/'));
        assert_eq!(fx.app.mode(), Mode::FindFile);
        assert_eq!(fx.app.draft(), "");
        fx.app.on_key(KeyCode::Esc);
        assert_eq!(fx.app.mode(), Mode::Normal);
    }

    #[test]
    fn finder_typing_filters_and_enter_jumps_to_match() {
        let mut fx = harness(&["a.txt", "b.txt"]);
        fx.app.on_key(KeyCode::Char('/'));
        for c in "b".chars() {
            fx.app.on_key(KeyCode::Char(c));
        }
        assert_eq!(fx.app.finder_matches(), vec![1]);
        fx.app.on_key(KeyCode::Enter);
        assert_eq!(fx.app.mode(), Mode::Normal);
        assert_eq!(fx.app.focus(), Focus::Status);
        assert_eq!(fx.app.selected(), 1);
        assert_eq!(fx.app.selected_file().unwrap().path, "b.txt");
    }

    #[test]
    fn finder_arrows_move_and_clamp() {
        let mut fx = harness(&["a.txt", "b.txt", "c.txt"]);
        fx.app.on_key(KeyCode::Char('/'));
        assert_eq!(fx.app.finder_matches().len(), 3);
        fx.app.on_key(KeyCode::Down);
        fx.app.on_key(KeyCode::Down);
        assert_eq!(fx.app.finder_cursor(), 2);
        fx.app.on_key(KeyCode::Down);
        assert_eq!(fx.app.finder_cursor(), 2, "clamps at end");
        fx.app.on_key(KeyCode::Up);
        assert_eq!(fx.app.finder_cursor(), 1);
    }

    #[test]
    fn finder_enter_with_no_matches_stays_open() {
        let mut fx = harness(&["a.txt"]);
        fx.app.on_key(KeyCode::Char('/'));
        for c in "zzz".chars() {
            fx.app.on_key(KeyCode::Char(c));
        }
        assert!(fx.app.finder_matches().is_empty());
        fx.app.on_key(KeyCode::Enter);
        assert_eq!(fx.app.mode(), Mode::FindFile);
    }

    #[test]
    fn slash_in_fullscreen_opens_finder_and_esc_returns_to_fullscreen() {
        let mut fx = harness(&["a.txt", "b.txt"]);
        fx.app.on_key(KeyCode::Enter);
        assert_eq!(fx.app.mode(), Mode::FullDiff);
        fx.app.on_key(KeyCode::Char('/'));
        assert_eq!(fx.app.mode(), Mode::FindFile);
        fx.app.on_key(KeyCode::Esc);
        assert_eq!(fx.app.mode(), Mode::FullDiff);
    }

    #[test]
    fn finder_enter_in_fullscreen_jumps_to_match_and_stays_fullscreen() {
        let mut fx = harness(&["a.txt", "b.txt"]);
        fx.app.on_key(KeyCode::Enter);
        assert_eq!(fx.app.mode(), Mode::FullDiff);
        fx.app.on_key(KeyCode::Char('/'));
        for c in "b".chars() {
            fx.app.on_key(KeyCode::Char(c));
        }
        assert_eq!(fx.app.finder_matches(), vec![1]);
        fx.app.on_key(KeyCode::Enter);
        assert_eq!(fx.app.mode(), Mode::FullDiff);
        assert_eq!(fx.app.selected_file().unwrap().path, "b.txt");
        let d = wait_for_diff(&mut fx.app, "b.txt");
        assert_eq!(d.path, "b.txt");
    }

    #[test]
    fn tab_cycles_focus_through_all_panels() {
        let mut fx = harness(&["a.txt"]);
        assert_eq!(fx.app.focus(), Focus::Status);
        fx.app.on_key(KeyCode::Tab);
        assert_eq!(fx.app.focus(), Focus::Branches);
        fx.app.on_key(KeyCode::Tab);
        assert_eq!(fx.app.focus(), Focus::Log);
        fx.app.on_key(KeyCode::Tab);
        assert_eq!(fx.app.focus(), Focus::Stash);
        // Tab skips the right-side Diff preview: Stash wraps to Status.
        fx.app.on_key(KeyCode::Tab);
        assert_eq!(fx.app.focus(), Focus::Status);
        // Diff is reached via `5` / Shift+Right, not via Tab.
        fx.app.on_key(KeyCode::Char('4'));
        assert_eq!(fx.app.focus(), Focus::Stash);
        fx.app.on_key(KeyCode::Char('3'));
        assert_eq!(fx.app.focus(), Focus::Log);
        fx.app.on_key(KeyCode::Char('2'));
        assert_eq!(fx.app.focus(), Focus::Branches);
        fx.app.on_key(KeyCode::Char('1'));
        assert_eq!(fx.app.focus(), Focus::Status);
        fx.app.on_key(KeyCode::Char('5'));
        assert_eq!(fx.app.focus(), Focus::Diff);
        // Tab from Diff drops back to the file list.
        fx.app.on_key(KeyCode::Tab);
        assert_eq!(fx.app.focus(), Focus::Status);
    }

    #[test]
    fn shift_right_from_files_focuses_diff_preview() {
        let mut fx = harness(&["a.txt"]);
        assert_eq!(fx.app.focus(), Focus::Status);
        // Plain Right expands folders and must stay in the file list.
        fx.app.on_key(KeyCode::Right);
        assert_eq!(fx.app.focus(), Focus::Status);
        // Shift+Right jumps to the right-side diff tab.
        fx.app.on_key_with_modifiers(KeyCode::Right, true);
        assert_eq!(fx.app.focus(), Focus::Diff);
        // Shift+Left (Left already owns focus_status) jumps back.
        fx.app.on_key_with_modifiers(KeyCode::Left, true);
        assert_eq!(fx.app.focus(), Focus::Status);
    }

    #[test]
    fn diff_focus_scrolls_preview_without_moving_selection() {
        let mut fx = two_hunk_fixture();
        assert_eq!(fx.app.focus(), Focus::Status);
        fx.app.on_key(KeyCode::Char('5'));
        assert_eq!(fx.app.focus(), Focus::Diff);
        let sel = fx.app.selected();
        assert_eq!(fx.app.diff_scroll(), 0);
        fx.app.on_key(KeyCode::Char('j'));
        assert_eq!(fx.app.diff_scroll(), 1);
        assert_eq!(fx.app.selected(), sel, "file cursor must not move");
        fx.app.on_key(KeyCode::Down);
        assert_eq!(fx.app.diff_scroll(), 2);
        fx.app.on_key(KeyCode::Char('k'));
        assert_eq!(fx.app.diff_scroll(), 1);
        // Left jumps back to the file list (focus_status owns Left).
        fx.app.on_key(KeyCode::Left);
        assert_eq!(fx.app.focus(), Focus::Status);
        // Enter from the preview opens fullscreen too.
        fx.app.on_key(KeyCode::Char('5'));
        fx.app.on_key(KeyCode::Enter);
        assert_eq!(fx.app.mode(), Mode::FullDiff);
    }

    #[test]
    fn diff_preview_scroll_clamps_at_content_end() {
        let mut fx = two_hunk_fixture();
        fx.app.on_key(KeyCode::Char('5'));
        assert_eq!(fx.app.focus(), Focus::Diff);
        let max = crate::ui::rows_unified_len(fx.app.diff_rows())
            .saturating_sub(1)
            .min(u16::MAX as usize) as u16;
        assert!(max > 2, "fixture must have scrollable content");
        for _ in 0..500 {
            fx.app.on_key(KeyCode::Char('j'));
        }
        assert_eq!(fx.app.diff_scroll(), max);
        for _ in 0..10 {
            fx.app.on_key(KeyCode::PageDown);
        }
        assert_eq!(fx.app.diff_scroll(), max, "PageDown must clamp too");
        for _ in 0..500 {
            fx.app.on_key(KeyCode::Char('k'));
        }
        assert_eq!(fx.app.diff_scroll(), 0);
        for _ in 0..10 {
            fx.app.on_key(KeyCode::PageUp);
        }
        assert_eq!(fx.app.diff_scroll(), 0, "PageUp must not underflow");
    }

    #[test]
    fn fullscreen_scroll_clamps_at_content_end() {
        let mut fx = two_hunk_fixture();
        fx.app.on_key(KeyCode::Enter);
        assert_eq!(fx.app.mode(), Mode::FullDiff);
        let max = fx
            .app
            .diff_rows()
            .len()
            .saturating_sub(1)
            .min(u16::MAX as usize) as u16;
        assert!(max > 2, "fixture must have scrollable content");
        for _ in 0..500 {
            fx.app.on_key(KeyCode::Down);
        }
        assert_eq!(fx.app.diff_scroll(), max);
        for _ in 0..500 {
            fx.app.on_key(KeyCode::Up);
        }
        assert_eq!(fx.app.diff_scroll(), 0);
    }

    #[test]
    fn diff_rows_are_paired_once_on_arrival() {
        use git_tui_core::diff::{DiffLine, FileDiff, Hunk, LineKind};
        let mut fx = harness(&["a.txt"]);
        assert!(fx.app.diff_rows().is_empty(), "no diff, no rows");
        fx.app.set_diff_for_test(
            FileDiff {
                path: "a.txt".into(),
                binary: false,
                hunks: vec![Hunk {
                    header: "@@ -1,1 +1,1 @@".into(),
                    old_start: 1,
                    new_start: 1,
                    lines: vec![
                        DiffLine {
                            kind: LineKind::Del,
                            text: "old".into(),
                        },
                        DiffLine {
                            kind: LineKind::Add,
                            text: "new".into(),
                        },
                    ],
                }],
            },
            false,
        );
        // Header + one paired del/add row, matching a fresh pairing.
        assert_eq!(fx.app.diff_rows().len(), 2);
        let fresh = crate::ui::diff_rows(fx.app.diff().unwrap());
        assert_eq!(fx.app.diff_rows(), fresh.as_slice());
    }

    #[test]
    fn moving_file_selection_loads_its_diff() {
        let mut fx = harness(&["a.txt", "b.txt"]);
        // Select b.txt; its diff should load automatically.
        fx.app.on_key(KeyCode::Char('j'));
        let d = wait_for_diff(&mut fx.app, "b.txt");
        assert_eq!(d.path, "b.txt");
    }

    #[test]
    fn fully_staged_file_loads_staged_diff() {
        let mut fx = harness(&["a.txt"]);
        fx.app.on_key(KeyCode::Char(' '));
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            fx.app.poll();
            if fx.app.diff_viewing_staged() == Some(true) {
                if let Some(d) = fx.app.diff() {
                    if !d.hunks.is_empty() {
                        return;
                    }
                }
            }
            assert!(Instant::now() < deadline, "staged diff never loaded");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn hunk_nav_clamps_and_snaps_scroll() {
        let mut fx = two_hunk_fixture();
        assert!(fx.app.diff().unwrap().hunks.len() >= 2);
        assert_eq!(fx.app.hunk(), 0);
        fx.app.on_key(KeyCode::Enter);
        assert_eq!(fx.app.mode(), Mode::FullDiff);
        fx.app.on_key(KeyCode::Char('J'));
        assert_eq!(fx.app.hunk(), 1);
        // Snaps the selected hunk to the top of the view.
        assert!(fx.app.diff_scroll() > 0);
        fx.app.on_key(KeyCode::Char('J'));
        assert_eq!(fx.app.hunk(), 1, "clamps at last hunk");
        fx.app.on_key(KeyCode::Char('K'));
        assert_eq!(fx.app.hunk(), 0);
    }

    #[test]
    fn jk_moves_line_cursor_and_syncs_hunk() {
        let mut fx = two_hunk_fixture();
        fx.app.on_key(KeyCode::Enter);
        assert_eq!(fx.app.mode(), Mode::FullDiff);
        assert_eq!(fx.app.cursor_row(), 0);
        fx.app.on_key(KeyCode::Char('j'));
        assert_eq!(fx.app.cursor_row(), 1);
        assert_eq!(fx.app.diff_scroll(), 1, "view follows the cursor");
        assert_eq!(fx.app.hunk(), 0, "still inside the first hunk");
        fx.app.on_key(KeyCode::Char('k'));
        assert_eq!(fx.app.cursor_row(), 0);
        assert_eq!(fx.app.diff_scroll(), 0);
    }

    #[test]
    fn shift_jk_jumps_between_hunks() {
        let mut fx = two_hunk_fixture();
        fx.app.on_key(KeyCode::Enter);
        fx.app.on_key(KeyCode::Char('J'));
        assert_eq!(fx.app.hunk(), 1);
        let start = crate::ui::rows_hunk_start(fx.app.diff_rows(), 1) as usize;
        assert_eq!(fx.app.cursor_row(), start);
        assert_eq!(fx.app.diff_scroll(), start as u16);
        // Clamps at the last hunk.
        fx.app.on_key(KeyCode::Char('J'));
        assert_eq!(fx.app.hunk(), 1);
        fx.app.on_key(KeyCode::Char('K'));
        assert_eq!(fx.app.hunk(), 0);
        assert_eq!(fx.app.cursor_row(), 0);
    }

    #[test]
    fn line_cursor_clamps_at_ends() {
        let mut fx = two_hunk_fixture();
        fx.app.on_key(KeyCode::Enter);
        let last = fx.app.diff_rows().len() - 1;
        assert!(last > 2, "fixture must have several rows");
        for _ in 0..500 {
            fx.app.on_key(KeyCode::Char('j'));
        }
        assert_eq!(fx.app.cursor_row(), last);
        assert_eq!(fx.app.diff_scroll(), last as u16);
        for _ in 0..500 {
            fx.app.on_key(KeyCode::Char('k'));
        }
        assert_eq!(fx.app.cursor_row(), 0);
        assert_eq!(fx.app.diff_scroll(), 0);
    }

    #[test]
    fn cursor_moves_within_view_before_scrolling_down() {
        let mut fx = two_hunk_fixture();
        fx.app.on_key(KeyCode::Enter);
        fx.app.set_view_h_for_test(4, 4);
        fx.app.on_key(KeyCode::Char('j'));
        fx.app.on_key(KeyCode::Char('j'));
        fx.app.on_key(KeyCode::Char('j'));
        assert_eq!(fx.app.cursor_row(), 3);
        assert_eq!(fx.app.diff_scroll(), 0, "view must not scroll yet");
        fx.app.on_key(KeyCode::Char('j'));
        assert_eq!(fx.app.cursor_row(), 4);
        assert_eq!(fx.app.diff_scroll(), 1, "scrolls only at the edge");
        fx.app.on_key(KeyCode::Char('j'));
        assert_eq!(fx.app.cursor_row(), 5);
        assert_eq!(fx.app.diff_scroll(), 2);
    }

    #[test]
    fn scrolling_up_follows_the_top_edge() {
        let mut fx = two_hunk_fixture();
        fx.app.on_key(KeyCode::Enter);
        fx.app.set_view_h_for_test(4, 4);
        for _ in 0..5 {
            fx.app.on_key(KeyCode::Char('j'));
        }
        assert_eq!((fx.app.cursor_row(), fx.app.diff_scroll()), (5, 2));
        fx.app.on_key(KeyCode::Char('k'));
        assert_eq!(fx.app.diff_scroll(), 2, "still visible, no scroll");
        fx.app.on_key(KeyCode::Char('k'));
        fx.app.on_key(KeyCode::Char('k'));
        assert_eq!(fx.app.diff_scroll(), 2);
        fx.app.on_key(KeyCode::Char('k'));
        assert_eq!((fx.app.cursor_row(), fx.app.diff_scroll()), (1, 1));
    }

    #[test]
    fn preview_follows_unified_lines_at_edges() {
        let mut fx = two_hunk_fixture();
        fx.app.on_key(KeyCode::Char('5'));
        assert_eq!(fx.app.focus(), Focus::Diff);
        fx.app.set_view_h_for_test(4, 4);
        fx.app.on_key(KeyCode::Char('j'));
        fx.app.on_key(KeyCode::Char('j'));
        fx.app.on_key(KeyCode::Char('j'));
        assert_eq!(fx.app.cursor_row(), 3);
        assert_eq!(fx.app.diff_scroll(), 0, "three context lines fit");
        // Row 4 is a del/add pair (two unified lines): the whole row
        // must become visible.
        fx.app.on_key(KeyCode::Char('j'));
        assert_eq!(fx.app.cursor_row(), 4);
        assert_eq!(fx.app.diff_scroll(), 2);
        fx.app.on_key(KeyCode::Char('k'));
        fx.app.on_key(KeyCode::Char('k'));
        assert_eq!(fx.app.diff_scroll(), 2, "still visible, no scroll");
        fx.app.on_key(KeyCode::Char('k'));
        assert_eq!((fx.app.cursor_row(), fx.app.diff_scroll()), (1, 1));
    }

    #[test]
    fn page_keys_move_cursor_with_follow() {
        let mut fx = two_hunk_fixture();
        fx.app.on_key(KeyCode::Enter);
        fx.app.set_view_h_for_test(5, 5);
        fx.app.on_key(KeyCode::PageDown);
        assert_eq!(fx.app.cursor_row(), 10);
        assert_eq!(fx.app.hunk(), 1, "crossed into the second hunk");
        assert_eq!(fx.app.diff_scroll(), 6);
        fx.app.on_key(KeyCode::PageUp);
        assert_eq!((fx.app.cursor_row(), fx.app.diff_scroll()), (0, 0));
    }

    #[test]
    fn arrows_move_line_cursor_in_fullscreen() {
        let mut fx = two_hunk_fixture();
        fx.app.on_key(KeyCode::Enter);
        fx.app.on_key(KeyCode::Down);
        assert_eq!(fx.app.cursor_row(), 1);
        assert_eq!(fx.app.diff_scroll(), 1);
        assert_eq!(fx.app.hunk(), 0, "arrow move must not jump hunks");
        fx.app.on_key(KeyCode::Up);
        assert_eq!(fx.app.cursor_row(), 0);
    }

    #[test]
    fn preview_jk_moves_line_cursor() {
        let mut fx = two_hunk_fixture();
        fx.app.on_key(KeyCode::Char('5'));
        assert_eq!(fx.app.focus(), Focus::Diff);
        assert_eq!(fx.app.cursor_row(), 0);
        fx.app.on_key(KeyCode::Char('j'));
        assert_eq!(fx.app.cursor_row(), 1);
        assert_eq!(fx.app.diff_scroll(), 1);
        assert_eq!(fx.app.hunk(), 0);
        fx.app.on_key(KeyCode::Char('k'));
        assert_eq!(fx.app.cursor_row(), 0);
        assert_eq!(fx.app.diff_scroll(), 0);
    }

    #[test]
    fn hl_moves_column_and_clamps_to_line_end() {
        let mut fx = two_hunk_fixture();
        fx.app.on_key(KeyCode::Enter);
        // Row 1 is a short context line; walk right past its end.
        fx.app.on_key(KeyCode::Char('j'));
        assert_eq!(fx.app.cursor_row(), 1);
        assert_eq!(fx.app.cursor_col(), 0);
        fx.app.on_key(KeyCode::Char('l'));
        assert_eq!(fx.app.cursor_col(), 1);
        let len = crate::ui::cursor_line_text(fx.app.diff().unwrap(), &fx.app.diff_rows()[1])
            .chars()
            .count();
        assert!(len > 2, "fixture line must have some width");
        for _ in 0..500 {
            fx.app.on_key(KeyCode::Char('l'));
        }
        assert_eq!(fx.app.cursor_col(), len - 1, "clamps on last char like $");
        fx.app.on_key(KeyCode::Char('h'));
        assert_eq!(fx.app.cursor_col(), len - 2);
        for _ in 0..500 {
            fx.app.on_key(KeyCode::Char('h'));
        }
        assert_eq!(fx.app.cursor_col(), 0);
    }

    #[test]
    fn column_is_sticky_across_rows_with_home_end() {
        let mut fx = two_hunk_fixture();
        fx.app.on_key(KeyCode::Enter);
        fx.app.on_key(KeyCode::Char('j'));
        fx.app.on_key(KeyCode::Char('l'));
        fx.app.on_key(KeyCode::Char('l'));
        fx.app.on_key(KeyCode::Char('l'));
        assert_eq!(fx.app.cursor_col(), 3);
        // Moving rows keeps the desired column (nvim sticky column).
        fx.app.on_key(KeyCode::Char('j'));
        assert_eq!(fx.app.cursor_row(), 2);
        assert_eq!(fx.app.cursor_col(), 3);
        fx.app.on_key(KeyCode::Char('k'));
        assert_eq!(fx.app.cursor_row(), 1);
        assert_eq!(fx.app.cursor_col(), 3);
        fx.app.on_key(KeyCode::End);
        let len = crate::ui::cursor_line_text(fx.app.diff().unwrap(), &fx.app.diff_rows()[1])
            .chars()
            .count();
        assert_eq!(fx.app.cursor_col(), len - 1);
        fx.app.on_key(KeyCode::Home);
        assert_eq!(fx.app.cursor_col(), 0);
        fx.app.on_key(KeyCode::Char('0'));
        assert_eq!(fx.app.cursor_col(), 0);
    }

    #[test]
    fn arrows_move_column_in_fullscreen() {
        let mut fx = two_hunk_fixture();
        fx.app.on_key(KeyCode::Enter);
        fx.app.on_key(KeyCode::Char('j'));
        fx.app.on_key(KeyCode::Right);
        assert_eq!(fx.app.cursor_col(), 1);
        fx.app.on_key(KeyCode::Left);
        assert_eq!(fx.app.cursor_col(), 0);
        fx.app.on_key(KeyCode::Left);
        assert_eq!(fx.app.cursor_col(), 0, "clamps at line start");
    }

    #[test]
    fn preview_hl_moves_column_and_left_still_leaves() {
        let mut fx = two_hunk_fixture();
        fx.app.on_key(KeyCode::Char('5'));
        assert_eq!(fx.app.focus(), Focus::Diff);
        fx.app.on_key(KeyCode::Char('l'));
        assert_eq!(fx.app.cursor_col(), 1);
        fx.app.on_key(KeyCode::Char('h'));
        assert_eq!(fx.app.cursor_col(), 0);
        // Plain Left keeps its back-to-files job in the preview.
        fx.app.on_key(KeyCode::Left);
        assert_eq!(fx.app.focus(), Focus::Status);
    }

    #[test]
    fn visual_linewise_yank_joins_whole_rows() {
        let mut fx = visual_fixture();
        fx.app.on_key(KeyCode::Enter);
        // Anchor on the header, extend one row down, yank whole lines.
        fx.app.on_key(KeyCode::Char('V'));
        fx.app.on_key(KeyCode::Char('j'));
        assert_eq!(
            fx.app.yank_text().as_deref(),
            Some("@@ -1,2 +1,2 @@\nsame\n")
        );
    }

    #[test]
    fn visual_charwise_yank_slices_endpoints() {
        let mut fx = visual_fixture();
        fx.app.on_key(KeyCode::Enter);
        fx.app.on_key(KeyCode::Char('j'));
        fx.app.on_key(KeyCode::Char('l'));
        fx.app.on_key(KeyCode::Char('v'));
        fx.app.on_key(KeyCode::Char('j'));
        // Anchor (1,1) on `same`, cursor (2,1) on the `new` side.
        assert_eq!(fx.app.yank_text().as_deref(), Some("ame\nne"));
    }

    #[test]
    fn visual_charwise_normalizes_reversed_selection() {
        let mut fx = visual_fixture();
        fx.app.on_key(KeyCode::Enter);
        fx.app.on_key(KeyCode::Char('j'));
        fx.app.on_key(KeyCode::Char('j'));
        fx.app.on_key(KeyCode::Char('l'));
        // Anchor below, move up: same text as extending downward.
        fx.app.on_key(KeyCode::Char('v'));
        fx.app.on_key(KeyCode::Char('k'));
        assert_eq!(fx.app.yank_text().as_deref(), Some("ame\nne"));
    }

    #[test]
    fn yank_without_visual_takes_cursor_line() {
        let mut fx = visual_fixture();
        fx.app.on_key(KeyCode::Enter);
        fx.app.on_key(KeyCode::Char('j'));
        assert_eq!(fx.app.yank_text().as_deref(), Some("same\n"));
        fx.app.on_key(KeyCode::Char('y'));
        assert!(fx.app.visual_selection().is_none());
        let notice = fx.app.notice().unwrap_or("").to_string();
        assert!(notice.contains("yanked"), "got: {notice:?}");
    }

    #[test]
    fn esc_leaves_visual_before_closing_fullscreen() {
        let mut fx = visual_fixture();
        fx.app.on_key(KeyCode::Enter);
        fx.app.on_key(KeyCode::Char('v'));
        assert!(fx.app.visual_selection().is_some());
        fx.app.on_key(KeyCode::Esc);
        assert!(fx.app.visual_selection().is_none());
        assert_eq!(fx.app.mode(), Mode::FullDiff, "still fullscreen");
        fx.app.on_key(KeyCode::Esc);
        assert_eq!(fx.app.mode(), Mode::Normal);
    }

    #[test]
    fn visual_toggle_and_mode_switch() {
        let mut fx = visual_fixture();
        fx.app.on_key(KeyCode::Enter);
        fx.app.on_key(KeyCode::Char('v'));
        assert!(fx.app.visual_selection().is_some());
        // Same key again leaves visual.
        fx.app.on_key(KeyCode::Char('v'));
        assert!(fx.app.visual_selection().is_none());
        // V enters linewise; v switches it back to charwise.
        fx.app.on_key(KeyCode::Char('V'));
        assert_eq!(fx.app.visual_selection().map(|s| s.2), Some(true));
        fx.app.on_key(KeyCode::Char('v'));
        assert_eq!(fx.app.visual_selection().map(|s| s.2), Some(false));
    }

    #[test]
    fn base64_vectors() {
        assert_eq!(super::base64_encode(b""), "");
        assert_eq!(super::base64_encode(b"f"), "Zg==");
        assert_eq!(super::base64_encode(b"fo"), "Zm8=");
        assert_eq!(super::base64_encode(b"foo"), "Zm9v");
        assert_eq!(super::base64_encode(b"hello"), "aGVsbG8=");
    }

    /// One-hunk diff with known text for visual tests: header, a `same`
    /// context line, and an old/new pair.
    fn visual_fixture() -> Fixture {
        use git_tui_core::diff::{DiffLine, FileDiff, Hunk, LineKind};
        let mut fx = harness(&["a.txt"]);
        fx.app.set_diff_for_test(
            FileDiff {
                path: "a.txt".into(),
                binary: false,
                hunks: vec![Hunk {
                    header: "@@ -1,2 +1,2 @@".into(),
                    old_start: 1,
                    new_start: 1,
                    lines: vec![
                        DiffLine {
                            kind: LineKind::Context,
                            text: "same".into(),
                        },
                        DiffLine {
                            kind: LineKind::Del,
                            text: "old".into(),
                        },
                        DiffLine {
                            kind: LineKind::Add,
                            text: "new".into(),
                        },
                    ],
                }],
            },
            false,
        );
        fx
    }

    #[test]
    fn cursor_resets_when_diff_reloads() {
        use git_tui_core::diff::{DiffLine, FileDiff, Hunk, LineKind};
        let mut fx = harness(&["a.txt"]);
        let diff = FileDiff {
            path: "a.txt".into(),
            binary: false,
            hunks: vec![Hunk {
                header: "@@ -1,2 +1,2 @@".into(),
                old_start: 1,
                new_start: 1,
                lines: vec![
                    DiffLine {
                        kind: LineKind::Context,
                        text: "same".into(),
                    },
                    DiffLine {
                        kind: LineKind::Del,
                        text: "old".into(),
                    },
                    DiffLine {
                        kind: LineKind::Add,
                        text: "new".into(),
                    },
                ],
            }],
        };
        fx.app.set_diff_for_test(diff.clone(), false);
        fx.app.on_key(KeyCode::Enter);
        fx.app.on_key(KeyCode::Char('j'));
        assert_eq!(fx.app.cursor_row(), 1);
        fx.app.set_diff_for_test(diff, false);
        assert_eq!(fx.app.cursor_row(), 0, "fresh diff resets the cursor");
        assert_eq!(fx.app.hunk(), 0);
        assert_eq!(fx.app.diff_scroll(), 0);
    }

    #[test]
    fn space_in_diff_stages_selected_hunk() {
        let mut fx = two_hunk_fixture();
        let before = fx.app.diff().unwrap().hunks.len();
        fx.app.on_key(KeyCode::Enter);
        fx.app.on_key(KeyCode::Char(' '));
        // Partial staging: status shows Both, diff reloads with one less hunk.
        let st = wait_for(&mut fx.app, |st| {
            st.files
                .iter()
                .any(|e| e.path == "a.txt" && e.state == FileState::BothStagedAndUnstaged)
        });
        let _ = st;
        let d = wait_for_diff(&mut fx.app, "a.txt");
        assert_eq!(d.hunks.len(), before - 1);
    }

    #[test]
    fn space_in_staged_diff_view_errors() {
        let mut fx = harness(&["a.txt"]);
        fx.app.on_key(KeyCode::Char(' '));
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            fx.app.poll();
            if fx.app.diff_viewing_staged() == Some(true) {
                break;
            }
            assert!(Instant::now() < deadline, "staged diff never loaded");
            std::thread::sleep(Duration::from_millis(10));
        }
        fx.app.on_key(KeyCode::Enter);
        fx.app.on_key(KeyCode::Char(' '));
        assert!(fx.app.error().is_some(), "expected staged-hunk error");
    }

    fn wait_for_branches(app: &mut App, min_count: usize) -> Vec<BranchInfo> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            app.poll();
            if let Some(b) = app.branches() {
                if b.len() >= min_count {
                    return b.to_vec();
                }
            }
            assert!(Instant::now() < deadline, "timed out waiting for branches");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn focus_branches(fx: &mut Fixture) {
        fx.app.on_key(KeyCode::Char('2'));
        assert_eq!(fx.app.focus(), Focus::Branches);
    }

    /// Move branch selection to the named branch (sort-order independent).
    fn select_branch(fx: &mut Fixture, name: &str) {
        let branches = wait_for_branches(&mut fx.app, 1);
        let pos = branches
            .iter()
            .position(|b| b.name == name)
            .unwrap_or_else(|| panic!("branch {name} missing in {branches:?}"));
        while fx.app.branch_selected() < pos {
            fx.app.on_key(KeyCode::Char('j'));
        }
        while fx.app.branch_selected() > pos {
            fx.app.on_key(KeyCode::Char('k'));
        }
    }

    #[test]
    fn panels_load_eagerly_at_startup_without_focusing() {
        let mut fx = harness(&["a.txt"]);
        // No focus keys pressed: branches, log, and stash must arrive on
        // their own so every pane shows data on launch.
        let branches = wait_for_branches(&mut fx.app, 1);
        assert!(branches.iter().any(|b| b.name == "main" && b.is_head));
        let entries = wait_for_log(&mut fx.app, 1);
        assert_eq!(entries[0].summary, "init");
        let stash = wait_for_stash(&mut fx.app, 0);
        assert!(stash.is_empty());
    }

    #[test]
    fn branches_load_when_panel_focused() {
        let mut fx = harness(&["a.txt"]);
        // Eager at startup; focusing must keep working.
        focus_branches(&mut fx);
        let branches = wait_for_branches(&mut fx.app, 1);
        assert!(branches.iter().any(|b| b.name == "main" && b.is_head));
    }

    #[test]
    fn create_branch_via_modal() {
        let mut fx = harness(&["a.txt"]);
        focus_branches(&mut fx);
        wait_for_branches(&mut fx.app, 1);
        fx.app.on_key(KeyCode::Char('a'));
        assert_eq!(fx.app.mode(), Mode::NewBranch);
        for c in "feat".chars() {
            fx.app.on_key(KeyCode::Char(c));
        }
        fx.app.on_key(KeyCode::Enter);
        assert_eq!(fx.app.mode(), Mode::Normal);
        let branches = wait_for_branches(&mut fx.app, 2);
        assert!(branches.iter().any(|b| b.name == "feat"));
    }

    #[test]
    fn checkout_branch_via_enter() {
        let mut fx = harness(&["a.txt"]);
        focus_branches(&mut fx);
        wait_for_branches(&mut fx.app, 1);
        // Create feat, then move selection to it and check out.
        fx.app.on_key(KeyCode::Char('a'));
        for c in "feat".chars() {
            fx.app.on_key(KeyCode::Char(c));
        }
        fx.app.on_key(KeyCode::Enter);
        wait_for_branches(&mut fx.app, 2);
        select_branch(&mut fx, "feat");
        fx.app.on_key(KeyCode::Enter);
        let st = wait_for(&mut fx.app, |st| st.branch == "feat");
        assert_eq!(st.branch, "feat");
    }

    #[test]
    fn delete_branch_via_key() {
        let mut fx = harness(&["a.txt"]);
        focus_branches(&mut fx);
        wait_for_branches(&mut fx.app, 1);
        fx.app.on_key(KeyCode::Char('a'));
        for c in "gone".chars() {
            fx.app.on_key(KeyCode::Char(c));
        }
        fx.app.on_key(KeyCode::Enter);
        wait_for_branches(&mut fx.app, 2);
        // Select "gone" and delete it.
        select_branch(&mut fx, "gone");
        fx.app.on_key(KeyCode::Char('D'));
        let branches = wait_for_branches(&mut fx.app, 1);
        // Wait until "gone" disappears (list reloads after MutationDone).
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            fx.app.poll();
            if let Some(b) = fx.app.branches() {
                if !b.iter().any(|x| x.name == "gone") {
                    break;
                }
            }
            assert!(
                Instant::now() < deadline,
                "gone never deleted: {branches:?}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn delete_checked_out_branch_surfaces_error() {
        let mut fx = harness(&["a.txt"]);
        focus_branches(&mut fx);
        wait_for_branches(&mut fx.app, 1);
        // "main" is checked out; deleting it must fail loudly.
        fx.app.on_key(KeyCode::Char('D'));
        let err = wait_for_error(&mut fx.app);
        assert!(err.contains("checked out"), "got: {err}");
    }

    fn wait_for_log(app: &mut App, min_count: usize) -> Vec<CommitInfo> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            app.poll();
            if let Some(entries) = app.log() {
                if entries.len() >= min_count {
                    return entries.to_vec();
                }
            }
            assert!(Instant::now() < deadline, "timed out waiting for log");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn log_loads_newest_first_when_focused() {
        let mut fx = harness(&["a.txt"]);
        // Eager at startup; focusing must keep working.
        fx.app.on_key(KeyCode::Char('3'));
        let entries = wait_for_log(&mut fx.app, 1);
        assert_eq!(entries[0].summary, "init");
    }

    #[test]
    fn new_commit_appears_at_top_of_log() {
        let mut fx = harness(&["a.txt"]);
        // Stage + commit, then open the log.
        fx.app.on_key(KeyCode::Char(' '));
        wait_for(&mut fx.app, |st| {
            st.files
                .iter()
                .any(|e| e.path == "a.txt" && e.state == FileState::Staged)
        });
        fx.app.on_key(KeyCode::Char('c'));
        for c in "second".chars() {
            fx.app.on_key(KeyCode::Char(c));
        }
        fx.app.on_key(KeyCode::Enter);
        fx.app.on_key(KeyCode::Char('3'));
        let entries = wait_for_log(&mut fx.app, 2);
        assert_eq!(entries[0].summary, "second");
        assert_eq!(entries[1].summary, "init");
    }

    fn wait_for_stash(app: &mut App, count: usize) -> Vec<StashEntry> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            app.poll();
            if let Some(entries) = app.stash() {
                if entries.len() == count {
                    return entries.to_vec();
                }
            }
            assert!(Instant::now() < deadline, "timed out waiting for stash");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn focus_stash(fx: &mut Fixture) {
        fx.app.on_key(KeyCode::Char('4'));
        assert_eq!(fx.app.focus(), Focus::Stash);
    }

    #[test]
    fn stash_push_cleans_status_and_lists_entry() {
        let mut fx = harness(&["a.txt"]);
        focus_stash(&mut fx);
        wait_for_stash(&mut fx.app, 0);
        fx.app.on_key(KeyCode::Char('a'));
        assert_eq!(fx.app.mode(), Mode::StashPush);
        for c in "wip".chars() {
            fx.app.on_key(KeyCode::Char(c));
        }
        fx.app.on_key(KeyCode::Enter);
        assert_eq!(fx.app.mode(), Mode::Normal);
        let entries = wait_for_stash(&mut fx.app, 1);
        assert!(
            entries[0].message.contains("wip"),
            "got {:?}",
            entries[0].message
        );
        // Status is clean after the push.
        let st = wait_for(&mut fx.app, |st| st.files.is_empty());
        assert!(st.files.is_empty());
    }

    #[test]
    fn stash_pop_restores_changes() {
        let mut fx = harness(&["a.txt"]);
        focus_stash(&mut fx);
        wait_for_stash(&mut fx.app, 0);
        fx.app.on_key(KeyCode::Char('a'));
        for c in "wip".chars() {
            fx.app.on_key(KeyCode::Char(c));
        }
        fx.app.on_key(KeyCode::Enter);
        wait_for_stash(&mut fx.app, 1);
        // Pop it back.
        fx.app.on_key(KeyCode::Enter);
        wait_for_stash(&mut fx.app, 0);
        let st = wait_for(&mut fx.app, |st| st.files.iter().any(|e| e.path == "a.txt"));
        assert_eq!(st.files[0].state, FileState::Unstaged);
    }

    #[test]
    fn stash_drop_removes_entry() {
        let mut fx = harness(&["a.txt"]);
        focus_stash(&mut fx);
        wait_for_stash(&mut fx.app, 0);
        fx.app.on_key(KeyCode::Char('a'));
        for c in "wip".chars() {
            fx.app.on_key(KeyCode::Char(c));
        }
        fx.app.on_key(KeyCode::Enter);
        wait_for_stash(&mut fx.app, 1);
        fx.app.on_key(KeyCode::Char('D'));
        wait_for_stash(&mut fx.app, 0);
    }

    #[test]
    fn overridden_bindings_take_effect() {
        use crate::config::Config;
        let (dir, repo) = init_repo();
        commit_file(&repo, "a.txt", "base\n", "init");
        {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(repo.workdir().unwrap().join("a.txt"))
                .unwrap();
            f.write_all(b"dirty\n").unwrap();
        }
        let path = repo.workdir().unwrap().to_path_buf();
        drop(repo);
        let keys = crate::config::KeyBindings {
            stage: vec![KeyCode::Char('s')],
            ..Default::default()
        };
        let config = Config {
            keys,
            ..Default::default()
        };
        let mut fx = Fixture {
            _dir: dir,
            app: App::new_with_config(JobQueue::spawn(path).unwrap(), config),
        };
        wait_for(&mut fx.app, |st| st.files.iter().any(|e| e.path == "a.txt"));
        // Space is no longer bound: nothing happens.
        fx.app.on_key(KeyCode::Char(' '));
        std::thread::sleep(Duration::from_millis(200));
        fx.app.poll();
        assert!(fx.app.error().is_none());
        let st = fx.app.status().unwrap();
        assert_eq!(st.files[0].state, FileState::Unstaged);
        // "s" stages.
        fx.app.on_key(KeyCode::Char('s'));
        let st = wait_for(&mut fx.app, |st| {
            st.files
                .iter()
                .any(|e| e.path == "a.txt" && e.state == FileState::Staged)
        });
        assert_eq!(st.files[0].state, FileState::Staged);
    }

    #[test]
    fn shift_a_in_commit_box_starts_generation_and_no_staged_errors() {
        let mut fx = harness(&["a.txt"]);
        fx.app.on_key(KeyCode::Char('c'));
        assert_eq!(fx.app.mode(), Mode::Committing);
        // Legacy single-KeyCode path: `A` means Shift+A.
        fx.app.on_key(KeyCode::Char('A'));
        assert!(fx.app.is_generating());
        let err = wait_for_error(&mut fx.app);
        assert!(
            err.contains("nothing staged") || err.contains("API key"),
            "got: {err}"
        );
        assert!(!fx.app.is_generating());
        assert_eq!(fx.app.mode(), Mode::Committing, "stay in the box to edit");
    }

    #[test]
    fn shift_a_without_shift_types_literal_a() {
        let mut fx = harness(&["a.txt"]);
        fx.app.on_key(KeyCode::Char('c'));
        // Real modifier path: no Shift means a literal `A` (caps lock).
        fx.app.on_key_with_modifiers(KeyCode::Char('A'), false);
        assert!(!fx.app.is_generating());
        assert_eq!(fx.app.draft(), "A");
        // Lowercase always types.
        fx.app.on_key_with_modifiers(KeyCode::Char('a'), false);
        assert_eq!(fx.app.draft(), "Aa");
        // Shift+A generates instead of typing.
        fx.app.on_key_with_modifiers(KeyCode::Char('a'), true);
        assert!(fx.app.is_generating());
        assert_eq!(
            fx.app.draft(),
            "Aa",
            "generation must not type into the draft"
        );
    }

    #[test]
    fn generated_message_fills_draft_and_moves_cursor_to_end() {
        let mut fx = harness(&["a.txt"]);
        fx.app.on_key(KeyCode::Char('c'));
        assert_eq!(fx.app.mode(), Mode::Committing);
        fx.app.generating = true;
        fx.app
            .apply(git_tui_core::jobqueue::AsyncResult::GeneratedMessage(
                "feat: add thing".into(),
            ));
        assert_eq!(fx.app.draft(), "feat: add thing");
        assert_eq!(fx.app.draft_cursor(), fx.app.draft().chars().count());
        assert!(!fx.app.is_generating());
        assert!(fx.app.error().is_none());
    }

    #[test]
    fn generated_message_arriving_after_esc_is_ignored() {
        let mut fx = harness(&["a.txt"]);
        fx.app.on_key(KeyCode::Char('c'));
        fx.app.generating = true;
        fx.app.on_key(KeyCode::Esc);
        assert_eq!(fx.app.mode(), Mode::Normal);
        fx.app.generating = true;
        fx.app
            .apply(git_tui_core::jobqueue::AsyncResult::GeneratedMessage(
                "feat: late".into(),
            ));
        assert!(!fx.app.is_generating());
        assert_eq!(fx.app.draft(), "", "closed box must not be filled");
    }

    #[test]
    fn shift_a_in_file_list_opens_llm_settings_prefilled() {
        let mut fx = harness(&["a.txt"]);
        assert_eq!(fx.app.mode(), Mode::Normal);
        fx.app.on_key(KeyCode::Char('A'));
        assert_eq!(fx.app.mode(), Mode::LlmSettings);
        assert_eq!(fx.app.llm_selected(), 0);
        // Prefilled from the session config (defaults here).
        assert_eq!(fx.app.llm_field_value(0), "openai");
        assert_eq!(fx.app.llm_field_value(1), "gpt-4o-mini");
    }

    #[test]
    fn llm_settings_tab_switches_fields_and_typing_edits() {
        let mut fx = harness(&["a.txt"]);
        fx.app.on_key(KeyCode::Char('A'));
        fx.app.on_key(KeyCode::Tab);
        assert_eq!(fx.app.llm_selected(), 1);
        for c in "x-model".chars() {
            fx.app.on_key(KeyCode::Char(c));
        }
        assert!(fx.app.llm_field_value(1).contains("x-model"));
        fx.app.on_key(KeyCode::Up);
        assert_eq!(fx.app.llm_selected(), 0);
        // Row 0 kept its prefilled value (stash/unstash round-trip).
        assert_eq!(fx.app.llm_field_value(0), "openai");
    }

    #[test]
    fn llm_settings_esc_cancels_without_applying() {
        let mut fx = harness(&["a.txt"]);
        fx.app.on_key(KeyCode::Char('A'));
        fx.app.on_key(KeyCode::Tab);
        fx.app.on_key(KeyCode::Tab);
        for c in "junk".chars() {
            fx.app.on_key(KeyCode::Char(c));
        }
        fx.app.on_key(KeyCode::Esc);
        assert_eq!(fx.app.mode(), Mode::Normal);
        assert_eq!(fx.app.llm.provider, "openai", "cancel must not apply");
        assert!(fx.app.notice().is_none());
    }

    #[test]
    fn llm_settings_enter_with_bad_provider_stays_open() {
        let mut fx = harness(&["a.txt"]);
        fx.app.on_key(KeyCode::Char('A'));
        fx.app.clear_draft();
        for c in "skynet".chars() {
            fx.app.on_key(KeyCode::Char(c));
        }
        fx.app.on_key(KeyCode::Enter);
        assert_eq!(fx.app.mode(), Mode::LlmSettings, "bad input stays open");
        assert!(fx.app.error().unwrap_or("").contains("unknown provider"));
        assert_eq!(fx.app.llm.provider, "openai", "bad input not applied");
    }

    #[test]
    fn llm_settings_enter_saves_and_persists_to_file() {
        let mut fx = harness(&["a.txt"]);
        let dir = tempfile::TempDir::new().unwrap();
        let cfg_path = dir.path().join("config.toml");
        fx.app.set_config_path(Some(cfg_path.clone()));
        fx.app.on_key(KeyCode::Char('A'));
        // Provider row: replace "openai" with "ollama".
        fx.app.clear_draft();
        for c in "ollama".chars() {
            fx.app.on_key(KeyCode::Char(c));
        }
        fx.app.on_key(KeyCode::Enter);
        assert_eq!(fx.app.mode(), Mode::Normal);
        assert_eq!(fx.app.llm.provider, "ollama");
        assert!(fx.app.notice().unwrap_or("").contains("saved"));
        // Round-trips through the real config file.
        let cfg = Config::load_from_path(&cfg_path).unwrap();
        assert_eq!(cfg.llm.provider, "ollama");
    }
}
