//! Which columns the table shows, in what order, and how wide each one is.
//!
//! One place, because a column is four things that have to agree: a header, an
//! alignment, a width, and a cell renderer. Those used to live in two parallel
//! `const` arrays in [`crate::report`] plus a struct of `usize` fields in
//! [`crate::tui::ui`], edited in lockstep, and adding VISIBILITY that way
//! silently cost the repo-name column fifteen characters on every terminal
//! narrower than about 140. Here, a column is one variant and the width
//! arithmetic is visible in one function.

use serde::{Deserialize, Serialize};

use crate::fmt;
use crate::model::RepoStatus;
use crate::paths;
use crate::report::Align;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Column {
    Group,
    Repo,
    Branch,
    State,
    Release,
    Visibility,
    /// The same value as [`Visibility`](Column::Visibility), rendered as just
    /// its marker. Once you know the glyphs the words are redundant, and this
    /// buys back ten characters of every row for the repo name.
    VisibilityShort,
    Changes,
    /// How many stash entries are parked on this repo.
    Stashes,
    Ahead,
    Behind,
    /// How long since anything fetched this repo. The freshness date on the
    /// BEHIND column beside it.
    Fetched,
    Tag,
    SinceTag,
    Age,
}

/// How a column claims horizontal space in the dashboard. The CLI table sizes
/// itself to its content instead, so this only matters to the TUI.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Width {
    /// Always this many characters, whatever else is on screen.
    Fixed(usize),
    /// Sized to the branch names actually listed, within a floor and a cap.
    Branch,
    /// Absorbs whatever the fixed columns leave behind.
    Fill,
}

