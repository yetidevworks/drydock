//! Filesystem watching, so the dashboard reflects edits within a second or two
//! instead of waiting for the next sweep.
//!
//! This is what makes leaving the dashboard open all day reasonable. A full
//! sweep costs seconds of syscall time; re-probing one repo costs milliseconds.
//! Steady state is therefore near-idle, and only what actually changed is
//! re-read.
//!
//! The watch set is per-repo and deliberately selective, and the reason is the
//! kernel's watch budget. On Linux every watched directory costs one inotify
//! watch, capped by `fs.inotify.max_user_watches` (59,483 on the machine this
//! was measured against). Watching each scan root recursively costs one watch
//! per directory in the tree: a fleet of ~550 repos is ~36,000 directories, so
//! adding another ~500 repos costs another ~33,000 watches and registration
//! starts failing with ENOSPC partway through. Failures that aren't loud leave
//! the dashboard silently degrading to the periodic sweep — exactly the
//! five-minute staleness the watcher exists to prevent. The worst offender is
//! `.git/objects`, which accumulates up to ~257 shard directories per repo
//! over time while contributing nothing to what's on screen.
//!
//! So instead of recursing over everything, each repo gets roughly 10–20
//! watches that cover exactly what can change an answer:
//!
//! - the repo root itself, non-recursive (top-level edits, plus the creation
//!   and deletion of whole subdirectories)
//! - working-tree directories, non-recursive, capped at 512 per repo — a
//!   pathological tree degrades to sweep-driven updates rather than exhausting
//!   the budget for every other repo
//! - `.git` itself, non-recursive (HEAD, index, packed-refs, FETCH_HEAD,
//!   MERGE_HEAD, …)
//! - `.git/refs` and `.git/logs` recursive, plus `.git/worktrees` when it
//!   exists, all of which stay small in any honest repo
//!
//! `.git/objects` is never watched, which is what makes the set immune to the
//! growth that breaks the recursive approach. Bare repos (no `.git` subdir,
//! but `HEAD` and `refs` at the root) are treated as their own git dir.
//!
//! The scan roots themselves are watched non-recursive purely as a cheap
//! fallback signal for top-level churn; the per-repo sets carry the real work.
//! `Handle::reconcile` diffs the desired set against what is registered after
//! every completed sweep, so repos cloned or deleted while the dashboard is
//! open are picked up without re-registering anything already watched.
//!
//! Filtering happens before anything else, and matters more than it looks.
//! A single `cargo build` or `npm install` can emit tens of thousands of
//! events, none of which change any answer this tool gives. So build output
//! and vendored trees are dropped on sight, and inside `.git` only the
//! handful of paths that actually reflect repo state are honoured.

use anyhow::{Context, Result};
use notify::{RecommendedWatcher, RecursiveMode};
use notify_debouncer_full::{new_debouncer_opt, DebounceEventResult, Debouncer, NoCache};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Mutex,
};
use walkdir::WalkDir;

use crate::config::Config;

/// Working-tree directories watched per repo, beyond the repo root and the
/// `.git` watches. A tree with more directories than this still works: the
/// deeper ones are covered by the periodic sweep instead of by the watcher.
/// The cap is what keeps one pathological checkout from consuming the whole
/// kernel budget on its own.
const MAX_WORKING_TREE_DIRS: usize = 512;

/// Paths under `.git` that reflect something worth re-reading. Everything else
/// in there, `objects/**` above all, is noise. `worktrees` counts too: the
/// watch set registers that subtree recursively, and each linked worktree
/// keeps its own HEAD, refs and logs beneath it.
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
    "worktrees",
    "rebase-merge",
    "rebase-apply",
    "shallow",
];

/// Keeps the watcher alive. Dropping it stops watching and ends the worker
/// thread.
pub struct Handle {
    shared: Arc<Mutex<WatcherState>>,
    roots: Arc<Vec<PathBuf>>,
    prune: Arc<HashSet<String>>,
    /// Reconcile sequencing: claimed per call, applied under the lock, so a
    /// slower older walk can never overwrite a newer reconcile's result.
    seq: Arc<AtomicU64>,
}

