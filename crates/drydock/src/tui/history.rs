//! The history view: a repo's recent commits down the left, and what the
//! selected one changed down the right.
//!
//! It's for looking, not doing. Space opens it on the selected row and space
//! closes it again, so checking what just landed in a repo costs two keys
//! rather than a git client window. Nothing in here writes to a repo.
//!
//! Every git call runs in the background and answers through the main loop,
//! tagged with what it was for. Holding `j` through a long list starts a diff
//! for each commit it passes; each one cancels the last, and an answer for a
//! commit the cursor has already left is dropped on arrival.

use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
    Frame,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use super::ui::{pad, ACCENT, BEHIND, CLEAN, DIM, DIRTY, TROUBLE, UNPUSHED, UNRELEASED};
use super::{App, Input, Mode};
use crate::commits::{
    self, Changes, Commit, FileStatus, Flow, LineKind, Log, LogRow, PatchLine, RefKind, RefLabel,
    Scope,
};
use crate::fmt;
use crate::model::RepoStatus;

/// How many diffs to keep in hand, so stepping back and forth between two
/// commits doesn't ask git twice. Dropped wholesale past this; they're cheap.
const CACHE_LIMIT: usize = 64;

/// Widest the graph gets before it's cut. A repo with a dozen long-lived
/// branches draws a graph wider than the subjects it's meant to sit beside.
const GRAPH_MAX: usize = 16;

/// Lines the wheel scrolls the diff by.
const WHEEL_LINES: usize = 3;

/// What the right-hand pane can be showing.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Entry {
    Uncommitted,
    Commit(String),
}

/// An answer from git, tagged with the question, so one that arrives after
/// the view has moved on can be recognised and dropped.
pub enum Loaded {
    Log {
        root: PathBuf,
        seq: u64,
        result: Result<Log, String>,
    },
    Changes {
        root: PathBuf,
        entry: Entry,
        result: Result<Arc<Changes>, String>,
    },
}

/// The parts of a row that, when they move, mean the list is out of date.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Fingerprint {
    head: Option<String>,
    dirty: bool,
    ahead: u32,
    behind: u32,
}

impl Fingerprint {
    fn of(repo: &RepoStatus) -> Self {
        Self {
            head: repo.head_sha().map(str::to_string),
            dirty: repo.flags().dirty,
            ahead: repo.branch_unpushed(),
            behind: repo.branch_behind(),
        }
    }
}

pub struct HistoryView {
    pub root: PathBuf,
    slug: String,
    branch: String,
    upstream: Option<String>,
    scope: Scope,
    /// The CHANGES cell for a dirty repo, which is also what puts the
    /// uncommitted entry at the top of the list. `None` when clean.
    dirty: Option<String>,
    fingerprint: Fingerprint,
    /// `None` until the first answer arrives. A reload keeps the old list on
    /// screen until the new one lands, rather than blanking it.
    log: Option<Result<Log, String>>,
    graph_width: usize,
    /// The row the cursor is on. Rows are the uncommitted entry, when there
    /// is one, then the log's own rows, graph-only lines included.
    cursor: usize,
    list_scroll: usize,
    diff_scroll: usize,
    changes: HashMap<Entry, Result<Arc<Changes>, String>>,
    log_seq: u64,
    log_task: Option<JoinHandle<()>>,
    changes_task: Option<(Entry, JoinHandle<()>)>,
}

impl Drop for HistoryView {
    // Dropping a task handle detaches it rather than stopping it, and a diff
    // of a big commit can run for a while after the view that wanted it has
    // gone. Aborting drops the git child, which kills it.
    fn drop(&mut self) {
        if let Some(task) = self.log_task.take() {
            task.abort();
        }
        if let Some((_, task)) = self.changes_task.take() {
            task.abort();
        }
    }
}

/// One row of the list, borrowed.
enum Row<'a> {
    Uncommitted,
    Commit(&'a str, &'a Commit),
    Graph(&'a str),
}

impl HistoryView {
    pub fn new(repo: &RepoStatus) -> Self {
        let upstream = repo
            .refs
            .as_ref()
            .and_then(|r| r.current_branch())
            .filter(|b| !b.gone)
            .and_then(|b| b.upstream.clone());
        Self {
            root: repo.root.clone(),
            slug: repo.slug(),
            branch: repo.branch_label(),
            upstream,
            scope: Scope::Branch,
            dirty: dirty_label(repo),
            fingerprint: Fingerprint::of(repo),
            log: None,
            graph_width: 0,
            cursor: 0,
            list_scroll: 0,
            diff_scroll: 0,
            changes: HashMap::new(),
            log_seq: 0,
            log_task: None,
            changes_task: None,
        }
    }

    fn offset(&self) -> usize {
        usize::from(self.dirty.is_some())
    }

    fn log_rows(&self) -> &[LogRow] {
        match &self.log {
            Some(Ok(log)) => &log.rows,
            _ => &[],
        }
    }

    fn commits(&self) -> &[Commit] {
        match &self.log {
            Some(Ok(log)) => &log.commits,
            _ => &[],
        }
    }

    fn row_count(&self) -> usize {
        self.offset() + self.log_rows().len()
    }

