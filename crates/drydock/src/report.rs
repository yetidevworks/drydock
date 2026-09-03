//! Plain-text and JSON output for the non-interactive commands.

use anyhow::Result;
use serde::Serialize;

use crate::column::Column;
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

pub fn list_table(repos: &[&RepoStatus], now: i64, show_paths: bool, columns: &[Column]) -> String {
    let rows: Vec<Vec<String>> = repos
        .iter()
        .map(|r| columns.iter().map(|c| c.cell(r, now, show_paths)).collect())
        .collect();

    let headers: Vec<&str> = columns.iter().map(|c| c.header(show_paths)).collect();
    let aligns: Vec<Align> = columns.iter().map(|c| c.align()).collect();
    table(&headers, &aligns, &rows)
}

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
    if let Some(v) = &repo.visibility {
        use crate::model::VisibilityStatus;
        match &v.status {
            VisibilityStatus::Known(_) => {
                out.push_str(&format!(
                    "  visibility   {} (checked {} ago)\n",
                    v.status.label(),
                    fmt::age(v.checked_at, now)
                ));
            }
            // A check really was attempted here, unlike the other non-Known
            // cases below, so this is the one place the reason gets spelled
            // out -- never in a table, only here and in --json.
            VisibilityStatus::CheckFailed(reason) => {
                out.push_str(&format!(
                    "  visibility   check failed {} ago: {reason}\n",
                    fmt::age(v.checked_at, now)
                ));
            }
            // Not a real check against a provider, just a read of the remote
            // URL or the config, so there's no "checked ... ago" to report.
            VisibilityStatus::Unsupported
            | VisibilityStatus::NoRemote
            | VisibilityStatus::Unknown
            | VisibilityStatus::CheckingDisabled => {
                out.push_str(&format!("  visibility   {}\n", v.status.label()));
            }
        }
    }
    out.push_str(&format!(
        "  activity     {} ago ({})\n",
        fmt::age(activity_at, now),
        source.label()
    ));

    if let Some(refs) = &repo.refs {
        out.push_str(&format!("  head         {}\n", refs.head.label()));
        if let Some(url) = &refs.remote_url {
            out.push_str(&format!("  remote       {url}\n"));
            match refs.fetched_at {
                Some(at) => {
                    out.push_str(&format!("  fetched      {} ago\n", fmt::age(at, now)));
                }
                None => {
                    out.push_str("  fetched      never — the behind count has never been checked\n")
                }
            }
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
    } else if repo.is_bare() {
        out.push_str("\n  changes      (bare repo, no working tree)\n");
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
    /// `"public"`, `"private"`, `"internal"`, `"unsupported"` (a remote on a
    /// host nothing recognises), `"no remote configured"`, `"checking
    /// disabled"`, `"check failed"`, or `null` before the repo has been
    /// probed at all.
    pub visibility: Option<&'static str>,
    /// The reason, only present when `visibility` is `"check failed"`.
    pub visibility_error: Option<&'a str>,
    pub branch: String,
    pub upstream: Option<String>,
    /// Commits unpushed across *every* local branch, not just `branch`. What
    /// `--unpushed` and `--sort ahead` work from.
    ///
    /// This reads oddly next to `branch` and `upstream`, and the AHEAD column
    /// it used to feed was wrong for exactly that reason. It stays repo-wide
    /// anyway: 1.0 promised the `--json` fields wouldn't change incompatibly
    /// without a major version, and quietly redefining a number is the one
    /// break a script can't notice. `branch_ahead` is the per-branch reading.
    pub ahead: u32,
    /// Commits behind across *every* local branch, the counterpart to
    /// `ahead`. `--behind` and `--sort behind` work from this, and
    /// `branch_behind` is the per-branch reading.
    pub behind: u32,
    /// Commits the checked-out branch hasn't pushed. The one that reads
    /// against the `branch` and `upstream` above it, and what the AHEAD
    /// column shows.
    pub branch_ahead: u32,
    /// Commits `upstream` has that `branch` doesn't. Per-branch, like
    /// `branch_ahead`, and what the BEHIND column shows.
    pub branch_behind: u32,
    /// When anything last fetched this repo, from `FETCH_HEAD`. `null` means
    /// nothing ever has, which is what makes `behind: 0` a number nobody
    /// checked rather than a repo in sync.
    pub fetched_at: Option<i64>,
    /// True when there is a remote to check against and nothing has ever
    /// checked. Derived from `fetched_at`, but worth its own field: it is the
    /// condition a script would otherwise have to know to look for.
    pub never_fetched: bool,
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
        visibility: repo.visibility.as_ref().map(|v| v.status.label()),
        visibility_error: repo.visibility.as_ref().and_then(|v| match &v.status {
            crate::model::VisibilityStatus::CheckFailed(reason) => Some(reason.as_str()),
            _ => None,
        }),
        branch: repo.branch_label(),
        upstream: refs
            .and_then(|r| r.current_branch())
            .and_then(|b| b.upstream.clone()),
        ahead: repo.unpushed_total(),
        behind: repo.behind_total(),
        branch_ahead: repo.branch_unpushed(),
        branch_behind: repo.branch_behind(),
        fetched_at: repo.fetched_at(),
        never_fetched: repo.never_fetched(),
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

    // 1.0 promised the `--json` fields wouldn't change incompatibly without a
    // major version, and `ahead`/`behind` have always been repo-wide sums. The
    // columns needed the per-branch reading (#8), but taking these two names
    // for it would have redefined a number under every script already reading
    // them, which is the one break nobody notices. The per-branch pair got new
    // names instead, and this pins which is which.
    #[test]
    fn json_keeps_ahead_and_behind_repo_wide_and_adds_the_branch_pair() {
        let mut repo = RepoStatus::new("/tmp/x".into(), "g".into(), "r".into());
        repo.refs = Some(crate::model::RefsInfo {
            head: crate::model::Head::Branch("master".into()),
            branches: vec![
                branch("master", 0, 0),
                // The abandoned topic branch from #8, still tracking master.
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

        let v = view(&repo, 1_000);
        assert_eq!(v.branch, "master");
        // Unchanged from every release before this one.
        assert_eq!((v.ahead, v.behind), (2, 128));
        // What the table shows, and what reads correctly against `branch`.
        assert_eq!((v.branch_ahead, v.branch_behind), (0, 0));
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
}
