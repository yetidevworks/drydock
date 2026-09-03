//! The data model for a single repo's observed state.
//!
//! Everything here is serializable, because the same structs are both the
//! in-memory rows the TUI paints and the on-disk cache that lets a restart
//! paint instantly. Probing is split in two tiers that live in separate
//! fields: `refs` is cheap (a couple of `for-each-ref` calls) and always
//! refreshed, `work` is expensive (a full working-tree scan) and cached
//! against `work_key` so it only reruns when something actually changed.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Where HEAD is pointing.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum Head {
    /// On a branch, e.g. `develop`.
    Branch(String),
    /// Detached at a commit.
    Detached { sha: String },
    /// A fresh repo with no commits yet.
    Unborn,
}

impl Head {
    pub fn label(&self) -> String {
        match self {
            Head::Branch(b) => b.clone(),
            Head::Detached { sha } => format!("@{sha}"),
            Head::Unborn => "(unborn)".into(),
        }
    }

    pub fn branch(&self) -> Option<&str> {
        match self {
            Head::Branch(b) => Some(b.as_str()),
            _ => None,
        }
    }
}

/// A git operation left half-finished in the working tree.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum Operation {
    Merge,
    Rebase,
    CherryPick,
    Revert,
    Bisect,
}

impl Operation {
    pub fn label(&self) -> &'static str {
        match self {
            Operation::Merge => "merging",
            Operation::Rebase => "rebasing",
            Operation::CherryPick => "cherry-picking",
            Operation::Revert => "reverting",
            Operation::Bisect => "bisecting",
        }
    }
}

/// One local branch, with its tracking position against its upstream.
///
/// Ahead/behind come from `%(upstream:track)`, which reads the already-fetched
/// remote-tracking ref. No network involved, so this is safe to compute on
/// every sweep; it tells you what you haven't pushed, not what you haven't
/// fetched.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BranchInfo {
    pub name: String,
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    pub gone: bool,
    pub committed_at: i64,
    pub sha: String,
    pub subject: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TagInfo {
    pub name: String,
    pub at: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CommitInfo {
    pub sha: String,
    pub at: i64,
    pub subject: String,
    pub author: String,
}

/// What the top block of `CHANGELOG.md` claims, versus the newest tag.
///
/// A changelog whose top version has no matching tag is an in-flight release
/// sitting there waiting to be cut.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChangelogInfo {
    pub version: String,
    pub tagged: bool,
    /// Number of distinct unreleased version headings found above the last
    /// tagged one. More than one means blocks have stacked up.
    pub unreleased_blocks: u32,
}

/// Tier 1: everything derivable from `.git` without touching the working tree.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RefsInfo {
    pub head: Head,
    pub branches: Vec<BranchInfo>,
    pub last_commit: Option<CommitInfo>,
    pub stashes: u32,
    pub operation: Option<Operation>,
    /// Newest tag by creation date, whether or not HEAD can reach it.
    pub newest_tag: Option<TagInfo>,
    /// Nearest tag reachable from HEAD (`git describe --tags --abbrev=0`).
    pub described_tag: Option<TagInfo>,
    /// Commits on HEAD since `described_tag`.
    pub commits_since_tag: Option<u32>,
    /// Subjects of those commits, newest first, capped.
    pub since_tag_subjects: Vec<String>,
    /// True when `newest_tag` sits on history no branch can reach, local or
    /// remote. That happens when a repo is reused: the tags come from an
    /// import or an earlier life, and the current work shares no commits with
    /// them. Such tags are not releases of what is checked out now.
    #[serde(default)]
    pub tags_orphaned: bool,
    pub index_mtime: Option<i64>,
    /// When this repo last fetched, from `FETCH_HEAD`. `None` means it never
    /// has, which is what makes a zero "behind" count meaningless rather than
    /// reassuring. Deserializes to `None` for cache entries written before
    /// this field existed -- the same "never checked" state, so no cache
    /// version bump was needed.
    #[serde(default)]
    pub fetched_at: Option<i64>,
    pub remote_url: Option<String>,
    pub changelog: Option<ChangelogInfo>,
    pub is_bare: bool,
    pub is_shallow: bool,
}