    fn row(&self, i: usize) -> Option<Row<'_>> {
        if self.dirty.is_some() && i == 0 {
            return Some(Row::Uncommitted);
        }
        match self.log_rows().get(i - self.offset())? {
            LogRow::Commit { graph, index } => Some(Row::Commit(graph, &self.commits()[*index])),
            LogRow::Graph(graph) => Some(Row::Graph(graph)),
        }
    }

    fn entry_at(&self, i: usize) -> Option<Entry> {
        match self.row(i)? {
            Row::Uncommitted => Some(Entry::Uncommitted),
            Row::Commit(_, c) => Some(Entry::Commit(c.sha.clone())),
            Row::Graph(_) => None,
        }
    }

    fn selected(&self) -> Option<Entry> {
        self.entry_at(self.cursor)
    }

    fn selected_commit(&self) -> Option<&Commit> {
        match self.row(self.cursor)? {
            Row::Commit(_, c) => Some(c),
            _ => None,
        }
    }

    fn row_of(&self, entry: &Entry) -> Option<usize> {
        (0..self.row_count()).find(|i| self.entry_at(*i).as_ref() == Some(entry))
    }

    fn first_entry_row(&self) -> usize {
        (0..self.row_count())
            .find(|i| self.entry_at(*i).is_some())
            .unwrap_or(0)
    }

    /// Whether anything is still on its way, so the spinner keeps turning.
    pub fn loading(&self) -> bool {
        self.log.is_none() || self.changes_task.is_some()
    }

    /// Step over `delta` selectable rows, skipping the graph-only lines
    /// between them. Returns whether the cursor landed somewhere new.
    fn move_cursor(&mut self, delta: isize, height: usize) -> bool {
        let n = self.row_count() as isize;
        let step = delta.signum();
        let mut left = delta.unsigned_abs();
        let mut at = self.cursor as isize;
        let mut landed = self.cursor;
        while left > 0 {
            at += step;
            if at < 0 || at >= n {
                break;
            }
            if self.entry_at(at as usize).is_some() {
                landed = at as usize;
                left -= 1;
            }
        }
        self.place_cursor(landed, height)
    }

    fn place_cursor(&mut self, row: usize, height: usize) -> bool {
        let moved = row != self.cursor;
        self.cursor = row;
        if moved {
            self.diff_scroll = 0;
        }
        self.list_scroll = follow(self.list_scroll, self.cursor, height, self.row_count());
        moved
    }

    fn set_log(&mut self, result: Result<Log, String>, height: usize) {
        let keep = self.selected();
        if let Ok(log) = &result {
            self.graph_width = log
                .rows
                .iter()
                .map(|r| match r {
                    LogRow::Commit { graph, .. } | LogRow::Graph(graph) => {
                        graph.trim_end().chars().count()
                    }
                })
                .max()
                .unwrap_or(0)
                .min(GRAPH_MAX);
        }
        self.log = Some(result);
        let scroll = self.diff_scroll;
        let found = keep.and_then(|e| self.row_of(&e));
        self.place_cursor(found.unwrap_or_else(|| self.first_entry_row()), height);
        // A reload that found the same commit again keeps its diff scrolled
        // where it was, even if new commits above it moved its row.
        self.diff_scroll = if found.is_some() { scroll } else { 0 };
    }

    /// The pane's title: whose history, and which part of it.
    fn title(&self) -> String {
        let what = match (self.scope, &self.upstream) {
            (Scope::All, _) => "all branches".to_string(),
            (Scope::Branch, Some(up)) => format!("{} ⇄ {up}", self.branch),
            (Scope::Branch, None) => self.branch.clone(),
        };
        format!(" {} · {what} ", self.slug)
    }
}

/// The CHANGES cell's text for a dirty repo.
fn dirty_label(repo: &RepoStatus) -> Option<String> {
    if !repo.flags().dirty {
        return None;
    }
    repo.work
        .as_ref()
        .map(|w| fmt::changes(w.staged, w.unstaged, w.untracked, w.conflicts))
}

/// Scroll the least needed to keep `cursor` inside a window of `height`.
fn follow(scroll: usize, cursor: usize, height: usize, len: usize) -> usize {
    let height = height.max(1);
    let mut scroll = scroll;
    if cursor < scroll {
        scroll = cursor;
    } else if cursor >= scroll + height {
        scroll = cursor + 1 - height;
    }
    scroll.min(len.saturating_sub(height))
}

// ---------------------------------------------------------------------------
// Layout
// ---------------------------------------------------------------------------

/// The whole view: everything above the key bar and the status line, so the
/// footer keeps saying which keys do what in here.
fn frame(area: Rect) -> Rect {
    Rect {
        height: area.height.saturating_sub(2),
        ..area
    }
}

/// The list and the diff, inside the frame's border, with a one-column rule
/// between them. The list gets two fifths: enough for a subject line, and the
/// diff is what's being read.
pub fn panes(area: Rect) -> (Rect, Rect) {
    let outer = frame(area);
    let inner = Rect {
        x: outer.x + 1,
        y: outer.y + 1,
        width: outer.width.saturating_sub(2),
        height: outer.height.saturating_sub(2),
    };
    let list_w = (inner.width * 2 / 5)
        .max(36)
        .min(inner.width.saturating_sub(24));
    let list = Rect {
        width: list_w,
        ..inner
    };
    let diff = Rect {
        x: inner.x + list_w + 1,
        width: inner.width.saturating_sub(list_w + 1),
        ..inner
    };
    (list, diff)
}

// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

/// Open the view on the repo under the table's cursor.
pub fn open(app: &mut App, tx: &mpsc::UnboundedSender<Input>) {
    let Some(repo) = app.current() else { return };
    let mut view = HistoryView::new(repo);
    request_log(&mut view, tx);
    request_changes(&mut view, tx);
    app.history = Some(view);
    app.mode = Mode::History;
}

