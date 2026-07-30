//! Filtering, searching and sorting.
//!
//! The same [`Query`] drives both `drydock list` and the dashboard, so the two
//! can never disagree about what "dirty in the last day" means.

use serde::{Deserialize, Serialize};
use std::str::FromStr;
use std::time::Duration;

use crate::config::parse_duration;
use crate::model::{ReleaseState, RepoStatus};

/// A state a repo can be in. Toggling these is the main way to narrow the list.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Filter {
    Dirty,
    Unpushed,
    /// Never released: no tags at all.
    Unreleased,
    /// Tagged, with commits or uncommitted changes past the tag.
    NeedsRelease,
    /// Tagged, with nothing since.
    Released,
    Behind,
    Conflicted,
    InProgress,
    Detached,
    NoRemote,
    NoUpstream,
    Stashed,
    Clean,
    Error,
}

impl Filter {
    pub fn matches(&self, repo: &RepoStatus) -> bool {
        let f = repo.flags();
        match self {
            Filter::Dirty => f.dirty,
            Filter::Unpushed => f.unpushed,
            Filter::Unreleased => repo.release_state() == ReleaseState::Unreleased,
            Filter::NeedsRelease => repo.release_state() == ReleaseState::NeedsRelease,
            Filter::Released => repo.release_state() == ReleaseState::Released,
            Filter::Behind => repo.behind_total() > 0,
            Filter::Conflicted => f.conflicted,
            Filter::InProgress => f.in_progress,
            Filter::Detached => f.detached,
            Filter::NoRemote => f.no_remote,
            Filter::NoUpstream => f.no_upstream,
            Filter::Stashed => f.stashed,
            Filter::Clean => f.clean(),
            Filter::Error => f.error,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Filter::Dirty => "dirty",
            Filter::Unpushed => "unpushed",
            Filter::Unreleased => "unreleased",
            Filter::NeedsRelease => "needs-release",
            Filter::Released => "released",
            Filter::Behind => "behind",
            Filter::Conflicted => "conflicted",
            Filter::InProgress => "in-progress",
            Filter::Detached => "detached",
            Filter::NoRemote => "no-remote",
            Filter::NoUpstream => "no-upstream",
            Filter::Stashed => "stashed",
            Filter::Clean => "clean",
            Filter::Error => "error",
        }
    }

    pub fn all() -> &'static [Filter] {
        &[
            Filter::Dirty,
            Filter::Unpushed,
            Filter::Unreleased,
            Filter::NeedsRelease,
            Filter::Released,
            Filter::Behind,
            Filter::Conflicted,
            Filter::InProgress,
            Filter::Detached,
            Filter::NoRemote,
            Filter::NoUpstream,
            Filter::Stashed,
            Filter::Clean,
            Filter::Error,
        ]
    }
}

impl FromStr for Filter {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let key = s.trim().to_ascii_lowercase().replace('_', "-");
        Filter::all()
            .iter()
            .copied()
            .find(|f| f.label() == key)
            .ok_or_else(|| format!("unknown filter: {s}"))
    }
}

/// Whether several active filters narrow or widen the result.
///
/// `Any` is the default. Toggling `dirty` and `unpushed` almost always means
/// "show me anything in either state" rather than the much smaller set that is
/// in both at once.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MatchMode {
    Any,
    All,
}

impl MatchMode {
    pub fn label(&self) -> &'static str {
        match self {
            MatchMode::Any => "any",
            MatchMode::All => "all",
        }
    }

    pub fn toggled(&self) -> Self {
        match self {
            MatchMode::Any => MatchMode::All,
            MatchMode::All => MatchMode::Any,
        }
    }
}

impl FromStr for MatchMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "any" | "or" => Ok(MatchMode::Any),
            "all" | "and" => Ok(MatchMode::All),
            other => Err(format!("unknown match mode: {other}")),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Sort {
    Activity,
    Name,
    Group,
    Dirty,
    Ahead,
    Behind,
    SinceTag,
    State,
}

impl Sort {
    pub fn label(&self) -> &'static str {
        match self {
            Sort::Activity => "activity",
            Sort::Name => "name",
            Sort::Group => "group",
            Sort::Dirty => "changes",
            Sort::Ahead => "unpushed",
            Sort::Behind => "behind",
            Sort::SinceTag => "since-tag",
            Sort::State => "state",
        }
    }

    pub fn all() -> &'static [Sort] {
        &[
            Sort::Activity,
            Sort::Name,
            Sort::Group,
            Sort::Dirty,
            Sort::Ahead,
            Sort::Behind,
            Sort::SinceTag,
            Sort::State,
        ]
    }

    pub fn next(&self) -> Sort {
        let all = Sort::all();
        let idx = all.iter().position(|s| s == self).unwrap_or(0);
        all[(idx + 1) % all.len()]
    }

    /// Most of these read better biggest-first; names read better A to Z.
    fn descending_by_default(&self) -> bool {
        !matches!(self, Sort::Name | Sort::Group)
    }
}