impl RefsInfo {
    /// Total commits sitting on local branches that their upstreams don't have.
    pub fn unpushed(&self) -> u32 {
        self.branches.iter().map(|b| b.ahead).sum()
    }

    /// Total commits upstreams have that we don't. Only as fresh as the last
    /// fetch.
    pub fn unpulled(&self) -> u32 {
        self.branches.iter().map(|b| b.behind).sum()
    }

    pub fn current_branch(&self) -> Option<&BranchInfo> {
        let name = self.head.branch()?;
        self.branches.iter().find(|b| b.name == name)
    }

    /// True when the newest tag isn't reachable from HEAD. Normal in git-flow
    /// (tags land on `master`, work continues on `develop`) but it means
    /// "commits since tag" needs reading with care.
    pub fn tag_off_branch(&self) -> bool {
        if self.tags_orphaned {
            return false;
        }
        match (&self.newest_tag, &self.described_tag) {
            (Some(newest), Some(described)) => newest.name != described.name,
            (Some(_), None) => true,
            _ => false,
        }
    }

    pub fn newest_commit_at(&self) -> i64 {
        self.branches
            .iter()
            .map(|b| b.committed_at)
            .max()
            .unwrap_or(0)
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum ChangeKind {
    Staged,
    Unstaged,
    Untracked,
    Conflicted,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChangedFile {
    pub path: String,
    /// The raw two-letter XY code from porcelain v2, e.g. `M.`, `.M`, `??`.
    pub code: String,
    pub kind: ChangeKind,
    pub mtime: Option<i64>,
}

/// Tier 2: the working-tree scan. This is the expensive half.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkInfo {
    pub staged: u32,
    pub unstaged: u32,
    pub untracked: u32,
    pub conflicts: u32,
    /// Newest mtime across changed files. This is what makes "modified in the
    /// last hour" work for a dirty repo, since working-tree edits never touch
    /// anything under `.git`.
    pub newest_mtime: Option<i64>,
    pub files: Vec<ChangedFile>,
    pub truncated: bool,
}

impl WorkInfo {
    pub fn total(&self) -> u32 {
        self.staged + self.unstaged + self.untracked + self.conflicts
    }

    pub fn is_dirty(&self) -> bool {
        self.total() > 0
    }
}

/// Cache validity key for tier 2. If HEAD and the index are unchanged, a
/// cached working-tree result is still good.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkKey {
    pub head_sha: Option<String>,
    pub index_mtime: Option<i64>,
    pub index_size: Option<u64>,
}

/// Why a repo's activity timestamp is what it is, so the age column can always
/// be explained rather than just asserted.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum ActivitySource {
    Commit,
    WorkingTree,
    Unknown,
}

impl ActivitySource {
    pub fn label(&self) -> &'static str {
        match self {
            ActivitySource::Commit => "last commit",
            ActivitySource::WorkingTree => "file edit",
            ActivitySource::Unknown => "unknown",
        }
    }
}

/// Where a repo stands against its own release history.
///
/// This is a separate axis from the working state. A repo can be dirty and
/// released, or spotless and still needing a release, and the two questions get
/// asked at different times: "what am I in the middle of" versus "what have I
/// not shipped".
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum ReleaseState {
    /// No tags at all. Never been released.
    Unreleased,
    /// Tagged, with nothing since.
    Released,
    /// Tagged, but there are commits or uncommitted changes past the tag.
    NeedsRelease,
}

impl ReleaseState {
    pub fn label(&self) -> &'static str {
        match self {
            ReleaseState::Unreleased => "unreleased",
            ReleaseState::Released => "released",
            ReleaseState::NeedsRelease => "needs release",
        }
    }

    pub fn key(&self) -> &'static str {
        match self {
            ReleaseState::Unreleased => "unreleased",
            ReleaseState::Released => "released",
            ReleaseState::NeedsRelease => "needs-release",
        }
    }
}

