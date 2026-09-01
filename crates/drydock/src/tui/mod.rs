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
use std::collections::{HashMap, VecDeque};
use std::io::{self, Stdout};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

use crate::cache;
use crate::column::Column;
use crate::config::{CloneProtocol, Config, OrgConfig, OrgProvider};
use crate::filter::{Filter, MatchMode, Query, Sort};
use crate::git;
use crate::model::RepoStatus;
use crate::org;
use crate::probe::{self, Tier, Timings};
use crate::provider;
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Search,
    Detail,
    Help,
    /// The column picker: toggle columns on and off, and reorder them.
    Columns,
    /// The org manager: registered owners, what's on disk for each, and the
    /// keys to add, edit, remove and sync them.
    Orgs,
    /// The org add/edit form: one field at a time, Enter to advance.
    OrgForm,
}

pub const SINCE_PRESETS: &[(&str, &str)] = &[
    ("0", ""),
    ("1", "1h"),
    ("2", "1d"),
    ("3", "1w"),
    ("4", "1mo"),
];

/// The org form's rows, top to bottom. The first six are value rows -- two
/// of them take typing (owner, path) and four cycle through what the CLI
/// tools reported as authenticated -- then three toggles. `OrgForm::field`
/// walks this order, and the overlay renders in it.
const FIELD_PROVIDER: usize = 0;
const FIELD_HOST: usize = 1;
const FIELD_OWNER: usize = 2;
const FIELD_PATH: usize = 3;
const FIELD_LOGIN: usize = 4;
const FIELD_PROTOCOL: usize = 5;
const ORG_TEXT_FIELDS: usize = 6;
const ORG_FIELD_COUNT: usize = ORG_TEXT_FIELDS + 3;

/// The add/edit form's state: which row is active, the picked values, the
/// toggles, and whether a save will edit an existing org or add one.
///
/// Provider and host are no longer typed: the form is seeded once with every
/// host the CLI tools reported as authenticated (see
/// [`crate::provider::authenticated_hosts`]) and both rows cycle through
/// that list. That is the whole auth gate -- an org on a host nothing is
/// logged in to could be registered but never listed, so the form never
/// offers one. Owner and path stay text (private memberships won't appear
/// in any listing), but the owner row can open a picker fed by
/// [`crate::provider::list_owners`].
pub struct OrgForm {
    /// `Some(i)` when editing `cfg.orgs[i]`, `None` when adding.
    pub editing: Option<usize>,
    /// Active row: `0..ORG_TEXT_FIELDS` are value rows, the rest are toggles.
    pub field: usize,
    /// Every host the tools reported as authenticated when the form opened.
    /// This is the gate the provider, host and login rows cycle through.
    pub authed: Vec<provider::AuthedHost>,
    /// True while the background probe that fills `authed` is still
    /// running. The form opens without waiting for it — a keypress must
    /// never cost a network round trip to `gh auth status` — and saving is
    /// refused until this clears.
    pub auth_probing: bool,
    pub provider: String,
    pub host: String,
    pub owner: String,
    pub path: String,
    /// The path row currently holds an auto-filled default rather than
    /// anything the user typed, so picking another owner may overwrite it.
    /// Any hand edit clears it.
    pub path_auto: bool,
    pub login: String,
    pub protocol: String,
    pub include_forks: bool,
    pub include_archived: bool,
    pub include_subgroups: bool,
    /// The owner picker is open over the form and swallows the keys.
    pub picking: bool,
    /// Owner listings per (provider, host), cached from the first fetch so
    /// reopening the picker is instant. Errors are cached too: re-probing on
    /// every open would turn a dead token into a visible stall.
    pub owners: HashMap<(String, String), Result<Vec<String>, String>>,
    /// A listing is in flight for the rows currently showing.
    pub owners_fetching: bool,
    pub picker_cursor: usize,
    pub picker_filter: String,
    /// The problem that refused the last save attempt. Drawn inside the
    /// overlay -- the status line sits underneath it, so a notify alone is
    /// invisible while the form is up.
    pub error: Option<String>,
}

impl OrgForm {
    /// Seed the form, either from an existing org for editing or from the
    /// authenticated hosts for adding. `authed` is everything the provider,
    /// host and login rows are allowed to hold; an edit whose org isn't in
    /// that list keeps its stored values (dimmed, and savable only
    /// unchanged) so tweaking one toggle never demands re-authenticating
    /// first. An `editing` index without a matching org falls back to the
    /// same shape -- it can only happen if the list shrank while the form
    /// was open, and the save is validated against the real config anyway.
    fn open(
        editing: Option<usize>,
        org: Option<&OrgConfig>,
        authed: Vec<provider::AuthedHost>,
    ) -> Self {
        let Some(org) = org else {
            let mut form = Self {
                editing: None,
                field: 0,
                authed,
                auth_probing: false,
                provider: String::new(),
                host: String::new(),
                owner: String::new(),
                path: String::new(),
                path_auto: false,
                login: String::new(),
                // The default everywhere else in the feature is ssh, so the
                // form starts there too rather than making it two keystrokes.
                protocol: "ssh".into(),
                include_forks: false,
                include_archived: false,
                include_subgroups: false,
                picking: false,
                owners: HashMap::new(),
                owners_fetching: false,
                picker_cursor: 0,
                picker_filter: String::new(),
                error: None,
            };
            // The first authenticated provider, with its first host, is
            // where a new org starts: the common case is one tool, one host.
            if let Some(first) = form.providers().first().copied() {
                form.set_provider(first);
            }
            return form;
        };
        Self {
            editing,
            field: 0,
            authed,
            auth_probing: false,
            // A stored org with no explicit provider infers it from its
            // host, the same resolution the orgs list and sync both use.
            provider: org
                .provider
                .or(OrgProvider::from_host(&org.host))
                .map(|p| p.as_str().to_string())
                .unwrap_or_default(),
            host: org.host.clone(),
            owner: org.owner.clone(),
            path: org.path.clone().unwrap_or_default(),
            path_auto: false,
            login: org.login.clone(),
            protocol: if org.protocol == CloneProtocol::Https {
                "https"
            } else {
                "ssh"
            }
            .to_string(),
            include_forks: org.include_forks,
            include_archived: org.include_archived,
            include_subgroups: org.include_subgroups,
            picking: false,
            owners: HashMap::new(),
            owners_fetching: false,
            picker_cursor: 0,
            picker_filter: String::new(),
            error: None,
        }
    }

    /// Open the form for an auth probe that is still in flight: the rows
    /// are usable the moment the probe lands, and until then the form is
    /// visible but refuses to save. `open` itself stays synchronous because
    /// tests and the probe-completion path seed it with a known list.
    fn open_probing(editing: Option<usize>, org: Option<&OrgConfig>) -> Self {
        let mut form = Self::open(editing, org, Vec::new());
        form.auth_probing = true;
        form
    }

    /// The providers that have at least one authenticated host, in the fixed
    /// order the config speaks about them, so the cycle is deterministic no
    /// matter what order the tools reported in.
    fn providers(&self) -> Vec<OrgProvider> {
        [OrgProvider::GitHub, OrgProvider::GitLab, OrgProvider::Gitea]
            .into_iter()
            .filter(|p| self.authed.iter().any(|h| h.provider == *p))
            .collect()
    }

    /// The authenticated hosts for one provider, deduped, in report order.
    fn hosts_for(&self, provider: OrgProvider) -> Vec<String> {
        let mut hosts: Vec<String> = Vec::new();
        for h in &self.authed {
            if h.provider == provider && !hosts.contains(&h.host) {
                hosts.push(h.host.clone());
            }
        }
        hosts
    }

    fn is_authed(&self, provider: OrgProvider, host: &str) -> bool {
        self.authed
            .iter()
            .any(|h| h.provider == provider && h.host == host)
    }

    /// The provider row's value parsed back. The row only ever holds the
    /// name of an authenticated provider or of the org being edited, so
    /// `None` means nothing usable is selected.
    fn provider_opt(&self) -> Option<OrgProvider> {
        match self.provider.as_str() {
            "github" => Some(OrgProvider::GitHub),
            "gitlab" => Some(OrgProvider::GitLab),
            "gitea" => Some(OrgProvider::Gitea),
            _ => None,
        }
    }