pub fn close(app: &mut App) {
    app.history = None;
    app.mode = Mode::Normal;
}

fn request_log(view: &mut HistoryView, tx: &mpsc::UnboundedSender<Input>) {
    if let Some(task) = view.log_task.take() {
        task.abort();
    }
    view.log_seq += 1;
    let (root, seq, scope) = (view.root.clone(), view.log_seq, view.scope);
    let upstream = view.upstream.clone();
    let tx = tx.clone();
    view.log_task = Some(tokio::spawn(async move {
        let result = commits::load_log(&root, upstream.as_deref(), scope)
            .await
            .map_err(|e| format!("{e:#}"));
        let _ = tx.send(Input::History(Box::new(Loaded::Log { root, seq, result })));
    }));
}

/// Ask for whatever the cursor is on, unless it's already in hand or on its
/// way. Anything else in flight is for a row the cursor has left, so it goes.
fn request_changes(view: &mut HistoryView, tx: &mpsc::UnboundedSender<Input>) {
    let Some(entry) = view.selected() else { return };
    if view.changes.contains_key(&entry) {
        return;
    }
    if let Some((pending, task)) = view.changes_task.take() {
        if pending == entry {
            view.changes_task = Some((pending, task));
            return;
        }
        task.abort();
    }
    let root = view.root.clone();
    let tx = tx.clone();
    let wanted = entry.clone();
    let task = tokio::spawn(async move {
        let result = load_entry(&root, &wanted).await;
        let _ = tx.send(Input::History(Box::new(Loaded::Changes {
            root,
            entry: wanted,
            result,
        })));
    });
    view.changes_task = Some((entry, task));
}

async fn load_entry(root: &Path, entry: &Entry) -> Result<Arc<Changes>, String> {
    match entry {
        Entry::Uncommitted => commits::load_uncommitted(root).await,
        Entry::Commit(sha) => commits::load_commit(root, sha).await,
    }
    .map(Arc::new)
    .map_err(|e| format!("{e:#}"))
}

/// An answer from git has arrived.
pub fn on_loaded(app: &mut App, loaded: Loaded, tx: &mpsc::UnboundedSender<Input>) {
    let height = panes(app.area).0.height as usize;
    let Some(view) = app.history.as_mut() else {
        return;
    };
    match loaded {
        Loaded::Log { root, seq, result } => {
            if root != view.root || seq != view.log_seq {
                return;
            }
            view.log_task = None;
            view.set_log(result, height);
            request_changes(view, tx);
        }
        Loaded::Changes {
            root,
            entry,
            result,
        } => {
            if root != view.root {
                return;
            }
            if view.changes_task.as_ref().is_some_and(|(e, _)| *e == entry) {
                view.changes_task = None;
            }
            if view.changes.len() >= CACHE_LIMIT {
                view.changes.clear();
            }
            view.changes.insert(entry, result);
        }
    }
}

/// A fresh probe of some repo has landed. If it's the one on screen and
/// something about it moved -- a commit, a pull, a fetch that brought in new
/// work -- the list is read again, and the uncommitted diff always is.
pub fn on_repo_update(app: &mut App, root: &Path, tx: &mpsc::UnboundedSender<Input>) {
    let height = panes(app.area).0.height as usize;
    let Some(&idx) = app.by_root.get(root) else {
        return;
    };
    let repo = &app.repos[idx];
    let Some(view) = app.history.as_mut() else {
        return;
    };
    if view.root != root {
        return;
    }

    let fingerprint = Fingerprint::of(repo);
    let dirty = dirty_label(repo);
    view.changes.remove(&Entry::Uncommitted);
    let was_dirty = view.dirty.is_some();
    view.dirty = dirty;
    // The uncommitted row coming or going shifts every row under it, and the
    // cursor has to shift with them to stay on the same commit.
    match (was_dirty, view.dirty.is_some()) {
        (false, true) => view.cursor += 1,
        (true, false) => view.cursor = view.cursor.saturating_sub(1),
        _ => {}
    }
    if view.selected().is_none() {
        view.cursor = view.first_entry_row();
    }
    view.list_scroll = follow(view.list_scroll, view.cursor, height, view.row_count());

    if fingerprint != view.fingerprint {
        view.fingerprint = fingerprint;
        request_log(view, tx);
    }
    request_changes(view, tx);
}

/// Load everything synchronously, for `tui-snapshot`.
pub async fn open_now(app: &mut App) {
    let Some(repo) = app.current() else { return };
    let mut view = HistoryView::new(repo);
    let log = commits::load_log(&view.root, view.upstream.as_deref(), view.scope)
        .await
        .map_err(|e| format!("{e:#}"));
    view.set_log(log, panes(app.area).0.height as usize);
    if let Some(entry) = view.selected() {
        let result = load_entry(&view.root, &entry).await;
        view.changes.insert(entry, result);
    }
    app.history = Some(view);
    app.mode = Mode::History;
}

// ---------------------------------------------------------------------------
// Input
// ---------------------------------------------------------------------------

