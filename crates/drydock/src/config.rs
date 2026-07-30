//! Configuration, with defaults tuned for a `~/Projects/<org>/<repo>` layout.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::Duration;

use crate::paths;

/// Directory names never worth descending into. These hold vendored code with
/// its own `.git` dirs, or build output that generates enormous numbers of
/// filesystem events.
pub const DEFAULT_PRUNE: &[&str] = &[
    "node_modules",
    "vendor",
    "target",
    "bower_components",
    "Pods",
    "Carthage",
    ".build",
    ".venv",
    "venv",
    "__pycache__",
    ".terraform",
    "dist",
    "build",
    ".next",
    ".nuxt",
    ".cache",
    ".tox",
    "DerivedData",
];

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Directories to scan. Each immediate subdirectory becomes a "group".
    pub roots: Vec<String>,
    /// How deep below a root to look for repos.
    pub max_depth: usize,
    /// Descend into a repo looking for more repos. Off by default, which keeps
    /// submodules and vendored checkouts out of the list.
    pub follow_nested_repos: bool,
    /// Follow symlinked directories while scanning. Off by default, so a
    /// symlink into another tree doesn't produce duplicate rows.
    pub follow_symlinks: bool,
    /// Glob patterns matched against the path relative to the root. Anything
    /// matching is skipped entirely.
    pub exclude: Vec<String>,
    /// Extra directory names to prune on top of [`DEFAULT_PRUNE`].
    pub prune: Vec<String>,

    pub refresh: RefreshConfig,
    pub status: StatusConfig,
    pub remote: RemoteConfig,
    pub release: ReleaseConfig,
    pub ui: UiConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            roots: vec!["~/Projects".into()],
            max_depth: 4,
            follow_nested_repos: false,
            follow_symlinks: false,
            exclude: vec![
                // Throwaway fixture repos: dozens of tiny checkouts that would
                // otherwise swamp the list.
                "riffle-testbed/**".into(),
                "riffle-pr-testbed/**".into(),
                "riffle-merge-test/**".into(),
            ],
            prune: Vec::new(),
            refresh: RefreshConfig::default(),
            status: StatusConfig::default(),
            remote: RemoteConfig::default(),
            release: ReleaseConfig::default(),
            ui: UiConfig::default(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RefreshConfig {
    /// How often to re-walk the roots and re-probe everything, as a backstop
    /// for anything the watcher misses. Every sweep re-discovers, so newly
    /// cloned repos appear on this interval too.
    pub interval: String,
    /// Watch the filesystem and re-probe individual repos as they change.
    pub watch: bool,
    /// How long to coalesce filesystem events before acting on them.
    pub debounce: String,
}

impl Default for RefreshConfig {
    fn default() -> Self {
        Self {
            interval: "5m".into(),
            watch: true,
            debounce: "1s".into(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StatusConfig {
    /// `normal` counts an untracked directory once, `all` counts every file in
    /// it, `no` skips untracked files (and is much faster).
    pub untracked: UntrackedMode,
    /// Concurrent working-tree scans. This work is syscall-bound, so past about
    /// one per core it stops helping.
    pub concurrency: Option<usize>,
    /// How many changed file paths to keep per repo for the detail pane.
    pub max_files: usize,
}

impl Default for StatusConfig {
    fn default() -> Self {
        Self {
            untracked: UntrackedMode::Normal,
            concurrency: None,
            max_files: 200,
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum UntrackedMode {
    No,
    Normal,
    All,
}

impl UntrackedMode {
    pub fn as_git_arg(&self) -> &'static str {
        match self {
            UntrackedMode::No => "--untracked-files=no",
            UntrackedMode::Normal => "--untracked-files=normal",
            UntrackedMode::All => "--untracked-files=all",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RemoteConfig {
    /// Periodically fetch, so "behind" counts mean something. Off by default:
    /// it is real network traffic against every remote you own, and a remote
    /// that wants credentials can hang.
    pub fetch: bool,
    pub interval: String,
    pub concurrency: usize,
    pub timeout: String,
}

impl Default for RemoteConfig {
    fn default() -> Self {
        Self {
            fetch: false,
            interval: "1h".into(),
            concurrency: 4,
            timeout: "20s".into(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ReleaseConfig {
    /// Glob deciding which tags count as releases. The default requires a
    /// digit, which keeps marker tags like `latest` or `polar-live` from being
    /// mistaken for the last release. Set to `*` to count every tag.
    pub tag_pattern: String,
    /// How many commit subjects since the last tag to keep for the detail pane.
    pub max_subjects: usize,
    /// Look at `CHANGELOG.md` and compare its top version against the newest
    /// tag.
    pub read_changelog: bool,
    /// Filename candidates for the changelog check.
    pub changelog_files: Vec<String>,
}

impl Default for ReleaseConfig {
    fn default() -> Self {
        Self {
            tag_pattern: "*[0-9]*".into(),
            max_subjects: 30,
            read_changelog: true,
            changelog_files: vec![
                "CHANGELOG.md".into(),
                "CHANGELOG".into(),
                "changelog.md".into(),
            ],
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UiConfig {
    /// Filters active when the dashboard opens. Empty means show everything.
    pub default_filters: Vec<String>,
    /// Initial sort key.
    pub default_sort: String,
    /// Initial "modified since" window, e.g. `1h`, `1d`, `1w`. Empty for all.
    pub default_since: String,
    /// Command used by the `o` key. `{path}` is replaced with the repo root.
    pub editor_command: Vec<String>,
    /// Command used by the `t` key.
    pub git_client_command: Vec<String>,
    /// Command used by the `T` key.
    pub terminal_command: Vec<String>,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            default_filters: Vec::new(),
            default_sort: "activity".into(),
            default_since: String::new(),
            editor_command: vec!["zed".into(), "{path}".into()],
            git_client_command: vec!["open".into(), "-a".into(), "Tower".into(), "{path}".into()],
            terminal_command: vec![
                "open".into(),
                "-a".into(),
                "Terminal".into(),
                "{path}".into(),
            ],
        }
    }
}

impl Config {
    pub fn root_paths(&self) -> Vec<PathBuf> {
        self.roots.iter().map(|r| paths::expand(r)).collect()
    }

    pub fn prune_names(&self) -> Vec<String> {
        let mut names: Vec<String> = DEFAULT_PRUNE.iter().map(|s| s.to_string()).collect();
        names.extend(self.prune.iter().cloned());
        names
    }

    pub fn work_concurrency(&self) -> usize {
        self.status
            .concurrency
            .unwrap_or_else(|| num_cpus::get().clamp(2, 12))
    }

    /// Tier 1 is cheap enough to oversubscribe: it reads a few small files per
    /// repo and spends most of its time waiting on process startup.
    pub fn refs_concurrency(&self) -> usize {
        (self.work_concurrency() * 2).clamp(4, 32)
    }

    pub fn refresh_interval(&self) -> Duration {
        parse_duration(&self.refresh.interval).unwrap_or(Duration::from_secs(300))
    }

    pub fn debounce(&self) -> Duration {
        parse_duration(&self.refresh.debounce).unwrap_or(Duration::from_secs(1))
    }

    pub fn remote_interval(&self) -> Duration {
        parse_duration(&self.remote.interval).unwrap_or(Duration::from_secs(3600))
    }

    pub fn remote_timeout(&self) -> Duration {
        parse_duration(&self.remote.timeout).unwrap_or(Duration::from_secs(20))
    }
}

/// Parse a short duration like `500ms`, `30s`, `5m`, `2h`, `7d`. A bare number
/// is seconds.
pub fn parse_duration(input: &str) -> Option<Duration> {
    let s = input.trim();
    if s.is_empty() {
        return None;
    }
    let (digits, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len()));
    let n: u64 = digits.parse().ok()?;
    let unit_key = unit.trim().to_ascii_lowercase();
    // Milliseconds are the one sub-second case worth having: the watcher's
    // debounce window is naturally expressed that way.
    if unit_key == "ms" {
        return Some(Duration::from_millis(n));
    }
    let secs = match unit_key.as_str() {
        "" | "s" | "sec" | "secs" => n,
        "m" | "min" | "mins" => n * 60,
        "h" | "hr" | "hrs" => n * 3600,
        "d" | "day" | "days" => n * 86_400,
        "w" | "wk" | "week" | "weeks" => n * 604_800,
        "mo" | "month" | "months" => n * 2_592_000,
        "y" | "yr" | "year" | "years" => n * 31_536_000,
        _ => return None,
    };
    Some(Duration::from_secs(secs))
}

pub fn load() -> Result<Config> {
    let path = paths::config_file()?;
    if !path.exists() {
        return Ok(Config::default());
    }
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("Reading config at {}", path.display()))?;
    let cfg: Config =
        toml::from_str(&raw).with_context(|| format!("Parsing config at {}", path.display()))?;
    Ok(cfg)
}

/// Load config, falling back to defaults with a warning rather than failing.
/// A dashboard that refuses to open because of one bad key is worse than one
/// that opens with defaults and says so.
pub fn load_or_default() -> (Config, Option<String>) {
    match load() {
        Ok(cfg) => (cfg, None),
        Err(err) => (Config::default(), Some(format!("{err:#}"))),
    }
}

pub fn save(cfg: &Config) -> Result<PathBuf> {
    let dir = paths::config_dir()?;
    std::fs::create_dir_all(&dir).with_context(|| format!("Creating {}", dir.display()))?;
    let path = paths::config_file()?;
    let body = toml::to_string_pretty(cfg).context("Serializing config")?;
    std::fs::write(&path, body).with_context(|| format!("Writing {}", path.display()))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_parse() {
        assert_eq!(parse_duration("250ms"), Some(Duration::from_millis(250)));
        assert_eq!(parse_duration("30s"), Some(Duration::from_secs(30)));
        assert_eq!(parse_duration("5m"), Some(Duration::from_secs(300)));
        assert_eq!(parse_duration("2h"), Some(Duration::from_secs(7200)));
        assert_eq!(parse_duration("7d"), Some(Duration::from_secs(604_800)));
        assert_eq!(parse_duration("1w"), Some(Duration::from_secs(604_800)));
        assert_eq!(parse_duration("90"), Some(Duration::from_secs(90)));
        assert_eq!(parse_duration(""), None);
        assert_eq!(parse_duration("nope"), None);
    }

    #[test]
    fn default_config_round_trips() {
        let cfg = Config::default();
        let body = toml::to_string_pretty(&cfg).unwrap();
        let back: Config = toml::from_str(&body).unwrap();
        assert_eq!(back.roots, cfg.roots);
        assert_eq!(back.max_depth, cfg.max_depth);
    }
}
