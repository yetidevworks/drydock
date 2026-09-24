//! Recent history for the quick view: the commit list, and what one commit
//! (or the uncommitted work on top of them) changed.
//!
//! Everything here is read on demand for one repo at a time, when someone asks
//! to look, so none of it goes near a sweep or the cache. It shells out to git
//! through the same rails as the probes, for the same reasons.

use anyhow::Result;
use std::collections::HashSet;
use std::path::Path;

use crate::git::{run_git, run_git_capped};

/// How many commits the list loads. Enough to scroll back through a few
/// releases of a busy repo, few enough that `--graph` stays instant.
pub const LOG_LIMIT: usize = 400;

/// Where a diff stops. A couple of megabytes is tens of thousands of lines,
/// far past what anyone scrolls through in a terminal, and it keeps a commit
/// that vendored a dependency from costing a hundred megabytes of memory.
const DIFF_CAP: usize = 2 * 1024 * 1024;

/// How many untracked files the uncommitted entry lists.
const UNTRACKED_LIMIT: usize = 200;

/// Field and record separators for `--format`. Control characters, because
/// they are the only things a subject line or a ref name can't contain.
const FS: char = '\x1f';
const RS: char = '\x1e';
/// Marks where git's graph drawing ends and the commit's own fields begin.
const START: char = '\x02';

// ---------------------------------------------------------------------------
// The commit list
// ---------------------------------------------------------------------------

/// A branch, tag or HEAD pointing at a commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefLabel {
    pub name: String,
    pub kind: RefKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefKind {
    Head,
    Local,
    Remote,
    Tag,
}

/// Which side of the upstream a commit is on. The reason drydock looks at a
/// repo in the first place is that something is ahead, behind or unreleased,
/// so the list says which commits those are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    /// On both sides, or there's no upstream to compare with.
    Shared,
    /// Committed here, not pushed yet.
    Unpushed,
    /// On the upstream, not pulled yet.
    Incoming,
}

#[derive(Debug, Clone)]
pub struct Commit {
    pub sha: String,
    pub short: String,
    pub author: String,
    pub at: i64,
    pub merge: bool,
    pub refs: Vec<RefLabel>,
    pub subject: String,
    pub flow: Flow,
}

/// One line of the list: a commit with the graph drawn to its left, or a line
/// of graph on its own where branches fork and join between commits.
#[derive(Debug, Clone)]
pub enum LogRow {
    Commit { graph: String, index: usize },
    Graph(String),
}

#[derive(Debug, Clone, Default)]
pub struct Log {
    pub rows: Vec<LogRow>,
    pub commits: Vec<Commit>,
    /// There were more commits than [`LOG_LIMIT`].
    pub truncated: bool,
}

/// Which history to list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// What's checked out, plus its upstream so incoming commits show up.
    Branch,
    /// Every branch and tag.
    All,
}

/// Load the commit list. `upstream` is the checked-out branch's upstream, as
/// `origin/develop`, when it has one.
pub async fn load_log(root: &Path, upstream: Option<&str>, scope: Scope) -> Result<Log> {
    let format = format!("--format={START}%H{FS}%h{FS}%an{FS}%at{FS}%P{FS}%D{FS}%s");
    let limit = format!("-n{}", LOG_LIMIT + 1);
    let mut args = vec![
        "log",
        "--graph",
        "--date-order",
        "--decorate=full",
        "--no-color",
        &format,
        &limit,
    ];
    match scope {
        Scope::All => args.extend(["--exclude=refs/stash", "--all"]),
        Scope::Branch => {
            args.push("HEAD");
            if let Some(up) = upstream {
                args.push(up);
            }
        }
    }
    args.push("--");

    let raw = match run_git(root, &args).await {
        Ok(raw) => raw,
        // A repo with nothing committed has no history, which is an answer
        // rather than a failure.
        Err(err) if unborn(&err) => return Ok(Log::default()),
        Err(err) => return Err(err),
    };

    let (unpushed, incoming) = match upstream {
        Some(up) => {
            let ahead = format!("{up}..HEAD");
            let behind = format!("HEAD..{up}");
            let cap = format!("-n{}", LOG_LIMIT + 1);
            let ahead = ["rev-list", &cap, &ahead, "--"];
            let behind = ["rev-list", &cap, &behind, "--"];
            let (a, b) = tokio::join!(run_git(root, &ahead), run_git(root, &behind));
            (shas(a.ok()), shas(b.ok()))
        }
        None => (HashSet::new(), HashSet::new()),
    };

    let mut log = parse_log(&raw);
    for commit in &mut log.commits {
        commit.flow = if unpushed.contains(&commit.sha) {
            Flow::Unpushed
        } else if incoming.contains(&commit.sha) {
            Flow::Incoming
        } else {
            Flow::Shared
        };
    }
    Ok(log)
}

