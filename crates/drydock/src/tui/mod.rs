//! The dashboard.
//!
//! Opens on the cache so there's a full table on screen immediately, then
//! streams in fresh results as a sweep runs behind it. Key handling and
//! rendering are split: this module owns state and events, `ui` owns pixels.

mod ui;

use anyhow::Result;
use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event as TermEvent, KeyCode, KeyEvent,
        KeyEventKind, KeyModifiers, KeyboardEnhancementFlags, ModifierKeyCode, MouseButton,
        MouseEvent, MouseEventKind, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Rect, Size},
    Terminal,
};
use std::collections::HashMap;
use std::io::{self, Stdout};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

use crate::cache;
use crate::column::Column;
use crate::config::Config;
use crate::filter::{Filter, MatchMode, Query, Sort};
use crate::git;
use crate::model::RepoStatus;
use crate::probe::{self, Tier, Timings};
use crate::watch;

/// How long a status message stays on screen.
const MESSAGE_TTL: Duration = Duration::from_secs(6);

/// Repaint cadence while a sweep is running, so the spinner reads as motion.
/// When idle, only every fourth tick repaints.
const TICK: Duration = Duration::from_millis(250);

/// How long a sweep may claim to be running before the next one starts anyway.
/// A cold sweep of 550-plus repos takes seconds, so anything past this means
/// the sweep died without saying so, and a dashboard left open for days would
/// otherwise sit on the state it started with.
const SWEEP_STUCK_AFTER: Duration = Duration::from_secs(180);

/// Braille spinner frames, advanced once per tick.
pub const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Search,
    Detail,
    Help,
    /// The column picker: toggle columns on and off, and reorder them.
    Columns,
}

/// The `since` presets, cycled with the number keys.
pub const SINCE_PRESETS: &[(&str, &str)] = &[
    ("0", ""),
    ("1", "1h"),
    ("2", "1d"),
    ("3", "1w"),
    ("4", "1mo"),
];

pub struct App {
    pub cfg: Arc<Config>,
    /// Every known repo, keyed for updates by path.
    pub repos: Vec<RepoStatus>,
    pub by_root: HashMap<PathBuf, usize>,
    pub query: Query,
    /// Indices into `repos`, after filtering and sorting.
    pub visible: Vec<usize>,
    pub selected: usize,
    /// Which repo the selection is on, so it survives a re-sort.
    pub selected_root: Option<PathBuf>,
    pub scroll: usize,
    pub detail_scroll: u16,
    pub mode: Mode,
    pub search_input: String,
    pub message: Option<(String, Instant)>,
    pub groups: Vec<String>,
    /// Sweep progress: how many repos have reported, and out of how many.
    pub progress: (usize, usize),
    pub sweeping: bool,
    pub timings: Timings,
    /// When the last sweep finished, as unix seconds. Wall clock rather than
    /// an `Instant`, because `Instant` stops while the machine is asleep and
    /// this is shown to a person.
    pub last_sweep_at: Option<i64>,
    /// When the in-flight sweep started, for the stuck check.
    sweep_started: Option<Instant>,
    pub watching: bool,
    /// Advances every tick; drives the scanning spinner.
    pub spinner: usize,
    pub now: i64,
    pub rows_on_screen: usize,
    /// Columns on screen, left to right. Resolved from the config at startup
    /// and edited live by the column picker, which writes it back.
    pub columns: Vec<Column>,
    /// Row the column picker is sitting on, indexing [`App::picker_rows`].
    pub column_cursor: usize,
    /// How far the overlay on screen can scroll before it's showing only
    /// empty space, measured by the last frame that drew it. Zero when there
    /// is no overlay, or when it fits.
    pub overlay_max_scroll: u16,
    /// Which modifier keys are held right now, so the footer can show what
    /// they'd do. Only ever non-empty when [`App::modifier_events`] is set:
    /// without the keyboard protocol a modifier is only seen alongside the key
    /// it modified, and the footer would latch on the last one pressed and
    /// stay wrong until something else was typed.
    pub mods: KeyModifiers,
    /// Whether this terminal reports modifier keys being pressed and released
    /// on their own (the kitty keyboard protocol: kitty, Ghostty, WezTerm,
    /// iTerm2 3.5+, foot). Apple Terminal doesn't, and there the footer stays
    /// the static list it always was.
    pub modifier_events: bool,
    pub should_quit: bool,
}

impl App {
    pub fn new(cfg: Arc<Config>) -> Self {
        let mut repos: Vec<RepoStatus> = cache::load().into_values().collect();
        repos.sort_by(|a, b| a.root.cmp(&b.root));

        let mut query = Query::default();
        for name in &cfg.ui.default_filters {
            if let Ok(f) = name.parse::<Filter>() {
                query.filters.push(f);
            }
        }
        if let Ok(sort) = cfg.ui.default_sort.parse::<Sort>() {
            query.sort = sort;
        }
        let _ = query.set_since(&cfg.ui.default_since);

        // Resolved before `cfg` moves into the struct.
        let columns = cfg.columns();

        let mut app = Self {
            cfg,
            repos,
            by_root: HashMap::new(),
            query,
            visible: Vec::new(),
            selected: 0,
            selected_root: None,
            scroll: 0,
            detail_scroll: 0,
            columns,
            column_cursor: 0,
            overlay_max_scroll: 0,
            mods: KeyModifiers::NONE,
            modifier_events: false,
            mode: Mode::Normal,
            search_input: String::new(),
            message: None,
            groups: Vec::new(),
            progress: (0, 0),
            sweeping: false,
            timings: Timings::default(),
            last_sweep_at: None,
            sweep_started: None,
            watching: false,
            spinner: 0,
            now: git::now_unix(),
            rows_on_screen: 20,
            should_quit: false,
        };
        app.reindex();
        app.recompute();
        app
    }

    fn reindex(&mut self) {
        self.by_root = self
            .repos
            .iter()
            .enumerate()
            .map(|(i, r)| (r.root.clone(), i))
            .collect();
        let mut groups: Vec<String> = self
            .repos
            .iter()
            .map(|r| r.group.clone())
            .filter(|g| !g.is_empty())
            .collect();
        groups.sort();
        groups.dedup();
        self.groups = groups;
    }

    /// Reapply the query and put the selection back on whatever it was on.
    pub fn recompute(&mut self) {
        self.now = git::now_unix();
        self.visible = self.query.apply_indices(&self.repos, self.now);

        if let Some(root) = &self.selected_root {
            if let Some(pos) = self
                .visible
                .iter()
                .position(|i| &self.repos[*i].root == root)
            {
                self.selected = pos;
            }
        }
        if self.selected >= self.visible.len() {
            self.selected = self.visible.len().saturating_sub(1);
        }
        self.clamp_scroll();
    }

    pub fn upsert(&mut self, status: RepoStatus) {
        match self.by_root.get(&status.root) {
            Some(&idx) => self.repos[idx] = status,
            None => {
                self.by_root.insert(status.root.clone(), self.repos.len());
                self.repos.push(status);
            }
        }
    }

    /// Drop rows for repos the latest walk didn't find.
    pub fn retain_roots(&mut self, roots: &[PathBuf]) {
        let keep: std::collections::HashSet<&PathBuf> = roots.iter().collect();
        let before = self.repos.len();
        self.repos.retain(|r| keep.contains(&r.root));
        if self.repos.len() != before {
            self.reindex();
        }
    }

    pub fn current(&self) -> Option<&RepoStatus> {
        self.visible.get(self.selected).map(|i| &self.repos[*i])
    }

