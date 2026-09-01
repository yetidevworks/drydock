//! Configuration, with defaults tuned for a `~/Projects/<org>/<repo>` layout.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::Duration;

use crate::column::Column;
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
    pub visibility: VisibilityConfig,
    pub release: ReleaseConfig,
    pub ui: UiConfig,
    /// Registered owners to clone and update on demand. The whole section is
    /// optional: existing configs load unchanged, and an empty list just means
    /// nothing to sync.
    pub orgs: Vec<OrgConfig>,
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
            visibility: VisibilityConfig::default(),
            release: ReleaseConfig::default(),
            ui: UiConfig::default(),
            orgs: Vec::new(),
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
    /// How long cached working-tree counts stay trusted. The cache key is HEAD
    /// plus the index, and editing a tracked file touches neither, so without
    /// an expiry a repo you edited but never staged reads as clean forever.
    /// `0` rescans on every sweep; empty trusts the key indefinitely.
    pub max_age: String,
}

impl Default for StatusConfig {
    fn default() -> Self {
        Self {
            untracked: UntrackedMode::Normal,
            concurrency: None,
            max_files: 200,
            max_age: "1h".into(),
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
pub struct VisibilityConfig {
    /// Check each repo's public/private status via `gh repo view`. Off by
    /// default, for the same reason `remote.fetch` is: real network traffic
    /// against every remote you own, and it depends on `gh` being installed
    /// and authenticated rather than anything `drydock` controls itself.
    pub enabled: bool,
    /// How long a checked visibility is trusted before it's worth asking
    /// again. Visibility changes rarely if ever, so this can be generous —
    /// unlike `remote.interval`, there's no "behind" count quietly going
    /// stale in the meantime.
    pub interval: String,
    pub concurrency: usize,
    pub timeout: String,
}

impl Default for VisibilityConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            interval: "24h".into(),
            concurrency: 4,
            timeout: "10s".into(),
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
    /// Command used by the `O` key. `{path}` is replaced with the repo root.
    pub editor_command: Vec<String>,
    /// Command used by the `t` key.
    pub git_client_command: Vec<String>,
    /// Command used by the `T` and `ctrl-o` keys.
    pub terminal_command: Vec<String>,
    /// Command used by the `o` key. `open {path}` shows the repo's folder in
    /// Finder; `open -R {path}` would reveal it in its parent instead.
    pub file_manager_command: Vec<String>,
    /// Columns to show, left to right. Unset means the defaults, which
    /// include VISIBILITY only when `visibility.enabled` is on. Set it and
    /// you get exactly what you list, in that order — the `c` key in the
    /// dashboard writes this.
    pub columns: Option<Vec<Column>>,
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
            file_manager_command: vec!["open".into(), "{path}".into()],
            columns: None,
        }
    }
}

/// Which CLI an org's listing goes through. Everything downstream — URLs,
/// filters, invocation shape — differs per provider, so the one thing config
/// owes the rest of the feature is a definite answer to "who are we asking".
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OrgProvider {
    GitHub,
    GitLab,
    Gitea,
}

impl OrgProvider {
    pub fn as_str(&self) -> &'static str {
        match self {
            OrgProvider::GitHub => "github",
            OrgProvider::GitLab => "gitlab",
            OrgProvider::Gitea => "gitea",
        }
    }

    /// The hosted instances everyone shares. Anything else is self-hosted and
    /// has to name its provider explicitly: guessing wrong there would send
    /// the wrong CLI — with the wrong auth — at somebody's instance.
    pub fn from_host(host: &str) -> Option<OrgProvider> {
        match host.trim().to_ascii_lowercase().as_str() {
            "github.com" => Some(OrgProvider::GitHub),
            "gitlab.com" => Some(OrgProvider::GitLab),
            "gitea.com" => Some(OrgProvider::Gitea),
            _ => None,
        }
    }
}

/// How to build clone URLs. SSH is the default because it is what the
/// provider APIs hand back and what a fleet of working checkouts wants; HTTPS
/// is there for the places where credentials are cert-based or keys are a
/// hassle.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CloneProtocol {
    #[default]
    Ssh,
    Https,
}