fn unborn(err: &anyhow::Error) -> bool {
    let text = format!("{err:#}");
    text.contains("does not have any commits") || text.contains("unknown revision")
}

fn shas(raw: Option<String>) -> HashSet<String> {
    raw.map(|r| r.lines().map(|l| l.trim().to_string()).collect())
        .unwrap_or_default()
}

/// Parse `git log --graph` output in the format [`load_log`] asks for.
fn parse_log(raw: &str) -> Log {
    let mut log = Log::default();
    for line in raw.lines() {
        let Some((graph, fields)) = line.split_once(START) else {
            // A line of graph with no commit on it: a fork or a join.
            let graph = line.trim_end();
            if !graph.is_empty() {
                log.rows.push(LogRow::Graph(graph.to_string()));
            }
            continue;
        };
        if log.commits.len() == LOG_LIMIT {
            log.truncated = true;
            break;
        }
        let f: Vec<&str> = fields.splitn(7, FS).collect();
        if f.len() < 7 {
            continue;
        }
        log.rows.push(LogRow::Commit {
            graph: graph.to_string(),
            index: log.commits.len(),
        });
        log.commits.push(Commit {
            sha: f[0].to_string(),
            short: f[1].to_string(),
            author: f[2].to_string(),
            at: f[3].parse().unwrap_or(0),
            merge: f[4].split_whitespace().count() > 1,
            refs: parse_refs(f[5]),
            subject: f[6].to_string(),
            flow: Flow::Shared,
        });
    }
    // Graph lines after the last commit loaded belong to commits that
    // weren't, and would dangle off the bottom of the list.
    while matches!(log.rows.last(), Some(LogRow::Graph(_))) {
        log.rows.pop();
    }
    log
}

/// Parse `%D` under `--decorate=full`, which keeps the `refs/heads/` and
/// `refs/remotes/` prefixes -- the only reliable way to tell a local branch
/// called `origin/x` from a remote one.
fn parse_refs(raw: &str) -> Vec<RefLabel> {
    let mut out = Vec::new();
    for part in raw.split(", ").map(str::trim).filter(|p| !p.is_empty()) {
        let (head, name) = match part.strip_prefix("HEAD -> ") {
            Some(rest) => (true, rest),
            None => (false, part),
        };
        if head || name == "HEAD" {
            out.push(RefLabel {
                name: "HEAD".into(),
                kind: RefKind::Head,
            });
            if name == "HEAD" {
                continue;
            }
        }
        let label = if let Some(tag) = name.strip_prefix("tag: ") {
            RefLabel {
                name: tag.trim_start_matches("refs/tags/").to_string(),
                kind: RefKind::Tag,
            }
        } else if let Some(local) = name.strip_prefix("refs/heads/") {
            RefLabel {
                name: local.to_string(),
                kind: RefKind::Local,
            }
        } else if let Some(remote) = name.strip_prefix("refs/remotes/") {
            // `origin/HEAD` says which branch the remote defaults to, and
            // sits beside that branch's own label saying nothing new.
            if remote.ends_with("/HEAD") {
                continue;
            }
            RefLabel {
                name: remote.to_string(),
                kind: RefKind::Remote,
            }
        } else {
            RefLabel {
                name: name.to_string(),
                kind: RefKind::Local,
            }
        };
        out.push(label);
    }
    out
}

