//! Finding the repos.
//!
//! A hand-rolled walk rather than a generic recursive iterator, because the
//! central rule is "stop descending the moment you find a repo". That single
//! rule is what keeps submodules, vendored checkouts, and test fixture repos
//! out of the list without needing to enumerate them.

use anyhow::{Context, Result};
use globset::{Glob, GlobSet, GlobSetBuilder};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::config::Config;

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
            if !cfg.follow_nested_repos {
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