/// The debouncer and the record of what is registered share a lock so that
/// `reconcile` can add and remove watches from the TUI thread while the
/// debouncer's own thread keeps firing. The critical section is a diff plus a
/// handful of inotify syscalls — nothing in it waits on the filesystem.
struct WatcherState {
    debouncer: Debouncer<RecommendedWatcher, NoCache>,
    /// Every directory currently registered, regardless of mode.
    watched: HashMap<PathBuf, ()>,
    /// Set the first time a registration fails, so the budget warning is
    /// logged once per session instead of once per failing directory.
    budget_warned: bool,
    /// The newest reconcile sequence applied to `watched`.
    last_seq: u64,
}

impl WatcherState {
    /// Lock the shared state. A panic elsewhere must not take watching down
    /// with it, so a poisoned lock is recovered rather than unwrapped.
    fn lock_shared(shared: &Mutex<WatcherState>) -> std::sync::MutexGuard<'_, WatcherState> {
        shared.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Register one directory, recording success and reporting failure once
    /// at warn level. A failure here is almost always the kernel watch budget
    /// (ENOSPC on Linux): watching continues with whatever registered, and
    /// the periodic sweep covers the rest. That degradation is deliberate —
    /// losing the watcher is a slowdown, not a crash.
    fn register(&mut self, path: &Path, mode: RecursiveMode) {
        match self.debouncer.watch(path, mode) {
            Ok(()) => {
                self.watched.insert(path.to_path_buf(), ());
            }
            Err(err) => {
                tracing::debug!(path = %path.display(), %err, "watch registration failed");
                if !self.budget_warned {
                    self.budget_warned = true;
                    tracing::warn!(
                        watched = self.watched.len(),
                        "filesystem watch registration failed; the kernel's watch budget is \
                         likely exhausted. Watching continues with what is registered, and the \
                         periodic sweep covers the rest. On Linux, raise \
                         fs.inotify.max_user_watches"
                    );
                }
            }
        }
    }

    /// Drop one registration. A path may already be gone (a repo deleted
    /// between the diff and the unwatch), so failures here are noise.
    fn unregister(&mut self, path: &Path) {
        if let Err(err) = self.debouncer.unwatch(path) {
            tracing::debug!(path = %path.display(), %err, "watch removal failed");
        }
        self.watched.remove(path);
    }
}