// ---------------------------------------------------------------------------
// One commit's changes
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    pub sha: String,
    pub parents: Vec<String>,
    pub author: String,
    pub author_email: String,
    pub author_date: String,
    pub author_at: i64,
    pub committer: String,
    pub committer_email: String,
    pub committer_date: String,
    pub refs: Vec<RefLabel>,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileStatus {
    Added,
    Modified,
    Deleted,
    Renamed,
    Copied,
    TypeChanged,
    Untracked,
}

impl FileStatus {
    pub fn letter(self) -> &'static str {
        match self {
            FileStatus::Added => "A",
            FileStatus::Modified => "M",
            FileStatus::Deleted => "D",
            FileStatus::Renamed => "R",
            FileStatus::Copied => "C",
            FileStatus::TypeChanged => "T",
            FileStatus::Untracked => "?",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChange {
    pub path: String,
    /// Where a rename or copy came from.
    pub from: Option<String>,
    pub status: FileStatus,
    /// Line counts, or `None` for a binary file.
    pub lines: Option<(u32, u32)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    /// The start of one file's diff. Its text is the path.
    File,
    /// `@@ -1,5 +1,6 @@`, with whatever context git put after it.
    Hunk,
    Added,
    Removed,
    Context,
    /// Something git said about the file rather than a line of it: a binary
    /// file, a mode change, a missing newline.
    Note,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatchLine {
    pub kind: LineKind,
    pub text: String,
    /// Line numbers in the old and new file, where the line has one.
    pub old: Option<u32>,
    pub new: Option<u32>,
}

/// What one entry in the list changed. The uncommitted entry has no header,
/// and lists untracked files as well as the diff.
#[derive(Debug, Clone, Default)]
pub struct Changes {
    pub header: Option<Header>,
    pub files: Vec<FileChange>,
    pub patch: Vec<PatchLine>,
    /// The patch stopped at [`DIFF_CAP`].
    pub truncated: bool,
}

impl Changes {
    pub fn totals(&self) -> (u32, u32) {
        self.files
            .iter()
            .filter_map(|f| f.lines)
            .fold((0, 0), |(a, r), (x, y)| (a + x, r + y))
    }
}

/// A merge is shown against its first parent, the way it reads from the
/// branch it landed on: what merging brought in. The default combined diff
/// shows only conflict resolutions, which is usually nothing at all.
const DIFF_ARGS: &[&str] = &[
    "--no-color",
    "--no-ext-diff",
    "-M",
    "--src-prefix=a/",
    "--dst-prefix=b/",
];

pub async fn load_commit(root: &Path, sha: &str) -> Result<Changes> {
    let format = format!(
        "--format=%H{FS}%P{FS}%an{FS}%ae{FS}%ad{FS}%at{FS}%cn{FS}%ce{FS}%cd{FS}%D{FS}%B{RS}"
    );
    let mut show = vec![
        "show",
        "-m",
        "--first-parent",
        "--decorate=full",
        "--date=format-local:%Y-%m-%d %H:%M",
        &format,
        "--patch",
    ];
    show.extend_from_slice(DIFF_ARGS);
    show.extend([sha, "--"]);

    let mut stat = vec![
        "show",
        "-m",
        "--first-parent",
        "--format=",
        "--raw",
        "--numstat",
        "-z",
    ];
    stat.extend_from_slice(DIFF_ARGS);
    stat.extend([sha, "--"]);

    let (patch, stat) = tokio::join!(run_git_capped(root, &show, DIFF_CAP), run_git(root, &stat));
    let (raw, truncated) = patch?;

    let (header, patch) = raw.split_once(RS).unwrap_or((raw.as_str(), ""));
    Ok(Changes {
        header: parse_header(header),
        files: parse_stat(&stat?),
        patch: parse_patch(patch),
        truncated,
    })
}

/// Everything not committed yet: staged and unstaged together, against HEAD,
/// then the untracked files by name.
pub async fn load_uncommitted(root: &Path) -> Result<Changes> {
    let mut diff = vec!["diff", "HEAD", "--patch"];
    diff.extend_from_slice(DIFF_ARGS);
    diff.push("--");
    let mut stat = vec!["diff", "HEAD", "--raw", "--numstat", "-z"];
    stat.extend_from_slice(DIFF_ARGS);
    stat.push("--");
    let untracked = ["ls-files", "--others", "--exclude-standard", "-z"];

    let (patch, stat, untracked) = tokio::join!(
        run_git_capped(root, &diff, DIFF_CAP),
        run_git(root, &stat),
        run_git(root, &untracked),
    );
    let (raw, truncated) = patch?;
    let mut files = parse_stat(&stat?);
    files.extend(
        untracked
            .unwrap_or_default()
            .split('\0')
            .filter(|p| !p.is_empty())
            .take(UNTRACKED_LIMIT)
            .map(|p| FileChange {
                path: p.to_string(),
                from: None,
                status: FileStatus::Untracked,
                lines: None,
            }),
    );
    Ok(Changes {
        header: None,
        files,
        patch: parse_patch(&raw),
        truncated,
    })
}

fn parse_header(raw: &str) -> Option<Header> {
    let f: Vec<&str> = raw.trim_start_matches('\n').splitn(11, FS).collect();
    if f.len() < 11 {
        return None;
    }
    Some(Header {
        sha: f[0].to_string(),
        parents: f[1].split_whitespace().map(str::to_string).collect(),
        author: f[2].to_string(),
        author_email: f[3].to_string(),
        author_date: f[4].to_string(),
        author_at: f[5].parse().unwrap_or(0),
        committer: f[6].to_string(),
        committer_email: f[7].to_string(),
        committer_date: f[8].to_string(),
        refs: parse_refs(f[9]),
        message: f[10].trim_end().to_string(),
    })
}

/// Parse `--raw --numstat -z`: the raw records first, which carry the status
/// letter, then the numstat records in the same order, which carry the counts.
fn parse_stat(raw: &str) -> Vec<FileChange> {
    let mut tokens = raw.split('\0').filter(|t| !t.is_empty()).peekable();
    let mut files: Vec<FileChange> = Vec::new();

    while let Some(meta) = tokens.peek().copied() {
        let Some(meta) = meta.strip_prefix(':') else {
            break;
        };
        tokens.next();
        let letter = meta.split_whitespace().last().unwrap_or("M");
        let status = match letter.chars().next() {
            Some('A') => FileStatus::Added,
            Some('D') => FileStatus::Deleted,
            Some('R') => FileStatus::Renamed,
            Some('C') => FileStatus::Copied,
            Some('T') => FileStatus::TypeChanged,
            _ => FileStatus::Modified,
        };
        let two = matches!(status, FileStatus::Renamed | FileStatus::Copied);
        let first = tokens.next().unwrap_or_default().to_string();
        let (path, from) = if two {
            (tokens.next().unwrap_or_default().to_string(), Some(first))
        } else {
            (first, None)
        };
        files.push(FileChange {
            path,
            from,
            status,
            lines: None,
        });
    }

    // Numstat: `added\tremoved\tpath`, or `added\tremoved\t` followed by the
    // old and new paths as tokens of their own for a rename. `-` for binary.
    let mut i = 0;
    while let Some(record) = tokens.next() {
        let mut parts = record.splitn(3, '\t');
        let (added, removed, path) = (parts.next(), parts.next(), parts.next());
        if matches!(path, None | Some("")) {
            // The rename form: two more tokens carry the paths.
            tokens.next();
            tokens.next();
        }
        if let Some(file) = files.get_mut(i) {
            file.lines = match (
                added.and_then(|a| a.parse().ok()),
                removed.and_then(|r| r.parse().ok()),
            ) {
                (Some(a), Some(r)) => Some((a, r)),
                _ => None,
            };
        }
        i += 1;
    }
    files
}

/// Parse a unified diff into lines worth drawing, numbering each one in the
/// old and new file as it goes.
fn parse_patch(raw: &str) -> Vec<PatchLine> {
    let mut out = Vec::new();
    let mut in_hunk = false;
    let (mut old, mut new) = (0u32, 0u32);
    // Held until `+++` says where the file ended up, since the `diff --git`
    // line is ambiguous for a path with a space in it.
    let mut pending: Option<(String, Vec<PatchLine>)> = None;

    let flush = |pending: &mut Option<(String, Vec<PatchLine>)>, out: &mut Vec<PatchLine>| {
        if let Some((path, notes)) = pending.take() {
            out.push(line(LineKind::File, path));
            out.extend(notes);
        }
    };

    for text in raw.lines() {
        if let Some(rest) = text.strip_prefix("diff --git ") {
            flush(&mut pending, &mut out);
            in_hunk = false;
            pending = Some((path_from_diff_line(rest), Vec::new()));
            continue;
        }
        if in_hunk {
            match text.as_bytes().first() {
                Some(b'+') => {
                    out.push(numbered(LineKind::Added, &text[1..], None, Some(new)));
                    new += 1;
                    continue;
                }
                Some(b'-') => {
                    out.push(numbered(LineKind::Removed, &text[1..], Some(old), None));
                    old += 1;
                    continue;
                }
                Some(b' ') => {
                    out.push(numbered(
                        LineKind::Context,
                        &text[1..],
                        Some(old),
                        Some(new),
                    ));
                    old += 1;
                    new += 1;
                    continue;
                }
                // An empty context line, from a tool that trimmed the space.
                None => {
                    out.push(numbered(LineKind::Context, "", Some(old), Some(new)));
                    old += 1;
                    new += 1;
                    continue;
                }
                Some(b'\\') => {
                    out.push(line(
                        LineKind::Note,
                        text.trim_start_matches("\\ ").to_string(),
                    ));
                    continue;
                }
                _ => in_hunk = false,
            }
        }
        if let Some(rest) = text.strip_prefix("@@") {
            flush(&mut pending, &mut out);
            let (o, n) = hunk_starts(rest);
            old = o;
            new = n;
            in_hunk = true;
            out.push(line(LineKind::Hunk, text.to_string()));
            continue;
        }
        let Some((path, notes)) = pending.as_mut() else {
            continue;
        };
        if let Some(p) = text.strip_prefix("+++ ") {
            if p != "/dev/null" {
                *path = p.strip_prefix("b/").unwrap_or(p).to_string();
            }
        } else if let Some(p) = text.strip_prefix("--- ") {
            // A deleted file only has its old path.
            if p != "/dev/null" {
                *path = p.strip_prefix("a/").unwrap_or(p).to_string();
            }
        } else if let Some(p) = text.strip_prefix("rename to ") {
            *path = p.to_string();
        } else if text.starts_with("Binary files ") {
            notes.push(line(LineKind::Note, "binary file, not shown".into()));
        } else if let Some(mode) = text.strip_prefix("new mode ") {
            notes.push(line(LineKind::Note, format!("mode changed to {mode}")));
        }
    }
    flush(&mut pending, &mut out);
    out
}

fn line(kind: LineKind, text: String) -> PatchLine {
    PatchLine {
        kind,
        text,
        old: None,
        new: None,
    }
}

fn numbered(kind: LineKind, text: &str, old: Option<u32>, new: Option<u32>) -> PatchLine {
    PatchLine {
        kind,
        text: text.to_string(),
        old,
        new,
    }
}

/// `a/x b/x` from a `diff --git` line. Only a fallback: the `+++` and `---`
/// lines that follow replace it, and only a binary file or a pure rename or
/// mode change goes without them.
fn path_from_diff_line(rest: &str) -> String {
    match rest.rfind(" b/") {
        Some(at) => rest[at + 3..].to_string(),
        None => rest.to_string(),
    }
}

/// Where a hunk starts in each file, from `@@ -12,7 +12,9 @@`.
fn hunk_starts(rest: &str) -> (u32, u32) {
    let mut old = 0;
    let mut new = 0;
    for part in rest.split_whitespace() {
        if let Some(n) = part.strip_prefix('-') {
            old = n
                .split(',')
                .next()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
        } else if let Some(n) = part.strip_prefix('+') {
            new = n
                .split(',')
                .next()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
        } else if part == "@@" && (old > 0 || new > 0) {
            break;
        }
    }
    (old, new)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_log_keeps_graph_lines_between_commits() {
        let raw = format!(
            "* {START}aaa{FS}a{FS}Andy{FS}100{FS}bbb ccc{FS}HEAD -> refs/heads/main, tag: refs/tags/1.0{FS}Merge x\n\
             |\\  \n\
             | * {START}ccc{FS}c{FS}Andy{FS}90{FS}bbb{FS}{FS}Side\n\
             |/  \n\
             * {START}bbb{FS}b{FS}Andy{FS}80{FS}{FS}{FS}Root\n"
        );
        let log = parse_log(&raw);
        assert_eq!(log.commits.len(), 3);
        assert_eq!(log.rows.len(), 5);
        assert!(log.commits[0].merge);
        assert!(!log.commits[1].merge);
        assert!(matches!(&log.rows[1], LogRow::Graph(g) if g == "|\\"));
        assert!(matches!(&log.rows[2], LogRow::Commit { graph, index: 1 } if graph == "| * "));
        assert_eq!(
            log.commits[0].refs,
            vec![
                RefLabel {
                    name: "HEAD".into(),
                    kind: RefKind::Head
                },
                RefLabel {
                    name: "main".into(),
                    kind: RefKind::Local
                },
                RefLabel {
                    name: "1.0".into(),
                    kind: RefKind::Tag
                },
            ]
        );
    }

    // A subject line can hold anything a person types, separators included
    // up to the last field -- splitn keeps the rest of the line whole.
    #[test]
    fn a_subject_keeps_everything_after_the_last_field() {
        let raw = format!("* {START}aaa{FS}a{FS}Andy{FS}100{FS}{FS}{FS}fix: a, b {FS} c\n");
        let log = parse_log(&raw);
        assert_eq!(log.commits[0].subject, format!("fix: a, b {FS} c"));
    }

    #[test]
    fn remote_head_pointers_are_left_out_and_remotes_are_told_apart() {
        let refs = parse_refs(
            "refs/remotes/origin/HEAD, refs/remotes/origin/develop, refs/heads/origin/x",
        );
        assert_eq!(
            refs,
            vec![
                RefLabel {
                    name: "origin/develop".into(),
                    kind: RefKind::Remote
                },
                RefLabel {
                    name: "origin/x".into(),
                    kind: RefKind::Local
                },
            ]
        );
        // A detached HEAD decorates as plain `HEAD`.
        assert_eq!(parse_refs("HEAD")[0].kind, RefKind::Head);
    }

    #[test]
    fn a_log_past_the_limit_says_so() {
        let mut raw = String::new();
        for i in 0..=LOG_LIMIT {
            raw.push_str(&format!("* {START}{i}{FS}{i}{FS}A{FS}1{FS}{FS}{FS}s\n|\n"));
        }
        let log = parse_log(&raw);
        assert_eq!(log.commits.len(), LOG_LIMIT);
        assert!(log.truncated);
        assert!(
            matches!(log.rows.last(), Some(LogRow::Commit { .. })),
            "no graph dangling past the last commit"
        );
    }

    #[test]
    fn the_stat_pairs_status_letters_with_line_counts() {
        let raw = ":100644 100644 aaa bbb M\0src/a.rs\0\
                   :000000 100644 000 ccc A\0new.txt\0\
                   :100644 100644 ddd eee R087\0old name.rs\0new name.rs\0\
                   :100644 100644 fff 000 M\0logo.png\0\
                   3\t1\tsrc/a.rs\0\
                   10\t0\tnew.txt\0\
                   2\t2\t\0old name.rs\0new name.rs\0\
                   -\t-\tlogo.png\0";
        let files = parse_stat(raw);
        assert_eq!(files.len(), 4);
        assert_eq!(files[0].lines, Some((3, 1)));
        assert_eq!(files[1].status, FileStatus::Added);
        assert_eq!(files[2].status, FileStatus::Renamed);
        assert_eq!(files[2].path, "new name.rs");
        assert_eq!(files[2].from.as_deref(), Some("old name.rs"));
        assert_eq!(files[2].lines, Some((2, 2)));
        assert_eq!(files[3].lines, None, "binary has no line counts");
    }

    #[test]
    fn the_patch_numbers_lines_in_both_files() {
        let raw = "diff --git a/a.txt b/a.txt\n\
                   index 111..222 100644\n\
                   --- a/a.txt\n\
                   +++ b/a.txt\n\
                   @@ -10,3 +10,3 @@ fn main\n \
                   same\n\
                   -old\n\
                   +new\n \
                   tail\n\
                   diff --git a/gone.txt b/gone.txt\n\
                   deleted file mode 100644\n\
                   --- a/gone.txt\n\
                   +++ /dev/null\n\
                   @@ -1 +0,0 @@\n\
                   -bye\n\
                   \\ No newline at end of file\n";
        let lines = parse_patch(raw);
        let kinds: Vec<LineKind> = lines.iter().map(|l| l.kind).collect();
        assert_eq!(
            kinds,
            vec![
                LineKind::File,
                LineKind::Hunk,
                LineKind::Context,
                LineKind::Removed,
                LineKind::Added,
                LineKind::Context,
                LineKind::File,
                LineKind::Hunk,
                LineKind::Removed,
                LineKind::Note,
            ]
        );
        assert_eq!(lines[0].text, "a.txt");
        assert_eq!((lines[2].old, lines[2].new), (Some(10), Some(10)));
        assert_eq!((lines[3].old, lines[3].new), (Some(11), None));
        assert_eq!((lines[4].old, lines[4].new), (None, Some(11)));
        assert_eq!((lines[5].old, lines[5].new), (Some(12), Some(12)));
        assert_eq!(lines[6].text, "gone.txt", "a deletion keeps its old path");
    }

    // Inside a hunk, a removed line that happens to start `-- ` is content,
    // not a file header.
    #[test]
    fn a_removed_line_that_looks_like_a_header_stays_content() {
        let raw =
            "diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1,2 +1,1 @@\n--- not a header\n keep\n";
        let lines = parse_patch(raw);
        assert_eq!(lines[2].kind, LineKind::Removed);
        assert_eq!(lines[2].text, "-- not a header");
    }

    #[test]
    fn a_binary_file_gets_a_note_instead_of_lines() {
        let raw = "diff --git a/logo.png b/logo.png\nindex 1..2 100644\nBinary files a/logo.png and b/logo.png differ\n";
        let lines = parse_patch(raw);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].text, "logo.png");
        assert_eq!(lines[1].kind, LineKind::Note);
    }

    #[test]
    fn the_header_splits_on_its_separators() {
        let raw = format!(
            "\nabc{FS}p1 p2{FS}Andy{FS}a@x{FS}2026-09-23 14:10{FS}100{FS}Bot{FS}b@x{FS}2026-09-23 15:00{FS}HEAD -> refs/heads/main{FS}Subject\n\nBody\n"
        );
        let h = parse_header(&raw).unwrap();
        assert_eq!(h.sha, "abc");
        assert_eq!(h.parents, vec!["p1", "p2"]);
        assert_eq!(h.committer, "Bot");
        assert_eq!(h.message, "Subject\n\nBody");
    }
}