    pub fn spinner_frame(&self) -> &'static str {
        SPINNER[self.spinner % SPINNER.len()]
    }

    /// What the dashboard is busy doing, if anything, for the header and the
    /// empty-table placeholder.
    pub fn activity_note(&self) -> Option<String> {
        if !self.sweeping {
            return None;
        }
        let (done, total) = self.progress;
        Some(if total == 0 {
            "walking the scan roots".to_string()
        } else {
            format!("scanning {done}/{total} repos")
        })
    }

    /// Every column, in picker order: the ones on screen first in the order
    /// they're rendered, then the hidden ones. `true` means it's on screen.
    pub fn picker_rows(&self) -> Vec<(Column, bool)> {
        let mut rows: Vec<(Column, bool)> = self.columns.iter().map(|c| (*c, true)).collect();
        for column in Column::all() {
            if !self.columns.contains(column) {
                rows.push((*column, false));
            }
        }
        rows
    }

    /// Turn the column under the picker cursor on or off, keeping the cursor
    /// on that same column as it moves between the two sections.
    ///
    /// Returns whether this also turned visibility checking on, which the
    /// caller uses to kick off a sweep — the values can't arrive without one.
    pub fn toggle_selected_column(&mut self) -> bool {
        let rows = self.picker_rows();
        let Some((column, shown)) = rows.get(self.column_cursor).copied() else {
            return false;
        };
        if !column.toggleable() {
            self.notify(format!("{} can't be hidden", column.header(false)));
            return false;
        }

        if shown {
            self.columns.retain(|c| *c != column);
        } else {
            // Put it back where it belongs rather than on the end, so turning
            // VISIBILITY on lands it between RELEASE and CHANGES the way the
            // defaults have it.
            let canonical = Column::all();
            let rank = |c: &Column| canonical.iter().position(|x| x == c).unwrap_or(usize::MAX);
            let at = self
                .columns
                .iter()
                .position(|c| rank(c) > rank(&column))
                .unwrap_or(self.columns.len());
            self.columns.insert(at, column);
        }
        self.follow_column(column);

        // Asking for the VISIBILITY column is asking for visibility, so turn
        // the checking on with it. Leaving it off would give a column that can
        // only ever say "checking off" -- the exact thing that made this
        // column worth making configurable in the first place.
        //
        // Not symmetrical on the way out: hiding the column doesn't turn
        // checking off, because `--public`, `--private` and `--json` still use
        // it, and silently disabling those isn't implied by tidying a table.
        let turned_on = self.columns.contains(&column);
        let wants_checking = matches!(column, Column::Visibility | Column::VisibilityShort);
        if wants_checking && turned_on && !self.cfg.visibility.enabled {
            let mut cfg = (*self.cfg).clone();
            cfg.visibility.enabled = true;
            self.cfg = Arc::new(cfg);
            return true;
        }
        false
    }

    /// Move the column under the cursor one place left or right. Only has any
    /// meaning for a column that's on screen, since the hidden ones are just a
    /// list to pick from.
    pub fn move_selected_column(&mut self, delta: isize) {
        let rows = self.picker_rows();
        let Some((column, true)) = rows.get(self.column_cursor).copied() else {
            return;
        };
        let Some(from) = self.columns.iter().position(|c| *c == column) else {
            return;
        };
        let to = from as isize + delta;
        if to < 0 || to as usize >= self.columns.len() {
            return;
        }
        self.columns.swap(from, to as usize);
        self.follow_column(column);
    }

    /// Put the picker cursor back on `column` wherever it ended up.
    fn follow_column(&mut self, column: Column) {
        if let Some(at) = self.picker_rows().iter().position(|(c, _)| *c == column) {
            self.column_cursor = at;
        }
    }

    /// Scroll the overlay, stopping at the end of its content rather than
    /// running on into blank space — which a wheel reaches far faster than
    /// j/k ever did.
    pub fn scroll_overlay(&mut self, delta: i32) {
        let next = self.detail_scroll as i32 + delta;
        self.detail_scroll = next.clamp(0, self.overlay_max_scroll as i32) as u16;
    }

    pub fn move_column_cursor(&mut self, delta: isize) {
        let len = self.picker_rows().len();
        if len == 0 {
            return;
        }
        let next = (self.column_cursor as isize + delta).clamp(0, len as isize - 1);
        self.column_cursor = next as usize;
    }

    /// Write the current column list back to the config, so it survives a
    /// restart. Everything else in the file is round-tripped untouched.
    pub fn save_columns(&mut self) {
        let mut cfg = (*self.cfg).clone();
        cfg.ui.columns = Some(self.columns.clone());
        match crate::config::save(&cfg) {
            Ok(path) => {
                self.cfg = Arc::new(cfg);
                self.notify(format!(
                    "columns saved to {}",
                    crate::paths::contract(&path)
                ));
            }
            Err(err) => self.notify(format!("could not save columns: {err:#}")),
        }
    }

    /// Drop the configured list and go back to what the defaults would show.
    pub fn reset_columns(&mut self) {
        self.columns = Column::defaults(self.cfg.visibility.enabled);
        self.column_cursor = 0;
    }

    pub fn notify(&mut self, message: impl Into<String>) {
        self.message = Some((message.into(), Instant::now()));
    }

    pub fn active_message(&self) -> Option<&str> {
        self.message
            .as_ref()
            .filter(|(_, at)| at.elapsed() < MESSAGE_TTL)
            .map(|(m, _)| m.as_str())
    }

    fn move_selection(&mut self, delta: isize) {
        if self.visible.is_empty() {
            return;
        }
        let last = self.visible.len() - 1;
        let next = (self.selected as isize + delta).clamp(0, last as isize) as usize;
        self.selected = next;
        self.selected_root = self.current().map(|r| r.root.clone());
        self.clamp_scroll();
    }

    /// Put the selection on whatever repo is drawn at this screen row.
    /// Returns false for a click on the header, the border, or past the end of
    /// the list, so the caller can leave the selection alone.
    fn select_at_row(&mut self, row: u16, first_row: u16) -> bool {
        if row < first_row {
            return false;
        }
        let idx = self.scroll + (row - first_row) as usize;
        if idx >= self.visible.len() {
            return false;
        }
        self.selected = idx;
        self.selected_root = self.current().map(|r| r.root.clone());
        true
    }

    fn clamp_scroll(&mut self) {
        let rows = self.rows_on_screen.max(1);
        if self.selected < self.scroll {
            self.scroll = self.selected;
        } else if self.selected >= self.scroll + rows {
            self.scroll = self.selected + 1 - rows;
        }
        let max_scroll = self.visible.len().saturating_sub(rows);
        self.scroll = self.scroll.min(max_scroll);
    }

    fn set_since_preset(&mut self, key: char) {
        if let Some((_, spec)) = SINCE_PRESETS.iter().find(|(k, _)| *k == key.to_string()) {
            let _ = self.query.set_since(spec);
            self.notify(if spec.is_empty() {
                "Showing all ages".to_string()
            } else {
                format!("Showing repos touched in the last {spec}")
            });
            self.recompute();
        }
    }

    fn cycle_group(&mut self, forward: bool) {
        if self.groups.is_empty() {
            return;
        }
        // The cycle runs: all groups, then each group in turn.
        let current = self.query.group.clone();
        let pos = current
            .as_ref()
            .and_then(|g| self.groups.iter().position(|x| x == g))
            .map(|i| i as isize)
            .unwrap_or(-1);
        let len = self.groups.len() as isize;
        let next = if forward { pos + 1 } else { pos - 1 };
        self.query.group = if next < 0 || next >= len {
            None
        } else {
            Some(self.groups[next as usize].clone())
        };
        self.recompute();
    }
}

// ---------------------------------------------------------------------------
// Running
// ---------------------------------------------------------------------------

/// Everything the loop selects over.
enum Input {
    Term(TermEvent),
    Probe(probe::Event),
    /// Repos the watcher saw change.
    Changed(Vec<PathBuf>),
    /// The periodic backstop sweep is due.
    Resweep,
    /// The periodic fetch is due, if one is configured.
    AutoFetch,
    Tick,
}

pub async fn run() -> Result<()> {
    let (cfg, warning) = crate::config::load_or_default();
    let cfg = Arc::new(cfg);

    let mut app = App::new(cfg.clone());
    if let Some(warning) = warning {
        app.notify(format!(
            "Config could not be read, using defaults: {warning}"
        ));
    }

    let (mut terminal, modifier_events) = setup_terminal()?;
    app.modifier_events = modifier_events;
    let result = run_loop(&mut terminal, &mut app).await;
    restore_terminal(&mut terminal, modifier_events)?;
    result
}

