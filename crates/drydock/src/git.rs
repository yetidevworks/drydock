//! Talking to git.
//!
//! Two deliberate choices here.
//!
//! First, this shells out to `git` rather than linking a library. Git's own
//! status has the untracked cache, index v4 and fsmonitor support, and it
//! honours whatever per-repo config is in play, so the numbers match exactly
//! what you see on the command line and in a GUI client. Process startup is
//! about half a millisecond, which is noise next to the working-tree scan it
//! wraps.
//!
//! Second, every invocation passes `--no-optional-locks`. Without it, polling
//! hundreds of repos would take `index.lock` and rewrite indexes constantly,
//! fighting whatever else is open on the same repo and churning mtimes that
//! this tool then reads back as activity.
//!
//! Anything derivable by reading a file under `.git` directly is read directly:
//! stash count, index mtime, in-progress operations, shallowness, remote URL.
//! At 500-plus repos, a process spawn avoided is worth having.

use anyhow::{anyhow, Context, Result};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::process::Command;

use crate::config::{Config, ReleaseConfig, StatusConfig};
use crate::model::{
    BranchInfo, ChangeKind, ChangedFile, ChangelogInfo, CommitInfo, Head, Operation, RefsInfo,
    TagInfo, WorkInfo, WorkKey,
};

/// Cap on how long any single git invocation may take. A repo on a stalled
/// network mount shouldn't be able to hold up a sweep.
const GIT_TIMEOUT: Duration = Duration::from_secs(30);

/// How many tags to pull dates for. Only used to date the tag `describe`
/// picked, so the newest few hundred is plenty.
const TAG_LIMIT: usize = 400;