pub fn handle_key(app: &mut App, key: KeyEvent, tx: &mpsc::UnboundedSender<Input>) {
    let diff_h = panes(app.area).1.height as usize;

    match key.code {
        KeyCode::Char(' ') | KeyCode::Esc | KeyCode::Char('q') => close(app),
        KeyCode::Char('j') | KeyCode::Down => move_cursor(app, 1, tx),
        KeyCode::Char('k') | KeyCode::Up => move_cursor(app, -1, tx),
        KeyCode::Home => move_cursor(app, -(isize::MAX / 2), tx),
        KeyCode::End => move_cursor(app, isize::MAX / 2, tx),
        KeyCode::Char('J') => scroll_diff(app, 1),
        KeyCode::Char('K') => scroll_diff(app, -1),
        KeyCode::PageDown => scroll_diff(app, diff_h.saturating_sub(2).max(1) as isize),
        KeyCode::PageUp => scroll_diff(app, -(diff_h.saturating_sub(2).max(1) as isize)),
        KeyCode::Char('n') => jump_file(app, true),
        KeyCode::Char('p') => jump_file(app, false),
        KeyCode::Char('a') => {
            if let Some(view) = app.history.as_mut() {
                view.scope = match view.scope {
                    Scope::Branch => Scope::All,
                    Scope::All => Scope::Branch,
                };
                request_log(view, tx);
                let what = match view.scope {
                    Scope::All => "every branch".to_string(),
                    Scope::Branch => match &view.upstream {
                        Some(up) => format!("{} and {up}", view.branch),
                        None => view.branch.clone(),
                    },
                };
                app.notify(format!("History of {what}"));
            }
        }
        KeyCode::Char('y') => {
            let sha = app
                .history
                .as_ref()
                .and_then(|v| v.selected_commit())
                .map(|c| c.sha.clone());
            match sha {
                Some(sha) => super::copy_text(app, &sha),
                None => app.notify("Nothing committed to copy a hash of"),
            }
        }
        KeyCode::Char('o') => super::open_file_manager(app),
        KeyCode::Char('O') => super::open_editor(app),
        KeyCode::Char('t') => super::open_git_client(app),
        KeyCode::Char('T') => super::open_terminal(app),
        KeyCode::Char('w') => super::open_remote(app),
        _ => {}
    }
}

/// Half a page of diff, for ctrl-d and ctrl-u.
pub fn half_page(app: &mut App, down: bool) {
    let half = (panes(app.area).1.height as isize / 2).max(1);
    scroll_diff(app, if down { half } else { -half });
}

fn move_cursor(app: &mut App, delta: isize, tx: &mpsc::UnboundedSender<Input>) {
    let height = panes(app.area).0.height as usize;
    if let Some(view) = app.history.as_mut() {
        if view.move_cursor(delta, height) {
            request_changes(view, tx);
        }
    }
}

fn scroll_diff(app: &mut App, delta: isize) {
    let height = panes(app.area).1.height as usize;
    let Some((total, _)) = diff_metrics(app) else {
        return;
    };
    if let Some(view) = app.history.as_mut() {
        let max = total.saturating_sub(height) as isize;
        view.diff_scroll = (view.diff_scroll as isize + delta).clamp(0, max.max(0)) as usize;
    }
}

/// Put the next (or previous) file's header at the top of the pane.
fn jump_file(app: &mut App, forward: bool) {
    let height = panes(app.area).1.height as usize;
    let Some((total, starts)) = diff_metrics(app) else {
        return;
    };
    let Some(view) = app.history.as_mut() else {
        return;
    };
    let at = view.diff_scroll;
    let target = if forward {
        starts.iter().copied().find(|s| *s > at)
    } else {
        starts.iter().copied().rev().find(|s| *s < at)
    };
    if let Some(target) = target {
        view.diff_scroll = target.min(total.saturating_sub(height));
    } else if !forward {
        view.diff_scroll = 0;
    }
}