    fn active_is_toggle(&self) -> bool {
        self.field >= ORG_TEXT_FIELDS
    }

    /// The active text field, if the active row takes typing -- owner and
    /// path. The other rows cycle instead: a hand-typed host is exactly how
    /// an org nothing can list gets registered.
    fn text_mut(&mut self) -> Option<&mut String> {
        match self.field {
            FIELD_OWNER => Some(&mut self.owner),
            FIELD_PATH => Some(&mut self.path),
            _ => None,
        }
    }

    /// Flip the active toggle row. A no-op on a value row: the key handler
    /// routes space and Enter by row kind, this is just the back half.
    fn flip_toggle(&mut self) {
        match self.field - ORG_TEXT_FIELDS {
            0 => self.include_forks = !self.include_forks,
            1 => self.include_archived = !self.include_archived,
            2 => self.include_subgroups = !self.include_subgroups,
            _ => {}
        }
    }

    /// Cycle whatever the active row cycles, one step forward or back. A
    /// no-op on rows with nothing to cycle -- including login outside gitea,
    /// where there are no logins to step through.
    fn cycle_active(&mut self, forward: bool) {
        match self.field {
            FIELD_PROVIDER => self.cycle_provider(forward),
            FIELD_HOST => self.cycle_host(forward),
            FIELD_LOGIN => self.cycle_login(forward),
            FIELD_PROTOCOL => {
                self.protocol = if self.protocol == "https" {
                    "ssh"
                } else {
                    "https"
                }
                .into();
            }
            _ => {}
        }
    }

    fn cycle_provider(&mut self, forward: bool) {
        let options = self.providers();
        if options.is_empty() {
            return;
        }
        let len = options.len() as isize;
        // An unauthenticated seed sits outside the list; either direction
        // lands on a real option rather than staying stuck on it.
        let pos = options
            .iter()
            .position(|p| p.as_str() == self.provider)
            .map(|i| i as isize)
            .unwrap_or(-1);
        let next = (pos + if forward { 1 } else { -1 }).rem_euclid(len);
        self.set_provider(options[next as usize]);
    }

    /// Move to a different provider: its first authenticated host comes with
    /// it, and the owner goes -- it belonged to the previous host's listing.
    fn set_provider(&mut self, provider: OrgProvider) {
        self.provider = provider.as_str().to_string();
        self.owner.clear();
        self.login.clear();
        if let Some(host) = self.hosts_for(provider).first() {
            self.set_host(host);
        }
    }

    /// Move to a different host on the current provider. Gitea names its
    /// `tea` logins, and the first one for this host is the sensible
    /// default -- the login row can cycle through the rest.
    fn set_host(&mut self, host: &str) {
        self.host = host.to_string();
        self.login = self
            .authed
            .iter()
            .find(|h| Some(h.provider) == self.provider_opt() && h.host == host)
            .and_then(|h| h.name.clone())
            .unwrap_or_default();
    }

    fn cycle_host(&mut self, forward: bool) {
        let Some(provider) = self.provider_opt() else {
            return;
        };
        let hosts = self.hosts_for(provider);
        if hosts.is_empty() {
            return;
        }
        let len = hosts.len() as isize;
        let pos = hosts
            .iter()
            .position(|h| h == &self.host)
            .map(|i| i as isize)
            .unwrap_or(-1);
        let next = (pos + if forward { 1 } else { -1 }).rem_euclid(len);
        self.set_host(&hosts[next as usize]);
    }

    /// The `tea` login names recorded for the chosen host, in report order.
    /// Empty for every provider but gitea, which is why the login row dims
    /// there.
    fn tea_logins(&self) -> Vec<String> {
        let Some(provider) = self.provider_opt() else {
            return Vec::new();
        };
        if provider != OrgProvider::Gitea {
            return Vec::new();
        }
        self.authed
            .iter()
            .filter(|h| h.provider == provider && h.host == self.host)
            .filter_map(|h| h.name.clone())
            .collect()
    }

    fn cycle_login(&mut self, forward: bool) {
        let logins = self.tea_logins();
        if logins.len() < 2 {
            return;
        }
        let len = logins.len() as isize;
        let pos = logins.iter().position(|l| l == &self.login).unwrap_or(0);
        let next = (pos as isize + if forward { 1 } else { -1 }).rem_euclid(len);
        self.login = logins[next as usize].clone();
    }

    /// The picker's rows: the cached listing for the current provider and
    /// host, filtered by what's typed. `None` when there is nothing to show
    /// for these rows -- still fetching, or the fetch failed.
    fn picker_rows(&self) -> Option<Vec<String>> {
        let key = (self.provider_opt()?.as_str().to_string(), self.host.clone());
        let needle = self.picker_filter.to_lowercase();
        match self.owners.get(&key)? {
            Ok(list) => Some(
                list.iter()
                    .filter(|o| needle.is_empty() || o.to_lowercase().contains(&needle))
                    .cloned()
                    .collect(),
            ),
            Err(_) => None,
        }
    }

    /// Open the owner picker. The first open for a provider+host fetches the
    /// listing in the background and reports back over the event channel;
    /// later opens reuse the cache and appear instantly.
    fn open_picker(&mut self, tx: &mpsc::UnboundedSender<Input>) {
        self.picker_filter.clear();
        self.picker_cursor = 0;
        let Some(provider) = self.provider_opt() else {
            self.error = Some("pick an authenticated provider first".into());
            return;
        };
        let host = self.host.clone();
        if !self.is_authed(provider, &host) {
            self.error = Some(format!(
                "{} on {} is not authenticated, so there is nothing to list",
                provider.as_str(),
                host
            ));
            return;
        }
        let key = (provider.as_str().to_string(), host.clone());
        if !self.owners.contains_key(&key) {
            self.owners_fetching = true;
            // Gitea names the login that selects the instance; the other
            // providers ignore the argument.
            let login = if provider == OrgProvider::Gitea {
                self.login.clone()
            } else {
                String::new()
            };
            let tx = tx.clone();
            tokio::spawn(async move {
                let result =
                    match provider::list_owners(provider, &host, &login, Duration::from_secs(15))
                        .await
                    {
                        Ok(list) => Ok(list),
                        Err(err) => Err(format!("{err:#}")),
                    };
                let _ = tx.send(Input::OwnersFetched {
                    provider,
                    host,
                    result,
                });
            });
        }
        self.picking = true;
    }

    /// A listing came back. Cached under the provider+host it was asked
    /// for, so a stale answer can't land on rows it doesn't belong to.
    fn owners_received(
        &mut self,
        provider: OrgProvider,
        host: &str,
        result: Result<Vec<String>, String>,
    ) {
        self.owners
            .insert((provider.as_str().to_string(), host.to_string()), result);
        self.owners_fetching = false;
    }

    fn move_picker_cursor(&mut self, delta: isize) {
        let Some(rows) = self.picker_rows() else {
            return;
        };
        if rows.is_empty() {
            return;
        }
        self.picker_cursor =
            (self.picker_cursor as isize + delta).clamp(0, rows.len() as isize - 1) as usize;
    }

    /// Put a picked owner on the owner row, then fill the path row with the
    /// resolved default (`<first configured root>/<owner>` via
    /// [`org::effective_path`] on the would-be config, contracted for
    /// display) -- but only while the row holds no user-entered value:
    /// empty, or the default a previous pick wrote. A typed path always
    /// wins.
    fn pick_owner(&mut self, cfg: &Config, owner: &str) {
        self.picking = false;
        self.owner = owner.to_string();
        if self.path.trim().is_empty() || self.path_auto {
            let would_be = OrgConfig {
                provider: self.provider_opt(),
                host: self.host.clone(),
                owner: self.owner.clone(),
                ..OrgConfig::default()
            };
            if let Ok(path) = org::effective_path(cfg, &would_be) {
                self.path = crate::paths::contract(&path);
                self.path_auto = true;
            }
        }
    }
}

