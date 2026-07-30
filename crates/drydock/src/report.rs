//! Plain-text and JSON output for the non-interactive commands.

use anyhow::Result;
use serde::Serialize;

use crate::fmt;
use crate::model::{ChangeKind, ReleaseState, RepoStatus};
use crate::paths;
use crate::probe::Timings;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Left,
    Right,
}

/// Minimal column-aligned table. Widths come from the content, so output stays
/// readable whether there are three rows or five hundred.
pub fn table(headers: &[&str], aligns: &[Align], rows: &[Vec<String>]) -> String {
    let cols = headers.len();
    let mut widths: Vec<usize> = headers.iter().map(|h| h.chars().count()).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate().take(cols) {
            widths[i] = widths[i].max(cell.chars().count());
        }
    }

    let mut out = String::new();
    let render = |out: &mut String, cells: &[String]| {
        let mut line = String::new();
        for (i, cell) in cells.iter().enumerate().take(cols) {
            let pad = widths[i].saturating_sub(cell.chars().count());
            if aligns.get(i).copied().unwrap_or(Align::Left) == Align::Right {
                line.push_str(&" ".repeat(pad));
                line.push_str(cell);
            } else {
                line.push_str(cell);
                // No trailing padding on the last column.
                if i + 1 < cols.min(cells.len()) {
                    line.push_str(&" ".repeat(pad));
                }
            }
            if i + 1 < cols {
                line.push_str("  ");
            }
        }
        out.push_str(line.trim_end());
        out.push('\n');
    };

    let header_cells: Vec<String> = headers.iter().map(|h| h.to_string()).collect();
    render(&mut out, &header_cells);
    for row in rows {
        render(&mut out, row);
    }
    out
}

const LIST_HEADERS: &[&str] = &[
    "GROUP", "REPO", "BRANCH", "STATE", "RELEASE", "CHANGES", "AHEAD", "BEHIND", "TAG", "+TAG",
    "AGE",
];

const LIST_ALIGNS: &[Align] = &[
    Align::Left,
    Align::Left,
    Align::Left,
    Align::Left,
    Align::Left,
    Align::Left,
    Align::Right,
    Align::Right,
    Align::Left,
    Align::Right,
    Align::Right,
];

pub fn list_table(repos: &[&RepoStatus], now: i64, show_paths: bool) -> String {
    let rows: Vec<Vec<String>> = repos
        .iter()
        .map(|r| {
            let work = r.work.as_ref();
            let changes = work
                .map(|w| fmt::changes(w.staged, w.unstaged, w.untracked, w.conflicts))
                .unwrap_or_else(|| "?".into());
            let repo_cell = if show_paths {
                paths::contract(&r.root)
            } else {
                r.name.clone()
            };
            vec![
                if r.group.is_empty() {
                    "·".into()
                } else {
                    r.group.clone()
                },
                repo_cell,
                fmt::truncate(&r.branch_label(), 24),
                r.state_label().to_string(),
                r.release_state().label().to_string(),
                changes,
                fmt::count(r.unpushed_total()),
                fmt::count(r.behind_total()),
                fmt::truncate(&r.tag_label(), 18),
                fmt::count(r.commits_since_tag()),
                fmt::age(r.activity_at(), now),
            ]
        })
        .collect();

    let mut headers: Vec<&str> = LIST_HEADERS.to_vec();
    if show_paths {
        headers[1] = "PATH";
    }
    table(&headers, LIST_ALIGNS, &rows)
}

