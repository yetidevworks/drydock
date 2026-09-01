//! Filtering, searching and sorting.
//!
//! The same [`Query`] drives both `drydock list` and the dashboard, so the two
//! can never disagree about what "dirty in the last day" means.

use serde::{Deserialize, Serialize};
use std::str::FromStr;
use std::time::Duration;

use crate::config::parse_duration;
use crate::model::{ReleaseState, RepoStatus, Visibility, VisibilityStatus};

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
    /// Checked and public. Only ever true when `visibility.enabled` is on.
    Public,
    /// Checked and private or internal.
    Private,
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
            Filter::Public => matches!(
                repo.visibility.as_ref().map(|v| &v.status),
                Some(VisibilityStatus::Known(Visibility::Public))
            ),
            Filter::Private => matches!(
                repo.visibility.as_ref().map(|v| &v.status),
                Some(VisibilityStatus::Known(Visibility::Private))
                    | Some(VisibilityStatus::Known(Visibility::Internal))
            ),
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
            Filter::Public => "public",
            Filter::Private => "private",
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
            Filter::Public,
            Filter::Private,
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
    Stashes,
    Ahead,
    Behind,
    SinceTag,
    State,
    Visibility,
}

impl Sort {
    pub fn label(&self) -> &'static str {
        match self {
            Sort::Activity => "activity",
            Sort::Name => "name",
            Sort::Group => "group",
            Sort::Dirty => "changes",
            Sort::Stashes => "stashes",
            Sort::Ahead => "unpushed",
            Sort::Behind => "behind",
            Sort::SinceTag => "since-tag",
            Sort::State => "state",
            Sort::Visibility => "visibility",
        }
    }

    pub fn all() -> &'static [Sort] {
        &[
            Sort::Activity,
            Sort::Name,
            Sort::Group,
            Sort::Dirty,
            Sort::Stashes,
            Sort::Ahead,
            Sort::Behind,
            Sort::SinceTag,
            Sort::State,
            Sort::Visibility,
        ]
    }

    /// Step to the next sort key in the `s`-cycle.
    ///
    /// `visibility_enabled` skips `Sort::Visibility` in that cycle when
    /// false, the same gate `Column::defaults` already applies to the
    /// VISIBILITY column. This is the ambient, no-argument-needed path
    /// through the sort keys, so it's the one place that matters most:
    /// someone who has never turned visibility checking on shouldn't find
    /// an extra stop added to the key they already press, offering a sort
    /// that can only ever tie. Explicit requests -- `--sort visibility`,
    /// `default_sort = "visibility"` -- go through `FromStr` instead and
    /// are always honoured, the same way `--public`/`--private` already
    /// work regardless of the flag: asking by name is always answered,
    /// only the ambient default stays out of the way until asked for.
    pub fn next(&self, visibility_enabled: bool) -> Sort {
        let all = Sort::all();
        let idx = all.iter().position(|s| s == self).unwrap_or(0);
        let mut i = idx;
        loop {
            i = (i + 1) % all.len();
            let candidate = all[i];
            if visibility_enabled || candidate != Sort::Visibility || i == idx {
                return candidate;
            }
        }
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
            // A repo nothing has probed yet sorts with the empty ones: this
            // key exists to bring stashes to the top, and "no answer" isn't
            // a stash. Unlike `visibility`, it isn't gated out of the `s`
            // cycle -- the count is always probed, so the ordering is always
            // real, and `changes` isn't gated on CHANGES being on the screen
            // either.
            Sort::Stashes => a
                .stash_count()
                .unwrap_or(0)
                .cmp(&b.stash_count().unwrap_or(0)),
            Sort::Ahead => a.unpushed_total().cmp(&b.unpushed_total()),
            Sort::Behind => a.behind_total().cmp(&b.behind_total()),
            Sort::SinceTag => a.commits_since_tag().cmp(&b.commits_since_tag()),
            Sort::State => state_rank(a).cmp(&state_rank(b)),
            Sort::Visibility => visibility_rank(a).cmp(&visibility_rank(b)),
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

/// Ranking for visibility sort: a failed check first, then private, then
/// public. Everything else -- internal, no remote, an unsupported host, a
/// repo that couldn't be read, checking turned off, or never probed at all
/// -- ties in one bottom tier rather than being split out into tiers of its
/// own.
///
/// Reaching for a visibility sort means wanting the closed work collected
/// in one place -- the repos with a client's name in them, the ones not
/// ready to be read yet -- so private leads. Public is the state you can
/// already see from anywhere, and there's nothing to scroll past to find
/// it. Above both sits a failed check, because it's the only non-answer
/// there's anything to do about: the tool tried to ask and couldn't, which
/// is worth a second look in a way that "no remote" or "checking is off"
/// never is.
///
/// The bottom tier being one tier, not several, is a separate decision:
/// `NoRemote` and `Unsupported` are "free" facts, computed straight from
/// the remote URL whether or not `visibility.enabled` is on, and giving
/// them their own tiers would mean choosing this sort key still reshuffles
/// the list even for someone who has never turned visibility checking on at
/// all. With only CheckFailed, Private and Public able to earn a distinct
/// rank, a repo can't move until it's actually been checked -- so with
/// checking off, this sort is a true no-op, identical to the activity/path
/// tie-break every other sort already falls back to.
///
/// The sort descends by default, so the highest rank is what lands at the
/// top of the table.
fn visibility_rank(repo: &RepoStatus) -> u8 {
    match repo.visibility.as_ref().map(|v| &v.status) {
        Some(VisibilityStatus::CheckFailed(_)) => 3,
        Some(VisibilityStatus::Known(Visibility::Private)) => 2,
        Some(VisibilityStatus::Known(Visibility::Public)) => 1,
        Some(VisibilityStatus::Known(Visibility::Internal))
        | Some(VisibilityStatus::Unsupported)
        | Some(VisibilityStatus::NoRemote)
        | Some(VisibilityStatus::Unknown)
        | Some(VisibilityStatus::CheckingDisabled)
        | None => 0,
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
    use crate::model::{BranchInfo, Head, RefsInfo, VisibilityInfo, WorkInfo};
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
            fetched_at: None,
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

    // Biggest pile of stashes first, and a repo nothing has probed sinks in
    // with the empty ones rather than floating up as an unknown.
    #[test]
    fn sort_stashes_is_descending_and_unprobed_sinks() {
        let mut repos = vec![
            repo("a", "one", 0, 0, 0),
            repo("b", "many", 0, 0, 0),
            repo("c", "none", 0, 0, 0),
            repo("d", "unprobed", 0, 0, 0),
        ];
        repos[0].refs.as_mut().unwrap().stashes = 1;
        repos[1].refs.as_mut().unwrap().stashes = 7;
        repos[3].refs = None;

        let q = Query {
            sort: Sort::Stashes,
            ..Query::default()
        };
        let out = q.apply(&repos, 10_000);
        let names: Vec<&str> = out.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(&names[..2], &["many", "one"]);
        // The two zeroes tie on the key and fall back to activity, so only
        // their being last is the claim here.
        assert!(names[2..].contains(&"none") && names[2..].contains(&"unprobed"));
    }

    #[test]
    fn sort_visibility_puts_failed_first_then_private_then_public() {
        // Internal doesn't get a tier of its own -- only CheckFailed,
        // Private and Public do. A failed check leads because it's the only
        // non-answer worth acting on; private outranks public because this
        // sort exists to collect the closed work in one place, and public
        // repos are already readable from anywhere. Distinct
        // activity on the two untiered repos (internal, never-probed) proves
        // they're genuinely tied on visibility_rank and only separated by
        // the usual activity fallback, not by some hidden ordering.
        // `activity_at()` is the max of the newest commit and the working
        // tree's newest mtime, and `repo()` fixes the latter at 2_000 --
        // higher than any committed_at worth setting here -- so it's the
        // mtime that has to move for these to actually differ.
        let mut failed_repo = repo("a", "flaky-repo", 0, 0, 0);
        failed_repo.visibility = Some(VisibilityInfo {
            status: VisibilityStatus::CheckFailed("rate limited".into()),
            checked_at: 0,
        });

        let mut public_repo = repo("b", "public-repo", 0, 0, 0);
        public_repo.visibility = Some(VisibilityInfo {
            status: VisibilityStatus::Known(Visibility::Public),
            checked_at: 0,
        });

        let mut private_repo = repo("c", "private-repo", 0, 0, 0);
        private_repo.visibility = Some(VisibilityInfo {
            status: VisibilityStatus::Known(Visibility::Private),
            checked_at: 0,
        });

        let mut internal_repo = repo("d", "internal-repo", 0, 0, 0);
        internal_repo.visibility = Some(VisibilityInfo {
            status: VisibilityStatus::Known(Visibility::Internal),
            checked_at: 0,
        });
        internal_repo.work.as_mut().unwrap().newest_mtime = Some(3_000);

        let mut never_probed = repo("e", "never-probed", 0, 0, 0);
        never_probed.work.as_mut().unwrap().newest_mtime = Some(1_500);

        let repos = vec![
            failed_repo,
            public_repo,
            private_repo,
            internal_repo,
            never_probed,
        ];
        let q = Query {
            sort: Sort::Visibility,
            ..Query::default()
        };
        let out = q.apply(&repos, 10_000);
        assert_eq!(
            out.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
            vec![
                "flaky-repo",
                "private-repo",
                "public-repo",
                "internal-repo",
                "never-probed",
            ],
            "failed, private and public each keep their own tier; internal \
             and a never-probed repo fall back to the activity tie-break, \
             which is 3000/1500 -- descending -- for those two"
        );
    }

    #[test]
    fn sort_visibility_orders_duplicates_within_a_tier_by_activity() {
        // Two repos in the same visibility state don't tie arbitrarily --
        // the usual activity fallback still applies inside a tier, same as
        // it does between tiers. This doubles every state from
        // sort_visibility_puts_failed_first_then_private_then_public:
        // two CheckFailed, two Public, two Private, and two pairs from the
        // untiered bottom group (Internal and NoRemote), all with distinct
        // activity so the exact order is provable rather than assumed. The
        // public pair deliberately carries more activity than the private
        // pair, so the tiers landing private-first proves the rank beats
        // the fallback rather than happening to agree with it.
        let make = |name: &str, status: VisibilityStatus, mtime: i64| {
            let mut r = repo("a", name, 0, 0, 0);
            r.visibility = Some(VisibilityInfo {
                status,
                checked_at: 0,
            });
            r.work.as_mut().unwrap().newest_mtime = Some(mtime);
            r
        };

        let repos = vec![
            make(
                "failed-newer",
                VisibilityStatus::CheckFailed("timeout".into()),
                8_000,
            ),
            make(
                "failed-older",
                VisibilityStatus::CheckFailed("timeout".into()),
                7_000,
            ),
            make(
                "public-newer",
                VisibilityStatus::Known(Visibility::Public),
                6_000,
            ),
            make(
                "public-older",
                VisibilityStatus::Known(Visibility::Public),
                5_000,
            ),
            make(
                "private-newer",
                VisibilityStatus::Known(Visibility::Private),
                4_000,
            ),
            make(
                "private-older",
                VisibilityStatus::Known(Visibility::Private),
                3_000,
            ),
            // These four are all rank 0 -- two different untiered states,
            // interleaved by activity alone, to show the bottom tier really
            // is one bucket and not secretly four.
            make(
                "internal-newest",
                VisibilityStatus::Known(Visibility::Internal),
                2_800,
            ),
            make("no-remote-newer", VisibilityStatus::NoRemote, 2_600),
            make(
                "internal-older",
                VisibilityStatus::Known(Visibility::Internal),
                2_400,
            ),
            make("no-remote-oldest", VisibilityStatus::NoRemote, 2_200),
        ];

        let q = Query {
            sort: Sort::Visibility,
            ..Query::default()
        };
        let out = q.apply(&repos, 10_000);
        assert_eq!(
            out.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
            vec![
                "failed-newer",
                "failed-older",
                "private-newer",
                "private-older",
                "public-newer",
                "public-older",
                "internal-newest",
                "no-remote-newer",
                "internal-older",
                "no-remote-oldest",
            ],
            "each ranked tier stays intact and ordered newest-first \
             internally; the two untiered states interleave by activity \
             alone since they share the same rank"
        );
    }

    #[test]
    fn sort_visibility_is_a_no_op_without_a_checked_answer() {
        // None of these five ever resulted from an actual check succeeding --
        // no remote, an unsupported host, a repo that couldn't be read,
        // checking turned off, and never having been probed at all. Every
        // one of them is real, distinct data the tool already tracks, but
        // none of it should earn a repo a different spot in this sort: with
        // no CheckFailed, Public or Private answer anywhere in the list (an
        // Internal repo would tie here too, for the same reason -- only
        // those three carry their own tier), `visibility` must order exactly
        // like `activity` does, so a repo can't get reshuffled by this sort
        // key before anyone has ever turned visibility checking on.
        let mut no_remote = repo("a", "no-remote-repo", 0, 0, 0);
        no_remote.visibility = Some(VisibilityInfo {
            status: VisibilityStatus::NoRemote,
            checked_at: 0,
        });

        let mut unsupported = repo("b", "gitlab-repo", 0, 0, 0);
        unsupported.visibility = Some(VisibilityInfo {
            status: VisibilityStatus::Unsupported,
            checked_at: 0,
        });

        let mut unreadable = repo("c", "unreadable-repo", 0, 0, 0);
        unreadable.visibility = Some(VisibilityInfo {
            status: VisibilityStatus::Unknown,
            checked_at: 0,
        });

        let mut disabled = repo("d", "checking-off-repo", 0, 0, 0);
        disabled.visibility = Some(VisibilityInfo {
            status: VisibilityStatus::CheckingDisabled,
            checked_at: 0,
        });

        let never_probed = repo("e", "never-probed-repo", 0, 0, 0);

        // Distinct activity so a genuine no-op has something to prove: any
        // reordering here would mean a tier leaked in that shouldn't have.
        // It's the mtime that has to move, not the commit date --
        // `activity_at()` is the max of the two and `repo()` pins the mtime
        // at 2_000, so nudging `committed_at` up from 1_000 would leave all
        // five tied at 2_000 and hand the ordering to the `root` path
        // tie-break instead of to activity.
        let mut repos = vec![no_remote, unsupported, unreadable, disabled, never_probed];
        for (i, r) in repos.iter_mut().enumerate() {
            r.work.as_mut().unwrap().newest_mtime = Some(3_000 + i as i64);
        }

        let by_activity = Query {
            sort: Sort::Activity,
            ..Query::default()
        }
        .apply(&repos, 10_000)
        .iter()
        .map(|r| r.name.clone())
        .collect::<Vec<_>>();

        let by_visibility = Query {
            sort: Sort::Visibility,
            ..Query::default()
        }
        .apply(&repos, 10_000)
        .iter()
        .map(|r| r.name.clone())
        .collect::<Vec<_>>();

        assert_eq!(by_visibility, by_activity);
    }

    #[test]
    fn next_skips_visibility_when_checking_is_off() {
        // Cycling wraps all the way around back to Activity without ever
        // landing on Visibility -- the `s` key shouldn't gain an extra stop
        // for someone who has never turned checking on.
        let expected = [
            Sort::Name,
            Sort::Group,
            Sort::Dirty,
            Sort::Stashes,
            Sort::Ahead,
            Sort::Behind,
            Sort::SinceTag,
            Sort::State,
            Sort::Activity,
            Sort::Name,
        ];
        let mut s = Sort::Activity;
        for want in expected {
            s = s.next(false);
            assert_eq!(s, want);
        }
    }

    #[test]
    fn next_visits_visibility_when_checking_is_on() {
        let expected = [
            Sort::Name,
            Sort::Group,
            Sort::Dirty,
            Sort::Stashes,
            Sort::Ahead,
            Sort::Behind,
            Sort::SinceTag,
            Sort::State,
            Sort::Visibility,
            Sort::Activity,
        ];
        let mut s = Sort::Activity;
        for want in expected {
            s = s.next(true);
            assert_eq!(s, want);
        }
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