/// One org sync in flight: who it's for, where it stands, and what has
/// landed so far. The counts accumulate on the loop side from the streamed
/// events, so the overlay stays live even though the work runs in a task.
pub struct OrgSyncRun {
    /// The owner being synced, or a count when several run back to back.
    pub owner: String,
    pub done: usize,
    pub total: usize,
    pub label: String,
    pub cloned: usize,
    pub updated: usize,
    pub current: usize,
    pub skipped: usize,
    pub orphaned: usize,
    pub errors: usize,
}

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
    /// Row the org manager is sitting on, indexing `cfg.orgs`.
    pub orgs_cursor: usize,
    /// Set between the first and second press of `x` in the org manager, so
    /// removing an owner takes a confirming second press. Cleared by any
    /// cursor move or close, like a delete key with a hair trigger.
    pub orgs_confirm_remove: bool,
    /// The add/edit form shown in `Mode::OrgForm`.
    pub org_form: OrgForm,
    /// The org sync in flight, if any. `Some` is what makes `s` and `S` say
    /// "already running" instead of starting a second one.
    pub org_sync: Option<OrgSyncRun>,
    /// Last recorded sync per (provider, host, owner), for the LAST SYNC
    /// column. Loaded once at startup and re-read after each sync lands,
    /// rather than kept in step by hand.
    pub org_states: HashMap<(String, String, String), cache::OrgSyncState>,
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
            orgs_cursor: 0,
            orgs_confirm_remove: false,
            org_form: OrgForm::open(None, None, Vec::new()),
            org_sync: None,
            org_states: cache::load_org_states(),
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

    /// The org under the org manager's cursor, cloned. Callers hand it to
    /// the form or the sync task, both of which want their own copy: the
    /// config behind `app.cfg` can be swapped out under an in-flight save.
    pub fn selected_org(&self) -> Option<OrgConfig> {
        self.cfg.orgs.get(self.orgs_cursor).cloned()
    }

    pub fn move_orgs_cursor(&mut self, delta: isize) {
        let len = self.cfg.orgs.len();
        if len == 0 {
            return;
        }
        let next = (self.orgs_cursor as isize + delta).clamp(0, len as isize - 1);
        self.orgs_cursor = next as usize;
        self.orgs_confirm_remove = false;
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
    /// An org sync's progress tick or per-repo outcome, streamed from the
    /// background task the way `Input::Probe` streams a sweep.
    Sync(org::SyncEvent),
    /// The org sync task finished: how many orgs ran, and the one-line
    /// verdict that goes in the status line.
    SyncDone {
        owner_count: usize,
        summary: String,
    },
    /// The org form's owner listing came back from the provider, tagged
    /// with the provider+host it was asked for so a slow answer can't land
    /// on rows it doesn't belong to.
    OwnersFetched {
        provider: OrgProvider,
        host: String,
        result: Result<Vec<String>, String>,
    },
    /// The auth probe started when the org form opened came back: every
    /// host the CLI tools reported as authenticated.
    AuthProbed(Vec<crate::provider::AuthedHost>),
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
    // The roots known at startup come from the cache, which is usually most of
    // the fleet already; whatever a sweep discovers later lands through
    // reconcile instead of restarting the watcher.
    let watcher = if app.cfg.refresh.watch {
        let tx = tx.clone();
        let roots: Vec<PathBuf> = app.repos.iter().map(|r| r.root.clone()).collect();
        match watch::spawn(app.cfg.clone(), roots, move |paths| {
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

    // Repaint scheduling. A streaming sweep can queue hundreds of probe
    // events, and repainting the full table once per event made startup read
    // as a multi-second freeze with keys answered only between frames. So:
    //
    // - a whole queue drain shares one repaint, and the drain happens before
    //   the draw, so a burst costs one frame no matter how many events;
    // - user input repaints on the spot; everything background repaints on
    //   the tick cadence (250ms while a sweep or sync runs, about a second
    //   idle), which is why the background arms below never touch `repaint`.
    let mut repaint = true;
    let mut idle_ticks = 0u32;
    let mut queued: VecDeque<Input> = VecDeque::new();
    loop {
        if app.should_quit {
            break;
        }
        if repaint {
            draw(terminal, app)?;
            repaint = false;
        }
        // Refill the batch only when it has run dry; the drain below then
        // takes everything that arrived while the batch was being handled.
        if queued.is_empty() {
            match rx.recv().await {
                Some(input) => queued.push_back(input),
                None => break,
            }
        }
        while let Some(input) = queued.pop_front().or_else(|| rx.try_recv().ok()) {
            match input {
                Input::Tick => {
                    // While a sweep is running, every tick repaints so the spinner
                    // turns and the counter climbs. Idle, repaint about once a
                    // second, which is often enough for the age column and quiet
                    // enough to leave open all day. A running org sync turns the
                    // spinner too, for the same reason.
                    app.spinner = app.spinner.wrapping_add(1);
                    idle_ticks += 1;
                    if app.sweeping || app.org_sync.is_some() || idle_ticks >= 4 {
                        idle_ticks = 0;
                        repaint = true;
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
                    repaint = true;
                }
                Input::Term(TermEvent::Resize(_, _)) => {
                    terminal.autoresize()?;
                    repaint = true;
                }
                Input::Term(TermEvent::Mouse(ev)) => {
                    handle_mouse(app, ev, terminal.size().ok());
                    repaint = true;
                }
                Input::Term(_) => {}
                Input::Probe(event) => {
                    handle_probe_event(app, event, watcher.as_ref());
                }
                Input::Changed(paths) => {
                    reprobe_paths(app, storm_filter(app.org_sync.is_some(), paths), &tx);
                }
                Input::Resweep => {
                    start_sweep(app, &tx, Tier::Full);
                }
                Input::OwnersFetched {
                    provider,
                    host,
                    result,
                } => {
                    app.org_form.owners_received(provider, &host, result);
                    repaint = true;
                }
                Input::AuthProbed(hosts) => {
                    apply_auth_probed(app, hosts);
                    repaint = true;
                }
                Input::Sync(event) => {
                    handle_sync_event(app, event);
                }
                Input::SyncDone {
                    owner_count,
                    summary,
                } => {
                    app.org_sync = None;
                    // The sync just recorded fresh states for every org it ran;
                    // re-reading the map beats keeping it in step by hand, the
                    // same trade the column picker makes with the config.
                    app.org_states = cache::load_org_states();
                    app.notify(format!(
                        "Synced {owner_count} org{}: {summary}",
                        if owner_count == 1 { "" } else { "s" }
                    ));
                    // Fresh clones exist on disk now. A full sweep is what puts
                    // them in the table without waiting for the refresh timer.
                    start_sweep(app, &tx, Tier::Full);
                    repaint = true;
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
                }
            }
            if app.should_quit {
                break;
            }
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

fn handle_probe_event(app: &mut App, event: probe::Event, watcher: Option<&watch::Handle>) {
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
            // Serializing the whole fleet is real work — a few hundred repos
            // is a sizeable JSON write — and it happens on a thread so the
            // event loop never waits on it. A snapshot is what moves: `app`
            // keeps mutating behind the write, and the next sweep's save
            // supersedes this one anyway.
            let snapshot = app.repos.clone();
            std::thread::spawn(move || {
                let _ = cache::save(&snapshot);
            });
            // The post-sweep repo list is the authoritative picture of what
            // exists on disk, so this is the moment to teach the watcher about
            // repos that appeared (a sync, a manual clone) or went away.
            // Reconcile diffs against what it already watches, so on a settled
            // fleet this is a cheap no-op.
            if let Some(handle) = watcher {
                let roots: Vec<PathBuf> = app.repos.iter().map(|r| r.root.clone()).collect();
                handle.reconcile(&roots);
            }
        }
    }
    app.recompute();
}

/// Kick off a sweep in the background, streaming results into the loop.
fn start_sweep(app: &mut App, tx: &mpsc::UnboundedSender<Input>, tier: Tier) {
    if app.sweeping {
        // Only skip if the sweep is plausibly still going. A sweep that never
        // reported back must not be able to block every one after it.
        //
        // `sweeping` without a timestamp is the first-paint state — the frame
        // before this sweep began — and is adopted silently. Warning there
        // made every startup announce a stuck sweep that had not started.
        match app.sweep_started {
            Some(at) if at.elapsed() < SWEEP_STUCK_AFTER => return,
            Some(at) => {
                tracing::warn!(
                    stuck_for = ?at.elapsed(),
                    "previous sweep never finished; starting another"
                );
                app.notify("The last sweep never finished. Starting another.");
            }
            None => {}
        }
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

/// How many changed paths one watcher batch will re-probe. A human edit
/// touches a handful of files in one or two repos; anything past this is the
/// residue of a mass clone, not a person at a keyboard.
const STORM_BATCH: usize = 64;

/// Decide what a watcher batch is worth re-probing. While an org sync is
/// running, nothing is: a 500-repo sync produces continuous events for every
/// clone over tens of minutes, and re-probing repos mid-clone spawns git
/// against work in progress and competes with the sync for the same disk and
/// CPU. The sync ends with its own full sweep, which is the authoritative
/// refresh anyway. Without a sync, an oversized batch is clone-storm residue;
/// the periodic backstop sweep and the post-sync sweep cover what we drop,
/// and unbounded re-probing is what made a freshly synced fleet sluggish.
/// Small batches pass through untouched, since that is what watching is for.
fn storm_filter(syncing: bool, paths: Vec<PathBuf>) -> Vec<PathBuf> {
    if syncing {
        return Vec::new();
    }
    if paths.len() > STORM_BATCH {
        return paths.into_iter().take(STORM_BATCH).collect();
    }
    paths
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
        // Same gesture as the picker: the org list is short, the wheel is
        // what people reach for first.
        Mode::Orgs => {
            match ev.kind {
                MouseEventKind::ScrollDown => app.move_orgs_cursor(1),
                MouseEventKind::ScrollUp => app.move_orgs_cursor(-1),
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
        Mode::Orgs => handle_orgs_key(app, key, tx),
        Mode::OrgForm => handle_org_form_key(app, key, tx),
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

/// Keys inside the org manager. The list on screen is `app.cfg.orgs` read
/// fresh every frame, so a save or removal shows up without any bookkeeping
/// here.
fn handle_orgs_key(app: &mut App, key: KeyEvent, tx: &mpsc::UnboundedSender<Input>) {
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => {
            app.mode = Mode::Normal;
            app.orgs_confirm_remove = false;
        }
        KeyCode::Char('j') | KeyCode::Down => app.move_orgs_cursor(1),
        KeyCode::Char('k') | KeyCode::Up => app.move_orgs_cursor(-1),
        KeyCode::Char('a') => {
            // Probed on every open, rather than held in app state: auth
            // changes outside the dashboard (a fresh `gh auth login`, a new
            // tea login) should be picked up the next time the form opens,
            // not stale from when it launched. Probed OFF the UI thread —
            // `gh auth status` is a network call, and running it here is
            // what made the form take seconds to appear.
            app.org_form = OrgForm::open_probing(None, None);
            app.mode = Mode::OrgForm;
            spawn_auth_probe(tx);
        }
        KeyCode::Char('e') => {
            let Some(org) = app.selected_org() else {
                return;
            };
            let editing = Some(app.orgs_cursor);
            app.org_form = OrgForm::open_probing(editing, Some(&org));
            app.mode = Mode::OrgForm;
            spawn_auth_probe(tx);
        }
        KeyCode::Char('x') => remove_org(app),
        KeyCode::Char('s') => {
            if app.org_sync.is_some() {
                app.notify("A sync is already running; let it finish first");
                return;
            }
            let Some(org) = app.selected_org() else {
                return;
            };
            start_org_sync(app, tx, vec![org]);
        }
        KeyCode::Char('S') => {
            if app.org_sync.is_some() {
                app.notify("A sync is already running; let it finish first");
                return;
            }
            let orgs: Vec<OrgConfig> = app
                .cfg
                .orgs
                .iter()
                .filter(|org| org.enabled)
                .cloned()
                .collect();
            if orgs.is_empty() {
                app.notify("No enabled orgs to sync");
                return;
            }
            start_org_sync(app, tx, orgs);
        }
        KeyCode::Char('?') => {
            app.mode = Mode::Help;
            app.detail_scroll = 0;
        }
        _ => {}
    }
}

/// Probe tool auth off the UI thread; the answer arrives as
/// [`Input::AuthProbed`]. See the comment on the `a` key above.
fn spawn_auth_probe(tx: &mpsc::UnboundedSender<Input>) {
    let tx = tx.clone();
    tokio::spawn(async move {
        let hosts = tokio::task::spawn_blocking(move || {
            provider::authenticated_hosts(Duration::from_secs(5))
        })
        .await
        .unwrap_or_default();
        let _ = tx.send(Input::AuthProbed(hosts));
    });
}

/// Apply a finished auth probe to whichever form is open. Escaping before
/// the probe lands drops it; opening another form lets it land there, since
/// the answer is form-independent.
fn apply_auth_probed(app: &mut App, hosts: Vec<provider::AuthedHost>) {
    if app.mode != Mode::OrgForm {
        return;
    }
    let adding = app.org_form.editing.is_none();
    app.org_form.authed = hosts;
    app.org_form.auth_probing = false;
    if adding && app.org_form.provider.is_empty() {
        if let Some(first) = app.org_form.providers().first().copied() {
            app.org_form.set_provider(first);
        }
    }
}

/// `x` twice removes: the first press asks, the second does. Only the
/// registration goes -- the checkouts on disk were never ours to touch.
fn remove_org(app: &mut App) {
    let Some(org) = app.selected_org() else {
        return;
    };
    if !app.orgs_confirm_remove {
        app.orgs_confirm_remove = true;
        app.notify(format!(
            "Press x again to remove {} from {}",
            org.owner, org.host
        ));
        return;
    }
    app.orgs_confirm_remove = false;
    let mut cfg = (*app.cfg).clone();
    cfg.orgs.remove(app.orgs_cursor);
    match crate::config::save(&cfg) {
        Ok(_) => {
            app.cfg = Arc::new(cfg);
            app.move_orgs_cursor(0); // re-clamps against the shorter list
            app.notify(format!("Removed {} (checkouts untouched)", org.owner));
        }
        Err(err) => app.notify(format!("could not save config: {err:#}")),
    }
}

/// Keys inside the add/edit form.
///
/// Rows are walked with Tab and the arrows. Left/Right -- and space, on the
/// cycle rows -- step provider, host, login and protocol through what the
/// CLI tools reported as authenticated; those rows take no typing, because
/// a hand-typed host is exactly how an org nothing can list gets
/// registered. Owner and path take text; Enter or space on the owner row
/// opens the picker. Enter elsewhere advances, and Enter on the last row
/// saves -- it never flips a toggle, so walking the rows to the end is
/// safe, and it never destroys the row it leaves.
fn handle_org_form_key(app: &mut App, key: KeyEvent, tx: &mpsc::UnboundedSender<Input>) {
    // The picker, when open, swallows everything: j/k move, typing filters,
    // enter picks and esc backs out, leaving the form as it was. Free
    // typing an owner stays possible the moment the picker is closed.
    if app.org_form.picking {
        match key.code {
            KeyCode::Esc => app.org_form.picking = false,
            KeyCode::Enter => {
                let Some(rows) = app.org_form.picker_rows() else {
                    return;
                };
                if let Some(name) = rows.get(app.org_form.picker_cursor).cloned() {
                    let cfg = app.cfg.clone();
                    app.org_form.pick_owner(&cfg, &name);
                    app.org_form.error = None;
                }
            }
            KeyCode::Char('j') | KeyCode::Down => app.org_form.move_picker_cursor(1),
            KeyCode::Char('k') | KeyCode::Up => app.org_form.move_picker_cursor(-1),
            KeyCode::Char(c) => {
                app.org_form.picker_filter.push(c);
                app.org_form.picker_cursor = 0;
            }
            KeyCode::Backspace => {
                app.org_form.picker_filter.pop();
                app.org_form.picker_cursor = 0;
            }
            _ => {}
        }
        return;
    }

    // Anything that changes state clears the last refusal; an action that
    // refuses again writes a fresh one.
    app.org_form.error = None;
    match key.code {
        KeyCode::Esc => app.mode = Mode::Orgs,
        KeyCode::Backspace => {
            if app.org_form.field == FIELD_PATH {
                app.org_form.path_auto = false;
            }
            if let Some(text) = app.org_form.text_mut() {
                text.pop();
            }
        }
        KeyCode::Tab | KeyCode::Down => {
            app.org_form.field = (app.org_form.field + 1).min(ORG_FIELD_COUNT - 1);
        }
        KeyCode::BackTab | KeyCode::Up => {
            app.org_form.field = app.org_form.field.saturating_sub(1);
        }
        KeyCode::Left => app.org_form.cycle_active(false),
        KeyCode::Right => app.org_form.cycle_active(true),
        // Space flips a toggle and stays put, so a misflip is one more press
        // to undo rather than a walk back from the next row. On the cycle
        // rows it cycles instead, and on owner it opens the picker.
        KeyCode::Char(' ') => match app.org_form.field {
            FIELD_OWNER => app.org_form.open_picker(tx),
            _ if app.org_form.active_is_toggle() => app.org_form.flip_toggle(),
            _ => app.org_form.cycle_active(true),
        },
        KeyCode::Char(c) => {
            if app.org_form.field == FIELD_PATH {
                app.org_form.path_auto = false;
            }
            if let Some(text) = app.org_form.text_mut() {
                text.push(c);
            }
        }
        KeyCode::Enter => {
            if app.org_form.field == FIELD_OWNER {
                app.org_form.open_picker(tx);
            } else if app.org_form.field == ORG_FIELD_COUNT - 1 {
                save_org_form(app);
            } else {
                app.org_form.field += 1;
            }
        }
        _ => {}
    }
}

/// Turn the form into an `OrgConfig`, splice it into a cloned config,
/// validate, and save. Swapping `app.cfg`'s Arc is the whole concurrency
/// story: in-flight sweeps holding the old config finish against it, and
/// the next sweep picks this one up.
///
/// Every refusal -- no auth, an unresolved provider, a problem reported by
/// [`Config::org_problems`] -- is written to the form's error line as well
/// as the status line: the overlay covers the status line, so a notify
/// alone would be invisible while the form is up. The form stays open --
/// the text is still on the fields, so a fix is an edit, not a retype.
fn save_org_form(app: &mut App) {
    if app.org_form.auth_probing {
        app.org_form.error = Some("still checking tool auth — try again in a moment".into());
        return;
    }
    let Some(provider) = app.org_form.provider_opt() else {
        // With nothing authenticated at all there is nothing the form can
        // offer, and the fix lives in the terminal, not the form.
        let msg = if app.org_form.authed.is_empty() {
            "no authenticated CLI: run `gh auth login`, `glab auth login` or `tea login add` first"
                .to_string()
        } else {
            "pick an authenticated provider first".to_string()
        };
        app.org_form.error = Some(msg.clone());
        app.notify(msg);
        return;
    };

    // The auth gate. An edit that hasn't touched provider or host is the
    // one exception: the org predates the gate, and refusing to save an
    // unchanged toggle would mean re-authenticating just to disable one.
    let untouched_seed = match app.org_form.editing {
        Some(ix) => app.cfg.orgs.get(ix).is_some_and(|org| {
            org.host == app.org_form.host && org.resolved_provider() == provider
        }),
        None => false,
    };
    if !untouched_seed && !app.org_form.is_authed(provider, &app.org_form.host) {
        let msg = format!(
            "{} on {} is not authenticated: run `{}` first",
            provider.as_str(),
            app.org_form.host,
            auth_command(provider),
        );
        app.org_form.error = Some(msg.clone());
        app.notify(msg);
        return;
    }

    let protocol = match app.org_form.protocol.as_str() {
        "ssh" => CloneProtocol::Ssh,
        "https" => CloneProtocol::Https,
        other => {
            let msg = format!("protocol must be ssh or https, not \"{other}\"");
            app.org_form.error = Some(msg.clone());
            app.notify(msg);
            return;
        }
    };

    let mut org = match app.org_form.editing {
        // Editing keeps the fields the form doesn't show -- exclude globs,
        // enabled -- exactly as they were.
        Some(ix) => app.cfg.orgs.get(ix).cloned().unwrap_or_default(),
        None => OrgConfig::default(),
    };
    org.provider = Some(provider);
    org.host = app.org_form.host.clone();
    org.owner = app.org_form.owner.trim().to_string();
    // An untouched path stores None: the config default keeps following the
    // configured roots instead of baking today's root into the file.
    org.path = match app.org_form.path.trim() {
        "" => None,
        p => Some(p.to_string()),
    };
    // The tea login selects the instance on gitea; the other providers have
    // no use for one, and storing it would read as meaningful.
    org.login = if provider == OrgProvider::Gitea {
        app.org_form.login.trim().to_string()
    } else {
        String::new()
    };
    org.protocol = protocol;
    org.include_forks = app.org_form.include_forks;
    org.include_archived = app.org_form.include_archived;
    org.include_subgroups = app.org_form.include_subgroups;

    let mut cfg = (*app.cfg).clone();
    // Registration implies visibility: an org whose checkouts live under no
    // configured root would sync fine and never show up on the dashboard, so
    // the save itself wires the path's parent in as a scan root. Running it
    // before the problem check means the validation sees the config exactly
    // as it will be written, and a resolution failure refuses the save like
    // any other -- half a registration would be worse than none.
    let added_root = match org::ensure_scan_root(&mut cfg, &org) {
        Ok(true) => cfg.roots.last().cloned(),
        Ok(false) => None,
        Err(err) => {
            let msg = format!("could not resolve the org path: {err:#}");
            app.org_form.error = Some(msg.clone());
            app.notify(msg);
            return;
        }
    };

    match app.org_form.editing {
        Some(ix) if ix < cfg.orgs.len() => cfg.orgs[ix] = org,
        _ => cfg.orgs.push(org),
    }

    if let Some(problem) = cfg.org_problems().first() {
        app.org_form.error = Some(problem.clone());
        app.notify(problem.clone());
        return;
    }

    match crate::config::save(&cfg) {
        Ok(_) => {
            app.cfg = Arc::new(cfg);
            app.mode = Mode::Orgs;
            app.notify(match added_root {
                // The root ensure_scan_root appended is already contracted:
                // that is the form the config stores, so that is what the
                // user should read back.
                Some(root) => format!("org saved — scan root added {root}"),
                None => "org saved".to_string(),
            });
        }
        Err(err) => {
            let msg = format!("could not save config: {err:#}");
            app.org_form.error = Some(msg.clone());
            app.notify(msg);
        }
    }
}

/// The CLI command that grants drydock access to a provider, for the
/// messages that refuse a save. These are the tools' own login flows --
/// drydock never sees the credentials, it only checks that a login exists.
fn auth_command(provider: OrgProvider) -> &'static str {
    match provider {
        OrgProvider::GitHub => "gh auth login",
        OrgProvider::GitLab => "glab auth login",
        OrgProvider::Gitea => "tea login add",
    }
}

/// Start the sync task for one or more orgs. Serial across orgs as well as
/// within them: `sync_org` is awaited in a plain loop, one owner at a time,
/// and each owner's repos run one at a time inside it.
fn start_org_sync(app: &mut App, tx: &mpsc::UnboundedSender<Input>, orgs: Vec<OrgConfig>) {
    // Same deal as the form save: syncing an org whose checkouts live under
    // no configured root would do the work and hide the results, so the sync
    // wires the parents in first. The task itself needs no changes -- it ends
    // with a full sweep, which is what actually surfaces the clones.
    let mut ensured = (*app.cfg).clone();
    let mut added: Vec<String> = Vec::new();
    for org in &orgs {
        if let Ok(true) = org::ensure_scan_root(&mut ensured, org) {
            // An Ok(true) means exactly one root was appended.
            if let Some(root) = ensured.roots.last() {
                added.push(root.clone());
            }
        }
    }
    if !added.is_empty() {
        match crate::config::save(&ensured) {
            Ok(_) => {
                app.cfg = Arc::new(ensured);
                for root in &added {
                    app.notify(format!("scan root added {root}"));
                }
            }
            // The sync runs against the on-disk config either way; without
            // the persisted root the new org's repos may simply not surface,
            // and the message says why.
            Err(err) => app.notify(format!("could not save config: {err:#}")),
        }
    }

    let owner = match orgs.as_slice() {
        [one] => one.owner.clone(),
        many => format!("{} orgs", many.len()),
    };
    app.org_sync = Some(OrgSyncRun {
        owner,
        done: 0,
        total: 0,
        label: "starting".into(),
        cloned: 0,
        updated: 0,
        current: 0,
        skipped: 0,
        orphaned: 0,
        errors: 0,
    });
    let cfg = app.cfg.clone();
    let inner = tx.clone();
    tokio::spawn(async move {
        let mut cloned = 0usize;
        let mut updated = 0;
        let mut current = 0;
        let mut skipped = 0;
        let mut orphaned = 0;
        let mut errors = 0;
        // `sync_org` streams `SyncEvent`s, the loop speaks `Input`: a
        // forwarder bridges the two, the same shape `start_sweep` uses for
        // probe events.
        let (stx, mut srx) = mpsc::unbounded_channel::<org::SyncEvent>();
        let forward_tx = inner.clone();
        let forward = tokio::spawn(async move {
            while let Some(event) = srx.recv().await {
                if forward_tx.send(Input::Sync(event)).is_err() {
                    break;
                }
            }
        });
        for org in &orgs {
            match org::sync_org(org, &cfg, Some(&stx)).await {
                Ok(outcomes) => {
                    for outcome in outcomes {
                        match outcome.action {
                            crate::org::Action::Cloned => cloned += 1,
                            crate::org::Action::Updated => updated += 1,
                            crate::org::Action::Current => current += 1,
                            crate::org::Action::Skipped => skipped += 1,
                            crate::org::Action::Orphaned => orphaned += 1,
                            crate::org::Action::Error => errors += 1,
                        }
                    }
                }
                Err(err) => {
                    errors += 1;
                    tracing::warn!(owner = %org.owner, error = %format!("{err:#}"), "org sync failed");
                }
            }
        }
        // Done emitting: closing the sync channel lets the forwarder drain
        // and finish, so the terminal SyncDone can't overtake the last Repo
        // event on its way to the loop.
        drop(stx);
        let _ = forward.await;
        // Counts only: the loop's SyncDone arm wraps these with how many
        // orgs ran.
        let owner_count = orgs.len();
        let summary = format!(
            "cloned {cloned}, updated {updated}, \
             current {current}, skipped {skipped}, orphans {orphaned}, errors {errors}"
        );
        // The channel outliving the dashboard is fine: a failed send just
        // means nobody is watching, and the sync already finished.
        let _ = inner.send(Input::SyncDone {
            owner_count,
            summary,
        });
    });
}

/// Fold a streamed sync event into the overlay's progress state.
fn handle_sync_event(app: &mut App, event: org::SyncEvent) {
    let Some(run) = app.org_sync.as_mut() else {
        return;
    };
    match event {
        org::SyncEvent::Progress { done, total, label } => {
            run.done = done;
            run.total = total;
            run.label = label;
        }
        org::SyncEvent::Repo(outcome) => match outcome.action {
            crate::org::Action::Cloned => run.cloned += 1,
            crate::org::Action::Updated => run.updated += 1,
            crate::org::Action::Current => run.current += 1,
            crate::org::Action::Skipped => run.skipped += 1,
            crate::org::Action::Orphaned => run.orphaned += 1,
            crate::org::Action::Error => run.errors += 1,
        },
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
        // The shifted-letter idiom the dashboard already uses (s/S, o/O,
        // f/F): lowercase `a` clears the filters, so the org manager rides
        // the shift, the same place `C` put the column picker.
        KeyCode::Char('A') => {
            app.orgs_cursor = 0;
            app.orgs_confirm_remove = false;
            app.mode = Mode::Orgs;
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
    // The org views are synthetic like "scanning": the real config on this
    // machine would make the snapshot machine-specific, and the probe below
    // would make it network-dependent. Neither belongs in a layout check.
    if view == "orgs" || view == "org-form" {
        let mut cfg = (*app.cfg).clone();
        cfg.roots = vec!["~/dev/github.com".into()];
        cfg.orgs = vec![
            OrgConfig {
                host: "github.com".into(),
                owner: "yetidevworks".into(),
                ..Default::default()
            },
            OrgConfig {
                host: "gitlab.com".into(),
                owner: "acme".into(),
                path: Some("~/Projects/acme".into()),
                ..Default::default()
            },
            OrgConfig {
                provider: Some(OrgProvider::Gitea),
                host: "git.example.com".into(),
                owner: "otter".into(),
                path: Some("~/Projects/otter".into()),
                ..Default::default()
            },
        ];
        app.cfg = Arc::new(cfg);
        app.repos = (0..31)
            .map(|i| {
                let root = crate::paths::expand("~/dev/github.com/yetidevworks");
                RepoStatus::new(
                    root.join(format!("repo{i}")),
                    "yetidevworks".into(),
                    format!("repo{i}"),
                )
            })
            .collect();
        app.reindex();
        app.mode = Mode::Orgs;
        if view == "org-form" {
            // Synthetic auth, like the synthetic config above: what the
            // tools on this machine report would make the snapshot
            // machine-specific and network-dependent.
            app.org_form = OrgForm::open(
                None,
                None,
                vec![provider::AuthedHost {
                    provider: OrgProvider::GitHub,
                    host: "github.com".into(),
                    login: Some("yetidevworks".into()),
                    name: None,
                }],
            );
            app.org_form.owner = "yetidevworks".into();
            app.mode = Mode::OrgForm;
        }
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
        // Both visibility forms and FETCHED sit in the hidden section by
        // default -- the three opt-in columns.
        let hidden: Vec<Column> = rows
            .iter()
            .filter(|(_, on)| !*on)
            .map(|(c, _)| *c)
            .collect();
        assert_eq!(
            hidden,
            vec![Column::Visibility, Column::VisibilityShort, Column::Fetched]
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
    /// The overlay reads owners straight out of the config: if a hand-edited
    /// `[[orgs]]` table doesn't show up here, the manager is showing a copy.
    #[test]
    fn the_orgs_overlay_lists_configured_owners() {
        let mut app = app_with(0);
        let mut cfg = (*app.cfg).clone();
        cfg.roots = vec!["~/dev/github.com".into()];
        cfg.orgs = vec![
            OrgConfig {
                host: "github.com".into(),
                owner: "yetidevworks".into(),
                ..Default::default()
            },
            OrgConfig {
                host: "gitlab.com".into(),
                owner: "acme".into(),
                ..Default::default()
            },
        ];
        app.cfg = Arc::new(cfg);
        let base = crate::paths::expand("~/dev/github.com/yetidevworks");
        app.repos = (0..3)
            .map(|i| {
                RepoStatus::new(
                    base.join(format!("repo{i}")),
                    "yetidevworks".into(),
                    format!("repo{i}"),
                )
            })
            .collect();
        app.reindex();
        app.mode = Mode::Orgs;

        let out = render_once(&app, 110, 22).unwrap();
        assert!(out.contains("PROVIDER"), "column header is drawn:\n{out}");
        assert!(
            out.contains("yetidevworks"),
            "owner from the config:\n{out}"
        );
        assert!(out.contains("acme"), "second owner from the config:\n{out}");
        assert!(out.contains("gitlab.com"), "host column:\n{out}");
        assert!(
            out.contains("3"),
            "ON DISK counts the repos under the org path:\n{out}"
        );
        // Nothing has synced yet, so the LAST SYNC column says so rather
        // than rendering a bare age of "-".
        assert!(
            out.contains("never"),
            "LAST SYNC for a never-synced org:\n{out}"
        );
    }

    #[test]
    fn the_org_form_renders_its_fields_and_toggles() {
        let mut app = app_with(0);
        app.mode = Mode::OrgForm;
        app.org_form = OrgForm::open(
            None,
            None,
            vec![provider::AuthedHost {
                provider: OrgProvider::GitHub,
                host: "github.com".into(),
                login: Some("me".into()),
                name: None,
            }],
        );
        app.org_form.owner = "yetidevworks".into();

        let out = render_once(&app, 90, 24).unwrap();
        for label in ["provider", "host", "owner", "path", "login", "protocol"] {
            assert!(out.contains(label), "{label} field is labelled:\n{out}");
        }
        // The provider row reads as a choice, with the CLI that must be
        // logged in named beside it.
        assert!(out.contains("github (gh)"), "provider choice:\n{out}");
        assert!(
            out.contains("github.com"),
            "selected host shows in its field:\n{out}"
        );
        assert!(
            out.contains("[ ]"),
            "unchecked toggles render as checkboxes:\n{out}"
        );

        // Flipping a toggle changes the glyph, which is the whole feedback
        // loop the form has for those rows.
        app.org_form.field = ORG_TEXT_FIELDS; // the include-forks row
        app.org_form.flip_toggle();
        let out = render_once(&app, 90, 24).unwrap();
        assert!(
            out.contains("[x]"),
            "flipped toggle renders checked:\n{out}"
        );
    }

    /// A save refusal has to be readable while the form is up, and the
    /// status line sits underneath the overlay -- so the message is drawn
    /// inside it too.
    #[test]
    fn the_org_form_draws_its_error_inside_the_overlay() {
        let mut app = app_with(0);
        app.mode = Mode::OrgForm;
        app.org_form = OrgForm::open(
            None,
            None,
            vec![provider::AuthedHost {
                provider: OrgProvider::GitHub,
                host: "github.com".into(),
                login: Some("me".into()),
                name: None,
            }],
        );
        app.org_form.error = Some("github on git.example.com is not authenticated".into());

        let out = render_once(&app, 90, 24).unwrap();
        assert!(
            out.contains("is not authenticated"),
            "refusal renders inside the overlay:\n{out}"
        );
    }

    fn org_form_app(authed: Vec<provider::AuthedHost>) -> App {
        org_form_app_with_roots(vec!["~/dev/github.com".into()], authed)
    }

    /// The default layout's first root already covers the default org path,
    /// so most tests never see the root fixup fire; the roots parameter
    /// makes the uncovered case constructible.
    fn org_form_app_with_roots(roots: Vec<String>, authed: Vec<provider::AuthedHost>) -> App {
        let mut app = app_with(0);
        let mut cfg = (*app.cfg).clone();
        cfg.roots = roots;
        app.cfg = Arc::new(cfg);
        app.mode = Mode::OrgForm;
        app.org_form = OrgForm::open(None, None, authed);
        app
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    /// The old form was effectively unsavable: Enter flipped toggles as it
    /// walked the rows, and the last row never saw a valid save. Walking
    /// every row down and pressing Enter on the last one must land exactly
    /// one org in the config, with the toggles untouched.
    #[test]
    fn enter_on_the_last_row_saves_a_new_org_without_flipping_toggles() {
        // The save writes the config file; keep it out of the real one.
        let xdg = tempfile::tempdir().unwrap();
        std::env::set_var("XDG_CONFIG_HOME", xdg.path());

        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = org_form_app(vec![
            provider::AuthedHost {
                provider: OrgProvider::GitLab,
                host: "gitlab.com".into(),
                login: Some("u".into()),
                name: None,
            },
            provider::AuthedHost {
                provider: OrgProvider::GitHub,
                host: "github.com".into(),
                login: Some("me".into()),
                name: None,
            },
        ]);
        assert_eq!(app.org_form.provider, "github", "first authed provider");
        assert_eq!(app.org_form.host, "github.com");

        // Cycle provider to gitlab and back: the host follows the provider,
        // and the owner (set below) survives the walk because it is cleared
        // by provider changes, not by row movement.
        handle_org_form_key(&mut app, key(KeyCode::Char(' ')), &tx);
        assert_eq!(app.org_form.provider, "gitlab");
        assert_eq!(app.org_form.host, "gitlab.com");
        handle_org_form_key(&mut app, key(KeyCode::Char(' ')), &tx);
        assert_eq!(app.org_form.provider, "github");
        assert_eq!(app.org_form.host, "github.com");

        // Down to the owner row and type the owner: free typing stays the
        // fallback for memberships no listing reports.
        handle_org_form_key(&mut app, key(KeyCode::Down), &tx); // host
        handle_org_form_key(&mut app, key(KeyCode::Down), &tx); // owner
        for c in "acme".chars() {
            handle_org_form_key(&mut app, key(KeyCode::Char(c)), &tx);
        }
        assert_eq!(app.org_form.owner, "acme");

        // Walk the rest of the rows -- Enter would work too, and must not
        // flip anything on the way.
        for _ in FIELD_OWNER..ORG_FIELD_COUNT - 1 {
            handle_org_form_key(&mut app, key(KeyCode::Down), &tx);
        }
        assert_eq!(app.org_form.field, ORG_FIELD_COUNT - 1);
        assert!(!app.org_form.include_forks);
        assert!(!app.org_form.include_archived);
        assert!(!app.org_form.include_subgroups);

        handle_org_form_key(&mut app, key(KeyCode::Enter), &tx);
        assert_eq!(app.mode, Mode::Orgs, "a saved form closes to the list");
        assert_eq!(app.cfg.orgs.len(), 1, "exactly one org was added");
        let org = &app.cfg.orgs[0];
        assert_eq!(org.resolved_provider(), OrgProvider::GitHub);
        assert_eq!(org.host, "github.com");
        assert_eq!(org.owner, "acme");
        assert_eq!(org.path, None, "an untouched path stores None");
        assert_eq!(org.login, "", "github carries no tea login");
        assert!(!org.include_forks);
        assert!(!org.include_archived);
        assert!(!org.include_subgroups);

        // The default path resolves to the first root plus the owner, so
        // its parent already is a root and the save must not touch them.
        assert_eq!(
            app.cfg.roots,
            vec!["~/dev/github.com".to_string()],
            "a path already under a root adds no scan root"
        );
    }

    /// Registration implies visibility: an org whose path sits under no
    /// configured root would sync fine and never appear on the dashboard,
    /// so the save itself wires the path's parent in as a scan root.
    #[test]
    fn a_save_under_an_uncovered_path_adds_the_parent_root() {
        // The save writes the config file; keep it out of the real one.
        let xdg = tempfile::tempdir().unwrap();
        std::env::set_var("XDG_CONFIG_HOME", xdg.path());

        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = org_form_app_with_roots(
            vec!["~/dev/github.com".into()],
            vec![provider::AuthedHost {
                provider: OrgProvider::GitHub,
                host: "github.com".into(),
                login: Some("me".into()),
                name: None,
            }],
        );

        // Owner row first, then the path row: an explicit path that no
        // root covers is the case the fixup exists for.
        handle_org_form_key(&mut app, key(KeyCode::Down), &tx); // host
        handle_org_form_key(&mut app, key(KeyCode::Down), &tx); // owner
        for c in "acme".chars() {
            handle_org_form_key(&mut app, key(KeyCode::Char(c)), &tx);
        }
        handle_org_form_key(&mut app, key(KeyCode::Down), &tx); // path
        for c in "~/custom/spot".chars() {
            handle_org_form_key(&mut app, key(KeyCode::Char(c)), &tx);
        }
        for _ in FIELD_PATH..ORG_FIELD_COUNT - 1 {
            handle_org_form_key(&mut app, key(KeyCode::Down), &tx);
        }
        handle_org_form_key(&mut app, key(KeyCode::Enter), &tx);

        assert_eq!(app.mode, Mode::Orgs, "the save succeeded");
        assert_eq!(app.cfg.orgs.len(), 1, "exactly one org was added");
        assert_eq!(
            app.cfg.roots,
            vec!["~/dev/github.com".to_string(), "~/custom".to_string()],
            "the parent of the org's path joined the roots, contracted"
        );
    }

    /// The form opens instantly while the auth probe is still running (a
    /// keypress must never wait on a network call), refuses to save until
    /// the probe lands, and the landing probe seeds the provider/host rows.
    #[test]
    fn the_auth_probe_lands_into_the_open_form() {
        let mut app = org_form_app(vec![]);
        app.org_form = OrgForm::open_probing(None, None);
        app.mode = Mode::OrgForm;
        assert!(app.org_form.auth_probing, "the form starts in probe state");
        assert!(app.org_form.providers().is_empty());

        // Saving while the probe is in flight is refused, with the reason
        // drawn inside the overlay. Reach the save the way a user does:
        // Enter walks to the last row, Enter there attempts the save.
        let (tx, _rx) = mpsc::unbounded_channel();
        for _ in 0..ORG_FIELD_COUNT - 1 {
            handle_org_form_key(&mut app, key(KeyCode::Down), &tx);
        }
        handle_org_form_key(&mut app, key(KeyCode::Enter), &tx);
        assert!(app.cfg.orgs.is_empty(), "no save while probing");
        assert!(
            app.org_form
                .error
                .as_deref()
                .is_some_and(|e| e.contains("checking tool auth")),
            "the refusal names the probe: {:?}",
            app.org_form.error
        );

        // The probe lands (as `Input::AuthProbed` delivers it): the rows
        // fill in and the probing state clears.
        apply_auth_probed(
            &mut app,
            vec![provider::AuthedHost {
                provider: OrgProvider::GitHub,
                host: "github.com".into(),
                login: Some("crueber".into()),
                name: None,
            }],
        );
        assert!(!app.org_form.auth_probing);
        assert_eq!(app.org_form.provider, "github");
        assert_eq!(app.org_form.host, "github.com");

        // The rendered row says what the wait was, then what it became.
        app.org_form = OrgForm::open_probing(None, None);
        let out = render_once(&app, 90, 24).unwrap();
        assert!(
            out.contains("probing tool auth"),
            "the probing state is visible: {out}"
        );
    }

    /// No auth for any tool means nothing can be listed, so nothing can be
    /// added -- and the refusal has to say what would fix it.
    #[test]
    fn an_empty_auth_list_refuses_to_save_and_says_what_to_run() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = org_form_app(vec![]);
        assert_eq!(app.org_form.provider, "", "nothing to select");

        for _ in 0..ORG_FIELD_COUNT - 1 {
            handle_org_form_key(&mut app, key(KeyCode::Down), &tx);
        }
        handle_org_form_key(&mut app, key(KeyCode::Enter), &tx);

        assert_eq!(app.mode, Mode::OrgForm, "refused saves keep the form open");
        assert!(app.cfg.orgs.is_empty());
        let err = app.org_form.error.as_deref().expect("a refusal message");
        assert!(err.contains("gh auth login"), "{err}");
        assert!(err.contains("tea login add"), "{err}");
    }

    /// The picker feeds the owner row from the provider and auto-fills the
    /// path with the resolved default while the row holds nothing the user
    /// typed. Seeding the cache keeps the whole thing network-free.
    #[test]
    fn the_owner_picker_fills_owner_and_auto_fills_the_path() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = org_form_app(vec![provider::AuthedHost {
            provider: OrgProvider::GitHub,
            host: "github.com".into(),
            login: Some("me".into()),
            name: None,
        }]);
        app.org_form.owners.insert(
            ("github".into(), "github.com".into()),
            Ok(vec!["acme".into(), "acme-corp".into()]),
        );

        handle_org_form_key(&mut app, key(KeyCode::Down), &tx); // host
        handle_org_form_key(&mut app, key(KeyCode::Down), &tx); // owner
        handle_org_form_key(&mut app, key(KeyCode::Enter), &tx);
        assert!(app.org_form.picking, "the picker opened");
        assert!(
            !app.org_form.owners_fetching,
            "a cached listing never refetches"
        );

        handle_org_form_key(&mut app, key(KeyCode::Enter), &tx);
        assert!(!app.org_form.picking);
        assert_eq!(app.org_form.owner, "acme");
        assert_eq!(
            app.org_form.path, "~/dev/github.com/acme",
            "the path auto-fills with the resolved default"
        );
        assert!(app.org_form.path_auto);

        // Picking again overwrites the previous default -- it is still
        // auto-filled -- and typing filters the list first.
        handle_org_form_key(&mut app, key(KeyCode::Char(' ')), &tx);
        assert!(app.org_form.picking);
        handle_org_form_key(&mut app, key(KeyCode::Char('c')), &tx);
        handle_org_form_key(&mut app, key(KeyCode::Char('o')), &tx);
        handle_org_form_key(&mut app, key(KeyCode::Char('r')), &tx);
        handle_org_form_key(&mut app, key(KeyCode::Char('p')), &tx);
        assert_eq!(
            app.org_form.picker_rows(),
            Some(vec!["acme-corp".into()]),
            "typing filters the list"
        );
        handle_org_form_key(&mut app, key(KeyCode::Enter), &tx);
        assert_eq!(app.org_form.owner, "acme-corp");
        assert_eq!(app.org_form.path, "~/dev/github.com/acme-corp");

        // A typed path beats the picker: the owner changes, the path stays.
        app.org_form.path = "~/somewhere-else".into();
        app.org_form.path_auto = false;
        handle_org_form_key(&mut app, key(KeyCode::Char(' ')), &tx);
        handle_org_form_key(&mut app, key(KeyCode::Down), &tx); // j once
        handle_org_form_key(&mut app, key(KeyCode::Enter), &tx);
        assert_eq!(
            app.org_form.owner, "acme-corp",
            "cursor moved to the second row"
        );
        assert_eq!(
            app.org_form.path, "~/somewhere-else",
            "a hand-typed path is never overwritten"
        );

        // Esc backs out of the picker without touching anything.
        handle_org_form_key(&mut app, key(KeyCode::Char(' ')), &tx);
        assert!(app.org_form.picking);
        handle_org_form_key(&mut app, key(KeyCode::Esc), &tx);
        assert!(!app.org_form.picking);
        assert_eq!(app.org_form.owner, "acme-corp");
    }
    /// Gitea names its `tea` logins; the host choice auto-selects the first
    /// one and the login row cycles the rest. Github and gitlab have none,
    /// and their saved orgs must not carry a login.
    #[test]
    fn the_gitea_login_row_auto_sets_and_cycles_the_tea_logins() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = org_form_app(vec![
            provider::AuthedHost {
                provider: OrgProvider::Gitea,
                host: "git.example.com".into(),
                login: Some("otter".into()),
                name: Some("work".into()),
            },
            provider::AuthedHost {
                provider: OrgProvider::Gitea,
                host: "git.example.com".into(),
                login: Some("otter".into()),
                name: Some("home".into()),
            },
            provider::AuthedHost {
                provider: OrgProvider::GitHub,
                host: "github.com".into(),
                login: Some("me".into()),
                name: None,
            },
        ]);
        assert_eq!(app.org_form.provider, "github", "canonical first provider");
        assert_eq!(app.org_form.login, "", "github has no tea login");

        // Right cycles the provider row forward through the canonical list,
        // skipping providers with no authenticated hosts: github, then gitea.
        handle_org_form_key(&mut app, key(KeyCode::Right), &tx);
        assert_eq!(app.org_form.provider, "gitea");
        assert_eq!(app.org_form.host, "git.example.com");
        assert_eq!(app.org_form.login, "work", "the host auto-sets its login");

        app.org_form.field = FIELD_LOGIN;
        handle_org_form_key(&mut app, key(KeyCode::Right), &tx);
        assert_eq!(app.org_form.login, "home", "the login row cycles");
        handle_org_form_key(&mut app, key(KeyCode::Left), &tx);
        assert_eq!(app.org_form.login, "work");

        // Back to github: the login goes, since saving one would read as
        // meaningful for a provider that ignores it.
        app.org_form.field = FIELD_PROVIDER;
        handle_org_form_key(&mut app, key(KeyCode::Right), &tx);
        assert_eq!(app.org_form.provider, "github");
        assert_eq!(app.org_form.login, "");
    }

    // While a sync runs, every watcher event is dropped: the clones still in
    // flight are not worth probing, and the sync's own final sweep refreshes
    // everything once it can actually be read.
    #[test]
    fn a_sync_in_flight_drops_every_watcher_event() {
        let paths = vec![PathBuf::from("/p/g/r0"), PathBuf::from("/p/g/r1")];
        assert!(storm_filter(true, paths).is_empty());
    }

    // A batch bigger than the cap is clone-storm residue, so only the first
    // slice is probed and the rest waits for the backstop sweep.
    #[test]
    fn an_oversized_batch_is_truncated_to_the_cap() {
        let paths: Vec<PathBuf> = (0..1000)
            .map(|i| PathBuf::from(format!("/p/g/r{i}")))
            .collect();
        let kept = storm_filter(false, paths);
        assert_eq!(kept.len(), STORM_BATCH);
        assert_eq!(kept[0], PathBuf::from("/p/g/r0"));
        assert_eq!(
            kept[STORM_BATCH - 1],
            PathBuf::from(format!("/p/g/r{}", STORM_BATCH - 1))
        );
    }

    // Small batches are the ordinary case the watcher exists for, so they
    // reach re-probing exactly as the kernel reported them.
    #[test]
    fn a_small_batch_passes_through_untouched() {
        let paths = vec![
            PathBuf::from("/p/g/r0"),
            PathBuf::from("/p/g/r1"),
            PathBuf::from("/p/g/r2"),
        ];
        assert_eq!(storm_filter(false, paths.clone()), paths);
    }
}