/// The one-line summary printed under a table or in the dashboard header.
pub fn summary(repos: &[RepoStatus], timings: Option<&Timings>) -> String {
    let dirty = repos.iter().filter(|r| r.flags().dirty).count();
    let unpushed = repos.iter().filter(|r| r.flags().unpushed).count();
    let needs_release = repos
        .iter()
        .filter(|r| r.release_state() == ReleaseState::NeedsRelease)
        .count();
    let never = repos
        .iter()
        .filter(|r| r.release_state() == ReleaseState::Unreleased)
        .count();
    let attention = repos
        .iter()
        .filter(|r| !r.flags().clean() || r.release_state() == ReleaseState::NeedsRelease)
        .count();
    let errors = repos.iter().filter(|r| r.error.is_some()).count();

    let mut parts = vec![
        format!("{} repos", repos.len()),
        format!("{dirty} dirty"),
        format!("{unpushed} unpushed"),
        format!("{needs_release} need release"),
        format!("{never} unreleased"),
        format!("{attention} need attention"),
    ];
    if errors > 0 {
        parts.push(format!("{errors} errored"));
    }
    if let Some(t) = timings {
        let cache_note = if t.work_cached > 0 {
            format!(", {} from cache", t.work_cached)
        } else {
            String::new()
        };
        parts.push(format!(
            "scanned in {} (walk {}, refs {}{})",
            fmt::duration(t.total),
            fmt::duration(t.discovery),
            fmt::duration(t.refs),
            cache_note
        ));
    }
    parts.join(" · ")
}

pub fn groups_table(repos: &[RepoStatus]) -> String {
    use std::collections::BTreeMap;

    #[derive(Default)]
    struct Tally {
        total: usize,
        dirty: usize,
        unpushed: usize,
        needs_release: usize,
        unreleased: usize,
    }

    let mut groups: BTreeMap<String, Tally> = BTreeMap::new();
    for repo in repos {
        let key = if repo.group.is_empty() {
            "·".to_string()
        } else {
            repo.group.clone()
        };
        let entry = groups.entry(key).or_default();
        let f = repo.flags();
        entry.total += 1;
        entry.dirty += f.dirty as usize;
        entry.unpushed += f.unpushed as usize;
        entry.needs_release += (repo.release_state() == ReleaseState::NeedsRelease) as usize;
        entry.unreleased += (repo.release_state() == ReleaseState::Unreleased) as usize;
    }

    let rows: Vec<Vec<String>> = groups
        .iter()
        .map(|(name, t)| {
            vec![
                name.clone(),
                t.total.to_string(),
                t.dirty.to_string(),
                t.unpushed.to_string(),
                t.needs_release.to_string(),
                t.unreleased.to_string(),
            ]
        })
        .collect();

    table(
        &[
            "GROUP",
            "REPOS",
            "DIRTY",
            "UNPUSHED",
            "NEEDS RELEASE",
            "UNRELEASED",
        ],
        &[
            Align::Left,
            Align::Right,
            Align::Right,
            Align::Right,
            Align::Right,
            Align::Right,
        ],
        &rows,
    )
}