/// Coarse state used for colouring and the default filters.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Flags {
    pub dirty: bool,
    pub unpushed: bool,
    pub conflicted: bool,
    pub in_progress: bool,
    pub detached: bool,
    pub no_remote: bool,
    pub no_upstream: bool,
    pub stashed: bool,
    pub error: bool,
}

impl Flags {
    /// Nothing outstanding in the working tree or against the upstream. Says
    /// nothing about releases; that is [`ReleaseState`].
    pub fn clean(&self) -> bool {
        !self.dirty && !self.unpushed && !self.conflicted && !self.in_progress && !self.error
    }
}

/// A repo's visibility on its hosting service (GitHub, for now), as last
/// observed via `gh`. This is not a `git` concept at all — nothing under
/// `.git` records whether a remote is public or private, so unlike every
/// other field here it can only come from asking the host.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum Visibility {
    Public,
    Private,
    /// GitHub Enterprise's "visible to the whole org, but not the world"
    /// tier. Kept distinct rather than folded into `Private` because that
    /// would misreport it to anyone scanning for truly private repos.
    Internal,
}

impl Visibility {
    pub fn label(&self) -> &'static str {
        match self {
            Visibility::Public => "public",
            Visibility::Private => "private",
            Visibility::Internal => "internal",
        }
    }
}

impl std::str::FromStr for Visibility {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_uppercase().as_str() {
            "PUBLIC" => Ok(Visibility::Public),
            "PRIVATE" => Ok(Visibility::Private),
            "INTERNAL" => Ok(Visibility::Internal),
            _ => Err(()),
        }
    }
}

/// What's known about a repo's visibility, or the specific reason nothing is.
/// Every variant but [`Known`](VisibilityStatus::Known) is a real, distinct
/// fact rather than a catch-all -- the point of having this many variants
/// instead of a plain `Option<Visibility>` is that "we don't know" always has
/// a specific cause worth saying.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum VisibilityStatus {
    /// A value a hosting provider actually reported (see [`crate::provider`]).
    Known(Visibility),
    /// This repo's remote isn't on any host [`crate::provider::detect`]
    /// recognises -- a different host than the ones supported, or a URL that
    /// doesn't parse into `owner/repo` at all. Determined from the remote URL
    /// alone, no network call involved, so unlike
    /// [`Known`](VisibilityStatus::Known) it's never stale and never costs a
    /// CLI invocation to say.
    Unsupported,
    /// No remote at all, so there's nothing any provider could ever check.
    /// Also free to determine.
    NoRemote,
    /// The repo itself couldn't be read, so whether it even has a remote is
    /// unknown. Distinct from [`NoRemote`](VisibilityStatus::NoRemote), which
    /// is a fact established about a repo that *was* read — claiming it here
    /// would state something nothing checked.
    Unknown,
    /// The remote is checkable -- a provider recognises it -- but
    /// `visibility.enabled` is off, so nothing has actually asked.
    CheckingDisabled,
    /// A provider check was attempted and failed (rate limited, not
    /// authenticated, timed out), and there was no previously cached value to
    /// fall back to. The reason is kept, but only surfaced in `status`,
    /// `--json`, and the TUI detail view -- never in a table, where one long
    /// message would widen the column for every row.
    CheckFailed(String),
}

impl VisibilityStatus {
    /// Short label for tables and the common case. `CheckFailed` deliberately
    /// does not include its reason here -- see the variant's own docs.
    pub fn label(&self) -> &'static str {
        match self {
            VisibilityStatus::Known(v) => v.label(),
            VisibilityStatus::Unsupported => "unsupported",
            VisibilityStatus::NoRemote => "no remote configured",
            VisibilityStatus::Unknown => "unknown",
            VisibilityStatus::CheckingDisabled => "checking disabled",
            VisibilityStatus::CheckFailed(_) => "check failed",
        }
    }
}