impl FromStr for Sort {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let key = s.trim().to_ascii_lowercase().replace('_', "-");
        Sort::all()
            .iter()
            .copied()
            .find(|s| s.label() == key)
            .ok_or_else(|| format!("unknown sort key: {s}"))
    }
}

#[derive(Clone, Debug)]
pub struct Query {
    pub filters: Vec<Filter>,
    pub match_mode: MatchMode,
    /// Only repos active within this window.
    pub since: Option<Duration>,
    /// Only repos in this group.
    pub group: Option<String>,
    pub search: String,
    pub sort: Sort,
    /// Flip the sort key's natural direction.
    pub reverse: bool,
}

impl Default for Query {
    fn default() -> Self {
        Self {
            filters: Vec::new(),
            match_mode: MatchMode::Any,
            since: None,
            group: None,
            search: String::new(),
            sort: Sort::Activity,
            reverse: false,
        }
    }
}

impl Query {
    pub fn toggle(&mut self, filter: Filter) {
        if let Some(pos) = self.filters.iter().position(|f| *f == filter) {
            self.filters.remove(pos);
        } else {
            self.filters.push(filter);
        }
    }

    pub fn has(&self, filter: Filter) -> bool {
        self.filters.contains(&filter)
    }

    pub fn set_since(&mut self, spec: &str) -> Result<(), String> {
        if spec.trim().is_empty() {
            self.since = None;
            return Ok(());
        }
        match parse_duration(spec) {
            Some(d) => {
                self.since = Some(d);
                Ok(())
            }
            None => Err(format!("could not read a duration from {spec:?}")),
        }
    }

    fn passes_filters(&self, repo: &RepoStatus) -> bool {
        if self.filters.is_empty() {
            return true;
        }
        match self.match_mode {
            MatchMode::Any => self.filters.iter().any(|f| f.matches(repo)),
            MatchMode::All => self.filters.iter().all(|f| f.matches(repo)),
        }
    }

    /// Apply the query. `now` is passed in so a whole render uses one clock.
    pub fn apply<'a>(&self, repos: &'a [RepoStatus], now: i64) -> Vec<&'a RepoStatus> {
        self.apply_indices(repos, now)
            .into_iter()
            .map(|i| &repos[i])
            .collect()
    }

    /// Same as [`Query::apply`], but returning positions. The dashboard keeps
    /// indices rather than references so it can hold the list mutably between
    /// repaints.
    pub fn apply_indices(&self, repos: &[RepoStatus], now: i64) -> Vec<usize> {
        let cutoff = self.since.map(|d| now - d.as_secs() as i64);
        let mut out: Vec<&RepoStatus> = repos
            .iter()
            .filter(|r| {
                if let Some(group) = &self.group {
                    if !r.group.eq_ignore_ascii_case(group) {
                        return false;
                    }
                }
                if let Some(cutoff) = cutoff {
                    if r.activity_at() < cutoff {
                        return false;
                    }
                }
                if !self.search.is_empty() && score(&self.search, r).is_none() {
                    return false;
                }
                self.passes_filters(r)
            })
            .collect();

        sort_repos(&mut out, self.sort, self.reverse);

        // Turn the sorted references back into positions. Paths are unique per
        // repo, so they're a safe key to map back through.
        let positions: std::collections::HashMap<&std::path::Path, usize> = repos
            .iter()
            .enumerate()
            .map(|(i, r)| (r.root.as_path(), i))
            .collect();
        out.into_iter()
            .filter_map(|r| positions.get(r.root.as_path()).copied())
            .collect()
    }
}

pub fn sort_repos(repos: &mut [&RepoStatus], sort: Sort, reverse: bool) {
    repos.sort_by(|a, b| {
        let ord = match sort {
            Sort::Activity => a.activity_at().cmp(&b.activity_at()),
            Sort::Name => a
                .name
                .to_ascii_lowercase()
                .cmp(&b.name.to_ascii_lowercase()),
            Sort::Group => a
                .group
                .to_ascii_lowercase()
                .cmp(&b.group.to_ascii_lowercase())
                .then_with(|| {
                    a.name
                        .to_ascii_lowercase()
                        .cmp(&b.name.to_ascii_lowercase())
                }),
            Sort::Dirty => a.dirty_total().cmp(&b.dirty_total()),
            Sort::Ahead => a.unpushed_total().cmp(&b.unpushed_total()),
            Sort::Behind => a.behind_total().cmp(&b.behind_total()),
            Sort::SinceTag => a.commits_since_tag().cmp(&b.commits_since_tag()),
            Sort::State => state_rank(a).cmp(&state_rank(b)),
        };
        // Ties fall back to activity, then path, so the order is stable across
        // repaints.
        let ord = ord
            .then_with(|| a.activity_at().cmp(&b.activity_at()))
            .then_with(|| a.root.cmp(&b.root));
        let descending = sort.descending_by_default() != reverse;
        if descending {
            ord.reverse()
        } else {
            ord
        }
    });
}