/// Start watching the scan roots and the given repos. `on_change` is called
/// with the repo roots that changed, already deduplicated. `reconcile` keeps
/// the watch set in step as repos come and go.
pub fn spawn<F>(cfg: Arc<Config>, repos: Vec<PathBuf>, on_change: F) -> Result<Handle>
where
    F: Fn(Vec<PathBuf>) + Send + 'static,
{
    let (tx, rx) = std::sync::mpsc::channel::<DebounceEventResult>();

    // `NoCache`, emphatically, rather than the default recommended cache.
    //
    // On macOS the default is a file-ID map used to stitch rename events back
    // together, and building it makes `watch()` walk the entire tree with
    // `follow_links(true)`, stat-ing every file it finds. Pointed at a tree like
    // `~/Projects` that means every `node_modules`, `target` and `.git/objects`
    // in it, plus anything symlinks lead to, all synchronously before the call
    // returns, and all retained in memory.
    //
    // Nothing here needs rename stitching: an event's only job is to name the
    // repo that changed, which `owning_repo` derives from the path. So the walk
    // buys nothing and costs everything.
    let debouncer: Debouncer<RecommendedWatcher, NoCache> = new_debouncer_opt(
        cfg.debounce(),
        None,
        tx,
        NoCache::new(),
        notify::Config::default(),
    )
    .context("Starting the filesystem watcher")?;

    // Canonicalize the roots. On macOS the events arrive with symlinks already
    // resolved (`/var/...` is reported as `/private/var/...`), so comparing
    // against an unresolved root would discard every event.
    let roots: Vec<PathBuf> = cfg
        .root_paths()
        .into_iter()
        .map(|r| r.canonicalize().unwrap_or(r))
        .collect();

    let prune: Arc<HashSet<String>> = Arc::new(cfg.prune_names().into_iter().collect());

    let mut state = WatcherState {
        debouncer,
        watched: HashMap::new(),
        budget_warned: false,
        last_seq: 0,
    };

    // Root watches are non-recursive and exist only as a cheap fallback signal
    // for top-level churn — the per-repo sets below carry the real work.
    let mut watched_roots = 0;
    for root in &roots {
        if !root.is_dir() {
            continue;
        }
        state.register(root, RecursiveMode::NonRecursive);
        watched_roots += 1;
    }
    if watched_roots == 0 {
        anyhow::bail!("no scan root could be watched");
    }

    // Per-repo sets register on the reconcile thread, not here: the initial
    // registration walks every working tree, and spawn runs on the
    // dashboard's event loop, where a walk of 500 trees reads as a freeze.

    let shared = Arc::new(Mutex::new(state));

    let thread_roots = roots.clone();
    let thread_prune = Arc::clone(&prune);
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
                    if !is_interesting(path, &thread_prune) {
                        continue;
                    }
                    if let Some(repo) = owning_repo(path, &thread_roots) {
                        changed.insert(repo);
                    }
                }
            }
            if !changed.is_empty() {
                on_change(changed.into_iter().collect());
            }
        }
    });

    let handle = Handle {
        shared,
        roots: Arc::new(roots),
        prune,
        seq: Arc::new(AtomicU64::new(0)),
    };
    handle.reconcile(&repos);
    Ok(handle)
}

impl Handle {
    /// Reconcile the watch set with the repos now known. Idempotent; diffs
    /// against what is registered. Called after every completed sweep.
    ///
    /// Registration is incremental: a directory already watched is never
    /// re-registered, so a steady-state sweep costs one diff, not a storm of
    /// inotify_add_watch calls.
    pub fn reconcile(&self, repos: &[PathBuf]) {
        // A sequence number is claimed per call and checked under the lock:
        // reconcile runs on a detached thread, and a walk that started
        // earlier must never overwrite a reconcile that started later.
        let seq = self.seq.fetch_add(1, Ordering::SeqCst) + 1;
        let shared = Arc::clone(&self.shared);
        let roots = Arc::clone(&self.roots);
        let prune = Arc::clone(&self.prune);
        let repos = repos.to_vec();
        // The expensive part — walking every working tree in the fleet —
        // happens here, on a thread of its own. It used to run on the
        // caller's thread, which in the dashboard is the event loop: each
        // sweep's end read as a UI freeze lasting as long as the walk.
        // Only the short diff-and-register section ever takes the lock.
        let _ = std::thread::Builder::new()
            .name("drydock watch reconcile".to_string())
            .spawn(move || {
                let mut desired: Vec<PathBuf> = Vec::new();
                let mut modes: HashMap<PathBuf, RecursiveMode> = HashMap::new();
                for root in roots.iter().filter(|r| r.is_dir()) {
                    modes
                        .entry(root.clone())
                        .or_insert(RecursiveMode::NonRecursive);
                    desired.push(root.clone());
                }
                for repo in &repos {
                    for (dir, mode) in watch_set_for(repo, &prune) {
                        modes.entry(dir.clone()).or_insert(mode);
                        desired.push(dir);
                    }
                }

                let mut state = WatcherState::lock_shared(&shared);
                if state.last_seq >= seq {
                    return;
                }
                state.last_seq = seq;
                let (add, remove) = diff_watch_sets(&state.watched, &desired);
                for path in remove {
                    state.unregister(&path);
                }
                for path in add {
                    let mode = modes
                        .get(&path)
                        .copied()
                        .unwrap_or(RecursiveMode::NonRecursive);
                    state.register(&path, mode);
                }
            });
    }
}

