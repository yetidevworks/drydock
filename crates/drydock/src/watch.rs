//! Filesystem watching, so the dashboard reflects edits within a second or two
//! instead of waiting for the next sweep.
//!
//! This is what makes leaving the dashboard open all day reasonable. A full
//! sweep costs seconds of syscall time; re-probing one repo costs milliseconds.
//! Steady state is therefore near-idle, and only what actually changed is
//! re-read.
//!
//! Filtering happens before anything else, and matters more than it looks.
//! Watching a tree like `~/Projects` recursively means a single `cargo build`
//! or `npm install` can emit tens of thousands of events, none of which change
//! any answer this tool gives. So build output and vendored trees are dropped
//! on sight, and inside `.git` only the handful of paths that actually reflect
//! repo state are honoured.

use anyhow::{Context, Result};
use notify::RecursiveMode;
use notify_debouncer_full::{new_debouncer, DebounceEventResult};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::config::Config;

/// Keeps the watcher alive. Dropping it stops watching and ends the worker
/// thread.
pub struct Handle {
    _debouncer: Box<dyn std::any::Any + Send>,
}

/// Paths under `.git` that reflect something worth re-reading. Everything else
/// in there, `objects/**` above all, is noise.
const GIT_PATHS_OF_INTEREST: &[&str] = &[
    "HEAD",
    "index",
    "packed-refs",
    "MERGE_HEAD",
    "ORIG_HEAD",
    "CHERRY_PICK_HEAD",
    "REVERT_HEAD",
    "BISECT_LOG",
    "refs",
    "logs",
    "rebase-merge",
    "rebase-apply",
    "shallow",
];

/// Start watching every configured root. `on_change` is called with the repo
/// roots that changed, already deduplicated.
pub fn spawn<F>(cfg: Arc<Config>, on_change: F) -> Result<Handle>
where
    F: Fn(Vec<PathBuf>) + Send + 'static,
{
    let (tx, rx) = std::sync::mpsc::channel::<DebounceEventResult>();

    let mut debouncer =
        new_debouncer(cfg.debounce(), None, tx).context("Starting the filesystem watcher")?;

    // Canonicalize the roots. On macOS the events arrive with symlinks already
    // resolved (`/var/...` is reported as `/private/var/...`), so comparing
    // against an unresolved root would discard every event.
    let roots: Vec<PathBuf> = cfg
        .root_paths()
        .into_iter()
        .map(|r| r.canonicalize().unwrap_or(r))
        .collect();

    let mut watched = 0;
    for root in &roots {
        if !root.is_dir() {
            continue;
        }
        match debouncer.watch(root, RecursiveMode::Recursive) {
            Ok(()) => watched += 1,
            Err(err) => tracing::warn!(root = %root.display(), %err, "could not watch root"),
        }
    }
    if watched == 0 {
        anyhow::bail!("no scan root could be watched");
    }

    let prune: HashSet<String> = cfg.prune_names().into_iter().collect();
    std::thread::spawn(move || {
        while let Ok(result) = rx.recv() {
            let events = match result {
                Ok(events) => events,
                Err(errors) => {
                    for err in errors {
                        tracing::debug!(%err, "watch error");
                    }
                    continue;
                }
            };

            let mut changed: HashSet<PathBuf> = HashSet::new();
            for event in events {
                for path in &event.paths {
                    if !is_interesting(path, &prune) {
                        continue;
                    }
                    if let Some(repo) = owning_repo(path, &roots) {
                        changed.insert(repo);
                    }
                }
            }
            if !changed.is_empty() {
                on_change(changed.into_iter().collect());
            }
        }
    });

    Ok(Handle {
        _debouncer: Box::new(debouncer),
    })
}

/// Decide whether a changed path could possibly affect what's on screen.
fn is_interesting(path: &Path, prune: &HashSet<String>) -> bool {
    let mut inside_git = false;
    let mut git_child: Option<String> = None;

    for component in path.components() {
        let name = component.as_os_str().to_string_lossy();
        if inside_git && git_child.is_none() {
            git_child = Some(name.to_string());
            continue;
        }
        if name == ".git" {
            inside_git = true;
            continue;
        }
        if prune.contains(name.as_ref()) {
            return false;
        }
    }

    match (inside_git, git_child) {
        // A write directly to `.git` itself.
        (true, None) => true,
        // Only the paths that reflect repo state.
        (true, Some(child)) => GIT_PATHS_OF_INTEREST.iter().any(|p| *p == child),
        // An ordinary working-tree file.
        (false, _) => true,
    }
}

