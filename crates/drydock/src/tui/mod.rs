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
        KeyEventKind, KeyModifiers, MouseEventKind,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};
use std::collections::HashMap;
use std::io::{self, Stdout};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

use crate::cache;
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

/// Braille spinner frames, advanced once per tick.
pub const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Search,
    Detail,
    Help,
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
    pub last_sweep_at: Option<Instant>,
    pub watching: bool,
    /// Advances every tick; drives the scanning spinner.
    pub spinner: usize,
    pub now: i64,
    pub rows_on_screen: usize,
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
            mode: Mode::Normal,
            search_input: String::new(),
            message: None,
            groups: Vec::new(),
            progress: (0, 0),
            sweeping: false,
            timings: Timings::default(),
            last_sweep_at: None,
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

    let mut terminal = setup_terminal()?;
    let result = run_loop(&mut terminal, &mut app).await;
    restore_terminal(&mut terminal)?;
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
            Input::Term(TermEvent::Key(key)) if key.kind == KeyEventKind::Press => {
                handle_key(app, key, &tx);
                dirty = true;
            }
            Input::Term(TermEvent::Resize(_, _)) => {
                terminal.autoresize()?;
                dirty = true;
            }
            Input::Term(TermEvent::Mouse(ev)) => {
                match ev.kind {
                    MouseEventKind::ScrollDown => app.move_selection(1),
                    MouseEventKind::ScrollUp => app.move_selection(-1),
                    _ => {}
                }
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
        ui::render(f, app);
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
            app.last_sweep_at = Some(Instant::now());
            app.reindex();
            let _ = cache::save(&app.repos);
        }
    }
    app.recompute();
}

/// Kick off a sweep in the background, streaming results into the loop.
fn start_sweep(app: &mut App, tx: &mpsc::UnboundedSender<Input>, tier: Tier) {
    if app.sweeping {
        return;
    }
    app.sweeping = true;
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
        if let Err(err) = probe::sweep(cfg, tier, Some(ptx)).await {
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
            let status = probe::probe_one(&d, &cfg, cached.get(&root), Tier::Full).await;
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

fn handle_key(app: &mut App, key: KeyEvent, tx: &mpsc::UnboundedSender<Input>) {
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
            KeyCode::Char('j') | KeyCode::Down => {
                app.detail_scroll = app.detail_scroll.saturating_add(1)
            }
            KeyCode::Char('k') | KeyCode::Up => {
                app.detail_scroll = app.detail_scroll.saturating_sub(1)
            }
            _ => {}
        },
        Mode::Detail => match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter => {
                app.mode = Mode::Normal;
                app.detail_scroll = 0;
            }
            KeyCode::Char('j') | KeyCode::Down => {
                app.detail_scroll = app.detail_scroll.saturating_add(1)
            }
            KeyCode::Char('k') | KeyCode::Up => {
                app.detail_scroll = app.detail_scroll.saturating_sub(1)
            }
            KeyCode::PageDown => app.detail_scroll = app.detail_scroll.saturating_add(10),
            KeyCode::PageUp => app.detail_scroll = app.detail_scroll.saturating_sub(10),
            KeyCode::Char('o') => open_editor(app),
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
        KeyCode::Char('r') => toggle(app, Filter::Unreleased),
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
            app.query.sort = app.query.sort.next();
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
        KeyCode::Char('o') => open_editor(app),
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
                let status = probe::probe_one(&d, &cfg, Some(&previous), Tier::Refs).await;
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

fn setup_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    Ok(Terminal::new(CrosstermBackend::new(stdout))?)
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    disable_raw_mode()?;
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
        let fleet = probe::sweep(cfg, Tier::Full, None).await?;
        app.repos = fleet.repos;
        app.timings = fleet.timings;
        app.reindex();
    }
    app.rows_on_screen = height.saturating_sub(6) as usize;
    app.recompute();

    match view {
        "help" => app.mode = Mode::Help,
        "detail" => app.mode = Mode::Detail,
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
    terminal.draw(|f| ui::render(f, app))?;

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