/// Everything known about one repo, as text.
pub fn detail(repo: &RepoStatus, now: i64) -> String {
    let mut out = String::new();
    let (activity_at, source) = repo.activity();

    out.push_str(&format!("{}\n", repo.slug()));
    out.push_str(&format!("  path         {}\n", paths::contract(&repo.root)));
    out.push_str(&format!("  state        {}\n", repo.state_label()));
    out.push_str(&format!(
        "  release      {}\n",
        repo.release_state().label()
    ));
    out.push_str(&format!(
        "  activity     {} ago ({})\n",
        fmt::age(activity_at, now),
        source.label()
    ));

    if let Some(refs) = &repo.refs {
        out.push_str(&format!("  head         {}\n", refs.head.label()));
        if let Some(url) = &refs.remote_url {
            out.push_str(&format!("  remote       {url}\n"));
        } else {
            out.push_str("  remote       (none)\n");
        }
        if refs.stashes > 0 {
            out.push_str(&format!("  stashes      {}\n", refs.stashes));
        }
        if let Some(op) = refs.operation {
            out.push_str(&format!("  in progress  {}\n", op.label()));
        }
        if refs.is_shallow {
            out.push_str("  shallow      yes\n");
        }

        match (&refs.described_tag, refs.commits_since_tag) {
            (Some(tag), Some(count)) => {
                out.push_str(&format!(
                    "  last tag     {} ({} ago), {count} commit{} since\n",
                    tag.name,
                    fmt::age(tag.at, now),
                    if count == 1 { "" } else { "s" }
                ));
            }
            _ => out.push_str("  last tag     (none reachable)\n"),
        }
        if refs.tag_off_branch() {
            if let Some(newest) = &refs.newest_tag {
                out.push_str(&format!(
                    "  newest tag   {} is not an ancestor of HEAD (normal with git-flow)\n",
                    newest.name
                ));
            }
        }
        if let Some(cl) = &refs.changelog {
            let note = if cl.tagged {
                "matches a tag".to_string()
            } else if cl.unreleased_blocks > 1 {
                format!(
                    "no tag yet, and {} unreleased blocks have stacked up",
                    cl.unreleased_blocks
                )
            } else {
                "no tag yet".to_string()
            };
            out.push_str(&format!("  changelog    {} ({note})\n", cl.version));
        }

        if !refs.branches.is_empty() {
            out.push_str("\n  branches\n");
            let mut branches: Vec<_> = refs.branches.iter().collect();
            branches.sort_by_key(|b| std::cmp::Reverse(b.committed_at));
            let rows: Vec<Vec<String>> = branches
                .iter()
                .take(20)
                .map(|b| {
                    let tracking = match (&b.upstream, b.gone) {
                        (_, true) => "upstream gone".to_string(),
                        (None, _) => "no upstream".to_string(),
                        (Some(u), _) => {
                            let mut s = u.clone();
                            if b.ahead > 0 {
                                s.push_str(&format!(" ↑{}", b.ahead));
                            }
                            if b.behind > 0 {
                                s.push_str(&format!(" ↓{}", b.behind));
                            }
                            s
                        }
                    };
                    vec![
                        b.name.clone(),
                        tracking,
                        fmt::age(b.committed_at, now),
                        fmt::truncate(&b.subject, 60),
                    ]
                })
                .collect();
            out.push_str(&indent(
                &table(
                    &["BRANCH", "TRACKING", "AGE", "SUBJECT"],
                    &[Align::Left, Align::Left, Align::Right, Align::Left],
                    &rows,
                ),
                4,
            ));
        }

        if !refs.since_tag_subjects.is_empty() {
            out.push_str(&format!(
                "\n  commits since {}\n",
                refs.described_tag
                    .as_ref()
                    .map(|t| t.name.as_str())
                    .unwrap_or("last tag")
            ));
            for subject in &refs.since_tag_subjects {
                out.push_str(&format!("    · {}\n", fmt::truncate(subject, 100)));
            }
        }
    }

    if let Some(work) = &repo.work {
        out.push_str(&format!(
            "\n  changes      {}\n",
            fmt::changes(work.staged, work.unstaged, work.untracked, work.conflicts)
        ));
        if !work.files.is_empty() {
            for file in work.files.iter().take(40) {
                let marker = match file.kind {
                    ChangeKind::Staged => "+",
                    ChangeKind::Unstaged => "~",
                    ChangeKind::Untracked => "?",
                    ChangeKind::Conflicted => "!",
                };
                out.push_str(&format!("    {marker} {}\n", file.path));
            }
            if work.truncated || work.files.len() > 40 {
                out.push_str("    … more\n");
            }
        }
    } else {
        out.push_str("\n  changes      (working tree not scanned)\n");
    }

    if let Some(err) = &repo.error {
        out.push_str(&format!("\n  error        {err}\n"));
    }
    out
}