async fn run_loop(terminal: &mut Terminal<CrosstermBackend<Stdout>>, app: &mut App) -> Result<()> {
    let (tx, mut rx) = mpsc::unbounded_channel::<Input>();

    // Paint before doing anything else, always. Nothing below this line gets to
    // decide whether the user sees a window or a blank screen: if some piece of
    // setup turns out to be slow, the dashboard is already up and saying so.
    app.sweeping = true;
    draw(terminal, app)?;

    // Terminal events come from a blocking thread; crossterm's reader isn't
    // async and this keeps the loop free of polling.
    {
        let tx = tx.clone();
        std::thread::spawn(move || {
            while let Ok(ev) = event::read() {
                if tx.send(Input::Term(ev)).is_err() {
                    break;
                }
            }
        });
    }

    // Repaint clock.
    {
        let tx = tx.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(TICK);
            loop {
                interval.tick().await;
                if tx.send(Input::Tick).is_err() {
                    break;
                }
            }
        });
    }

    // Start scanning before setting up the watcher, so the slower of the two
    // never delays the other.
    start_sweep(app, &tx, Tier::Full);

    // Filesystem watcher, so edits show up without waiting for the next sweep.
    // The handle lives until the loop exits; dropping it stops the watcher.
    let _watcher = if app.cfg.refresh.watch {
        let tx = tx.clone();
        match watch::spawn(app.cfg.clone(), move |paths| {
            let _ = tx.send(Input::Changed(paths));
        }) {
            Ok(handle) => {
                app.watching = true;
                Some(handle)
            }
            Err(err) => {
                app.notify(format!("Filesystem watching is off: {err:#}"));
                None
            }
        }
    } else {
        None
    };

    // Periodic full sweep as a backstop for anything the watcher misses.
    {
        let tx = tx.clone();
        let interval = app.cfg.refresh_interval();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            ticker.tick().await; // the first tick fires immediately
            loop {
                ticker.tick().await;
                if tx.send(Input::Resweep).is_err() {
                    break;
                }
            }
        });
    }

    // Optional periodic fetch, so behind counts don't silently go stale.
    if app.cfg.remote.fetch {
        let tx = tx.clone();
        let interval = app.cfg.remote_interval();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            ticker.tick().await;
            loop {
                ticker.tick().await;
                if tx.send(Input::AutoFetch).is_err() {
                    break;
                }
            }
        });
    }

    let mut dirty = true;
    let mut idle_ticks = 0u32;
    loop {
        if dirty {
            draw(terminal, app)?;
            dirty = false;
        }

        let Some(input) = rx.recv().await else { break };
        match input {
            Input::Tick => {
                // While a sweep is running, every tick repaints so the spinner
                // turns and the counter climbs. Idle, repaint about once a
                // second, which is often enough for the age column and quiet
                // enough to leave open all day.
                app.spinner = app.spinner.wrapping_add(1);
                idle_ticks += 1;
                if app.sweeping || idle_ticks >= 4 {
                    idle_ticks = 0;
                    dirty = true;
                }
            }
            Input::Term(TermEvent::Key(key)) => {
                // Every key event carries the modifier state, and with the
                // protocol on, the modifiers arrive as events of their own.
                // Tracked before dispatch so the footer is right in the same
                // frame as whatever the key did.
                track_modifiers(app, &key);
                // Repeat counts as press. Without the keyboard protocol a held
                // key just sends more presses, but `REPORT_EVENT_TYPES` splits
                // them out -- so ignoring them would mean holding `j` scrolled
                // exactly one row in the terminals this feature is for.
                if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
                    && !matches!(key.code, KeyCode::Modifier(_))
                {
                    handle_key(app, key, &tx);
                }
                dirty = true;
            }
            Input::Term(TermEvent::Resize(_, _)) => {
                terminal.autoresize()?;
                dirty = true;
            }
            Input::Term(TermEvent::Mouse(ev)) => {
                handle_mouse(app, ev, terminal.size().ok());
                dirty = true;
            }
            Input::Term(_) => {}
            Input::Probe(event) => {
                handle_probe_event(app, event);
                dirty = true;
            }
            Input::Changed(paths) => {
                reprobe_paths(app, paths, &tx);
                dirty = true;
            }
            Input::Resweep => {
                start_sweep(app, &tx, Tier::Full);
                dirty = true;
            }
            Input::AutoFetch => {
                let roots: Vec<PathBuf> = app
                    .repos
                    .iter()
                    .filter(|r| {
                        r.refs
                            .as_ref()
                            .and_then(|refs| refs.remote_url.as_ref())
                            .is_some()
                    })
                    .map(|r| r.root.clone())
                    .collect();
                if !roots.is_empty() {
                    app.notify(format!("Fetching {} repos in the background", roots.len()));
                    spawn_fetch(app, &tx, roots);
                }
                dirty = true;
            }
        }

        if app.should_quit {
            break;
        }
    }
    Ok(())
}

/// One frame. Kept separate so the first paint can happen before any setup.
fn draw(terminal: &mut Terminal<CrosstermBackend<Stdout>>, app: &mut App) -> Result<()> {
    app.now = git::now_unix();
    terminal.draw(|f| {
        app.rows_on_screen = ui::table_rows(f.area());
        app.overlay_max_scroll = ui::render(f, app);
    })?;
    Ok(())
}

fn handle_probe_event(app: &mut App, event: probe::Event) {
    match event {
        probe::Event::Discovered { roots } => {
            app.progress = (0, roots.len());
            app.retain_roots(&roots);
        }
        probe::Event::Refs(status) => {
            app.upsert(*status);
            app.progress.0 += 1;
        }
        probe::Event::Work(status) => {
            app.upsert(*status);
        }
        probe::Event::Phase { name, elapsed } => {
            if name == "refs" {
                app.timings.refs = elapsed;
            }
        }
        probe::Event::Done { elapsed } => {
            app.sweeping = false;
            app.timings.total = elapsed;
            app.last_sweep_at = Some(git::now_unix());
            app.sweep_started = None;
            app.reindex();
            let _ = cache::save(&app.repos);
        }
    }
    app.recompute();
}

/// Kick off a sweep in the background, streaming results into the loop.
fn start_sweep(app: &mut App, tx: &mpsc::UnboundedSender<Input>, tier: Tier) {
    if app.sweeping {
        // Only skip if the sweep is plausibly still going. A sweep that never
        // reported back must not be able to block every one after it.
        let stuck = app
            .sweep_started
            .map(|at| at.elapsed() >= SWEEP_STUCK_AFTER)
            .unwrap_or(true);
        if !stuck {
            return;
        }
        tracing::warn!("previous sweep never finished; starting another");
        app.notify("The last sweep never finished. Starting another.");
    }
    app.sweeping = true;
    app.sweep_started = Some(Instant::now());
    let cfg = app.cfg.clone();
    let tx = tx.clone();
    tokio::spawn(async move {
        let (ptx, mut prx) = mpsc::unbounded_channel::<probe::Event>();
        let forward = tokio::spawn(async move {
            while let Some(event) = prx.recv().await {
                if tx.send(Input::Probe(event)).is_err() {
                    break;
                }
            }
        });
        if let Err(err) = probe::sweep(cfg, tier, probe::Fetch::Skip, Some(ptx)).await {
            tracing::warn!(error = %format!("{err:#}"), "sweep failed");
        }
        let _ = forward.await;
    });
}

/// Re-probe just the repos the watcher flagged. This is the whole point of
/// watching: a single repo costs milliseconds, where a full sweep costs seconds.
fn reprobe_paths(app: &mut App, paths: Vec<PathBuf>, tx: &mpsc::UnboundedSender<Input>) {
    let mut targets: Vec<(PathBuf, String, String)> = Vec::new();
    for path in paths {
        if let Some(&idx) = app.by_root.get(&path) {
            let repo = &app.repos[idx];
            targets.push((repo.root.clone(), repo.group.clone(), repo.name.clone()));
        }
    }
    if targets.is_empty() {
        return;
    }

    let cfg = app.cfg.clone();
    let tx = tx.clone();
    let cached: HashMap<PathBuf, RepoStatus> = targets
        .iter()
        .filter_map(|(root, _, _)| {
            app.by_root
                .get(root)
                .map(|&i| (root.clone(), app.repos[i].clone()))
        })
        .collect();

    tokio::spawn(async move {
        for (root, group, name) in targets {
            let d = crate::discover::Discovered {
                root: root.clone(),
                group,
                name,
            };
            // Forced: the watcher only flags a repo because a file under it
            // changed, and the cache key would not notice an unstaged edit.
            let status = probe::probe_one(&d, &cfg, cached.get(&root), Tier::Full, true).await;
            if tx
                .send(Input::Probe(probe::Event::Work(Box::new(status))))
                .is_err()
            {
                break;
            }
        }
    });
}

