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
    Changes,
    Ahead,
    Behind,
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
            Column::Changes,
            Column::Ahead,
            Column::Behind,
            Column::Tag,
            Column::SinceTag,
            Column::Age,
        ]
    }

    /// What's shown when nothing is configured. VISIBILITY is in here only
    /// when checking is actually on: it costs real width on every row, and
    /// with checking off every cell would read "checking off" forever. Every
    /// other column is free — it's already been probed.
    pub fn defaults(visibility_enabled: bool) -> Vec<Column> {
        Column::all()
            .iter()
            .copied()
            .filter(|c| *c != Column::Visibility || visibility_enabled)
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
            Column::Changes => "changes",
            Column::Ahead => "ahead",
            Column::Behind => "behind",
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
            Column::Changes => "CHANGES",
            Column::Ahead => "AHEAD",
            Column::Behind => "BEHIND",
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
            Column::Changes => "staged, unstaged and untracked counts",
            Column::Ahead => "commits not pushed to the upstream",
            Column::Behind => "commits on the upstream and not here",
            Column::Tag => "the newest tag",
            Column::SinceTag => "commits since that tag",
            Column::Age => "time since the last activity",
        }
    }

    pub fn align(&self) -> Align {
        match self {
            Column::Ahead | Column::Behind | Column::SinceTag | Column::Age => Align::Right,
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
            // "· checking off" and "· check failed" are 14, wider than the
            // header, so 15 leaves a gap before CHANGES.
            Column::Visibility => Width::Fixed(15),
            Column::Changes => Width::Fixed(12),
            Column::Ahead => Width::Fixed(6),
            Column::Behind => Width::Fixed(7),
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
            Column::Ahead => fmt::count(repo.unpushed_total()),
            Column::Behind => fmt::count(repo.behind_total()),
            Column::Tag => fmt::truncate(&repo.tag_label(), 18),
            Column::SinceTag => fmt::count(repo.commits_since_tag()),
            Column::Age => fmt::age(repo.activity_at(), now),
        }
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

/// Clean up a configured list: drop anything listed twice, and put REPO back
/// if it's missing. A list is someone's explicit choice, so it's honoured as
/// written otherwise — including the order, and including leaving out columns
/// the defaults would have shown.
pub fn sanitise(mut columns: Vec<Column>) -> Vec<Column> {
    let mut seen = std::collections::HashSet::new();
    columns.retain(|c| seen.insert(*c));
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
                if *column == Column::Visibility {
                    continue;
                }
                assert!(shown.contains(column), "{} missing", column.key());
            }
        }
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