/// One registered owner: an instance, who to list there, and where the
/// checkouts live. Defaults exist so a hand-written `[[orgs]]` table can stay
/// minimal — three lines is the common case, and every field beyond
/// `host`/`owner` has a sensible answer.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OrgConfig {
    /// Unset means infer from the host; see [`OrgProvider::from_host`].
    pub provider: Option<OrgProvider>,
    /// Instance hostname: `github.com`, `gitlab.com`, or a self-hosted host.
    pub host: String,
    /// An organization or a single user — both list the same way.
    pub owner: String,
    /// Where checkouts live. Unset means `<first configured root>/<owner>`,
    /// which is what drops the owner into the dashboard as a group for free.
    pub path: Option<String>,
    /// Gitea only: which `tea` login entry to use. Empty means tea's default.
    pub login: String,
    pub protocol: CloneProtocol,
    pub include_forks: bool,
    pub include_archived: bool,
    /// GitLab groups only: whether to descend into subgroups when listing.
    pub include_subgroups: bool,
    /// Repo-name globs to skip.
    pub exclude: Vec<String>,
    /// Sync skips a disabled org without forgetting it, so parking one for a
    /// while is a one-word edit instead of a delete-plus-retyping.
    pub enabled: bool,
}

impl Default for OrgConfig {
    fn default() -> Self {
        Self {
            provider: None,
            host: String::new(),
            owner: String::new(),
            path: None,
            login: String::new(),
            protocol: CloneProtocol::Ssh,
            include_forks: false,
            include_archived: false,
            include_subgroups: false,
            exclude: Vec::new(),
            enabled: true,
        }
    }
}