// ---------------------------------------------------------------------------
// Keys
// ---------------------------------------------------------------------------

/// Mouse input. The wheel moves the selection rather than scrolling the
/// viewport under it, so it lands in the same place as `j` and `k` and the
/// selected row never drifts off screen.
fn handle_mouse(app: &mut App, ev: MouseEvent, size: Option<Size>) {
    // The detail and help panes cover the table, so a click there has nothing
    // to hit and the wheel belongs to the pane.
    match app.mode {
        // Both panes scroll on the same state, and the wheel is what people
        // reach for before they find j/k.
        Mode::Detail | Mode::Help => {
            match ev.kind {
                MouseEventKind::ScrollDown => app.scroll_overlay(1),
                MouseEventKind::ScrollUp => app.scroll_overlay(-1),
                _ => {}
            }
            return;
        }
        // The picker doesn't scroll -- it's short enough to fit -- so the
        // wheel moves the cursor, which is the equivalent gesture.
        Mode::Columns => {
            match ev.kind {
                MouseEventKind::ScrollDown => app.move_column_cursor(1),
                MouseEventKind::ScrollUp => app.move_column_cursor(-1),
                _ => {}
            }
            return;
        }
        _ => {}
    }

    match ev.kind {
        MouseEventKind::ScrollDown => app.move_selection(1),
        MouseEventKind::ScrollUp => app.move_selection(-1),
        MouseEventKind::Down(MouseButton::Left) => {
            let Some(size) = size else { return };
            let first = ui::table_first_row(Rect::new(0, 0, size.width, size.height));
            if app.select_at_row(ev.row, first) {
                app.detail_scroll = 0;
            }
        }
        _ => {}
    }
}

/// Keep [`App::mods`] in step with what's actually held down.
///
/// A modifier key reports as its own press and release; anything else carries
/// the modifiers that were down when it was typed. Both are used: the first
/// is what makes holding shift alone change the footer, the second keeps the
/// state honest if a press is somehow missed.
///
/// Does nothing at all unless the terminal reports modifier keys, since
/// otherwise the only sighting of shift is the `O` you just typed, and the
/// footer would sit there claiming shift was held long after it wasn't.
fn track_modifiers(app: &mut App, key: &KeyEvent) {
    if !app.modifier_events {
        return;
    }
    match key.code {
        KeyCode::Modifier(which) => {
            let flag = match which {
                ModifierKeyCode::LeftShift | ModifierKeyCode::RightShift => KeyModifiers::SHIFT,
                ModifierKeyCode::LeftControl | ModifierKeyCode::RightControl => {
                    KeyModifiers::CONTROL
                }
                ModifierKeyCode::LeftAlt | ModifierKeyCode::RightAlt => KeyModifiers::ALT,
                ModifierKeyCode::LeftSuper | ModifierKeyCode::RightSuper => KeyModifiers::SUPER,
                _ => return,
            };
            match key.kind {
                KeyEventKind::Press | KeyEventKind::Repeat => app.mods.insert(flag),
                KeyEventKind::Release => app.mods.remove(flag),
            }
        }
        // A released key reports the modifiers that were down for the press,
        // which is history by the time it arrives.
        _ if key.kind == KeyEventKind::Release => {}
        _ => app.mods = key.modifiers,
    }
}

/// Fold a held shift into the character itself, so `O` means `O`.
///
/// Under the kitty protocol a shifted letter can arrive as the unshifted
/// codepoint with a shift flag beside it, which no `KeyCode::Char('O')` arm
/// would ever match. Every binding in here is written as the character you
/// actually type, so this puts the event back into that shape rather than
/// making each arm ask about modifiers.
fn normalise_shift(mut key: KeyEvent) -> KeyEvent {
    if key.modifiers.contains(KeyModifiers::SHIFT) {
        if let KeyCode::Char(c) = key.code {
            if let Some(upper) = c.to_uppercase().next() {
                if upper != c {
                    key.code = KeyCode::Char(upper);
                }
            }
        }
    }
    key
}

fn handle_key(app: &mut App, key: KeyEvent, tx: &mpsc::UnboundedSender<Input>) {
    let key = normalise_shift(key);
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        match key.code {
            KeyCode::Char('c') => {
                app.should_quit = true;
                return;
            }
            KeyCode::Char('r') => {
                start_sweep(app, tx, Tier::Full);
                app.notify("Rescanning");
                return;
            }
            // Beside ctrl-r because they're the pair: one re-reads every repo
            // on disk, the other re-checks every repo against its remote.
            KeyCode::Char('f') => {
                fetch_fleet(app, tx);
                return;
            }
            // The third of the o-family: `o` Finder, `O` editor, `ctrl-o`
            // terminal. `T` still does the same thing.
            KeyCode::Char('o') => {
                open_terminal(app);
                return;
            }
            KeyCode::Char('d') => {
                let page = app.rows_on_screen as isize / 2;
                app.move_selection(page);
                return;
            }
            KeyCode::Char('u') => {
                let page = app.rows_on_screen as isize / 2;
                app.move_selection(-page);
                return;
            }
            _ => {}
        }
    }

    match app.mode {
        Mode::Search => handle_search_key(app, key),
        Mode::Help => match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('?') => {
                app.mode = Mode::Normal;
                app.detail_scroll = 0;
            }
            KeyCode::Char('j') | KeyCode::Down => app.scroll_overlay(1),
            KeyCode::Char('k') | KeyCode::Up => app.scroll_overlay(-1),
            _ => {}
        },
        Mode::Columns => match key.code {
            // Closing is what commits the change: the table has been redrawing
            // live the whole time, so this is a confirmation, not an apply.
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter | KeyCode::Char('C') => {
                app.save_columns();
                app.mode = Mode::Normal;
            }
            KeyCode::Char('j') | KeyCode::Down => app.move_column_cursor(1),
            KeyCode::Char('k') | KeyCode::Up => app.move_column_cursor(-1),
            KeyCode::Char(' ') => {
                if app.toggle_selected_column() {
                    // Persisted straight away rather than at esc: this changed
                    // more than the table, and the sweep below is about to act
                    // on it.
                    app.save_columns();
                    start_sweep(app, tx, Tier::Refs);
                    app.notify("Visibility checking on. Asking gh now.");
                }
            }
            KeyCode::Char('J') => app.move_selected_column(1),
            KeyCode::Char('K') => app.move_selected_column(-1),
            KeyCode::Char('a') => app.reset_columns(),
            _ => {}
        },
        Mode::Detail => match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter => {
                app.mode = Mode::Normal;
                app.detail_scroll = 0;
            }
            KeyCode::Char('j') | KeyCode::Down => app.scroll_overlay(1),
            KeyCode::Char('k') | KeyCode::Up => app.scroll_overlay(-1),
            KeyCode::PageDown => app.detail_scroll = app.detail_scroll.saturating_add(10),
            KeyCode::PageUp => app.detail_scroll = app.detail_scroll.saturating_sub(10),
            KeyCode::Char('o') => open_file_manager(app),
            KeyCode::Char('O') => open_editor(app),
            KeyCode::Char('T') => open_terminal(app),
            KeyCode::Char('t') => open_git_client(app),
            KeyCode::Char('y') => copy_path(app),
            _ => {}
        },
        Mode::Normal => handle_normal_key(app, key, tx),
    }
}

fn handle_search_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Esc => {
            app.search_input.clear();
            app.query.search.clear();
            app.mode = Mode::Normal;
            app.recompute();
        }
        KeyCode::Enter => app.mode = Mode::Normal,
        KeyCode::Backspace => {
            app.search_input.pop();
            app.query.search = app.search_input.clone();
            app.recompute();
        }
        KeyCode::Char(c) => {
            app.search_input.push(c);
            app.query.search = app.search_input.clone();
            app.recompute();
        }
        _ => {}
    }
}