pub fn handle_mouse(app: &mut App, ev: MouseEvent, tx: &mpsc::UnboundedSender<Input>) {
    let (list, diff) = panes(app.area);
    let over_list = ev.column < diff.x;
    match ev.kind {
        MouseEventKind::ScrollDown if over_list => move_cursor(app, 1, tx),
        MouseEventKind::ScrollUp if over_list => move_cursor(app, -1, tx),
        MouseEventKind::ScrollDown => scroll_diff(app, WHEEL_LINES as isize),
        MouseEventKind::ScrollUp => scroll_diff(app, -(WHEEL_LINES as isize)),
        MouseEventKind::Down(MouseButton::Left) if over_list => {
            if ev.row < list.y || ev.row >= list.y + list.height {
                return;
            }
            let Some(view) = app.history.as_mut() else {
                return;
            };
            let row = view.list_scroll + (ev.row - list.y) as usize;
            if view.entry_at(row).is_some() && view.place_cursor(row, list.height as usize) {
                request_changes(view, tx);
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Drawing
// ---------------------------------------------------------------------------

pub fn render(f: &mut Frame, app: &App) {
    let Some(view) = app.history.as_ref() else {
        return;
    };
    let outer = frame(f.area());
    let (list, diff) = panes(f.area());
    f.render_widget(Clear, outer);

    let mut title = vec![Span::styled(
        view.title(),
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    )];
    if view.scope == Scope::Branch {
        let fp = &view.fingerprint;
        if fp.ahead > 0 {
            title.push(Span::styled(
                format!("↑{} unpushed ", fp.ahead),
                Style::default().fg(UNPUSHED),
            ));
        }
        if fp.behind > 0 {
            title.push(Span::styled(
                format!("↓{} incoming ", fp.behind),
                Style::default().fg(BEHIND).add_modifier(Modifier::BOLD),
            ));
        }
    }

    let mut block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(ACCENT))
        .title(Line::from(title));
    if let Some(Ok(log)) = &view.log {
        let n = log.commits.len();
        block = block.title_bottom(Span::styled(
            if log.truncated {
                format!(" newest {n} commits ")
            } else {
                format!(" {n} commit{} ", if n == 1 { "" } else { "s" })
            },
            Style::default().fg(DIM),
        ));
    }
    if let Some((total, _)) = diff_metrics(app) {
        if total > diff.height as usize {
            let first = view.diff_scroll + 1;
            let last = (view.diff_scroll + diff.height as usize).min(total);
            block = block.title_bottom(
                Line::from(Span::styled(
                    format!(" lines {first}–{last} of {total} "),
                    Style::default().fg(DIM),
                ))
                .right_aligned(),
            );
        }
    }
    f.render_widget(block, outer);

    // The rule between the panes.
    let rule: Vec<Line> = (0..list.height)
        .map(|_| Line::from(Span::styled("│", Style::default().fg(DIM))))
        .collect();
    f.render_widget(
        Paragraph::new(rule),
        Rect {
            x: list.x + list.width,
            width: 1,
            ..list
        },
    );

    render_list(f, app, view, list);
    render_diff(f, app, view, diff);
}

fn render_list(f: &mut Frame, app: &App, view: &HistoryView, area: Rect) {
    let height = area.height as usize;
    let width = area.width as usize;
    let total = view.row_count();
    let scroll = follow(view.list_scroll, view.cursor, height, total);

    let mut lines: Vec<Line> = (scroll..(scroll + height).min(total))
        .filter_map(|i| {
            let selected = i == view.cursor;
            Some(match view.row(i)? {
                Row::Uncommitted => uncommitted_line(view, width, selected),
                Row::Commit(graph, commit) => {
                    commit_line(graph, commit, view.graph_width, width, app.now, selected)
                }
                Row::Graph(graph) => Line::from(graph_spans(graph, None, view.graph_width)),
            })
        })
        .collect();

    let note =
        |text: String, colour: Color| Line::from(Span::styled(text, Style::default().fg(colour)));
    match &view.log {
        None => lines.push(note(
            format!(" {} reading history…", app.spinner_frame()),
            ACCENT,
        )),
        Some(Err(err)) => lines.push(note(format!(" {err}"), TROUBLE)),
        Some(Ok(log)) if log.commits.is_empty() => {
            lines.push(note(" nothing committed yet".into(), DIM))
        }
        Some(Ok(log)) if log.truncated && scroll + height >= total && lines.len() < height => lines
            .push(note(
                format!(" … older than {} commits not shown", log.commits.len()),
                DIM,
            )),
        _ => {}
    }
    f.render_widget(Paragraph::new(lines), area);
}

fn uncommitted_line(view: &HistoryView, width: usize, selected: bool) -> Line<'static> {
    let label = view.dirty.clone().unwrap_or_default();
    let lead = format!("●{}uncommitted changes", " ".repeat(view.graph_width));
    let room = width.saturating_sub(lead.chars().count() + 1);
    let mut spans = vec![
        Span::styled(
            lead,
            Style::default().fg(DIRTY).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("{:>room$} ", fmt::truncate(&label, room)),
            Style::default().fg(DIRTY),
        ),
    ];
    if selected {
        reverse(&mut spans);
    }
    Line::from(spans)
}

fn commit_line(
    graph: &str,
    commit: &Commit,
    graph_width: usize,
    width: usize,
    now: i64,
    selected: bool,
) -> Line<'static> {
    let mut spans = graph_spans(graph, Some(commit), graph_width);

    // Unpushed and incoming commits carry the table's own colours for those
    // states, on the hash, so the list says which side of the upstream each
    // one is on without a column spent on it.
    let sha_style = match commit.flow {
        Flow::Unpushed => Style::default().fg(UNPUSHED).add_modifier(Modifier::BOLD),
        Flow::Incoming => Style::default().fg(BEHIND).add_modifier(Modifier::BOLD),
        Flow::Shared => Style::default().fg(Color::Yellow),
    };
    spans.push(Span::styled(format!("{} ", commit.short), sha_style));

    let age = fmt::age(commit.at, now);
    let right = if width >= 64 {
        format!(" {:<12} {age:>4} ", fmt::truncate(&commit.author, 12))
    } else {
        format!(" {age:>4} ")
    };
    let used = graph_width + 1 + commit.short.chars().count() + 1 + right.chars().count();
    let mut room = width.saturating_sub(used);

    // Labels get up to two thirds of what's left, and the subject the rest.
    // A commit with six branches on it says `+4` rather than losing its
    // subject.
    let labels = compact_labels(&commit.refs);
    let budget = room * 2 / 3;
    let mut spent = 0;
    for (i, label) in labels.iter().enumerate() {
        let text = format!(" {} ", fmt::truncate(&label.name, 24));
        let w = text.chars().count() + 1;
        if spent + w > budget {
            let more = format!("+{} ", labels.len() - i);
            spent += more.chars().count();
            spans.push(Span::styled(more, Style::default().fg(DIM)));
            break;
        }
        spent += w;
        spans.push(Span::styled(text, chip_style(label)));
        spans.push(Span::raw(" "));
    }
    room = room.saturating_sub(spent);

    spans.push(Span::styled(
        pad(&fmt::truncate(&commit.subject, room), room),
        Style::default().fg(if commit.merge { DIM } else { Color::White }),
    ));
    spans.push(Span::styled(right, Style::default().fg(DIM)));
    if selected {
        reverse(&mut spans);
    }
    Line::from(spans)
}

/// The labels worth a row's width. HEAD folds into the branch it's on, which
/// then wears HEAD's colour, and a remote branch sitting on the same commit as
/// the local one of the same name is dropped: in sync is the usual case, and
/// the one worth seeing is a remote branch somewhere else in the list. The
/// diff pane lists every label, unabridged.
fn compact_labels(refs: &[RefLabel]) -> Vec<RefLabel> {
    let head_branch = refs.iter().any(|r| r.kind == RefKind::Head)
        && refs.iter().any(|r| r.kind == RefKind::Local);
    let locals: Vec<&str> = refs
        .iter()
        .filter(|r| r.kind == RefKind::Local)
        .map(|r| r.name.as_str())
        .collect();
    let mut out = Vec::new();
    let mut head_given = false;
    for r in refs {
        match r.kind {
            RefKind::Head if head_branch => {}
            RefKind::Local if head_branch && !head_given => {
                head_given = true;
                out.push(RefLabel {
                    name: r.name.clone(),
                    kind: RefKind::Head,
                });
            }
            RefKind::Remote
                if r.name
                    .split_once('/')
                    .is_some_and(|(_, branch)| locals.contains(&branch)) => {}
            _ => out.push(r.clone()),
        }
    }
    out
}

fn reverse(spans: &mut [Span<'static>]) {
    for span in spans {
        span.style = span.style.add_modifier(Modifier::REVERSED);
    }
}

fn chip_style(label: &RefLabel) -> Style {
    match label.kind {
        RefKind::Head => Style::default()
            .fg(Color::Black)
            .bg(ACCENT)
            .add_modifier(Modifier::BOLD),
        RefKind::Local => Style::default().fg(Color::White).bg(Color::Blue),
        RefKind::Remote => Style::default().fg(Color::Black).bg(Color::Gray),
        RefKind::Tag => Style::default().fg(Color::Black).bg(Color::Yellow),
    }
}

/// Lane colours for the graph, cycled by column the way git's own are.
const LANES: &[Color] = &[
    Color::Blue,
    Color::Magenta,
    Color::Green,
    Color::Yellow,
    Color::Cyan,
    Color::LightRed,
];

/// Git's ASCII graph, redrawn with box-drawing characters and a colour per
/// lane, cut or padded to `width`.
fn graph_spans(graph: &str, commit: Option<&Commit>, width: usize) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut count = 0;
    for (i, ch) in graph.chars().enumerate() {
        if count == width {
            break;
        }
        let glyph = match ch {
            '*' if commit.is_some_and(|c| c.merge) => '◆',
            '*' => '●',
            '|' => '│',
            '/' => '╱',
            '\\' => '╲',
            '-' => '─',
            other => other,
        };
        let colour = LANES[(i / 2) % LANES.len()];
        spans.push(Span::styled(glyph.to_string(), Style::default().fg(colour)));
        count += 1;
    }
    // One space past the widest graph, so the hash never touches a lane.
    spans.push(Span::raw(" ".repeat(width - count + 1)));
    spans
}

/// How long the diff pane's content is, and where each file's header sits in
/// it, for scrolling and for `n`/`p`. `None` while there's nothing loaded.
fn diff_metrics(app: &App) -> Option<(usize, Vec<usize>)> {
    let view = app.history.as_ref()?;
    let entry = view.selected()?;
    let Ok(changes) = view.changes.get(&entry)? else {
        return None;
    };
    let prefix = prefix_lines(view, changes, app.now).len();
    let starts = changes
        .patch
        .iter()
        .enumerate()
        .filter(|(_, l)| l.kind == LineKind::File)
        .map(|(i, _)| prefix + i)
        .collect();
    Some((
        prefix + changes.patch.len() + usize::from(changes.truncated),
        starts,
    ))
}

fn render_diff(f: &mut Frame, app: &App, view: &HistoryView, area: Rect) {
    let dim = |text: String| Line::from(Span::styled(text, Style::default().fg(DIM)));
    let Some(entry) = view.selected() else {
        f.render_widget(Paragraph::new(Vec::<Line>::new()), area);
        return;
    };
    let changes = match view.changes.get(&entry) {
        Some(Ok(changes)) => changes,
        Some(Err(err)) => {
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    format!(" {err}"),
                    Style::default().fg(TROUBLE),
                ))),
                area,
            );
            return;
        }
        None => {
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    format!(" {} reading the diff…", app.spinner_frame()),
                    Style::default().fg(ACCENT),
                ))),
                area,
            );
            return;
        }
    };

    let prefix = prefix_lines(view, changes, app.now);
    let total = prefix.len() + changes.patch.len() + usize::from(changes.truncated);
    let height = area.height as usize;
    let scroll = view.diff_scroll.min(total.saturating_sub(height));
    let lines: Vec<Line> = (scroll..(scroll + height).min(total))
        .map(|i| {
            if i < prefix.len() {
                prefix[i].clone()
            } else if let Some(line) = changes.patch.get(i - prefix.len()) {
                patch_line(line)
            } else {
                dim(" … the diff stops here, a couple of megabytes in. t opens it in your git client.".into())
            }
        })
        .collect();
    f.render_widget(Paragraph::new(lines), area);
}