/// Split a desired watch set into additions and removals relative to what is
/// currently registered. Both halves are sorted, so tests and logs see a
/// deterministic order no matter how the callers collected their paths.
fn diff_watch_sets(
    current: &HashMap<PathBuf, ()>,
    desired: &[PathBuf],
) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let mut add: Vec<PathBuf> = desired
        .iter()
        .filter(|d| !current.contains_key(*d))
        .cloned()
        .collect();
    add.sort();
    add.dedup();

    let mut remove: Vec<PathBuf> = current
        .keys()
        .filter(|c| !desired.contains(c))
        .cloned()
        .collect();
    remove.sort();

    (add, remove)
}

/// The paths registered for one repo, without modes — the fixture-friendly
/// view used by tests. See [`watch_set_for`] for the real thing.
#[cfg(test)]
fn watch_dirs_for(repo: &Path, prune: &HashSet<String>) -> Vec<PathBuf> {
    watch_set_for(repo, prune)
        .into_iter()
        .map(|(dir, _)| dir)
        .collect()
}

/// The directories to watch for one repo, with their modes. Returns an empty
/// set for a repo that does not exist yet; `reconcile` picks it up once a
/// sweep reports it. The set is the one described in the module docs: root,
/// capped working-tree dirs, and the small `.git` subtrees that actually move.
fn watch_set_for(repo: &Path, prune: &HashSet<String>) -> Vec<(PathBuf, RecursiveMode)> {
    // Canonicalize so registrations and later diffs agree with each other and
    // with the canonicalized scan roots, regardless of how the caller spelled
    // the path.
    let repo = repo.canonicalize().unwrap_or_else(|_| repo.to_path_buf());
    if !repo.is_dir() {
        return Vec::new();
    }

    let mut out = vec![(repo.clone(), RecursiveMode::NonRecursive)];

    let git_dir = repo.join(".git");
    let bare = !git_dir.is_dir() && repo.join("HEAD").is_file() && repo.join("refs").is_dir();

    if bare {
        // A bare repo is its own git dir: the root watch above already covers
        // HEAD, index and friends, so only the recursively-varying subtrees
        // need watches of their own.
        for sub in ["refs", "logs", "worktrees"] {
            let dir = repo.join(sub);
            if dir.is_dir() {
                out.push((dir, RecursiveMode::Recursive));
            }
        }
        return out;
    }

    if !git_dir.is_dir() {
        // Not a checkout yet — the root watch is enough until a clone lands
        // and the next reconcile registers the rest.
        return out;
    }

    out.push((git_dir.clone(), RecursiveMode::NonRecursive));
    for sub in ["refs", "logs", "worktrees"] {
        let dir = git_dir.join(sub);
        if dir.is_dir() {
            out.push((dir, RecursiveMode::Recursive));
        }
    }

    // Working-tree directories, capped. Skipping `.git` here is what keeps
    // `objects/**` (and everything else in there) off the watch list.
    let mut working = 0;
    let walk = WalkDir::new(&repo)
        .min_depth(1)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|e| {
            let name = e.file_name().to_string_lossy();
            name.as_ref() != ".git" && !prune.contains(name.as_ref())
        });
    for entry in walk.flatten() {
        if !entry.file_type().is_dir() {
            continue;
        }
        if working >= MAX_WORKING_TREE_DIRS {
            break;
        }
        working += 1;
        out.push((entry.into_path(), RecursiveMode::NonRecursive));
    }

    out
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
        // A bare repo has no `.git` inside it — it *is* the git dir. Detect
        // that shape so events from its watched refs and logs resolve to the
        // repo instead of being dropped.
        if current.join("HEAD").is_file() && current.join("refs").is_dir() {
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

    fn watched_count(handle: &Handle) -> usize {
        WatcherState::lock_shared(&handle.shared).watched.len()
    }

    /// Reconcile applies on a background thread; tests wait for the
    /// registration to land instead of racing it. Returns the final count.
    fn wait_watched(handle: &Handle, min: usize) -> usize {
        for _ in 0..250 {
            let n = watched_count(handle);
            if n >= min {
                return n;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        watched_count(handle)
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

    /// The watch set registers `.git/worktrees` recursively, and each linked
    /// worktree keeps its own HEAD and refs below it — those writes have to
    /// stay interesting even though they are two levels under `.git`.
    #[test]
    fn linked_worktree_writes_are_interesting() {
        let prune = prune_set();
        assert!(is_interesting(
            Path::new("/p/grav/grav/.git/worktrees"),
            &prune
        ));
        assert!(is_interesting(
            Path::new("/p/grav/grav/.git/worktrees/wt/HEAD"),
            &prune
        ));
        assert!(is_interesting(
            Path::new("/p/grav/grav/.git/worktrees/wt/refs/heads/topic"),
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

    #[test]
    fn per_repo_set_covers_the_tree_but_not_git_internals() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        for dir in [
            "src",
            "src/deep",
            ".git/refs/heads",
            ".git/logs/refs/heads",
            ".git/objects/ab",
            ".git/worktrees/wt",
            "target/debug",
            "node_modules/x",
        ] {
            std::fs::create_dir_all(repo.join(dir)).unwrap();
        }

        let dirs = watch_dirs_for(&repo, &prune_set());
        let has = |rel: &str| dirs.contains(&repo.join(rel));
        // Working-tree dirs and the git dir itself are watched…
        assert!(has("src") && has("src/deep"));
        assert!(has(".git"));
        // …but objects and pruned trees are not, and neither is anything
        // beneath the skipped directories.
        assert!(!has(".git/objects") && !has(".git/objects/ab"));
        assert!(!has("target") && !has("target/debug"));
        assert!(!has("node_modules") && !has("node_modules/x"));

        // Modes: only the small git subtrees recurse.
        let set = watch_set_for(&repo, &prune_set());
        let mode_of = |rel: &str| {
            set.iter()
                .find(|(p, _)| p == &repo.join(rel))
                .map(|(_, m)| *m)
        };
        assert!(matches!(
            mode_of(".git/refs"),
            Some(RecursiveMode::Recursive)
        ));
        assert!(matches!(
            mode_of(".git/logs"),
            Some(RecursiveMode::Recursive)
        ));
        assert!(matches!(
            mode_of(".git/worktrees"),
            Some(RecursiveMode::Recursive)
        ));
        assert!(matches!(mode_of("src"), Some(RecursiveMode::NonRecursive)));
        assert!(matches!(mode_of(".git"), Some(RecursiveMode::NonRecursive)));
    }

    /// A tree with more directories than the cap still gets a bounded watch
    /// set: the surplus is covered by the sweep instead of eating the kernel
    /// budget.
    #[test]
    fn working_tree_dirs_are_capped() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(repo.join(".git/refs")).unwrap();
        for i in 0..MAX_WORKING_TREE_DIRS + 100 {
            std::fs::create_dir_all(repo.join(format!("d{i}"))).unwrap();
        }

        let dirs = watch_dirs_for(&repo, &prune_set());
        let top_level = dirs
            .iter()
            .filter(|d| {
                d.parent() == Some(repo.as_path())
                    && d.file_name() != Some(std::ffi::OsStr::new(".git"))
            })
            .count();
        assert_eq!(top_level, MAX_WORKING_TREE_DIRS);
        // repo root + .git + .git/refs, plus the capped working-tree dirs.
        assert_eq!(dirs.len(), MAX_WORKING_TREE_DIRS + 3);
    }

    #[test]
    fn bare_repos_treat_the_root_as_the_git_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo.git");
        std::fs::create_dir_all(repo.join("refs/heads")).unwrap();
        std::fs::create_dir_all(repo.join("logs/refs")).unwrap();
        std::fs::write(repo.join("HEAD"), "ref: refs/heads/main\n").unwrap();

        let dirs = watch_dirs_for(&repo, &prune_set());
        assert!(dirs.contains(&repo));
        assert!(dirs.contains(&repo.join("refs")));
        assert!(dirs.contains(&repo.join("logs")));
        // Nothing pretends there is a `.git` in a bare repo.
        assert!(!dirs.iter().any(|d| d
            .components()
            .any(|c| c.as_os_str() == std::ffi::OsStr::new(".git"))));

        // A repo that does not exist yet yields nothing to watch; reconcile
        // registers it once a sweep reports it.
        assert!(watch_dirs_for(&tmp.path().join("missing"), &prune_set()).is_empty());
    }

    #[test]
    fn diff_adds_new_and_removes_gone() {
        let a = PathBuf::from("/a");
        let b = PathBuf::from("/b");
        let c = PathBuf::from("/c");
        let current: HashMap<PathBuf, ()> = [(a.clone(), ()), (b.clone(), ())].into();

        let (add, remove) = diff_watch_sets(&current, &[b.clone(), c.clone()]);
        assert_eq!(add, vec![c.clone()]);
        assert_eq!(remove, vec![a.clone()]);

        // No change at all means no work.
        let (add, remove) = diff_watch_sets(&current, &[a.clone(), b.clone()]);
        assert!(add.is_empty() && remove.is_empty());
    }

    /// End to end over the reconcile path: nothing beyond the root is watched
    /// until a repo is reconciled in, and reconciling the same repo twice
    /// registers nothing new.
    #[test]
    fn reconcile_is_incremental_and_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("scan");
        let repo = root.join("repo");
        std::fs::create_dir_all(repo.join(".git/refs")).unwrap();

        let mut cfg = Config {
            roots: vec![root.to_string_lossy().to_string()],
            ..Config::default()
        };
        cfg.refresh.debounce = "50ms".into();

        let handle = spawn(Arc::new(cfg), Vec::new(), |_| {}).unwrap();
        let before = watched_count(&handle);
        handle.reconcile(std::slice::from_ref(&repo));
        let after = wait_watched(&handle, before + 1);
        assert!(after > before, "reconcile should register the new repo");

        handle.reconcile(std::slice::from_ref(&repo));
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert_eq!(
            watched_count(&handle),
            after,
            "reconciling the same repo must not re-register anything"
        );
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
        let _handle = spawn(Arc::new(cfg), vec![repo.clone()], move |paths| {
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

    /// The watcher must start promptly even on a very large tree.
    ///
    /// This is a regression test with teeth: the default debouncer cache walks
    /// the whole tree inside `watch()`, which froze the dashboard before its
    /// first frame. Run with `cargo test -- --ignored` (it needs a real, large
    /// directory, so it isn't part of the normal run).
    #[test]
    #[ignore = "needs a large real tree; run explicitly"]
    fn watcher_starts_promptly_on_a_large_tree() {
        let cfg = Config::default();
        let root = cfg.root_paths().into_iter().next().unwrap();
        if !root.is_dir() {
            eprintln!("skipping: {} is not a directory", root.display());
            return;
        }

        let started = std::time::Instant::now();
        let handle = spawn(Arc::new(cfg), Vec::new(), |_| {}).expect("watcher should start");
        let elapsed = started.elapsed();
        drop(handle);

        println!("watch::spawn on {} took {elapsed:?}", root.display());
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "watch::spawn took {elapsed:?}; it must not walk the tree"
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

    /// Bare repos have no `.git` segment to cut at, so their events resolve
    /// by recognizing the git-dir shape on the way up — which does touch the
    /// disk, so this needs a real fixture. Without the shape check, every
    /// event from a watched bare repo would be dropped.
    #[test]
    fn bare_repo_paths_resolve_to_the_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("scan");
        let repo = root.join("repo.git");
        std::fs::create_dir_all(repo.join("refs/heads")).unwrap();
        std::fs::write(repo.join("HEAD"), "ref: refs/heads/main\n").unwrap();

        let roots = vec![root.canonicalize().unwrap()];
        let repo = repo.canonicalize().unwrap();
        assert_eq!(
            owning_repo(&repo.join("refs/heads/main"), &roots),
            Some(repo)
        );
    }
}