fn handle_normal_key(app: &mut App, key: KeyEvent, tx: &mpsc::UnboundedSender<Input>) {
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => app.should_quit = true,
        KeyCode::Char('C') => {
            app.column_cursor = 0;
            app.mode = Mode::Columns;
        }
        KeyCode::Char('?') => {
            app.mode = Mode::Help;
            app.detail_scroll = 0;
        }
        KeyCode::Enter => {
            if app.current().is_some() {
                app.mode = Mode::Detail;
                app.detail_scroll = 0;
            }
        }

        KeyCode::Char('j') | KeyCode::Down => app.move_selection(1),
        KeyCode::Char('k') | KeyCode::Up => app.move_selection(-1),
        KeyCode::PageDown => app.move_selection(app.rows_on_screen as isize),
        KeyCode::PageUp => app.move_selection(-(app.rows_on_screen as isize)),
        KeyCode::Home => app.move_selection(-(app.visible.len() as isize)),
        KeyCode::End => app.move_selection(app.visible.len() as isize),

        // Filter toggles.
        KeyCode::Char('d') => toggle(app, Filter::Dirty),
        KeyCode::Char('u') => toggle(app, Filter::Unpushed),
        KeyCode::Char('r') => toggle(app, Filter::NeedsRelease),
        KeyCode::Char('N') => toggle(app, Filter::Unreleased),
        KeyCode::Char('b') => toggle(app, Filter::Behind),
        KeyCode::Char('c') => toggle(app, Filter::Conflicted),
        KeyCode::Char('i') => toggle(app, Filter::InProgress),
        KeyCode::Char('x') => toggle(app, Filter::Detached),
        KeyCode::Char('e') => toggle(app, Filter::Error),
        KeyCode::Char('n') => toggle(app, Filter::Clean),
        KeyCode::Char('a') => {
            app.query.filters.clear();
            app.query.group = None;
            app.query.since = None;
            app.query.search.clear();
            app.search_input.clear();
            app.notify("Cleared all filters");
            app.recompute();
        }
        KeyCode::Char('&') => {
            app.query.match_mode = app.query.match_mode.toggled();
            app.notify(match app.query.match_mode {
                MatchMode::Any => "Matching any active filter",
                MatchMode::All => "Matching all active filters",
            });
            app.recompute();
        }

        KeyCode::Char('s') => {
            app.query.sort = app.query.sort.next(app.cfg.visibility.enabled);
            app.notify(format!("Sorted by {}", app.query.sort.label()));
            app.recompute();
        }
        KeyCode::Char('S') => {
            app.query.reverse = !app.query.reverse;
            app.recompute();
        }
        KeyCode::Char('[') => app.cycle_group(false),
        KeyCode::Char(']') => app.cycle_group(true),
        KeyCode::Char(c @ '0'..='4') => app.set_since_preset(c),
        KeyCode::Char('/') => {
            app.mode = Mode::Search;
            app.search_input = app.query.search.clone();
        }

        // Handing off to other tools.
        // `o` is the one you reach for most, so it's the file manager; the
        // editor is a shift away.
        KeyCode::Char('o') => open_file_manager(app),
        KeyCode::Char('O') => open_editor(app),
        KeyCode::Char('t') => open_git_client(app),
        KeyCode::Char('T') => open_terminal(app),
        KeyCode::Char('w') => open_remote(app),
        KeyCode::Char('y') => copy_path(app),
        KeyCode::Char('R') => {
            start_sweep(app, tx, Tier::Full);
            app.notify("Rescanning");
        }
        KeyCode::Char('f') => fetch_selected(app, tx),
        KeyCode::Char('F') => fetch_visible(app, tx),
        _ => {}
    }
}

/// Fetch one repo, then re-probe it so the behind count updates.
fn fetch_selected(app: &mut App, tx: &mpsc::UnboundedSender<Input>) {
    let Some(repo) = app.current() else { return };
    if repo
        .refs
        .as_ref()
        .and_then(|r| r.remote_url.as_ref())
        .is_none()
    {
        app.notify("That repo has no remote to fetch from");
        return;
    }
    let root = repo.root.clone();
    let slug = repo.slug();
    app.notify(format!("Fetching {slug}"));
    spawn_fetch(app, tx, vec![root]);
}

/// Fetch everything currently on screen. Bounded by the configured concurrency,
/// because this is the one operation here that touches the network.
fn fetch_visible(app: &mut App, tx: &mpsc::UnboundedSender<Input>) {
    let roots: Vec<PathBuf> = app
        .visible
        .iter()
        .map(|i| &app.repos[*i])
        .filter(|r| {
            r.refs
                .as_ref()
                .and_then(|refs| refs.remote_url.as_ref())
                .is_some()
        })
        .map(|r| r.root.clone())
        .collect();

    if roots.is_empty() {
        app.notify("Nothing on screen has a remote");
        return;
    }
    app.notify(format!("Fetching {} repos", roots.len()));
    spawn_fetch(app, tx, roots);
}

/// Fetch every repo in the fleet, filters and scrolling irrelevant.
///
/// `F` fetches what's on screen, which is the right default -- it's bounded by
/// what you're looking at. But the repos worth knowing about are exactly the
/// ones you aren't looking at: a repo you haven't filtered to, haven't
/// scrolled to, and haven't thought about is where an unnoticed upstream
/// change sits. This is the one that checks those.
fn fetch_fleet(app: &mut App, tx: &mpsc::UnboundedSender<Input>) {
    let roots = fleet_roots(app);
    if roots.is_empty() {
        app.notify("Nothing in the fleet has a remote");
        return;
    }
    app.notify(format!(
        "Fetching all {} repos with a remote, rows update as they land",
        roots.len()
    ));
    spawn_fetch(app, tx, roots);
}

/// Every repo with a remote, `app.visible` deliberately not consulted.
fn fleet_roots(app: &App) -> Vec<PathBuf> {
    app.repos
        .iter()
        .filter(|r| {
            r.refs
                .as_ref()
                .and_then(|refs| refs.remote_url.as_ref())
                .is_some()
        })
        .map(|r| r.root.clone())
        .collect()
}

fn spawn_fetch(app: &App, tx: &mpsc::UnboundedSender<Input>, roots: Vec<PathBuf>) {
    let cfg = app.cfg.clone();
    let tx = tx.clone();
    // The current rows come along as the cache, so re-probing after a fetch
    // updates the tracking counts without discarding working-tree numbers.
    let known: HashMap<PathBuf, RepoStatus> = roots
        .iter()
        .filter_map(|root| {
            app.by_root
                .get(root)
                .map(|&i| (root.clone(), app.repos[i].clone()))
        })
        .collect();

    tokio::spawn(async move {
        let timeout = cfg.remote_timeout();
        let limit = Arc::new(tokio::sync::Semaphore::new(cfg.remote.concurrency.max(1)));
        let mut set = tokio::task::JoinSet::new();

        for (root, previous) in known {
            let cfg = cfg.clone();
            let tx = tx.clone();
            let limit = limit.clone();
            set.spawn(async move {
                let _permit = limit.acquire().await;
                if let Err(err) = git::fetch(&root, timeout).await {
                    tracing::debug!(repo = %root.display(), error = %format!("{err:#}"), "fetch failed");
                }
                let d = crate::discover::Discovered {
                    root: root.clone(),
                    group: previous.group.clone(),
                    name: previous.name.clone(),
                };
                let status = probe::probe_one(&d, &cfg, Some(&previous), Tier::Refs, false).await;
                let _ = tx.send(Input::Probe(probe::Event::Refs(Box::new(status))));
            });
        }
        while set.join_next().await.is_some() {}
    });
}

fn toggle(app: &mut App, filter: Filter) {
    app.query.toggle(filter);
    let on = app.query.has(filter);
    app.notify(format!(
        "{} {}",
        if on { "Showing" } else { "No longer filtering" },
        filter.label()
    ));
    app.recompute();
}

// ---------------------------------------------------------------------------
// Handing off to other tools
// ---------------------------------------------------------------------------