/// Everything above the patch: who, when, where, why, and which files.
fn prefix_lines(view: &HistoryView, changes: &Changes, now: i64) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let label = |text: &str| Span::styled(format!(" {text:<10} "), Style::default().fg(DIM));

    match &changes.header {
        Some(h) => {
            lines.push(Line::from(Span::styled(
                format!(" {}", h.sha),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )));
            lines.push(Line::from(vec![
                label("author"),
                Span::raw(format!("{} <{}>", h.author, h.author_email)),
            ]));
            lines.push(Line::from(vec![
                label("date"),
                Span::raw(h.author_date.clone()),
                Span::styled(
                    format!("  ·  {} ago", fmt::age(h.author_at, now)),
                    Style::default().fg(DIM),
                ),
            ]));
            // Only when it says something: a rebase, a cherry-pick, a merge
            // button on a website. For most commits it's the author again.
            if h.committer != h.author || h.committer_email != h.author_email {
                lines.push(Line::from(vec![
                    label("committer"),
                    Span::raw(format!("{} <{}>", h.committer, h.committer_email)),
                    Span::styled(
                        format!("  ·  {}", h.committer_date),
                        Style::default().fg(DIM),
                    ),
                ]));
            }
            if !h.refs.is_empty() {
                let mut spans = vec![label("refs")];
                for r in &h.refs {
                    spans.push(Span::styled(format!(" {} ", r.name), chip_style(r)));
                    spans.push(Span::raw(" "));
                }
                lines.push(Line::from(spans));
            }
            if !h.parents.is_empty() {
                let parents: Vec<String> = h
                    .parents
                    .iter()
                    .map(|p| p.chars().take(8).collect())
                    .collect();
                let mut spans = vec![
                    label(if h.parents.len() > 1 {
                        "parents"
                    } else {
                        "parent"
                    }),
                    Span::styled(parents.join("  "), Style::default().fg(Color::Yellow)),
                ];
                if h.parents.len() > 1 {
                    spans.push(Span::styled(
                        "  ·  diff against the first",
                        Style::default().fg(DIM),
                    ));
                }
                lines.push(Line::from(spans));
            }
            if let Some(commit) = view.selected_commit() {
                let up = view.upstream.as_deref().unwrap_or("the upstream");
                match commit.flow {
                    Flow::Unpushed => lines.push(Line::from(vec![
                        label(""),
                        Span::styled(
                            format!("↑ not pushed to {up} yet"),
                            Style::default().fg(UNPUSHED),
                        ),
                    ])),
                    Flow::Incoming => lines.push(Line::from(vec![
                        label(""),
                        Span::styled(
                            format!("↓ on {up}, not pulled yet"),
                            Style::default().fg(BEHIND).add_modifier(Modifier::BOLD),
                        ),
                    ])),
                    Flow::Shared => {}
                }
            }
            lines.push(Line::from(""));
            for (i, text) in h.message.lines().enumerate() {
                let style = if i == 0 {
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                lines.push(Line::from(Span::styled(format!(" {}", clean(text)), style)));
            }
        }
        None => {
            lines.push(Line::from(Span::styled(
                " uncommitted changes",
                Style::default().fg(DIRTY).add_modifier(Modifier::BOLD),
            )));
            lines.push(Line::from(Span::styled(
                " staged and unstaged together, against HEAD, then untracked files by name",
                Style::default().fg(DIM),
            )));
        }
    }

    lines.push(Line::from(""));
    let (added, removed) = changes.totals();
    let n = changes.files.len();
    lines.push(Line::from(vec![
        Span::styled(
            format!(" {n} file{} changed", if n == 1 { "" } else { "s" }),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("  +{added}"), Style::default().fg(CLEAN)),
        Span::styled(format!(" −{removed}"), Style::default().fg(TROUBLE)),
    ]));
    for file in &changes.files {
        let colour = match file.status {
            FileStatus::Added => CLEAN,
            FileStatus::Deleted => TROUBLE,
            FileStatus::Renamed | FileStatus::Copied => UNRELEASED,
            FileStatus::Modified | FileStatus::TypeChanged => DIRTY,
            FileStatus::Untracked => DIM,
        };
        let path = match &file.from {
            Some(from) => format!("{from} → {}", file.path),
            None => file.path.clone(),
        };
        let mut spans = vec![
            Span::styled(
                format!("   {}  ", file.status.letter()),
                Style::default().fg(colour).add_modifier(Modifier::BOLD),
            ),
            Span::raw(path),
        ];
        match file.lines {
            Some((a, r)) => {
                if a > 0 {
                    spans.push(Span::styled(format!("  +{a}"), Style::default().fg(CLEAN)));
                }
                if r > 0 {
                    spans.push(Span::styled(format!(" −{r}"), Style::default().fg(TROUBLE)));
                }
            }
            None if file.status != FileStatus::Untracked => {
                spans.push(Span::styled("  binary", Style::default().fg(DIM)));
            }
            None => {}
        }
        lines.push(Line::from(spans));
    }
    lines.push(Line::from(""));
    lines
}

