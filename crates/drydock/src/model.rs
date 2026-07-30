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
    pub index_mtime: Option<i64>,
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
    /// released even though some other one has.
    pub fn release_state(&self) -> ReleaseState {
        let Some(refs) = self.refs.as_ref() else {
            return ReleaseState::Unreleased;
        };
        if refs.newest_tag.is_none() {
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
        match self.refs.as_ref().and_then(|r| r.described_tag.as_ref()) {
            Some(t) => t.name.clone(),
            None => self
                .refs
                .as_ref()
                .and_then(|r| r.newest_tag.as_ref())
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
        } else if self.work.is_none() {
            "…"
        } else {
            "clean"
        }
    }
}