fn spawn_command(app: &mut App, template: &[String], path: &std::path::Path, what: &str) {
    if template.is_empty() {
        app.notify(format!("No {what} command is configured"));
        return;
    }
    let args: Vec<String> = template
        .iter()
        .map(|a| a.replace("{path}", &path.to_string_lossy()))
        .collect();
    match std::process::Command::new(&args[0])
        .args(&args[1..])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(_) => app.notify(format!("Opened in {what}")),
        Err(err) => app.notify(format!("Could not run {}: {err}", args[0])),
    }
}

fn open_editor(app: &mut App) {
    let Some(path) = app.current().map(|r| r.root.clone()) else {
        return;
    };
    let template = app.cfg.ui.editor_command.clone();
    spawn_command(app, &template, &path, "editor");
}

fn open_git_client(app: &mut App) {
    let Some(path) = app.current().map(|r| r.root.clone()) else {
        return;
    };
    let template = app.cfg.ui.git_client_command.clone();
    spawn_command(app, &template, &path, "git client");
}

fn open_terminal(app: &mut App) {
    let Some(path) = app.current().map(|r| r.root.clone()) else {
        return;
    };
    let template = app.cfg.ui.terminal_command.clone();
    spawn_command(app, &template, &path, "terminal");
}

fn open_file_manager(app: &mut App) {
    let Some(path) = app.current().map(|r| r.root.clone()) else {
        return;
    };
    let template = app.cfg.ui.file_manager_command.clone();
    spawn_command(app, &template, &path, "Finder");
}

/// Open the repo's remote in a browser, converting an SSH remote to its https
/// equivalent first.
fn open_remote(app: &mut App) {
    let Some(url) = app
        .current()
        .and_then(|r| r.refs.as_ref())
        .and_then(|refs| refs.remote_url.clone())
    else {
        app.notify("That repo has no remote");
        return;
    };
    let web = web_url(&url);
    match std::process::Command::new("open").arg(&web).spawn() {
        Ok(_) => app.notify(format!("Opened {web}")),
        Err(err) => app.notify(format!("Could not open {web}: {err}")),
    }
}

/// Turn a git remote into something a browser can open.
pub fn web_url(remote: &str) -> String {
    let trimmed = remote.trim().trim_end_matches(".git");
    if let Some(rest) = trimmed.strip_prefix("git@") {
        if let Some((host, path)) = rest.split_once(':') {
            return format!("https://{host}/{path}");
        }
    }
    if let Some(rest) = trimmed.strip_prefix("ssh://git@") {
        return format!("https://{rest}");
    }
    trimmed.to_string()
}

fn copy_path(app: &mut App) {
    let Some(path) = app.current().map(|r| r.root.clone()) else {
        return;
    };
    let text = path.to_string_lossy().to_string();
    let result = (|| -> std::io::Result<()> {
        use std::io::Write;
        let mut child = std::process::Command::new("pbcopy")
            .stdin(std::process::Stdio::piped())
            .spawn()?;
        if let Some(stdin) = child.stdin.as_mut() {
            stdin.write_all(text.as_bytes())?;
        }
        child.wait()?;
        Ok(())
    })();
    match result {
        Ok(()) => app.notify(format!("Copied {text}")),
        Err(err) => app.notify(format!("Could not copy: {err}")),
    }
}

// ---------------------------------------------------------------------------
// Terminal lifecycle and headless rendering
// ---------------------------------------------------------------------------

/// Returns the terminal, and whether it reports modifier keys on their own.
fn setup_terminal() -> Result<(Terminal<CrosstermBackend<Stdout>>, bool)> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;

    // Standalone modifier keys are only reported under the kitty keyboard
    // protocol, and only with `REPORT_ALL_KEYS_AS_ESCAPE_CODES` -- the spec is
    // explicit that a bare shift is invisible without it. Both flags together,
    // or neither: asking for event types alone would buy nothing.
    //
    // Terminals that don't support it are left exactly as they were. This is
    // the whole reason the footer's live hints are a nicety rather than the
    // only way to learn a key: `?` lists everything, whatever you're running.
    let modifier_events = crossterm::terminal::supports_keyboard_enhancement().unwrap_or(false);
    if modifier_events {
        let _ = execute!(
            stdout,
            PushKeyboardEnhancementFlags(
                KeyboardEnhancementFlags::REPORT_EVENT_TYPES
                    | KeyboardEnhancementFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES
                    // Without this, a shifted key arrives as its *unshifted*
                    // codepoint plus a shift flag -- `shift+o` as `o`, not
                    // `O` -- and every uppercase binding here would stop
                    // working the moment the protocol was on. See
                    // [`normalise_shift`], which covers it even if a terminal
                    // accepts the flag and then doesn't honour it.
                    | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
            )
        );
    }
    Ok((
        Terminal::new(CrosstermBackend::new(stdout))?,
        modifier_events,
    ))
}

fn restore_terminal(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    modifier_events: bool,
) -> Result<()> {
    disable_raw_mode()?;
    // Popped before leaving the alternate screen, so the flags don't outlive
    // the dashboard and leave the shell that follows reading keys differently.
    if modifier_events {
        let _ = execute!(terminal.backend_mut(), PopKeyboardEnhancementFlags);
    }
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;
    Ok(())
}

/// Render one frame to plain text. Lets layout be checked without a terminal,
/// which is the only practical way to review a TUI from a script.
pub async fn snapshot(width: u16, height: u16, view: &str) -> Result<String> {
    let (cfg, _) = crate::config::load_or_default();
    let cfg = Arc::new(cfg);
    let mut app = App::new(cfg.clone());

    // Cold-start states are rendered without probing, so the "first scan"
    // placeholder can be checked the same way every other view is.
    if view == "scanning" {
        app.repos.clear();
        app.reindex();
        app.sweeping = true;
        app.progress = (0, 0);
        app.rows_on_screen = height.saturating_sub(6) as usize;
        app.recompute();
        return render_once(&app, width, height);
    }
    if view == "scanning-partial" {
        app.sweeping = true;
        app.progress = (137, 558);
    }

    if app.repos.is_empty() {
        // Nothing cached, so probe enough to render something real.
        let fleet = probe::sweep(cfg, Tier::Full, probe::Fetch::Skip, None).await?;
        app.repos = fleet.repos;
        app.timings = fleet.timings;
        app.reindex();
    }
    app.rows_on_screen = height.saturating_sub(6) as usize;
    app.recompute();

    match view {
        // The footer changes with what's held down, and holding a key is the
        // one thing a snapshot can't do on its own.
        "shift" => app.mods = KeyModifiers::SHIFT,
        "ctrl" => app.mods = KeyModifiers::CONTROL,
        "help" => app.mode = Mode::Help,
        "detail" => app.mode = Mode::Detail,
        "columns" => app.mode = Mode::Columns,
        "search" => {
            app.mode = Mode::Search;
            app.search_input = "grav-plugin".into();
            app.query.search = app.search_input.clone();
            app.recompute();
        }
        "filtered" => {
            app.query.filters = vec![Filter::Dirty, Filter::Unpushed];
            let _ = app.query.set_since("1w");
            app.recompute();
        }
        _ => {}
    }

    render_once(&app, width, height)
}