fn patch_line(line: &PatchLine) -> Line<'static> {
    let gutter = |old: Option<u32>, new: Option<u32>| {
        let n = |v: Option<u32>| v.map(|v| v.to_string()).unwrap_or_default();
        Span::styled(
            format!("{:>5}{:>5} ", n(old), n(new)),
            Style::default().fg(DIM),
        )
    };
    let blank = || Span::raw(" ".repeat(11));
    match line.kind {
        LineKind::File => Line::from(vec![
            Span::styled(" ── ", Style::default().fg(ACCENT)),
            Span::styled(
                line.text.clone(),
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!(" {}", "─".repeat(400)), Style::default().fg(ACCENT)),
        ]),
        LineKind::Hunk => Line::from(vec![
            blank(),
            Span::styled(clean(&line.text), Style::default().fg(ACCENT)),
        ]),
        LineKind::Added => Line::from(vec![
            gutter(line.old, line.new),
            Span::styled(
                format!("+{}", clean(&line.text)),
                Style::default().fg(CLEAN),
            ),
        ]),
        LineKind::Removed => Line::from(vec![
            gutter(line.old, line.new),
            Span::styled(
                format!("-{}", clean(&line.text)),
                Style::default().fg(TROUBLE),
            ),
        ]),
        LineKind::Context => Line::from(vec![
            gutter(line.old, line.new),
            Span::raw(format!(" {}", clean(&line.text))),
        ]),
        LineKind::Note => Line::from(vec![
            blank(),
            Span::styled(
                clean(&line.text),
                Style::default().fg(DIM).add_modifier(Modifier::ITALIC),
            ),
        ]),
    }
}