/// One `git` invocation against a repo, with the safety rails applied.
async fn run_git(root: &Path, args: &[&str]) -> Result<String> {
    let mut cmd = Command::new("git");
    cmd.arg("--no-optional-locks")
        .arg("-c")
        .arg("core.quotepath=false")
        .arg("-c")
        .arg("color.ui=false")
        .arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ASKPASS", "")
        .env("LC_ALL", "C")
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);

    let output = tokio::time::timeout(GIT_TIMEOUT, cmd.output())
        .await
        .map_err(|_| anyhow!("git {} timed out", args.join(" ")))?
        .with_context(|| format!("Running git {}", args.join(" ")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(anyhow!(
            "git {} failed: {}",
            args.join(" "),
            if stderr.is_empty() {
                "no output".into()
            } else {
                stderr
            }
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Like [`run_git`], but a non-zero exit is an expected outcome rather than an
/// error. `describe` on a repo with no tags is the motivating case.
async fn try_git(root: &Path, args: &[&str]) -> Option<String> {
    run_git(root, args).await.ok()
}

/// Resolve the real git directory for a checkout, following the `gitdir:`
/// pointer that worktrees and submodules leave in a `.git` file.
pub fn resolve_git_dir(root: &Path) -> Result<PathBuf> {
    let dot_git = root.join(".git");
    let meta =
        std::fs::metadata(&dot_git).with_context(|| format!("No .git at {}", root.display()))?;
    if meta.is_dir() {
        return Ok(dot_git);
    }
    let body = std::fs::read_to_string(&dot_git)
        .with_context(|| format!("Reading {}", dot_git.display()))?;
    let target = body
        .lines()
        .find_map(|l| l.strip_prefix("gitdir:"))
        .map(|s| s.trim())
        .ok_or_else(|| anyhow!("{} has no gitdir pointer", dot_git.display()))?;
    let target = PathBuf::from(target);
    Ok(if target.is_absolute() {
        target
    } else {
        root.join(target)
    })
}

fn mtime_secs(path: &Path) -> Option<i64> {
    std::fs::metadata(path)
        .ok()?
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs() as i64)
}

fn file_size(path: &Path) -> Option<u64> {
    std::fs::metadata(path).ok().map(|m| m.len())
}

pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// The cheap fingerprint that decides whether a cached working-tree scan is
/// still valid: if HEAD hasn't moved and the index hasn't been rewritten,
/// nothing git knows about has changed.
///
/// Note this deliberately doesn't try to capture unsaved working-tree edits;
/// those are caught by the filesystem watcher instead.
pub fn work_key(root: &Path, head_sha: Option<String>) -> WorkKey {
    let index = resolve_git_dir(root)
        .map(|d| d.join("index"))
        .unwrap_or_else(|_| root.join(".git/index"));
    WorkKey {
        head_sha,
        index_mtime: mtime_secs(&index),
        index_size: file_size(&index),
    }
}

// ---------------------------------------------------------------------------
// Tier 1: refs, tags, and everything readable straight out of .git
// ---------------------------------------------------------------------------

/// Single format string covering both branches and tags, so one
/// `for-each-ref` call answers most of tier 1. The leading full refname says
/// which kind of ref each line describes.
const REF_FORMAT: &str = concat!(
    "%(refname)\t",
    "%(upstream:short)\t",
    "%(upstream:track)\t",
    "%(committerdate:unix)\t",
    "%(creatordate:unix)\t",
    "%(objectname:short)\t",
    "%(contents:subject)"
);

pub async fn probe_refs(root: &Path, cfg: &Config) -> Result<RefsInfo> {
    let git_dir = resolve_git_dir(root)?;

    let raw = run_git(
        root,
        &[
            "for-each-ref",
            &format!("--format={REF_FORMAT}"),
            "refs/heads",
            "refs/tags",
        ],
    )
    .await?;

    let mut branches: Vec<BranchInfo> = Vec::new();
    let mut tags: Vec<TagInfo> = Vec::new();
    for line in raw.lines() {
        let fields: Vec<&str> = line.splitn(7, '\t').collect();
        if fields.len() < 7 {
            continue;
        }
        let refname = fields[0];
        if let Some(name) = refname.strip_prefix("refs/heads/") {
            let (ahead, behind, gone) = parse_track(fields[2]);
            branches.push(BranchInfo {
                name: name.to_string(),
                upstream: (!fields[1].is_empty()).then(|| fields[1].to_string()),
                ahead,
                behind,
                gone,
                committed_at: fields[3].parse().unwrap_or(0),
                sha: fields[5].to_string(),
                subject: fields[6].to_string(),
            });
        } else if let Some(name) = refname.strip_prefix("refs/tags/") {
            tags.push(TagInfo {
                name: name.to_string(),
                at: fields[4].parse().unwrap_or(0),
            });
        }
    }

    // Only tags matching the release pattern count. Without this, a marker tag
    // like `latest` or `polar-live` gets treated as the last release and the
    // "commits since tag" number becomes meaningless.
    let tag_matcher = compile_tag_pattern(&cfg.release.tag_pattern);
    if let Some(matcher) = &tag_matcher {
        tags.retain(|t| matcher.is_match(t.name.as_str()));
    }
    tags.sort_by_key(|t| std::cmp::Reverse(t.at));
    tags.truncate(TAG_LIMIT);
    let newest_tag = tags.first().cloned();

    let head = read_head(&git_dir, &branches)?;
    let head_branch = head.branch().map(|s| s.to_string());
    let last_commit = match &head {
        Head::Unborn => None,
        Head::Branch(name) => branches
            .iter()
            .find(|b| &b.name == name)
            .map(|b| CommitInfo {
                sha: b.sha.clone(),
                at: b.committed_at,
                subject: b.subject.clone(),
                author: String::new(),
            }),
        Head::Detached { .. } => None,
    };
    // Detached HEAD isn't in refs/heads, so it needs its own lookup. Same for
    // a branch whose ref somehow didn't come back above.
    let last_commit = match last_commit {
        Some(c) => Some(c),
        None if !matches!(head, Head::Unborn) => head_commit(root).await,
        None => None,
    };

    let (described_tag, commits_since_tag, since_tag_subjects) = if matches!(head, Head::Unborn) {
        (None, None, Vec::new())
    } else {
        describe_since_tag(root, &tags, &cfg.release).await
    };

    let cfg_scan = scan_git_config(&git_dir);

    let mut refs = RefsInfo {
        head,
        branches,
        last_commit,
        stashes: count_stashes(&git_dir),
        operation: detect_operation(&git_dir),
        newest_tag,
        described_tag,
        commits_since_tag,
        since_tag_subjects,
        index_mtime: mtime_secs(&git_dir.join("index")),
        remote_url: cfg_scan.remote_url,
        changelog: None,
        is_bare: cfg_scan.bare,
        is_shallow: git_dir.join("shallow").exists(),
    };

    if cfg.release.read_changelog {
        refs.changelog = read_changelog(root, &refs, &cfg.release);
    }
    let _ = head_branch;
    Ok(refs)
}

/// Parse `%(upstream:track)`, which looks like `[ahead 3, behind 1]`,
/// `[behind 2]`, `[gone]`, or is empty when in sync.
fn parse_track(raw: &str) -> (u32, u32, bool) {
    let inner = raw.trim().trim_start_matches('[').trim_end_matches(']');
    if inner.is_empty() {
        return (0, 0, false);
    }
    if inner == "gone" {
        return (0, 0, true);
    }
    let mut ahead = 0;
    let mut behind = 0;
    for part in inner.split(',') {
        let part = part.trim();
        if let Some(n) = part.strip_prefix("ahead ") {
            ahead = n.trim().parse().unwrap_or(0);
        } else if let Some(n) = part.strip_prefix("behind ") {
            behind = n.trim().parse().unwrap_or(0);
        }
    }
    (ahead, behind, false)
}

/// Read HEAD out of the git directory rather than asking git, which also gets
/// the unborn case right: a symref pointing at a branch that has no commits
/// yet.
fn read_head(git_dir: &Path, branches: &[BranchInfo]) -> Result<Head> {
    let raw = std::fs::read_to_string(git_dir.join("HEAD"))
        .with_context(|| format!("Reading {}", git_dir.join("HEAD").display()))?;
    let raw = raw.trim();
    if let Some(refname) = raw.strip_prefix("ref:") {
        let refname = refname.trim();
        let name = refname
            .strip_prefix("refs/heads/")
            .unwrap_or(refname)
            .to_string();
        if branches.iter().any(|b| b.name == name) {
            Ok(Head::Branch(name))
        } else {
            // Symref to a branch that doesn't exist: a repo with no commits.
            Ok(Head::Unborn)
        }
    } else if raw.len() >= 7 {
        Ok(Head::Detached {
            sha: raw[..7].to_string(),
        })
    } else {
        Ok(Head::Unborn)
    }
}

async fn head_commit(root: &Path) -> Option<CommitInfo> {
    let raw = try_git(
        root,
        &["log", "-1", "--format=%h%x09%ct%x09%an%x09%s", "HEAD"],
    )
    .await?;
    let line = raw.lines().next()?;
    let f: Vec<&str> = line.splitn(4, '\t').collect();
    if f.len() < 4 {
        return None;
    }
    Some(CommitInfo {
        sha: f[0].to_string(),
        at: f[1].parse().unwrap_or(0),
        author: f[2].to_string(),
        subject: f[3].to_string(),
    })
}

/// Work out the nearest tag reachable from HEAD and how far ahead of it we are.
///
/// `describe` is used rather than "newest tag by date" on purpose. With
/// git-flow, tags land on `master` while work carries on on `develop`, so the
/// newest tag by date is often not an ancestor of HEAD at all. Both values get
/// stored so the difference can be shown rather than silently papered over.
///
/// Merge commits are excluded from the count. Immediately after a git-flow
/// release, `develop` carries a "Merge tag 'x.y.z' into develop" commit that is
/// not reachable from the tag, so a raw count reports one commit to release
/// when there is nothing to release at all. Across a real tree of 558 repos
/// that accounted for 86 of 193 unreleased flags, and not one of them had a
/// tree that differed from its tag. A merge commit contributes no work of its
/// own; whatever it brought along is counted through its own commits.
async fn describe_since_tag(
    root: &Path,
    tags: &[TagInfo],
    cfg: &ReleaseConfig,
) -> (Option<TagInfo>, Option<u32>, Vec<String>) {
    let match_arg = format!("--match={}", cfg.tag_pattern);
    let mut describe_args = vec!["describe", "--tags", "--abbrev=0"];
    if cfg.tag_pattern != "*" && !cfg.tag_pattern.is_empty() {
        describe_args.push(&match_arg);
    }
    describe_args.push("HEAD");
    let name = match try_git(root, &describe_args).await {
        Some(out) => out.trim().to_string(),
        None => return (None, None, Vec::new()),
    };
    if name.is_empty() {
        return (None, None, Vec::new());
    }
    let at = tags
        .iter()
        .find(|t| t.name == name)
        .map(|t| t.at)
        .unwrap_or(0);
    let tag = TagInfo {
        name: name.clone(),
        at,
    };

    let range = format!("{name}..HEAD");
    let count = try_git(root, &["rev-list", "--count", "--no-merges", &range])
        .await
        .and_then(|s| s.trim().parse::<u32>().ok());

    let subjects = match count {
        Some(n) if n > 0 && cfg.max_subjects > 0 => {
            let limit = format!("--max-count={}", cfg.max_subjects);
            try_git(root, &["log", &limit, "--no-merges", "--format=%s", &range])
                .await
                .map(|s| s.lines().map(|l| l.to_string()).collect())
                .unwrap_or_default()
        }
        _ => Vec::new(),
    };

    (Some(tag), count, subjects)
}

/// Compile the release tag glob. `None` means "match everything", so an
/// unparseable pattern degrades to no filtering rather than to no tags.
fn compile_tag_pattern(pattern: &str) -> Option<globset::GlobMatcher> {
    if pattern.is_empty() || pattern == "*" {
        return None;
    }
    match globset::Glob::new(pattern) {
        Ok(glob) => Some(glob.compile_matcher()),
        Err(err) => {
            tracing::warn!(%pattern, %err, "ignoring invalid release.tag_pattern");
            None
        }
    }
}

/// Stash entries are reflog lines, so the count is just the line count of the
/// stash reflog. No process needed.
fn count_stashes(git_dir: &Path) -> u32 {
    let log = git_dir.join("logs/refs/stash");
    match std::fs::read_to_string(log) {
        Ok(body) => body.lines().filter(|l| !l.trim().is_empty()).count() as u32,
        Err(_) => 0,
    }
}

/// A half-finished operation shows up as a marker file or directory in the git
/// dir.
fn detect_operation(git_dir: &Path) -> Option<Operation> {
    let checks: [(&str, Operation); 7] = [
        ("rebase-merge", Operation::Rebase),
        ("rebase-apply", Operation::Rebase),
        ("MERGE_HEAD", Operation::Merge),
        ("CHERRY_PICK_HEAD", Operation::CherryPick),
        ("REVERT_HEAD", Operation::Revert),
        ("BISECT_LOG", Operation::Bisect),
        ("BISECT_START", Operation::Bisect),
    ];
    checks
        .iter()
        .find(|(name, _)| git_dir.join(name).exists())
        .map(|(_, op)| *op)
}

#[derive(Default)]
struct GitConfigScan {
    remote_url: Option<String>,
    bare: bool,
}

/// Pull the bits of `.git/config` worth having without paying for a `git
/// config` process. `origin` wins if present, otherwise the first remote found.
fn scan_git_config(git_dir: &Path) -> GitConfigScan {
    let mut scan = GitConfigScan::default();
    let body = match std::fs::read_to_string(git_dir.join("config")) {
        Ok(b) => b,
        Err(_) => return scan,
    };

    let mut section = String::new();
    let mut subsection = String::new();
    let mut first_remote: Option<String> = None;

    for line in body.lines() {
        let line = line.trim();
        if line.starts_with('[') && line.ends_with(']') {
            let inner = &line[1..line.len() - 1];
            match inner.split_once(' ') {
                Some((s, sub)) => {
                    section = s.trim().to_ascii_lowercase();
                    subsection = sub.trim().trim_matches('"').to_string();
                }
                None => {
                    section = inner.trim().to_ascii_lowercase();
                    subsection.clear();
                }
            }
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim().to_ascii_lowercase();
        let value = value.trim().to_string();

        if section == "core" && key == "bare" {
            scan.bare = value.eq_ignore_ascii_case("true");
        }
        if section == "remote" && key == "url" {
            if subsection == "origin" {
                scan.remote_url = Some(value.clone());
            } else if first_remote.is_none() {
                first_remote = Some(value);
            }
        }
    }
    if scan.remote_url.is_none() {
        scan.remote_url = first_remote;
    }
    scan
}

/// Compare the top version heading in the changelog against the tags that
/// exist. A changelog sitting above its newest tag is an in-flight release.
fn read_changelog(root: &Path, refs: &RefsInfo, cfg: &ReleaseConfig) -> Option<ChangelogInfo> {
    let path = cfg
        .changelog_files
        .iter()
        .map(|f| root.join(f))
        .find(|p| p.is_file())?;
    let body = std::fs::read_to_string(&path).ok()?;

    let mut versions: Vec<String> = Vec::new();
    for line in body.lines().take(400) {
        if let Some(v) = parse_changelog_version(line) {
            versions.push(v);
            if versions.len() >= 12 {
                break;
            }
        }
    }
    let top = versions.first()?.clone();

    let tag_matches = |v: &str| {
        refs.newest_tag.is_some()
            && (refs
                .described_tag
                .as_ref()
                .is_some_and(|t| tag_eq_version(&t.name, v))
                || refs
                    .newest_tag
                    .as_ref()
                    .is_some_and(|t| tag_eq_version(&t.name, v)))
    };

    let tagged = tag_matches(&top);
    // Count the leading run of versions that have no tag yet. More than one
    // means release blocks have stacked up instead of being consolidated.
    let unreleased_blocks = versions.iter().take_while(|v| !tag_matches(v)).count() as u32;

    Some(ChangelogInfo {
        version: top,
        tagged,
        unreleased_blocks,
    })
}

/// Recognise a changelog version heading. Covers `# 1.2.3`, `## [1.2.3]`,
/// `# v1.2.3 - date`, and the bare `1.2.3` headings Grav plugins use.
fn parse_changelog_version(line: &str) -> Option<String> {
    let t = line.trim();
    if t.is_empty() {
        return None;
    }
    let t = t.trim_start_matches('#').trim();
    let t = t.trim_start_matches('[');
    let token = t.split_whitespace().next()?;
    let token = token
        .trim_end_matches(']')
        .trim_end_matches(':')
        .trim_start_matches('v');
    let mut chars = token.chars();
    if !chars.next()?.is_ascii_digit() {
        return None;
    }
    if !token.contains('.') {
        return None;
    }
    if !token
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+' | '_'))
    {
        return None;
    }
    Some(token.to_string())
}

fn tag_eq_version(tag: &str, version: &str) -> bool {
    tag.trim_start_matches('v') == version.trim_start_matches('v')
}

// ---------------------------------------------------------------------------
// Remote freshness
// ---------------------------------------------------------------------------

/// Fetch, so "behind" counts mean something.
///
/// Everything else in this module is local and free. This is not: it hits the
/// network, and against a few hundred repos that is real traffic. So it only
/// ever happens on request or on a long timer, credential prompts are refused
/// rather than left to block, and a hung remote is capped by its own timeout
/// instead of the general one.
pub async fn fetch(root: &Path, timeout: Duration) -> Result<()> {
    let mut cmd = Command::new("git");
    cmd.arg("--no-optional-locks")
        .arg("-C")
        .arg(root)
        .args([
            "-c",
            "credential.interactive=false",
            "fetch",
            "--all",
            "--prune",
            "--quiet",
            "--no-tags",
        ])
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ASKPASS", "")
        .env("SSH_ASKPASS", "")
        .env("LC_ALL", "C")
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);

    let output = tokio::time::timeout(timeout, cmd.output())
        .await
        .map_err(|_| anyhow!("fetch timed out after {}s", timeout.as_secs()))?
        .context("Running git fetch")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(anyhow!(
            "fetch failed: {}",
            if stderr.is_empty() {
                "no output".to_string()
            } else {
                stderr.lines().next().unwrap_or("").to_string()
            }
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tier 2: the working tree
// ---------------------------------------------------------------------------

pub async fn probe_work(root: &Path, cfg: &StatusConfig) -> Result<WorkInfo> {
    // No `--branch`: that makes git compute ahead/behind with a revwalk, and
    // tier 1 already has those numbers from the tracking refs.
    let raw = run_git(
        root,
        &[
            "status",
            "--porcelain=v2",
            cfg.untracked.as_git_arg(),
            "--ignore-submodules=dirty",
        ],
    )
    .await?;

    let mut info = WorkInfo {
        staged: 0,
        unstaged: 0,
        untracked: 0,
        conflicts: 0,
        newest_mtime: None,
        files: Vec::new(),
        truncated: false,
    };

    for line in raw.lines() {
        let Some((tag, rest)) = line.split_once(' ') else {
            continue;
        };
        let (path, code, kind) = match tag {
            // 1 <XY> <sub> <mH> <mI> <mW> <hH> <hI> <path>
            // 2 <XY> <sub> <mH> <mI> <mW> <hH> <hI> <X><score> <path><tab><orig>
            "1" | "2" => {
                let leading = if tag == "2" { 9 } else { 8 };
                let path = field_tail(rest, leading);
                let xy = rest.get(..2).unwrap_or("..");
                let mut chars = xy.chars();
                let x = chars.next().unwrap_or('.');
                let y = chars.next().unwrap_or('.');
                if x != '.' {
                    info.staged += 1;
                }
                if y != '.' {
                    info.unstaged += 1;
                }
                let kind = if y != '.' {
                    ChangeKind::Unstaged
                } else {
                    ChangeKind::Staged
                };
                (path, xy.to_string(), kind)
            }
            // u <XY> <sub> <m1> <m2> <m3> <mW> <h1> <h2> <h3> <path>
            "u" => {
                info.conflicts += 1;
                let path = field_tail(rest, 10);
                let xy = rest.get(..2).unwrap_or("UU");
                (path, xy.to_string(), ChangeKind::Conflicted)
            }
            "?" => {
                info.untracked += 1;
                (rest.to_string(), "??".to_string(), ChangeKind::Untracked)
            }
            _ => continue,
        };

        if info.files.len() < cfg.max_files {
            info.files.push(ChangedFile {
                path,
                code,
                kind,
                mtime: None,
            });
        } else {
            info.truncated = true;
        }
    }

    stat_changed_files(root, &mut info);
    Ok(info)
}

/// Take the final field of a porcelain v2 record, given how many
/// space-separated fields precede the path. Paths can contain spaces, and for
/// rename records a tab separates the new path from the original.
fn field_tail(rest: &str, field_count: usize) -> String {
    rest.splitn(field_count, ' ')
        .last()
        .unwrap_or("")
        .split('\t')
        .next()
        .unwrap_or("")
        .to_string()
}

/// Timestamp the changed files.
///
/// This is what makes "modified in the last hour" work at all: a working-tree
/// edit touches nothing under `.git`, so a dirty repo's real activity time can
/// only come from the files themselves. Status already named them, so this is
/// a handful of stat calls on the repos that have changes and nothing at all
/// on the ones that don't.
fn stat_changed_files(root: &Path, info: &mut WorkInfo) {
    let mut newest: Option<i64> = None;
    for file in info.files.iter_mut() {
        let path = root.join(file.path.trim_end_matches('/'));
        if let Some(m) = mtime_secs(&path) {
            file.mtime = Some(m);
            newest = Some(newest.map_or(m, |n: i64| n.max(m)));
        }
    }
    info.newest_mtime = newest;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn track_parsing() {
        assert_eq!(parse_track(""), (0, 0, false));
        assert_eq!(parse_track("[ahead 3]"), (3, 0, false));
        assert_eq!(parse_track("[behind 2]"), (0, 2, false));
        assert_eq!(parse_track("[ahead 3, behind 1]"), (3, 1, false));
        assert_eq!(parse_track("[gone]"), (0, 0, true));
    }

    #[test]
    fn changelog_headings() {
        assert_eq!(parse_changelog_version("# 1.2.3"), Some("1.2.3".into()));
        assert_eq!(parse_changelog_version("## [2.0.0]"), Some("2.0.0".into()));
        assert_eq!(
            parse_changelog_version("# v1.7.48 - 2026-01-01"),
            Some("1.7.48".into())
        );
        assert_eq!(
            parse_changelog_version("# 2.0.0-rc.3"),
            Some("2.0.0-rc.3".into())
        );
        assert_eq!(parse_changelog_version("## Unreleased"), None);
        assert_eq!(parse_changelog_version("Some prose line."), None);
        assert_eq!(parse_changelog_version("# 1"), None);
    }

    #[test]
    fn porcelain_paths() {
        // Ordinary change: 8 fields precede the path.
        let rest = "M. N... 100644 100644 100644 abc123 abc123 src/main.rs";
        assert_eq!(field_tail(rest, 8), "src/main.rs");
        // Path containing spaces.
        let rest = "?? some dir/a file.txt";
        assert_eq!(field_tail(rest, 1), "?? some dir/a file.txt");
        // Rename: 9 fields precede, and the original path follows a tab.
        let rest = "R. N... 100644 100644 100644 abc abc R100 new/name.rs\told/name.rs";
        assert_eq!(field_tail(rest, 9), "new/name.rs");
        // Unmerged: 10 fields precede the path.
        let rest = "UU N... 100644 100644 100644 100644 h1 h2 h3 conflicted.rs";
        assert_eq!(field_tail(rest, 10), "conflicted.rs");
    }

    /// Build a repo shaped like a git-flow release: work on `develop`, the
    /// release merged to `master` and tagged there, then the tag merged back
    /// into `develop`.
    fn git_flow_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(path)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@example.com")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@example.com")
                .output()
                .expect("git should run");
            assert!(
                out.status.success(),
                "git {:?} failed: {}",
                args,
                String::from_utf8_lossy(&out.stderr)
            );
        };

        git(&["init", "-q", "-b", "master"]);
        std::fs::write(path.join("a.txt"), "a").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-qm", "initial"]);

        git(&["checkout", "-q", "-b", "develop"]);
        std::fs::write(path.join("b.txt"), "b").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-qm", "the work being released"]);

        // Release: develop merges to master, tagged there.
        git(&["checkout", "-q", "master"]);
        git(&[
            "merge",
            "-q",
            "--no-ff",
            "develop",
            "-m",
            "Merge release/1.0.0",
        ]);
        git(&["tag", "1.0.0"]);

        // And the tag merges back into develop, which is the whole problem.
        git(&["checkout", "-q", "develop"]);
        git(&[
            "merge",
            "-q",
            "--no-ff",
            "1.0.0",
            "-m",
            "Merge tag '1.0.0' into develop",
        ]);
        dir
    }

    /// A git-flow back-merge must not read as something to release.
    #[tokio::test]
    async fn back_merge_is_not_unreleased_work() {
        let dir = git_flow_repo();
        let cfg = Config::default();

        // The back-merge is genuinely a commit the tag cannot reach, so a raw
        // count sees one. It carries no work, so the reported count is zero.
        let raw = std::process::Command::new("git")
            .args(["rev-list", "--count", "1.0.0..HEAD"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&raw.stdout).trim(), "1");

        let refs = probe_refs(dir.path(), &cfg).await.unwrap();
        assert_eq!(refs.described_tag.map(|t| t.name), Some("1.0.0".into()));
        assert_eq!(refs.commits_since_tag, Some(0));
        assert!(refs.since_tag_subjects.is_empty());
    }

    /// Real work after the tag still counts, back-merge or not.
    #[tokio::test]
    async fn commits_after_the_back_merge_still_count() {
        let dir = git_flow_repo();
        std::fs::write(dir.path().join("c.txt"), "c").unwrap();
        for args in [
            vec!["add", "-A"],
            vec!["commit", "-qm", "a genuine fix after the release"],
        ] {
            std::process::Command::new("git")
                .args(&args)
                .current_dir(dir.path())
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@example.com")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@example.com")
                .output()
                .unwrap();
        }

        let refs = probe_refs(dir.path(), &Config::default()).await.unwrap();
        assert_eq!(refs.commits_since_tag, Some(1));
        assert_eq!(
            refs.since_tag_subjects,
            vec!["a genuine fix after the release".to_string()]
        );
    }

    #[test]
    fn tag_version_comparison() {
        assert!(tag_eq_version("1.2.3", "1.2.3"));
        assert!(tag_eq_version("v1.2.3", "1.2.3"));
        assert!(!tag_eq_version("1.2.4", "1.2.3"));
    }
}