/// Visibility as last checked, plus when. Kept separate from [`RefsInfo`]
/// because it comes from a different tool (`gh`, not `git`), costs real
/// network and API time, and goes stale on its own clock rather than whenever
/// HEAD or the index moves.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VisibilityInfo {
    pub status: VisibilityStatus,
    pub checked_at: i64,
}

/// One repo, as most recently observed.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RepoStatus {
    pub root: PathBuf,
    /// First path segment below the scan root, e.g. `grav` or `yetidevworks`.
    /// Empty when the repo sits directly in a root.
    pub group: String,
    pub name: String,

    pub refs: Option<RefsInfo>,
    pub work: Option<WorkInfo>,
    pub error: Option<String>,

    pub refs_probed_at: i64,
    pub work_probed_at: i64,
    pub work_key: Option<WorkKey>,

    /// `probe::fill_visibility` always sets this to `Some` once it has run --
    /// even "no remote" and "checking is off" are real, stored facts (see
    /// [`VisibilityStatus`]), not just an absence. `None` only means it
    /// hasn't run at all: a fresh [`RepoStatus`], or a cache entry from
    /// before this field existed (which deserializes to `None` here too, the
    /// same "hasn't run yet" state, so no cache version bump was needed).
    #[serde(default)]
    pub visibility: Option<VisibilityInfo>,
}

impl RepoStatus {
    pub fn new(root: PathBuf, group: String, name: String) -> Self {
        Self {
            root,
            group,
            name,
            refs: None,
            work: None,
            error: None,
            refs_probed_at: 0,
            work_probed_at: 0,
            work_key: None,
            visibility: None,
        }
    }