impl OrgConfig {
    /// An unset provider falls back through host inference; a host nothing
    /// knows reads as GitHub rather than panicking — [`Config::org_problems`]
    /// is what reports the ambiguity to a human.
    pub fn resolved_provider(&self) -> OrgProvider {
        self.provider
            .or(OrgProvider::from_host(&self.host))
            .unwrap_or(OrgProvider::GitHub)
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

    /// `None` means cached working-tree counts never expire on their own, which
    /// is what an empty `max_age` asks for.
    pub fn work_max_age(&self) -> Option<Duration> {
        let raw = self.status.max_age.trim();
        if raw.is_empty() {
            return None;
        }
        Some(parse_duration(raw).unwrap_or(Duration::from_secs(3600)))
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

    pub fn visibility_interval(&self) -> Duration {
        parse_duration(&self.visibility.interval).unwrap_or(Duration::from_secs(86_400))
    }

    /// The columns to render, resolved from `[ui] columns` or the defaults.
    /// A configured list is honoured as written — order included — beyond
    /// dropping repeats and putting REPO back if it was left out.
    pub fn columns(&self) -> Vec<Column> {
        match &self.ui.columns {
            Some(list) => crate::column::sanitise(list.clone()),
            None => Column::defaults(self.visibility.enabled),
        }
    }

    pub fn visibility_timeout(&self) -> Duration {
        parse_duration(&self.visibility.timeout).unwrap_or(Duration::from_secs(10))
    }

    /// Where this org's checkouts belong, expanded to an absolute path. An
    /// unset `path` lands under the first root so the owner shows up in the
    /// dashboard as a group; with no roots at all there is nothing to resolve
    /// against, and `None` says "don't check this one for overlap".
    fn resolved_org_path(&self, org: &OrgConfig) -> Option<PathBuf> {
        match &org.path {
            Some(p) => Some(paths::expand(p)),
            None => self
                .root_paths()
                .into_iter()
                .next()
                .map(|root| root.join(&org.owner)),
        }
    }

    /// Everything wrong with the `[[orgs]]` section, one human-readable line
    /// per problem; an empty vec means safe to sync against. This runs before
    /// any sync so a typo'd config fails loudly instead of cloning a fleet
    /// into the wrong tree — which is also why the checks are conservative:
    /// overlapping paths and duplicate owners are refused even though sync
    /// itself could probably cope.
    pub fn org_problems(&self) -> Vec<String> {
        let mut problems = Vec::new();

        for (idx, org) in self.orgs.iter().enumerate() {
            let tag = format!("orgs[{}]", idx + 1);
            if org.owner.trim().is_empty() {
                problems.push(format!("{tag}: owner is empty"));
            }
            if org.host.trim().is_empty() {
                problems.push(format!("{tag}: host is empty"));
            } else if org.provider.is_none() && OrgProvider::from_host(&org.host).is_none() {
                problems.push(format!(
                    "{tag}: host \"{}\" is not a known instance; set provider = \"github\" | \"gitlab\" | \"gitea\" explicitly",
                    org.host
                ));
            }
        }

        // Two orgs listing the same owner would fight over the same
        // checkouts, so the second registration is a mistake however you
        // slice it. Lists this short don't justify a map.
        for i in 0..self.orgs.len() {
            for j in (i + 1)..self.orgs.len() {
                let (a, b) = (&self.orgs[i], &self.orgs[j]);
                if a.resolved_provider() == b.resolved_provider()
                    && a.host.eq_ignore_ascii_case(&b.host)
                    && a.owner == b.owner
                {
                    problems.push(format!(
                        "orgs[{}] and orgs[{}]: duplicate org {} on {} for owner \"{}\"",
                        i + 1,
                        j + 1,
                        a.resolved_provider().as_str(),
                        a.host,
                        a.owner,
                    ));
                }
            }
        }

        // Nested paths mean one sync would fast-forward repos inside another
        // org's directory — or report them as orphans. Either way it's a
        // config bug worth catching before anything runs.
        let resolved: Vec<Option<PathBuf>> = self
            .orgs
            .iter()
            .map(|org| self.resolved_org_path(org))
            .collect();
        for i in 0..resolved.len() {
            for j in (i + 1)..resolved.len() {
                let pair = match (&resolved[i], &resolved[j]) {
                    (Some(a), Some(b)) => (a, b),
                    _ => continue,
                };
                if pair.0.starts_with(pair.1) || pair.1.starts_with(pair.0) {
                    problems.push(format!(
                        "orgs[{}] path {} overlaps orgs[{}] path {}",
                        i + 1,
                        pair.0.display(),
                        j + 1,
                        pair.1.display(),
                    ));
                }
            }
        }

        problems
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

    // What the column picker writes has to be readable again on the next
    // start, or the panel silently forgets everything on quit.
    #[test]
    fn a_saved_column_list_round_trips() {
        let cfg = Config {
            ui: UiConfig {
                columns: Some(vec![Column::Repo, Column::State, Column::SinceTag]),
                ..UiConfig::default()
            },
            ..Config::default()
        };
        let body = toml::to_string_pretty(&cfg).unwrap();
        let back: Config = toml::from_str(&body).unwrap();
        assert_eq!(back.ui.columns, cfg.ui.columns);
        assert_eq!(
            back.columns(),
            vec![Column::Repo, Column::State, Column::SinceTag]
        );
    }

    // Unset has to stay unset through a save, or `config init` would bake
    // today's defaults in and the list would stop tracking visibility.enabled.
    #[test]
    fn an_unset_column_list_stays_unset() {
        let body = toml::to_string_pretty(&Config::default()).unwrap();
        assert!(!body.contains("columns"), "{body}");
        let back: Config = toml::from_str(&body).unwrap();
        assert_eq!(back.ui.columns, None);
        assert_eq!(back.columns(), Column::defaults(false));
    }

    #[test]
    fn default_config_round_trips() {
        let cfg = Config::default();
        let body = toml::to_string_pretty(&cfg).unwrap();
        let back: Config = toml::from_str(&body).unwrap();
        assert_eq!(back.roots, cfg.roots);
        assert_eq!(back.max_depth, cfg.max_depth);
    }

    #[test]
    fn default_config_has_no_orgs() {
        assert!(Config::default().orgs.is_empty());
    }

    // Lowercase TOML names are what a user writes, so they have to survive
    // the trip into the enums — and the optional fields must fall back to
    // defaults, since a minimal three-line `[[orgs]]` table is the common case.
    #[test]
    fn org_tables_parse_with_defaults() {
        let cfg: Config = toml::from_str(
            r#"
            [[orgs]]
            host = "github.com"
            owner = "acme"

            [[orgs]]
            provider = "gitea"
            host = "git.example.com"
            owner = "otter"
            path = "~/dev/gitea/otter"
            login = "work"
            protocol = "https"
            include_forks = true
            include_archived = true
            include_subgroups = true
            exclude = ["sandbox-*"]
            enabled = false
            "#,
        )
        .unwrap();

        assert_eq!(cfg.orgs.len(), 2);

        let github = &cfg.orgs[0];
        assert_eq!(github.provider, None);
        assert_eq!(github.resolved_provider(), OrgProvider::GitHub);
        assert_eq!(github.host, "github.com");
        assert_eq!(github.owner, "acme");
        assert_eq!(github.path, None);
        assert_eq!(github.login, "");
        assert_eq!(github.protocol, CloneProtocol::Ssh);
        assert!(github.exclude.is_empty());
        assert!(github.enabled);

        let gitea = &cfg.orgs[1];
        assert_eq!(gitea.provider, Some(OrgProvider::Gitea));
        assert_eq!(gitea.resolved_provider(), OrgProvider::Gitea);
        assert_eq!(gitea.protocol, CloneProtocol::Https);
        assert_eq!(gitea.exclude, vec!["sandbox-*"]);
        assert!(!gitea.enabled);
    }

    #[test]
    fn unknown_keys_inside_org_tables_are_rejected() {
        let err = toml::from_str::<Config>(
            r#"
            [[orgs]]
            host = "github.com"
            owner = "acme"
            bogon = true
            "#,
        );
        assert!(err.is_err());
    }

    #[test]
    fn from_host_knows_the_hosted_instances() {
        assert_eq!(
            OrgProvider::from_host("github.com"),
            Some(OrgProvider::GitHub)
        );
        assert_eq!(
            OrgProvider::from_host("GitHub.Com"),
            Some(OrgProvider::GitHub)
        );
        assert_eq!(
            OrgProvider::from_host(" gitlab.com "),
            Some(OrgProvider::GitLab)
        );
        assert_eq!(
            OrgProvider::from_host("gitea.com"),
            Some(OrgProvider::Gitea)
        );
        assert_eq!(OrgProvider::from_host("git.example.com"), None);
        assert_eq!(OrgProvider::from_host(""), None);
    }

    #[test]
    fn org_problems_flags_duplicates_and_nested_paths() {
        let cfg: Config = toml::from_str(
            r#"
            [[orgs]]
            host = "github.com"
            owner = "acme"

            [[orgs]]
            host = "github.com"
            owner = "acme"

            [[orgs]]
            host = "github.com"
            owner = "other"
            path = "~/dev/github.com"

            [[orgs]]
            host = "github.com"
            owner = "nested"
            path = "~/dev/github.com/nested"
            "#,
        )
        .unwrap();

        let problems = cfg.org_problems();
        assert!(
            problems.iter().any(|p| p.contains("duplicate")),
            "{problems:?}"
        );
        assert!(
            problems.iter().any(|p| p.contains("overlaps")),
            "{problems:?}"
        );
    }

    #[test]
    fn org_problems_accepts_a_valid_config() {
        let cfg: Config = toml::from_str(
            r#"
            roots = ["~/Projects"]

            [[orgs]]
            host = "github.com"
            owner = "acme"

            [[orgs]]
            provider = "gitlab"
            host = "git.example.com"
            owner = "platform"
            path = "~/dev/gitlab/platform"

            [[orgs]]
            provider = "gitea"
            host = "git.example.com"
            owner = "otter"
            path = "~/dev/gitea/otter"
            "#,
        )
        .unwrap();

        assert_eq!(cfg.org_problems(), Vec::<String>::new());
    }
}