/// Ranking for state sort: the things most likely to need attention first.
fn state_rank(repo: &RepoStatus) -> u8 {
    let f = repo.flags();
    if f.error {
        7
    } else if f.conflicted {
        6
    } else if f.in_progress {
        5
    } else if f.dirty {
        4
    } else if f.unpushed {
        3
    } else if repo.release_state() == ReleaseState::NeedsRelease {
        2
    } else if repo.work.is_none() {
        1
    } else {
        0
    }
}

/// Score a repo against a search string.
///
/// An exact substring anywhere in `group/name` wins, a prefix match on the name
/// scores higher still, and failing that a subsequence match lets `gpapi` find
/// `grav/grav-plugin-api`. Lower is better. `None` means no match.
pub fn score(needle: &str, repo: &RepoStatus) -> Option<u32> {
    let needle = needle.trim().to_ascii_lowercase();
    if needle.is_empty() {
        return Some(0);
    }
    let name = repo.name.to_ascii_lowercase();
    let slug = repo.slug().to_ascii_lowercase();
    let branch = repo.branch_label().to_ascii_lowercase();

    if name.starts_with(&needle) {
        return Some(0);
    }
    if let Some(pos) = name.find(&needle) {
        return Some(10 + pos as u32);
    }
    if let Some(pos) = slug.find(&needle) {
        return Some(100 + pos as u32);
    }
    if branch.contains(&needle) {
        return Some(500);
    }
    if is_subsequence(&needle, &slug) {
        return Some(1_000);
    }
    None
}

fn is_subsequence(needle: &str, haystack: &str) -> bool {
    let mut chars = haystack.chars();
    needle.chars().all(|c| chars.any(|h| h == c))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{BranchInfo, Head, RefsInfo, WorkInfo};
    use std::path::PathBuf;

    fn repo(group: &str, name: &str, ahead: u32, dirty: u32, since_tag: u32) -> RepoStatus {
        let mut r = RepoStatus::new(
            PathBuf::from(format!("/p/{group}/{name}")),
            group.into(),
            name.into(),
        );
        r.refs = Some(RefsInfo {
            head: Head::Branch("main".into()),
            branches: vec![BranchInfo {
                name: "main".into(),
                upstream: Some("origin/main".into()),
                ahead,
                behind: 0,
                gone: false,
                committed_at: 1_000,
                sha: "abc1234".into(),
                subject: "work".into(),
            }],
            last_commit: None,
            tags_orphaned: false,
            stashes: 0,
            operation: None,
            newest_tag: None,
            described_tag: None,
            commits_since_tag: Some(since_tag),
            since_tag_subjects: Vec::new(),
            index_mtime: None,
            remote_url: Some("git@github.com:x/y.git".into()),
            changelog: None,
            is_bare: false,
            is_shallow: false,
        });
        r.work = Some(WorkInfo {
            staged: 0,
            unstaged: dirty,
            untracked: 0,
            conflicts: 0,
            newest_mtime: Some(2_000),
            files: Vec::new(),
            truncated: false,
        });
        r
    }

    #[test]
    fn any_mode_widens_all_mode_narrows() {
        let repos = vec![
            repo("grav", "a", 3, 0, 0), // unpushed only
            repo("grav", "b", 0, 2, 0), // dirty only
            repo("grav", "c", 1, 1, 0), // both
        ];
        let mut q = Query {
            filters: vec![Filter::Dirty, Filter::Unpushed],
            ..Query::default()
        };

        q.match_mode = MatchMode::Any;
        assert_eq!(q.apply(&repos, 10_000).len(), 3);

        q.match_mode = MatchMode::All;
        let all = q.apply(&repos, 10_000);
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].name, "c");
    }

    #[test]
    fn since_uses_activity() {
        let repos = vec![repo("grav", "a", 0, 0, 0)];
        let mut q = Query::default();
        // Activity for this fixture is the working-tree mtime, 2000.
        q.set_since("1h").unwrap();
        assert_eq!(q.apply(&repos, 2_100).len(), 1);
        assert_eq!(q.apply(&repos, 100_000).len(), 0);
    }

    #[test]
    fn search_finds_by_subsequence() {
        let r = repo("grav", "grav-plugin-api", 0, 0, 0);
        assert!(score("api", &r).is_some());
        assert!(score("gpapi", &r).is_some());
        assert!(score("zzz", &r).is_none());
    }

    #[test]
    fn sort_since_tag_is_descending() {
        let repos = vec![
            repo("a", "low", 0, 0, 1),
            repo("b", "high", 0, 0, 20),
            repo("c", "mid", 0, 0, 5),
        ];
        let q = Query {
            sort: Sort::SinceTag,
            ..Query::default()
        };
        let out = q.apply(&repos, 10_000);
        assert_eq!(
            out.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
            vec!["high", "mid", "low"]
        );
    }

    #[test]
    fn filter_names_round_trip() {
        for f in Filter::all() {
            assert_eq!(Filter::from_str(f.label()).unwrap(), *f);
        }
        for s in Sort::all() {
            assert_eq!(Sort::from_str(s.label()).unwrap(), *s);
        }
    }
}
