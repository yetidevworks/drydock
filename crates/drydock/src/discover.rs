//! Finding the repos.
//!
//! A hand-rolled walk rather than a generic recursive iterator, because the
//! central rule is "stop descending the moment you find a repo". That single
//! rule is what keeps submodules, vendored checkouts, and test fixture repos
//! out of the list without needing to enumerate them. Bare repos are the one
//! exception: having no working tree, they can't contain a nested checkout in
//! the first place, and what they usually do contain is their own worktrees.

use anyhow::{Context, Result};
use globset::{Glob, GlobSet, GlobSetBuilder};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::git;

#[derive(Clone, Debug)]
pub struct Discovered {
    pub root: PathBuf,
    pub group: String,
    pub name: String,
}

#[derive(Debug, Default)]
pub struct DiscoveryStats {
    pub dirs_visited: usize,
    pub repos_found: usize,
    pub pruned: usize,
    pub unreadable: usize,
}

/// Walk every configured root and return the repos found, sorted by path.
pub fn discover(cfg: &Config) -> Result<(Vec<Discovered>, DiscoveryStats)> {
    let excludes = build_excludes(&cfg.exclude)?;
    let prune: HashSet<String> = cfg.prune_names().into_iter().collect();
    let mut stats = DiscoveryStats::default();
    let mut found = Vec::new();
    let mut seen: HashSet<PathBuf> = HashSet::new();

    for root in cfg.root_paths() {
        if !root.is_dir() {
            tracing::warn!(root = %root.display(), "scan root does not exist");
            continue;
        }
        let canonical_root = root.canonicalize().unwrap_or_else(|_| root.clone());
        walk_root(
            &canonical_root,
            cfg,
            &excludes,
            &prune,
            &mut found,
            &mut seen,
            &mut stats,
        );
    }

    found.sort_by(|a, b| a.root.cmp(&b.root));
    stats.repos_found = found.len();
    Ok((found, stats))
}

fn walk_root(
    root: &Path,
    cfg: &Config,
    excludes: &Option<GlobSet>,
    prune: &HashSet<String>,
    found: &mut Vec<Discovered>,
    seen: &mut HashSet<PathBuf>,
    stats: &mut DiscoveryStats,
) {
    // Depth-first with an explicit stack. Depth counts path segments below the
    // root, so `~/Projects/grav/grav-plugin-api` sits at depth 2.
    let mut stack: Vec<(PathBuf, usize)> = vec![(root.to_path_buf(), 0)];

    while let Some((dir, depth)) = stack.pop() {
        stats.dirs_visited += 1;

        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(err) => {
                stats.unreadable += 1;
                tracing::debug!(dir = %dir.display(), %err, "unreadable directory");
                continue;
            }
        };

        let mut children: Vec<(PathBuf, bool)> = Vec::new();
        let mut is_repo = false;

        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let path = entry.path();

            if name == ".git" {
                // Either a directory, or a file pointing elsewhere for a
                // worktree or submodule. `git::resolve_git_dir` handles both,
                // so the distinction doesn't need recording here.
                is_repo = true;
                continue;
            }

            let file_type = match entry.file_type() {
                Ok(ft) => ft,
                Err(_) => continue,
            };
            let is_symlink = file_type.is_symlink();
            if is_symlink && !cfg.follow_symlinks {
                continue;
            }
            let is_dir = if is_symlink {
                path.is_dir()
            } else {
                file_type.is_dir()
            };
            if !is_dir {
                continue;
            }

            // Hidden directories are almost never project checkouts, and
            // descending them turns up caches and tool state.
            if name.starts_with('.') {
                continue;
            }
            if prune.contains(name.as_ref()) {
                stats.pruned += 1;
                continue;
            }
            if let Some(set) = excludes {
                if let Ok(rel) = path.strip_prefix(root) {
                    if set.is_match(rel) {
                        stats.pruned += 1;
                        continue;
                    }
                }
            }
            children.push((path, is_symlink));
        }

        if is_repo {
            // Canonicalize so a symlinked path and its target can't both land
            // in the list.
            let canonical = dir.canonicalize().unwrap_or_else(|_| dir.clone());
            if seen.insert(canonical) && dir != root {
                let (group, name) = split_slug(root, &dir);
                found.push(Discovered {
                    root: dir.clone(),
                    group,
                    name,
                });
            }
            // A bare repo has no working tree, so nothing can be nested
            // *inside* one — which is the only thing `follow_nested_repos`
            // guards against. Its subdirectories are almost always its
            // worktrees, which is the entire point of the bare-plus-worktrees
            // layout, so descend regardless of the setting.
            if !cfg.follow_nested_repos && !git::is_bare(&dir) {
                continue;
            }
        }

        if depth >= cfg.max_depth {
            continue;
        }
        for (child, _) in children {
            stack.push((child, depth + 1));
        }
    }
}