    /// Short label for the VISIBILITY column. See [`VisibilityStatus::label`]
    /// for the possible values; `-` only appears before the repo has been
    /// probed at all, which a rendered row should never actually show.
    pub fn visibility_label(&self) -> &'static str {
        self.visibility
            .as_ref()
            .map(|v| v.status.label())
            .unwrap_or("-")
    }

    /// The marker for the VISIBILITY column — on its own in the short form,
    /// and in front of the label in the long one, so the two can't drift.
    ///
    /// Filled means "out in the open". `⊘` (circled division slash) is the
    /// same stroke as a prohibited sign and reads as "access denied" more
    /// directly than a hollow circle would; `◐` says internal is half of
    /// each. A check that was attempted and *failed* is worth telling apart
    /// from one that was never made, since it's the only one you can act on.
    pub fn visibility_marker(&self) -> &'static str {
        match self.visibility.as_ref().map(|v| &v.status) {
            Some(VisibilityStatus::Known(Visibility::Public)) => "●",
            Some(VisibilityStatus::Known(Visibility::Private)) => "⊘",
            Some(VisibilityStatus::Known(Visibility::Internal)) => "◐",
            Some(VisibilityStatus::CheckFailed(_)) => "!",
            Some(VisibilityStatus::Unsupported)
            | Some(VisibilityStatus::NoRemote)
            | Some(VisibilityStatus::Unknown)
            | Some(VisibilityStatus::CheckingDisabled) => "·",
            None => "-",
        }
    }

    /// `group/name`, or just `name` for repos sitting directly in a root.
    pub fn slug(&self) -> String {
        if self.group.is_empty() {
            self.name.clone()
        } else {
            format!("{}/{}", self.group, self.name)
        }
    }

    /// When this repo was last touched in any way that counts, and by what.
    ///
    /// A clean repo's activity is its newest commit. A dirty repo's activity is
    /// when a changed file was last saved, which is usually much more recent.
    ///
    /// The index mtime is deliberately not part of this. It looks like a good
    /// signal and isn't: any tool that runs `git status` refreshes the index,
    /// so an editor or GUI client sitting open on a repo makes it read as
    /// active when nothing has actually happened. Nothing is lost by ignoring
    /// it, because staged changes still have working-tree mtimes of their own.
    pub fn activity(&self) -> (i64, ActivitySource) {
        let mut best = (0i64, ActivitySource::Unknown);
        if let Some(refs) = &self.refs {
            let commit = refs.newest_commit_at();
            if commit > best.0 {
                best = (commit, ActivitySource::Commit);
            }
        }
        if let Some(work) = &self.work {
            if let Some(m) = work.newest_mtime {
                if m > best.0 {
                    best = (m, ActivitySource::WorkingTree);
                }
            }
        }
        best
    }

    pub fn activity_at(&self) -> i64 {
        self.activity().0
    }

    pub fn flags(&self) -> Flags {
        let work = self.work.as_ref();
        let refs = self.refs.as_ref();
        Flags {
            dirty: work.map(|w| w.is_dirty()).unwrap_or(false),
            unpushed: refs.map(|r| r.unpushed() > 0).unwrap_or(false),
            conflicted: work.map(|w| w.conflicts > 0).unwrap_or(false),
            in_progress: refs.map(|r| r.operation.is_some()).unwrap_or(false),
            detached: refs
                .map(|r| matches!(r.head, Head::Detached { .. }))
                .unwrap_or(false),
            no_remote: refs.map(|r| r.remote_url.is_none()).unwrap_or(false),
            no_upstream: refs
                .map(|r| {
                    r.current_branch()
                        .map(|b| b.upstream.is_none())
                        .unwrap_or(false)
                })
                .unwrap_or(false),
            stashed: refs.map(|r| r.stashes > 0).unwrap_or(false),
            error: self.error.is_some(),
        }
    }

    /// Where this repo stands against its own release history.
    ///
    /// Uncommitted changes count as needing a release: whatever is in the
    /// working tree is not in the tag either. A tag that exists but is not
    /// reachable from HEAD also counts, because this branch has never been
    /// released even though some other one has. Orphaned tags are the
    /// exception, and read as never released: see `tags_orphaned`.
    pub fn release_state(&self) -> ReleaseState {
        let Some(refs) = self.refs.as_ref() else {
            return ReleaseState::Unreleased;
        };
        if refs.newest_tag.is_none() || refs.tags_orphaned {
            return ReleaseState::Unreleased;
        }
        let past_tag = refs.commits_since_tag.unwrap_or(0) > 0;
        let dirty = self.work.as_ref().map(|w| w.is_dirty()).unwrap_or(false);
        let unreachable_tag = refs.described_tag.is_none();
        if past_tag || dirty || unreachable_tag {
            ReleaseState::NeedsRelease
        } else {
            ReleaseState::Released
        }
    }

    pub fn dirty_total(&self) -> u32 {
        self.work.as_ref().map(|w| w.total()).unwrap_or(0)
    }

    pub fn unpushed_total(&self) -> u32 {
        self.refs.as_ref().map(|r| r.unpushed()).unwrap_or(0)
    }

    pub fn behind_total(&self) -> u32 {
        self.refs.as_ref().map(|r| r.unpulled()).unwrap_or(0)
    }

    /// Commits the checked-out branch hasn't pushed. What the AHEAD column
    /// shows, because it sits next to BRANCH and has to mean the same branch
    /// BRANCH names. Zero on a detached HEAD: there is no branch to be ahead.
    pub fn branch_unpushed(&self) -> u32 {
        self.refs
            .as_ref()
            .and_then(|r| r.current_branch())
            .map(|b| b.ahead)
            .unwrap_or(0)
    }

    /// Commits the checked-out branch's upstream has and it doesn't. The
    /// per-branch half of [`Self::behind_total`], for the same reason as
    /// [`Self::branch_unpushed`].
    pub fn branch_behind(&self) -> u32 {
        self.refs
            .as_ref()
            .and_then(|r| r.current_branch())
            .map(|b| b.behind)
            .unwrap_or(0)
    }

    /// True when a branch you don't have checked out is ahead of its upstream.
    /// The columns show the current branch, so this is what keeps a stale side
    /// branch from vanishing out of the table entirely -- it earns the cell a
    /// trailing `*`.
    pub fn other_branches_unpushed(&self) -> bool {
        self.unpushed_total() > self.branch_unpushed()
    }

    /// True when a branch you don't have checked out is behind its upstream.
    pub fn other_branches_behind(&self) -> bool {
        self.behind_total() > self.branch_behind()
    }

    /// When anything last fetched this repo, or `None` if nothing ever has.
    pub fn fetched_at(&self) -> Option<i64> {
        self.refs.as_ref().and_then(|r| r.fetched_at)
    }

    /// True when this repo has a remote worth asking about and nobody has
    /// ever asked.
    ///
    /// The distinction the "behind" column exists to draw. Zero behind reads
    /// as "in sync", and for a repo that has never fetched that is a claim
    /// nothing checked: the count is zero because there are no remote-tracking
    /// refs to compare against, not because the remote has nothing new. A repo
    /// with no remote at all is not in this state -- there is nothing to be
    /// behind.
    pub fn never_fetched(&self) -> bool {
        match self.refs.as_ref() {
            Some(refs) => refs.remote_url.is_some() && refs.fetched_at.is_none(),
            None => false,
        }
    }

    /// How many stash entries this repo has, or `None` if nothing has probed
    /// it yet.
    ///
    /// Optional rather than a bare count for the same reason CHANGES draws
    /// the distinction: zero stashes is a fact worth reporting, and a repo
    /// nobody has looked at yet is not that fact. A bare repo does come back
    /// `Some(0)` -- the stash reflog is read straight off disk, so its absence
    /// there is a real answer rather than a missing one.
    pub fn stash_count(&self) -> Option<u32> {
        self.refs.as_ref().map(|r| r.stashes)
    }

    pub fn commits_since_tag(&self) -> u32 {
        self.refs
            .as_ref()
            .and_then(|r| r.commits_since_tag)
            .unwrap_or(0)
    }

    pub fn branch_label(&self) -> String {
        self.refs
            .as_ref()
            .map(|r| r.head.label())
            .unwrap_or_else(|| "?".into())
    }

    pub fn tag_label(&self) -> String {
        let Some(refs) = self.refs.as_ref() else {
            return "-".into();
        };
        match refs.described_tag.as_ref() {
            Some(t) => t.name.clone(),
            // An orphaned tag is not this repo's version, so showing it would
            // only mislead.
            None if refs.tags_orphaned => "-".into(),
            None => refs
                .newest_tag
                .as_ref()
                .map(|t| format!("({})", t.name))
                .unwrap_or_else(|| "-".into()),
        }
    }

    /// Short one-word state used in the table's STATE column.
    pub fn state_label(&self) -> &'static str {
        let f = self.flags();
        if f.error {
            "error"
        } else if f.conflicted {
            "conflict"
        } else if f.in_progress {
            self.refs
                .as_ref()
                .and_then(|r| r.operation)
                .map(|o| o.label())
                .unwrap_or("in progress")
        } else if f.dirty {
            "dirty"
        } else if f.unpushed {
            "unpushed"
        } else if self.is_bare() {
            // Checked after `unpushed`, because a bare repo holding branches
            // its remote hasn't seen is still worth saying so about. But it
            // has no working tree to be dirty and never runs tier 2, so the
            // "not scanned yet" ellipsis below would be permanent here.
            "bare"
        } else if self.work.is_none() {
            "…"
        } else {
            "clean"
        }
    }

    /// Whether this is a bare repo — no working tree, so nothing to scan for
    /// uncommitted changes and nothing `git status` can be asked about. Most
    /// often the container of a bare-plus-worktrees layout, whose worktrees
    /// are discovered as repos in their own right.
    pub fn is_bare(&self) -> bool {
        self.refs.as_ref().map(|r| r.is_bare).unwrap_or(false)
    }
}