/// Walk up from a changed path to the checkout that contains it.
///
/// Done by looking for `.git` rather than by matching against a list of known
/// repos, so it stays correct as repos are cloned and removed without anything
/// needing to tell the watcher.
fn owning_repo(path: &Path, roots: &[PathBuf]) -> Option<PathBuf> {
    let under_a_root = |p: &Path| roots.iter().any(|r| p.starts_with(r));
    if !under_a_root(path) {
        return None;
    }

    // Cut at a `.git` segment when there is one, so `.git/refs/heads/x`
    // resolves with no filesystem probing at all.
    if let Some(pos) = path
        .components()
        .position(|c| c.as_os_str() == std::ffi::OsStr::new(".git"))
    {
        let trimmed: PathBuf = path.components().take(pos).collect();
        return (!trimmed.as_os_str().is_empty()).then_some(trimmed);
    }

    let mut current = path;
    loop {
        if current.join(".git").exists() {
            return Some(current.to_path_buf());
        }
        current = current.parent()?;
        if !under_a_root(current) {
            return None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prune_set() -> HashSet<String> {
        crate::config::DEFAULT_PRUNE
            .iter()
            .map(|s| s.to_string())
            .collect()
    }

    #[test]
    fn build_output_is_ignored() {
        let prune = prune_set();
        assert!(!is_interesting(
            Path::new("/p/yetidevworks/drydock/target/debug/build.rs"),
            &prune
        ));
        assert!(!is_interesting(
            Path::new("/p/site/node_modules/react/index.js"),
            &prune
        ));
        assert!(!is_interesting(
            Path::new("/p/plugin/vendor/pkg/src/A.php"),
            &prune
        ));
    }

    #[test]
    fn git_internals_are_mostly_ignored() {
        let prune = prune_set();
        // Object writes happen constantly and tell us nothing on their own.
        assert!(!is_interesting(
            Path::new("/p/grav/grav/.git/objects/ab/cdef"),
            &prune
        ));
        assert!(!is_interesting(
            Path::new("/p/grav/grav/.git/COMMIT_EDITMSG"),
            &prune
        ));
        // These do reflect state.
        assert!(is_interesting(Path::new("/p/grav/grav/.git/HEAD"), &prune));
        assert!(is_interesting(Path::new("/p/grav/grav/.git/index"), &prune));
        assert!(is_interesting(
            Path::new("/p/grav/grav/.git/refs/heads/develop"),
            &prune
        ));
        assert!(is_interesting(
            Path::new("/p/grav/grav/.git/MERGE_HEAD"),
            &prune
        ));
    }

    #[test]
    fn working_tree_edits_are_interesting() {
        let prune = prune_set();
        assert!(is_interesting(
            Path::new("/p/grav/grav/system/src/Grav.php"),
            &prune
        ));
    }

    /// End to end: start a real watcher on a temp tree, touch a file, and check
    /// the owning repo comes back. Guards against the watcher silently
    /// delivering nothing, which no amount of unit testing the filters would
    /// catch.
    #[test]
    fn a_real_edit_reaches_the_callback() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let repo = root.join("group").join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::write(repo.join("tracked.txt"), "before").unwrap();

        let mut cfg = Config {
            roots: vec![root.to_string_lossy().to_string()],
            ..Config::default()
        };
        cfg.refresh.debounce = "200ms".into();

        let (tx, rx) = std::sync::mpsc::channel();
        let _handle = spawn(Arc::new(cfg), move |paths| {
            let _ = tx.send(paths);
        })
        .expect("watcher should start");

        // Give the watcher a moment to register before generating events.
        std::thread::sleep(std::time::Duration::from_millis(300));
        std::fs::write(repo.join("tracked.txt"), "after").unwrap();

        let paths = rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("an edit should reach the callback");
        let canonical_repo = repo.canonicalize().unwrap();
        assert!(
            paths.iter().any(|p| p
                .canonicalize()
                .map(|c| c == canonical_repo)
                .unwrap_or(false)),
            "expected {} in {:?}",
            canonical_repo.display(),
            paths
        );
    }

    #[test]
    fn git_paths_resolve_without_touching_disk() {
        let roots = vec![PathBuf::from("/p")];
        assert_eq!(
            owning_repo(Path::new("/p/grav/grav/.git/refs/heads/develop"), &roots),
            Some(PathBuf::from("/p/grav/grav"))
        );
        assert_eq!(owning_repo(Path::new("/elsewhere/x"), &roots), None);
    }
}