impl Column {
    /// Every column, in the order they're laid out by default.
    pub fn all() -> &'static [Column] {
        &[
            Column::Group,
            Column::Repo,
            Column::Branch,
            Column::State,
            Column::Release,
            Column::Visibility,
            Column::VisibilityShort,
            Column::Changes,
            Column::Stashes,
            Column::Ahead,
            Column::Behind,
            Column::Fetched,
            Column::Tag,
            Column::SinceTag,
            Column::Age,
        ]
    }

    /// What's shown when nothing is configured. VISIBILITY is in here only
    /// when checking is actually on: it costs real width on every row, and
    /// with checking off every cell would read "checking off" forever.
    ///
    /// Every other column is free — it's already been probed — but free to
    /// compute isn't free to show. FETCHED and STASH are held back too, not
    /// because they cost anything to know but because on most fleets they'd
    /// read the same on nearly every row while charging the repo name for
    /// the privilege.
    pub fn defaults(visibility_enabled: bool) -> Vec<Column> {
        Column::all()
            .iter()
            .copied()
            // The short form is opt-in: someone has to have learnt the
            // glyphs before a column of bare glyphs is an improvement.
            .filter(|c| *c != Column::VisibilityShort)
            .filter(|c| *c != Column::Visibility || visibility_enabled)
            // FETCHED is opt-in too: BEHIND already says `?` when nothing has
            // ever fetched, which is the part you have to know. The exact age
            // of the last fetch is for people who want to watch it, and it's
            // in the detail pane for everyone else.
            .filter(|c| *c != Column::Fetched)
            // STASH is opt-in on the same reasoning: most repos have no
            // stashes, so on most fleets it would be a column of `·` charging
            // six characters a row to the repo name. The people who stash
            // across a tree know they do, and `C` is where they say so.
            .filter(|c| *c != Column::Stashes)
            .collect()
    }

    /// The name used in `[ui] columns` and in the picker.
    pub fn key(&self) -> &'static str {
        match self {
            Column::Group => "group",
            Column::Repo => "repo",
            Column::Branch => "branch",
            Column::State => "state",
            Column::Release => "release",
            Column::Visibility => "visibility",
            Column::VisibilityShort => "visibility_short",
            Column::Changes => "changes",
            Column::Stashes => "stashes",
            Column::Ahead => "ahead",
            Column::Behind => "behind",
            Column::Fetched => "fetched",
            Column::Tag => "tag",
            Column::SinceTag => "since_tag",
            Column::Age => "age",
        }
    }

    /// The table header. REPO becomes PATH when full paths are being shown.
    pub fn header(&self, show_paths: bool) -> &'static str {
        match self {
            Column::Group => "GROUP",
            Column::Repo => {
                if show_paths {
                    "PATH"
                } else {
                    "REPO"
                }
            }
            Column::Branch => "BRANCH",
            Column::State => "STATE",
            Column::Release => "RELEASE",
            Column::Visibility => "VISIBILITY",
            Column::VisibilityShort => "VIS",
            Column::Changes => "CHANGES",
            Column::Stashes => "STASH",
            Column::Ahead => "AHEAD",
            Column::Behind => "BEHIND",
            Column::Fetched => "FETCHED",
            Column::Tag => "TAG",
            Column::SinceTag => "+TAG",
            Column::Age => "AGE",
        }
    }

    /// One line for the picker, saying what the column is actually for.
    pub fn describe(&self) -> &'static str {
        match self {
            Column::Group => "first path segment below the scan root",
            Column::Repo => "repo name, or full path with --paths",
            Column::Branch => "current branch, or the detached HEAD",
            Column::State => "dirty, unpushed, conflict, bare, clean",
            Column::Release => "whether there's work past the newest tag",
            Column::Visibility => "public or private, asked of the host via `gh`",
            Column::VisibilityShort => "the same, as just its marker: ● public, ⊘ private",
            Column::Changes => "staged, unstaged and untracked counts",
            Column::Stashes => "how many stash entries are parked here",
            Column::Ahead => "commits this branch hasn't pushed, * if another branch has some too",
            Column::Behind => {
                "commits the upstream has and this branch doesn't, ? if never fetched"
            }
            Column::Fetched => "how long since anything fetched this repo",
            Column::Tag => "the newest tag",
            Column::SinceTag => "commits since that tag",
            Column::Age => "time since the last activity",
        }
    }

    pub fn align(&self) -> Align {
        match self {
            Column::Ahead
            | Column::Behind
            | Column::SinceTag
            | Column::Age
            | Column::Fetched
            | Column::Stashes => Align::Right,
            _ => Align::Left,
        }
    }

    pub fn width(&self) -> Width {
        match self {
            Column::Group => Width::Fixed(14),
            // Absorbs the leftover, so the right-hand numbers stay put as the
            // terminal resizes.
            Column::Repo => Width::Fill,
            Column::Branch => Width::Branch,
            Column::State => Width::Fixed(12),
            // "◆ needs release" is 15 wide, plus a space before the next.
            Column::Release => Width::Fixed(16),
            // "· check failed" is 14, wider than the header, so 15 leaves a
            // gap before CHANGES.
            Column::Visibility => Width::Fixed(15),
            // Just the header's own width plus a single-space gutter: the
            // marker underneath is one character.
            Column::VisibilityShort => Width::Fixed(4),
            Column::Changes => Width::Fixed(12),
            // The header's own five characters plus a single-space gutter.
            // Nobody has a five-digit stash.
            Column::Stashes => Width::Fixed(6),
            Column::Ahead => Width::Fixed(6),
            Column::Behind => Width::Fixed(7),
            // "never" is 5, the header is 7, and one more leaves a gutter.
            Column::Fetched => Width::Fixed(8),
            Column::Tag => Width::Fixed(14),
            Column::SinceTag => Width::Fixed(5),
            Column::Age => Width::Fixed(5),
        }
    }

    /// Whether this column can be turned off. Only REPO can't: a table of
    /// rows you can't identify isn't worth rendering.
    pub fn toggleable(&self) -> bool {
        *self != Column::Repo
    }

    /// The cell for the plain-text table. The dashboard renders its own,
    /// because it colours and marks cells this can't.
    pub fn cell(&self, repo: &RepoStatus, now: i64, show_paths: bool) -> String {
        match self {
            Column::Group => {
                if repo.group.is_empty() {
                    "·".into()
                } else {
                    repo.group.clone()
                }
            }
            Column::Repo => {
                if show_paths {
                    paths::contract(&repo.root)
                } else {
                    repo.name.clone()
                }
            }
            Column::Branch => fmt::truncate(&repo.branch_label(), 24),
            Column::State => repo.state_label().to_string(),
            Column::Release => repo.release_state().label().to_string(),
            Column::Visibility => repo.visibility_label().to_string(),
            Column::VisibilityShort => repo.visibility_marker().to_string(),
            Column::Changes => repo
                .work
                .as_ref()
                .map(|w| fmt::changes(w.staged, w.unstaged, w.untracked, w.conflicts))
                // `?` means "not scanned yet". A bare repo has nothing to
                // scan, which is a different thing and reads as `·` like
                // every other known-nothing in this table.
                .unwrap_or_else(|| {
                    if repo.is_bare() {
                        "·".into()
                    } else {
                        "?".into()
                    }
                }),
            // `?` means "not probed yet", the same as CHANGES. A bare repo
            // isn't in that state: its stash reflog is read off disk like
            // anyone else's, and simply isn't there, which is a real zero.
            Column::Stashes => match repo.stash_count() {
                Some(n) => fmt::count(n),
                None => "?".into(),
            },
            // The checked-out branch, not every branch summed: this cell sits
            // next to BRANCH and has to mean the branch BRANCH names. `*`
            // marks a repo where some *other* local branch is unpushed too,
            // so a stale side branch still shows up in the table.
            Column::Ahead => fmt::marked(
                fmt::count(repo.branch_unpushed()),
                repo.other_branches_unpushed(),
            ),
            // `?` rather than `·` when nothing has ever fetched: the count is
            // zero because there was nothing to compare against, not because
            // the remote has nothing new. Same reading as CHANGES' `?`.
            Column::Behind => {
                if repo.behind_total() == 0 && repo.never_fetched() {
                    "?".into()
                } else {
                    fmt::marked(
                        fmt::count(repo.branch_behind()),
                        repo.other_branches_behind(),
                    )
                }
            }
            Column::Fetched => fetched_label(repo, now),
            Column::Tag => fmt::truncate(&repo.tag_label(), 18),
            Column::SinceTag => fmt::count(repo.commits_since_tag()),
            Column::Age => fmt::age(repo.activity_at(), now),
        }
    }
}