fn indent(text: &str, spaces: usize) -> String {
    let pad = " ".repeat(spaces);
    text.lines()
        .map(|l| {
            if l.is_empty() {
                String::from("\n")
            } else {
                format!("{pad}{l}\n")
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// JSON
// ---------------------------------------------------------------------------

/// A flattened view for scripting. Derived values are computed here rather than
/// left for the consumer to work out.
#[derive(Serialize)]
pub struct RepoView<'a> {
    pub path: &'a std::path::Path,
    pub group: &'a str,
    pub name: &'a str,
    pub slug: String,
    pub state: &'a str,
    pub release_state: &'static str,
    pub branch: String,
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    pub staged: u32,
    pub unstaged: u32,
    pub untracked: u32,
    pub conflicts: u32,
    pub stashes: u32,
    pub operation: Option<&'static str>,
    pub last_tag: Option<String>,
    pub newest_tag: Option<String>,
    pub tag_off_branch: bool,
    pub commits_since_tag: u32,
    pub changelog_version: Option<String>,
    pub changelog_untagged: bool,
    pub remote_url: Option<String>,
    pub activity_at: i64,
    pub activity_source: &'static str,
    pub age: String,
    pub work_scanned: bool,
    pub error: Option<&'a str>,
    pub flags: Vec<&'static str>,
}

pub fn view<'a>(repo: &'a RepoStatus, now: i64) -> RepoView<'a> {
    let refs = repo.refs.as_ref();
    let work = repo.work.as_ref();
    let (activity_at, source) = repo.activity();
    let f = repo.flags();

    let mut flags = Vec::new();
    for (on, label) in [
        (f.dirty, "dirty"),
        (f.unpushed, "unpushed"),
        (f.conflicted, "conflicted"),
        (f.in_progress, "in-progress"),
        (f.detached, "detached"),
        (f.no_remote, "no-remote"),
        (f.no_upstream, "no-upstream"),
        (f.stashed, "stashed"),
        (f.error, "error"),
    ] {
        if on {
            flags.push(label);
        }
    }
    if flags.is_empty() {
        flags.push("clean");
    }

    RepoView {
        path: &repo.root,
        group: &repo.group,
        name: &repo.name,
        slug: repo.slug(),
        state: repo.state_label(),
        release_state: repo.release_state().key(),
        branch: repo.branch_label(),
        upstream: refs
            .and_then(|r| r.current_branch())
            .and_then(|b| b.upstream.clone()),
        ahead: repo.unpushed_total(),
        behind: repo.behind_total(),
        staged: work.map(|w| w.staged).unwrap_or(0),
        unstaged: work.map(|w| w.unstaged).unwrap_or(0),
        untracked: work.map(|w| w.untracked).unwrap_or(0),
        conflicts: work.map(|w| w.conflicts).unwrap_or(0),
        stashes: refs.map(|r| r.stashes).unwrap_or(0),
        operation: refs.and_then(|r| r.operation).map(|o| o.label()),
        last_tag: refs
            .and_then(|r| r.described_tag.as_ref())
            .map(|t| t.name.clone()),
        newest_tag: refs
            .and_then(|r| r.newest_tag.as_ref())
            .map(|t| t.name.clone()),
        tag_off_branch: refs.map(|r| r.tag_off_branch()).unwrap_or(false),
        commits_since_tag: repo.commits_since_tag(),
        changelog_version: refs
            .and_then(|r| r.changelog.as_ref())
            .map(|c| c.version.clone()),
        changelog_untagged: refs
            .and_then(|r| r.changelog.as_ref())
            .map(|c| !c.tagged)
            .unwrap_or(false),
        remote_url: refs.and_then(|r| r.remote_url.clone()),
        activity_at,
        activity_source: source.label(),
        age: fmt::age(activity_at, now),
        work_scanned: work.is_some(),
        error: repo.error.as_deref(),
        flags,
    }
}

pub fn list_json(repos: &[&RepoStatus], now: i64) -> Result<String> {
    let views: Vec<RepoView> = repos.iter().map(|r| view(r, now)).collect();
    Ok(serde_json::to_string_pretty(&views)?)
}

pub fn detail_json(repo: &RepoStatus, now: i64) -> Result<String> {
    #[derive(Serialize)]
    struct Full<'a> {
        #[serde(flatten)]
        view: RepoView<'a>,
        refs: Option<&'a crate::model::RefsInfo>,
        work: Option<&'a crate::model::WorkInfo>,
    }
    let full = Full {
        view: view(repo, now),
        refs: repo.refs.as_ref(),
        work: repo.work.as_ref(),
    };
    Ok(serde_json::to_string_pretty(&full)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_aligns_columns() {
        let rows = vec![
            vec!["a".into(), "1".into()],
            vec!["longer".into(), "22".into()],
        ];
        let out = table(&["NAME", "N"], &[Align::Left, Align::Right], &rows);
        let lines: Vec<&str> = out.lines().collect();
        // The right-aligned column widens to fit "22", so single digits and
        // the header both get a leading space.
        assert_eq!(lines[0], "NAME     N");
        assert_eq!(lines[1], "a        1");
        assert_eq!(lines[2], "longer  22");
        // Every line ends at the same column.
        assert!(lines.iter().all(|l| l.chars().count() == 10));
    }
}