/// Tabs to spaces, and anything else that would move the terminal's cursor
/// dropped. A diff is file contents, and file contents can hold anything.
fn clean(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '\t' => out.push_str("    "),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commits::{Flow, LogRow};

    fn commit(sha: &str) -> Commit {
        Commit {
            sha: sha.into(),
            short: sha.into(),
            author: "Andy".into(),
            at: 0,
            merge: false,
            refs: Vec::new(),
            subject: "s".into(),
            flow: Flow::Shared,
        }
    }

    fn view(dirty: bool) -> HistoryView {
        let mut repo = RepoStatus::new(PathBuf::from("/p/g/r"), "g".into(), "r".into());
        repo.work = None;
        let mut v = HistoryView::new(&repo);
        if dirty {
            v.dirty = Some("~1".into());
        }
        let log = Log {
            rows: vec![
                LogRow::Commit {
                    graph: "* ".into(),
                    index: 0,
                },
                LogRow::Graph("|\\".into()),
                LogRow::Commit {
                    graph: "| * ".into(),
                    index: 1,
                },
                LogRow::Graph("|/".into()),
                LogRow::Commit {
                    graph: "* ".into(),
                    index: 2,
                },
            ],
            commits: vec![commit("a"), commit("b"), commit("c")],
            truncated: false,
        };
        v.set_log(Ok(log), 10);
        v
    }

    // The graph-only lines between commits are drawn but never selected.
    #[test]
    fn the_cursor_steps_over_graph_lines() {
        let mut v = view(false);
        assert_eq!(v.selected(), Some(Entry::Commit("a".into())));
        v.move_cursor(1, 10);
        assert_eq!(v.selected(), Some(Entry::Commit("b".into())));
        v.move_cursor(1, 10);
        assert_eq!(v.selected(), Some(Entry::Commit("c".into())));
        assert!(!v.move_cursor(1, 10), "nothing past the last commit");
        v.move_cursor(-5, 10);
        assert_eq!(v.selected(), Some(Entry::Commit("a".into())));
    }

    // Uncommitted work is what's going on right now, so it's what the view
    // opens on.
    #[test]
    fn a_dirty_repo_opens_on_its_uncommitted_changes() {
        let mut v = view(true);
        assert_eq!(v.selected(), Some(Entry::Uncommitted));
        v.move_cursor(1, 10);
        assert_eq!(v.selected(), Some(Entry::Commit("a".into())));
    }

    // A reload after a commit lands mustn't throw the cursor back to the top.
    #[test]
    fn a_reload_keeps_the_cursor_on_the_same_commit() {
        let mut v = view(false);
        v.move_cursor(2, 10);
        v.diff_scroll = 7;
        let mut log = match &v.log {
            Some(Ok(log)) => log.clone(),
            _ => unreachable!(),
        };
        log.rows.insert(
            0,
            LogRow::Commit {
                graph: "* ".into(),
                index: 3,
            },
        );
        log.commits.push(commit("new"));
        v.set_log(Ok(log), 10);
        assert_eq!(v.selected(), Some(Entry::Commit("c".into())));
        assert_eq!(
            v.diff_scroll, 7,
            "same commit, so the same place in its diff"
        );
    }

    #[test]
    fn the_list_scrolls_to_keep_the_cursor_in_sight() {
        assert_eq!(follow(0, 12, 10, 50), 3);
        assert_eq!(follow(8, 2, 10, 50), 2);
        assert_eq!(follow(0, 4, 10, 50), 0);
        assert_eq!(follow(45, 49, 10, 50), 40, "never past the end");
    }

    #[test]
    fn head_folds_into_its_branch_and_an_in_sync_remote_is_dropped() {
        let label = |name: &str, kind| RefLabel {
            name: name.into(),
            kind,
        };
        let refs = vec![
            label("HEAD", RefKind::Head),
            label("develop", RefKind::Local),
            label("1.1.1", RefKind::Tag),
            label("origin/develop", RefKind::Remote),
            label("origin/feature", RefKind::Remote),
        ];
        assert_eq!(
            compact_labels(&refs),
            vec![
                label("develop", RefKind::Head),
                label("1.1.1", RefKind::Tag),
                label("origin/feature", RefKind::Remote),
            ]
        );
        // Detached, HEAD has no branch to fold into and keeps its own label.
        let detached = vec![label("HEAD", RefKind::Head), label("1.0", RefKind::Tag)];
        assert_eq!(compact_labels(&detached), detached);
    }

    #[test]
    fn control_characters_never_reach_the_terminal() {
        assert_eq!(clean("a\tb\x1b[2Jc\r"), "a    b[2Jc");
    }
}