/// Split a repo path into a group (its first segment below the root) and a
/// name (everything after). A repo sitting directly in a root has no group.
fn split_slug(root: &Path, repo: &Path) -> (String, String) {
    let rel = match repo.strip_prefix(root) {
        Ok(r) => r,
        Err(_) => return (String::new(), repo.display().to_string()),
    };
    let parts: Vec<String> = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().to_string())
        .collect();
    match parts.len() {
        0 => (String::new(), root.display().to_string()),
        1 => (String::new(), parts[0].clone()),
        _ => (parts[0].clone(), parts[1..].join("/")),
    }
}

/// Build the exclude matcher. Patterns are matched against a directory's path
/// relative to its scan root. A pattern ending in `/**` also excludes the
/// directory it names, so `foo/**` and `foo` both keep `foo` out entirely.
fn build_excludes(patterns: &[String]) -> Result<Option<GlobSet>> {
    if patterns.is_empty() {
        return Ok(None);
    }
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        let glob =
            Glob::new(pattern).with_context(|| format!("Invalid exclude pattern: {pattern}"))?;
        builder.add(glob);
        if let Some(prefix) = pattern.strip_suffix("/**") {
            if !prefix.is_empty() {
                let glob = Glob::new(prefix)
                    .with_context(|| format!("Invalid exclude pattern: {prefix}"))?;
                builder.add(glob);
            }
        }
    }
    Ok(Some(builder.build().context("Building exclude set")?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_splitting() {
        let root = Path::new("/p");
        assert_eq!(
            split_slug(root, Path::new("/p/grav/grav-plugin-api")),
            ("grav".to_string(), "grav-plugin-api".to_string())
        );
        assert_eq!(
            split_slug(root, Path::new("/p/loose-repo")),
            (String::new(), "loose-repo".to_string())
        );
        assert_eq!(
            split_slug(root, Path::new("/p/a/b/c")),
            ("a".to_string(), "b/c".to_string())
        );
    }

    /// The layout from issue #4: a bare repo whose worktrees live beside it.
    /// Returns the scan root.
    fn bare_repo_with_worktrees() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        let upstream = root.join("upstream");
        std::fs::create_dir_all(&upstream).unwrap();
        let git = |cwd: &Path, args: &[&str]| {
            let ok = std::process::Command::new("git")
                .current_dir(cwd)
                .args(args)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@e")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@e")
                .output()
                .unwrap();
            assert!(ok.status.success(), "git {args:?}: {ok:?}");
        };
        git(&upstream, &["init", "-q", "-b", "main", "."]);
        std::fs::write(upstream.join("a.txt"), "hi").unwrap();
        git(&upstream, &["add", "-A"]);
        git(&upstream, &["commit", "-qm", "init"]);

        let repo = root.join("dev").join("myrepo");
        std::fs::create_dir_all(&repo).unwrap();
        git(
            &repo,
            &["clone", "-q", "--bare", upstream.to_str().unwrap(), ".git"],
        );
        git(
            &repo,
            &["--git-dir=.git", "worktree", "add", "-q", "trunk", "main"],
        );
        dir
    }

    /// `group/name` for everything discovered, sorted, so assertions read the
    /// way the table does.
    fn slugs_found(root: &Path, follow_nested: bool) -> Vec<String> {
        let cfg = Config {
            roots: vec![root.to_string_lossy().to_string()],
            follow_nested_repos: follow_nested,
            ..Config::default()
        };
        let (found, _) = discover(&cfg).unwrap();
        let mut slugs: Vec<String> = found
            .iter()
            .map(|d| {
                if d.group.is_empty() {
                    d.name.clone()
                } else {
                    format!("{}/{}", d.group, d.name)
                }
            })
            .collect();
        slugs.sort();
        slugs
    }

    // Issue #4: the bare repo is what discovery finds first, and pruning at
    // that point hides the worktrees -- which are the only things in the
    // layout with a working tree to report on.
    #[test]
    fn a_bare_repos_worktrees_are_found_without_following_nested_repos() {
        let dir = bare_repo_with_worktrees();
        let slugs = slugs_found(&dir.path().join("dev"), false);
        assert!(
            slugs.contains(&"myrepo/trunk".to_string()),
            "worktree missing from {slugs:?}"
        );
        assert!(
            slugs.contains(&"myrepo".to_string()),
            "bare repo itself missing from {slugs:?}"
        );
    }

    // The rule bare repos are an exception to still has to hold for everyone
    // else: a checkout nested inside a normal working tree stays pruned.
    #[test]
    fn a_repo_nested_in_a_working_tree_is_still_pruned() {
        let dir = bare_repo_with_worktrees();
        let nested = dir.path().join("dev").join("myrepo").join("trunk");
        std::fs::create_dir_all(nested.join("vendored").join(".git")).unwrap();
        let slugs = slugs_found(&dir.path().join("dev"), false);
        assert!(
            !slugs.iter().any(|n| n.contains("vendored")),
            "vendored checkout should have been pruned, got {slugs:?}"
        );
    }

    #[test]
    fn excludes_match_relative_paths() {
        let set = build_excludes(&["riffle-testbed/**".into()])
            .unwrap()
            .unwrap();
        assert!(set.is_match(Path::new("riffle-testbed/work")));
        assert!(set.is_match(Path::new("riffle-testbed")));
        assert!(!set.is_match(Path::new("grav/grav")));
    }
}