fn render_once(app: &App, width: u16, height: u16) -> Result<String> {
    let backend = ratatui::backend::TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend)?;
    terminal.draw(|f| {
        ui::render(f, app);
    })?;

    let buffer = terminal.backend().buffer();
    let mut out = String::new();
    for y in 0..height {
        let mut line = String::new();
        for x in 0..width {
            if let Some(cell) = buffer.cell((x, y)) {
                line.push_str(cell.symbol());
            }
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app_with(rows: usize) -> App {
        let mut app = App::new(Arc::new(Config::default()));
        app.repos = (0..rows)
            .map(|i| {
                RepoStatus::new(
                    PathBuf::from(format!("/p/g/r{i}")),
                    "g".into(),
                    format!("r{i}"),
                )
            })
            .collect();
        app.reindex();
        app.query.filters.clear();
        app.recompute();
        app.visible = (0..rows).collect();
        app
    }

    fn picker_app() -> App {
        let mut app = app_with(1);
        app.columns = Column::defaults(false);
        app.column_cursor = 0;
        app
    }

    // Holding shift should say what shift does. The terminal that can't
    // report a bare modifier keeps `mods` empty and gets the plain row, which
    // is the same row it always had.
    #[test]
    fn the_footer_follows_the_modifier_being_held() {
        let mut app = app_with(1);
        let plain = crate::tui::ui::key_hints(&app);
        assert!(plain.iter().any(|(k, what)| *k == "o" && *what == "finder"));

        app.mods = KeyModifiers::SHIFT;
        let shifted = crate::tui::ui::key_hints(&app);
        assert!(shifted
            .iter()
            .any(|(k, what)| *k == "O" && *what == "editor"));

        app.mods = KeyModifiers::CONTROL;
        let ctrl = crate::tui::ui::key_hints(&app);
        assert!(ctrl
            .iter()
            .any(|(k, what)| *k == "^o" && *what == "terminal"));

        // Both down is a chord in progress; ctrl's bindings are the same in
        // every mode, so showing them is never wrong.
        app.mods = KeyModifiers::SHIFT | KeyModifiers::CONTROL;
        assert_eq!(crate::tui::ui::key_hints(&app), ctrl);
    }

    // A terminal that reports the unshifted codepoint with a shift flag would
    // otherwise miss every uppercase binding in the dashboard.
    #[test]
    fn a_shifted_letter_is_the_letter_you_typed() {
        let shifted = normalise_shift(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::SHIFT));
        assert_eq!(shifted.code, KeyCode::Char('O'));

        // Terminals that send the shifted key directly are already right.
        let already = normalise_shift(KeyEvent::new(KeyCode::Char('O'), KeyModifiers::SHIFT));
        assert_eq!(already.code, KeyCode::Char('O'));

        // Nothing else is touched: ctrl-f is not ctrl-F.
        let ctrl = normalise_shift(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL));
        assert_eq!(ctrl.code, KeyCode::Char('f'));
    }

    // Without the keyboard protocol the only sighting of shift is the `O` you
    // just typed, and a latched footer would claim it was still held.
    #[test]
    fn modifiers_are_not_tracked_when_the_terminal_cannot_report_them() {
        let mut app = app_with(1);
        app.modifier_events = false;
        track_modifiers(
            &mut app,
            &KeyEvent::new(KeyCode::Char('O'), KeyModifiers::SHIFT),
        );
        assert_eq!(app.mods, KeyModifiers::NONE);

        app.modifier_events = true;
        track_modifiers(
            &mut app,
            &KeyEvent::new(KeyCode::Char('O'), KeyModifiers::SHIFT),
        );
        assert_eq!(app.mods, KeyModifiers::SHIFT);

        // Letting go of shift itself clears it.
        let mut release = KeyEvent::new(
            KeyCode::Modifier(ModifierKeyCode::LeftShift),
            KeyModifiers::SHIFT,
        );
        release.kind = KeyEventKind::Release;
        track_modifiers(&mut app, &release);
        assert_eq!(app.mods, KeyModifiers::NONE);
    }

    /// The whole point of ctrl-f over `F`: the repo you can't see is exactly
    /// the one whose upstream changes you don't know about.
    #[test]
    fn the_fleet_fetch_ignores_what_is_filtered_or_scrolled_away() {
        let mut app = app_with(3);
        for (i, repo) in app.repos.iter_mut().enumerate() {
            let mut refs = crate::model::RefsInfo {
                head: crate::model::Head::Branch("main".into()),
                branches: Vec::new(),
                last_commit: None,
                stashes: 0,
                operation: None,
                newest_tag: None,
                described_tag: None,
                commits_since_tag: None,
                since_tag_subjects: Vec::new(),
                tags_orphaned: false,
                index_mtime: None,
                fetched_at: None,
                remote_url: Some(format!("git@github.com:owner/r{i}.git")),
                changelog: None,
                is_bare: false,
                is_shallow: false,
            };
            // The last one has no remote, so there's nothing to fetch from.
            if i == 2 {
                refs.remote_url = None;
            }
            repo.refs = Some(refs);
        }
        // Only one row is on screen. `F` would fetch that one.
        app.visible = vec![0];

        let roots = fleet_roots(&app);
        assert_eq!(
            roots,
            vec![PathBuf::from("/p/g/r0"), PathBuf::from("/p/g/r1")]
        );
    }

    fn cursor_on(app: &App) -> Column {
        app.picker_rows()[app.column_cursor].0
    }

    /// The list is "on screen, in render order" then "not on screen", so the
    /// panel reads as the table does rather than as an alphabet.
    #[test]
    fn the_picker_lists_shown_columns_first_then_hidden_ones() {
        let app = picker_app();
        let rows = app.picker_rows();
        assert_eq!(rows.len(), Column::all().len(), "every column is listed");
        let shown: Vec<Column> = rows.iter().filter(|(_, on)| *on).map(|(c, _)| *c).collect();
        assert_eq!(shown, app.columns);
        // Both visibility forms, STASH and FETCHED sit in the hidden section
        // by default -- the four opt-in columns.
        let hidden: Vec<Column> = rows
            .iter()
            .filter(|(_, on)| !*on)
            .map(|(c, _)| *c)
            .collect();
        assert_eq!(
            hidden,
            vec![
                Column::Visibility,
                Column::VisibilityShort,
                Column::Stashes,
                Column::Fetched
            ]
        );
    }

    // Turning a column back on should put it where the defaults have it, not
    // on the far right -- otherwise enabling VISIBILITY lands it past AGE.
    #[test]
    fn a_column_turned_back_on_returns_to_its_canonical_place() {
        let mut app = picker_app();
        let at = app
            .picker_rows()
            .iter()
            .position(|(c, _)| *c == Column::Visibility)
            .unwrap();
        app.column_cursor = at;
        app.toggle_selected_column();
        assert_eq!(
            app.columns,
            Column::defaults(true),
            "enabling VISIBILITY should reproduce the enabled defaults exactly"
        );
    }

    /// The cursor tracks the column, not the row index, so a toggle doesn't
    /// leave it pointing at whatever slid into that slot.
    #[test]
    fn the_cursor_follows_the_column_it_was_on_across_a_toggle() {
        let mut app = picker_app();
        let at = app
            .picker_rows()
            .iter()
            .position(|(c, _)| *c == Column::Tag)
            .unwrap();
        app.column_cursor = at;
        app.toggle_selected_column();
        assert!(!app.columns.contains(&Column::Tag));
        assert_eq!(cursor_on(&app), Column::Tag, "cursor drifted off TAG");
        app.toggle_selected_column();
        assert!(app.columns.contains(&Column::Tag));
        assert_eq!(cursor_on(&app), Column::Tag);
    }

    // Asking for the column is asking for the values. Leaving checking off
    // would give a column that can only ever read "checking off".
    // The legend only earns its space when one of the visibility columns is
    // actually on screen, and it has to appear for either form.
    // A wheel reaches the end of a pane far faster than j/k, so an unclamped
    // scroll leaves you staring at blank space with no clue which way is back.
    #[test]
    fn an_overlay_scroll_stops_at_the_end_of_its_content() {
        let mut app = app_with(1);
        app.overlay_max_scroll = 4;
        app.scroll_overlay(100);
        assert_eq!(app.detail_scroll, 4);
        app.scroll_overlay(1);
        assert_eq!(app.detail_scroll, 4, "should not have gone past the end");
        app.scroll_overlay(-100);
        assert_eq!(app.detail_scroll, 0);
        app.scroll_overlay(-1);
        assert_eq!(app.detail_scroll, 0, "should not have gone above the top");
    }

    // A pane that fits needs no scrolling at all, and the wheel shouldn't
    // pretend otherwise.
    #[test]
    fn an_overlay_that_fits_does_not_scroll() {
        let mut app = app_with(1);
        app.overlay_max_scroll = 0;
        app.scroll_overlay(3);
        assert_eq!(app.detail_scroll, 0);
    }

    #[test]
    fn the_help_legend_follows_the_columns_on_screen() {
        let titles = |app: &App| -> Vec<&'static str> {
            ui::marker_legend(app).into_iter().map(|(t, _)| t).collect()
        };
        let mut app = picker_app();
        assert!(!titles(&app).contains(&"VISIBILITY"));
        for form in [Column::Visibility, Column::VisibilityShort] {
            app.columns = Column::defaults(false);
            app.columns.push(form);
            assert!(
                titles(&app).contains(&"VISIBILITY"),
                "legend missing with {}",
                form.key()
            );
        }

        // Every group follows its own column, so a legend never explains
        // glyphs that aren't on screen.
        app.columns = Column::defaults(false);
        assert_eq!(titles(&app), vec!["STATE", "RELEASE", "CHANGES"]);
        app.columns = vec![Column::Repo];
        assert!(titles(&app).is_empty(), "nothing to explain, no legend");
    }

    #[test]
    fn showing_the_visibility_column_turns_checking_on_with_it() {
        let mut app = picker_app();
        assert!(!app.cfg.visibility.enabled);
        let at = app
            .picker_rows()
            .iter()
            .position(|(c, _)| *c == Column::Visibility)
            .unwrap();
        app.column_cursor = at;
        assert!(app.toggle_selected_column(), "should report the change");
        assert!(app.cfg.visibility.enabled);
    }

    // The short form is the same data, so it needs checking on just as much.
    #[test]
    fn showing_the_short_visibility_column_also_turns_checking_on() {
        let mut app = picker_app();
        assert!(!app.cfg.visibility.enabled);
        let at = app
            .picker_rows()
            .iter()
            .position(|(c, _)| *c == Column::VisibilityShort)
            .unwrap();
        app.column_cursor = at;
        assert!(app.toggle_selected_column());
        assert!(app.cfg.visibility.enabled);
    }

    // Not symmetrical: --public, --private and --json still read visibility,
    // so tidying the table mustn't silently disable them.
    #[test]
    fn hiding_the_visibility_column_leaves_checking_alone() {
        let mut app = picker_app();
        let at = app
            .picker_rows()
            .iter()
            .position(|(c, _)| *c == Column::Visibility)
            .unwrap();
        app.column_cursor = at;
        app.toggle_selected_column();
        assert!(app.cfg.visibility.enabled);
        assert!(
            !app.toggle_selected_column(),
            "hiding is not a config change"
        );
        assert!(app.columns.iter().all(|c| *c != Column::Visibility));
        assert!(app.cfg.visibility.enabled, "checking should have survived");
    }

    // Only VISIBILITY carries a config change; nothing else should touch it.
    #[test]
    fn toggling_any_other_column_changes_no_config() {
        let mut app = picker_app();
        // Found by identity each time rather than by a fixed index: toggling
        // moves a column between the two sections, so the row order shifts
        // underneath a plain counting loop.
        for column in Column::all() {
            if matches!(column, Column::Visibility | Column::VisibilityShort) {
                continue;
            }
            let at = app
                .picker_rows()
                .iter()
                .position(|(c, _)| c == column)
                .unwrap();
            app.column_cursor = at;
            assert!(
                !app.toggle_selected_column(),
                "{} reported a config change",
                column.key()
            );
        }
        assert!(!app.cfg.visibility.enabled);
    }

    #[test]
    fn reordering_moves_the_column_and_keeps_the_cursor_on_it() {
        let mut app = picker_app();
        app.column_cursor = 0;
        let first = cursor_on(&app);
        app.move_selected_column(1);
        assert_eq!(app.columns[1], first);
        assert_eq!(cursor_on(&app), first);
        assert_eq!(app.column_cursor, 1);
    }

    #[test]
    fn reordering_stops_at_the_ends_rather_than_wrapping() {
        let mut app = picker_app();
        let before = app.columns.clone();
        app.column_cursor = 0;
        app.move_selected_column(-1);
        assert_eq!(app.columns, before);
        app.column_cursor = before.len() - 1;
        app.move_selected_column(1);
        assert_eq!(app.columns, before);
    }

    // A table of rows you can't tell apart isn't worth rendering, so space on
    // REPO says so instead of quietly doing nothing.
    #[test]
    fn repo_cannot_be_toggled_off() {
        let mut app = picker_app();
        let at = app
            .picker_rows()
            .iter()
            .position(|(c, _)| *c == Column::Repo)
            .unwrap();
        app.column_cursor = at;
        app.toggle_selected_column();
        assert!(app.columns.contains(&Column::Repo));
        assert!(app.message.is_some(), "should have said why");
    }

    // Hidden columns aren't in a meaningful order, so J/K on one is a no-op
    // rather than a silent reorder of a list nobody sees.
    #[test]
    fn a_hidden_column_cannot_be_reordered() {
        let mut app = picker_app();
        let before = app.columns.clone();
        app.column_cursor = app.picker_rows().len() - 1;
        assert!(!app.picker_rows()[app.column_cursor].1);
        app.move_selected_column(-1);
        assert_eq!(app.columns, before);
    }

    #[test]
    fn the_cursor_cannot_leave_the_list() {
        let mut app = picker_app();
        app.move_column_cursor(-5);
        assert_eq!(app.column_cursor, 0);
        app.move_column_cursor(500);
        assert_eq!(app.column_cursor, app.picker_rows().len() - 1);
    }

    /// The click-to-row arithmetic has to agree with what actually gets drawn.
    /// Both sides are pinned here so a layout change can't quietly shift the
    /// selection by a row.
    #[test]
    fn a_click_lands_on_the_row_under_it() {
        let mut app = app_with(50);
        app.rows_on_screen = 10;
        let first = ui::table_first_row(Rect::new(0, 0, 130, 16));
        assert_eq!(first, 4, "title, filter bar, border, column header");

        assert!(app.select_at_row(first, first));
        assert_eq!(app.selected, 0);
        assert!(app.select_at_row(first + 3, first));
        assert_eq!(app.selected, 3);

        // Scrolled, the same screen row is a different repo.
        app.scroll = 20;
        assert!(app.select_at_row(first + 3, first));
        assert_eq!(app.selected, 23);

        // The header, the border and the title are not rows.
        app.selected = 7;
        for y in 0..first {
            assert!(!app.select_at_row(y, first), "row {y} is chrome");
        }
        assert_eq!(app.selected, 7, "a click on chrome leaves the selection");
    }

    /// Clicking past the end of a short list must not select a row that isn't
    /// there. Easy to get wrong, since the table is drawn full height.
    #[test]
    fn a_click_below_the_last_repo_does_nothing() {
        let mut app = app_with(3);
        let first = ui::table_first_row(Rect::new(0, 0, 130, 40));
        assert!(app.select_at_row(first + 2, first));
        assert_eq!(app.selected, 2);
        assert!(!app.select_at_row(first + 3, first));
        assert_eq!(app.selected, 2);
        assert!(!app.select_at_row(first + 30, first));
        assert_eq!(app.selected, 2);
    }

    /// A sweep that never reports back must not block the ones after it. This
    /// is what left a dashboard sitting on four-day-old data: one sweep died
    /// without emitting Done, and every later sweep hit the in-flight guard.
    #[tokio::test]
    async fn a_sweep_that_never_finished_does_not_block_the_next_one() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = app_with(1);

        start_sweep(&mut app, &tx, Tier::Full);
        assert!(app.sweeping);
        let first = app.sweep_started.expect("a sweep in flight is timed");

        // Still plausibly running, so nothing new starts.
        start_sweep(&mut app, &tx, Tier::Full);
        assert_eq!(app.sweep_started, Some(first), "no second sweep yet");

        // Past the point where it can still be alive, another one starts.
        app.sweep_started = Some(first - SWEEP_STUCK_AFTER);
        start_sweep(&mut app, &tx, Tier::Full);
        assert!(app.sweep_started.expect("restarted") > first);

        // And a sweep marked in flight with no start time at all, which no
        // longer happens but would be indistinguishable from wedged, restarts.
        app.sweep_started = None;
        start_sweep(&mut app, &tx, Tier::Full);
        assert!(app.sweep_started.is_some());
    }

    #[test]
    fn ssh_remotes_become_browsable() {
        assert_eq!(
            web_url("git@github.com:getgrav/grav.git"),
            "https://github.com/getgrav/grav"
        );
        assert_eq!(
            web_url("https://github.com/getgrav/grav.git"),
            "https://github.com/getgrav/grav"
        );
        assert_eq!(
            web_url("ssh://git@git.example.com/team/repo.git"),
            "https://git.example.com/team/repo"
        );
    }
}
