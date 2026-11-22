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
            // in the list. The root itself counts too, so a single checkout
            // like `~/.dotfiles` can be a root of its own.
            let canonical = dir.canonicalize().unwrap_or_else(|_| dir.clone());
            if seen.insert(canonical) {
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

/// Whether a full walk would list this checkout, and under what name.
///
/// For the callers that are handed one repo rather than walking for it -- a
/// `drydock scan <path>`, or the dashboard noticing a new worktree -- so
/// neither can add a row a sweep would never have found, and then lose it
/// again at the next one. The rules are [`walk_root`]'s, applied to the one
/// path: under a root, within `max_depth`, no hidden, pruned or excluded
/// directory on the way down, and not inside another checkout's working tree
/// unless `follow_nested_repos` is on.
pub fn admit(cfg: &Config, path: &Path) -> Option<Discovered> {
    let path = path.canonicalize().ok()?;
    if !path.join(".git").exists() {
        return None;
    }
    let excludes = build_excludes(&cfg.exclude).ok()?;
    let prune: HashSet<String> = cfg.prune_names().into_iter().collect();
    for root in cfg.root_paths() {
        let root = root.canonicalize().unwrap_or(root);
        let Ok(rel) = path.strip_prefix(&root) else {
            continue;
        };
        if !reachable(&root, rel, cfg, &excludes, &prune) {
            continue;
        }
        let (group, name) = split_slug(&root, &path);
        return Some(Discovered {
            root: path,
            group,
            name,
        });
    }
    None
}

/// Whether [`walk_root`] would get from `root` down to `root/rel`.
fn reachable(
    root: &Path,
    rel: &Path,
    cfg: &Config,
    excludes: &Option<GlobSet>,
    prune: &HashSet<String>,
) -> bool {
    if rel.components().count() > cfg.max_depth {
        return false;
    }
    let mut dir = root.to_path_buf();
    for component in rel.components() {
        // Every directory above the repo, the root included, has to be one
        // the walk keeps descending through.
        if dir.join(".git").exists() && !cfg.follow_nested_repos && !git::is_bare(&dir) {
            return false;
        }
        let name = component.as_os_str().to_string_lossy();
        if name.starts_with('.') || prune.contains(name.as_ref()) {
            return false;
        }
        dir.push(component);
        if let (Some(set), Ok(rel)) = (excludes, dir.strip_prefix(root)) {
            if set.is_match(rel) {
                return false;
            }
        }
    }
    true
}

/// Split a repo path into a group (its first segment below the root) and a
/// name (everything after). A repo sitting directly in a root has no group,
/// and neither does a root that is a repo, which is named after its directory.
pub(crate) fn split_slug(root: &Path, repo: &Path) -> (String, String) {
    let rel = match repo.strip_prefix(root) {
        Ok(r) => r,
        Err(_) => return (String::new(), repo.display().to_string()),
    };
    let parts: Vec<String> = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().to_string())
        .collect();
    match parts.len() {
        0 => (
            String::new(),
            root.file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| root.display().to_string()),
        ),
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

    // Issue #15: a repo handed over by path gets the walk's answer, both for
    // whether it's listed and for the group and name it's listed under.
    #[test]
    fn admitting_one_repo_agrees_with_the_walk() {
        let dir = bare_repo_with_worktrees();
        let base = dir.path().canonicalize().unwrap();
        let repo = base.join("dev").join("myrepo");
        std::fs::create_dir_all(repo.join("trunk").join("vendored").join(".git")).unwrap();
        std::fs::create_dir_all(base.join("dev").join("skipme").join(".git")).unwrap();
        std::fs::create_dir_all(base.join("dev").join("node_modules/x/.git")).unwrap();

        let cfg = Config {
            roots: vec![base.join("dev").to_string_lossy().to_string()],
            exclude: vec!["skipme/**".into()],
            ..Config::default()
        };
        let (found, _) = discover(&cfg).unwrap();
        assert!(!found.is_empty());
        for d in &found {
            let admitted = admit(&cfg, &d.root).expect("the walk found it");
            assert_eq!(
                (&admitted.root, &admitted.group, &admitted.name),
                (&d.root, &d.group, &d.name)
            );
        }

        for left_out in [
            repo.join("trunk").join("vendored"),     // inside a working tree
            base.join("dev").join("skipme"),         // excluded
            base.join("dev").join("node_modules/x"), // pruned
            base.join("upstream"),                   // outside every root
            base.join("dev"),                        // a root, but not a repo
        ] {
            assert!(
                admit(&cfg, &left_out).is_none(),
                "{} should not be admitted",
                left_out.display()
            );
            assert!(!found.iter().any(|d| d.root == left_out));
        }

        let shallow = Config {
            max_depth: 1,
            ..cfg.clone()
        };
        assert!(admit(&shallow, &repo).is_some());
        assert!(admit(&shallow, &repo.join("trunk")).is_none());
    }

    // A root can be one checkout rather than a directory of them, like
    // `~/.dotfiles`: it's listed under its own name, with no group.
    #[test]
    fn a_root_that_is_a_repo_is_listed() {
        let dir = bare_repo_with_worktrees();
        let base = dir.path().canonicalize().unwrap();
        let upstream = base.join("upstream");
        std::fs::create_dir_all(upstream.join("vendored").join(".git")).unwrap();

        assert_eq!(slugs_found(&upstream, false), vec!["upstream".to_string()]);

        let cfg = Config {
            roots: vec![upstream.to_string_lossy().to_string()],
            ..Config::default()
        };
        let admitted = admit(&cfg, &upstream).expect("the root is a repo");
        assert_eq!(
            (admitted.group.as_str(), admitted.name.as_str()),
            ("", "upstream")
        );
        assert!(admit(&cfg, &upstream.join("vendored")).is_none());

        // A bare root still descends to its worktrees.
        let slugs = slugs_found(&base.join("dev").join("myrepo"), false);
        assert_eq!(slugs, vec!["myrepo".to_string(), "trunk".to_string()]);
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