/// The FETCHED cell, and the same wording the detail pane uses. `never` is a
/// worse state than an old fetch, not a missing value, so it gets a word
/// rather than the `·` that means "nothing to say here" -- which is what a
/// repo with no remote does get.
pub fn fetched_label(repo: &RepoStatus, now: i64) -> String {
    match repo.fetched_at() {
        Some(at) => fmt::age(at, now),
        None if repo.never_fetched() => "never".into(),
        None => "·".into(),
    }
}

impl std::str::FromStr for Column {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let key = s.trim().to_ascii_lowercase().replace(['-', ' '], "_");
        Column::all()
            .iter()
            .find(|c| c.key() == key)
            // `+tag` is what the header says, so accept it as well as the
            // `since_tag` the config uses.
            .or_else(|| {
                if key == "+tag" {
                    Some(&Column::SinceTag)
                } else {
                    None
                }
            })
            .copied()
            .ok_or_else(|| format!("unknown column {s:?}"))
    }
}

/// Clean up a configured list: drop anything listed twice, collapse the two
/// visibility forms to one, and put REPO back if it's missing. A list is someone's explicit choice, so it's honoured as
/// written otherwise — including the order, and including leaving out columns
/// the defaults would have shown.
pub fn sanitise(mut columns: Vec<Column>) -> Vec<Column> {
    let mut seen = std::collections::HashSet::new();
    columns.retain(|c| seen.insert(*c));
    // The two visibility forms are the same column twice. Whichever was asked
    // for first wins; showing both would render the value beside itself.
    let is_visibility = |c: &Column| matches!(c, Column::Visibility | Column::VisibilityShort);
    if let Some(first) = columns.iter().copied().find(is_visibility) {
        columns.retain(|c| !is_visibility(c) || *c == first);
    }
    if !columns.contains(&Column::Repo) {
        // Restore it in its canonical position rather than at the front, so a
        // list that simply forgot it still reads the way it was meant to.
        let at = columns
            .iter()
            .position(|c| *c == Column::Branch)
            .unwrap_or(0);
        columns.insert(at, Column::Repo);
    }
    columns
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_round_trip_through_parsing() {
        for column in Column::all() {
            assert_eq!(column.key().parse::<Column>().as_ref(), Ok(column));
        }
    }

    #[test]
    fn the_header_spelling_of_since_tag_parses_too() {
        assert_eq!("+tag".parse::<Column>(), Ok(Column::SinceTag));
        assert_eq!("since-tag".parse::<Column>(), Ok(Column::SinceTag));
    }

    #[test]
    fn an_unknown_column_is_rejected_by_name() {
        assert!("nonsense"
            .parse::<Column>()
            .unwrap_err()
            .contains("nonsense"));
    }

    // The whole point of the change: with checking off, every VISIBILITY cell
    // would read "checking off" while costing fifteen characters of every row.
    #[test]
    fn visibility_is_absent_from_the_defaults_until_it_is_enabled() {
        assert!(!Column::defaults(false).contains(&Column::Visibility));
        assert!(Column::defaults(true).contains(&Column::Visibility));
    }

    #[test]
    fn every_other_column_shows_by_default_either_way() {
        for enabled in [false, true] {
            let shown = Column::defaults(enabled);
            for column in Column::all() {
                // Both visibility forms are conditional: the long one on the
                // flag, the short one on being asked for. FETCHED is opt-in
                // for the same reason as the short form — BEHIND's `?` is
                // what you have to know, and this is the detail behind it.
                // STASH is opt-in because most repos have none.
                if matches!(
                    column,
                    Column::Visibility
                        | Column::VisibilityShort
                        | Column::Fetched
                        | Column::Stashes
                ) {
                    continue;
                }
                assert!(shown.contains(column), "{} missing", column.key());
            }
        }
    }

    // The distinction the column exists to draw: a zero that was checked
    // against a remote reads as `·`, and a zero that never was reads as `?`.
    #[test]
    fn behind_says_unknown_until_something_has_fetched() {
        let mut repo = RepoStatus::new("/tmp/x".into(), "g".into(), "r".into());
        repo.refs = Some(crate::model::RefsInfo {
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
            remote_url: Some("git@github.com:owner/repo.git".into()),
            changelog: None,
            is_bare: false,
            is_shallow: false,
        });
        assert_eq!(Column::Behind.cell(&repo, 0, false), "?");
        assert_eq!(Column::Fetched.cell(&repo, 0, false), "never");

        // Fetched, and genuinely in sync.
        let refs = repo.refs.as_mut().unwrap();
        refs.fetched_at = Some(1_000);
        assert_eq!(Column::Behind.cell(&repo, 1_000, false), "·");

        // No remote at all: nothing to be behind, and nothing to fetch, so
        // neither column claims otherwise.
        repo.refs.as_mut().unwrap().remote_url = None;
        repo.refs.as_mut().unwrap().fetched_at = None;
        assert_eq!(Column::Behind.cell(&repo, 0, false), "·");
        assert_eq!(Column::Fetched.cell(&repo, 0, false), "·");
    }

    // The bug this pair of columns used to have: BRANCH named the checked-out
    // branch while AHEAD and BEHIND summed every branch, so a stale side
    // branch made a perfectly up-to-date checkout read as 128 behind.
    #[test]
    fn ahead_and_behind_report_the_checked_out_branch() {
        let mut repo = RepoStatus::new("/tmp/x".into(), "g".into(), "r".into());
        repo.refs = Some(crate::model::RefsInfo {
            head: crate::model::Head::Branch("master".into()),
            branches: vec![
                branch("master", 0, 0),
                // A month-old topic branch, left tracking origin/master.
                branch("topic", 2, 128),
            ],
            last_commit: None,
            stashes: 0,
            operation: None,
            newest_tag: None,
            described_tag: None,
            commits_since_tag: None,
            since_tag_subjects: Vec::new(),
            tags_orphaned: false,
            index_mtime: None,
            fetched_at: Some(1_000),
            remote_url: Some("git@github.com:owner/repo.git".into()),
            changelog: None,
            is_bare: false,
            is_shallow: false,
        });

        // master is in sync, and says so -- with `*` so the topic branch
        // isn't silently dropped from the table.
        assert_eq!(Column::Behind.cell(&repo, 1_000, false), "·*");
        assert_eq!(Column::Ahead.cell(&repo, 1_000, false), "·*");
        // The repo-wide sums are still there for the filters and sorts.
        assert_eq!(repo.behind_total(), 128);
        assert_eq!(repo.unpushed_total(), 2);

        // Check out the topic branch and the same numbers surface, unmarked:
        // there is no *other* branch with anything of its own now.
        repo.refs.as_mut().unwrap().head = crate::model::Head::Branch("topic".into());
        assert_eq!(Column::Behind.cell(&repo, 1_000, false), "128");
        assert_eq!(Column::Ahead.cell(&repo, 1_000, false), "2");

        // Detached HEAD has no branch to be ahead or behind, but the marker
        // still points at the branches that do.
        repo.refs.as_mut().unwrap().head = crate::model::Head::Detached {
            sha: "abc1234".into(),
        };
        assert_eq!(Column::Behind.cell(&repo, 1_000, false), "·*");
        assert_eq!(Column::Ahead.cell(&repo, 1_000, false), "·*");
    }

    fn branch(name: &str, ahead: u32, behind: u32) -> crate::model::BranchInfo {
        crate::model::BranchInfo {
            name: name.into(),
            upstream: Some("origin/master".into()),
            ahead,
            behind,
            gone: false,
            committed_at: 1_000,
            sha: "abc1234".into(),
            subject: "s".into(),
        }
    }

    // Six characters off every repo name to tell most fleets, on most rows,
    // that nothing is stashed.
    #[test]
    fn stash_is_never_a_default_either_way() {
        for enabled in [false, true] {
            assert!(!Column::defaults(enabled).contains(&Column::Stashes));
        }
        assert!(Column::all().contains(&Column::Stashes));
    }

    // The same distinction CHANGES draws: a zero somebody checked reads `·`,
    // and a repo nobody has looked at yet reads `?`. A bare repo is checked
    // like any other -- its stash reflog just isn't there -- so it gets the
    // real zero rather than the shrug.
    #[test]
    fn stash_says_unknown_only_until_something_has_probed() {
        let mut repo = RepoStatus::new("/tmp/x".into(), "g".into(), "r".into());
        assert_eq!(Column::Stashes.cell(&repo, 0, false), "?");
        assert_eq!(repo.stash_count(), None);

        repo.refs = Some(crate::model::RefsInfo {
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
            remote_url: None,
            changelog: None,
            is_bare: true,
            is_shallow: false,
        });
        assert_eq!(Column::Stashes.cell(&repo, 0, false), "·");

        repo.refs.as_mut().unwrap().stashes = 3;
        assert_eq!(Column::Stashes.cell(&repo, 0, false), "3");
    }

    // Right-aligned like every other count, and no wider than its own header
    // plus a gutter.
    #[test]
    fn stash_is_a_narrow_right_aligned_count() {
        assert_eq!(Column::Stashes.header(false), "STASH");
        assert_eq!(Column::Stashes.width(), Width::Fixed(6));
        assert!(matches!(Column::Stashes.align(), Align::Right));
        assert_eq!("stashes".parse::<Column>(), Ok(Column::Stashes));
    }

    // The short form is a column of bare glyphs -- worth having, but only
    // once you've chosen it.
    #[test]
    fn the_short_visibility_form_is_never_a_default() {
        for enabled in [false, true] {
            assert!(!Column::defaults(enabled).contains(&Column::VisibilityShort));
        }
    }

    // They're the same value twice; showing both would render it beside
    // itself. Whichever was asked for first wins.
    #[test]
    fn the_two_visibility_forms_collapse_to_one() {
        let cleaned = sanitise(vec![
            Column::Repo,
            Column::Visibility,
            Column::VisibilityShort,
        ]);
        assert_eq!(cleaned, vec![Column::Repo, Column::Visibility]);
        let cleaned = sanitise(vec![
            Column::Repo,
            Column::VisibilityShort,
            Column::Visibility,
        ]);
        assert_eq!(cleaned, vec![Column::Repo, Column::VisibilityShort]);
    }

    #[test]
    fn the_short_form_is_narrower_than_its_own_header_allows_for() {
        assert_eq!(Column::VisibilityShort.header(false), "VIS");
        assert_eq!(Column::VisibilityShort.width(), Width::Fixed(4));
        assert_eq!(Column::Visibility.width(), Width::Fixed(15));
    }

    #[test]
    fn duplicates_are_dropped_and_order_is_kept() {
        let cleaned = sanitise(vec![Column::Age, Column::Repo, Column::Age, Column::State]);
        assert_eq!(cleaned, vec![Column::Age, Column::Repo, Column::State]);
    }

    // A table of rows you can't tell apart isn't worth rendering, so REPO
    // comes back even if the config leaves it out.
    #[test]
    fn repo_is_restored_when_a_configured_list_omits_it() {
        let cleaned = sanitise(vec![Column::Group, Column::Branch, Column::Age]);
        assert_eq!(
            cleaned,
            vec![Column::Group, Column::Repo, Column::Branch, Column::Age]
        );
        assert!(!Column::Repo.toggleable());
    }
}
