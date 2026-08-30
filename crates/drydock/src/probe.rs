//! Sweep orchestration: discovery, then tier 1 for everything, then tier 2
//! for whatever needs it.
//!
//! The split matters. On a tree of ~550 repos, the cheap tier costs a couple of
//! seconds and the working-tree scans cost roughly forty seconds of syscall
//! time. So tier 1 runs on every sweep, and tier 2 runs only where the cache
//! says something moved, ordered so the most recently active repos land first
//! and the top of the table fills in straight away.

use anyhow::Result;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, Semaphore};
use tokio::task::JoinSet;

use crate::cache;
use crate::config::Config;
use crate::discover::{self, Discovered};
use crate::git;
use crate::model::{RepoStatus, VisibilityInfo};
use crate::provider;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tier {
    /// Refs, tags and `.git` metadata only. Fast.
    Refs,
    /// Also scan working trees. Slow, and cached.
    Full,
}

/// Progress from a sweep in flight. The dashboard consumes these to repaint as
/// results arrive; the CLI ignores them and takes the final vector.
#[derive(Debug)]
pub enum Event {
    /// The repos the walk turned up. Carries the full list so a live dashboard
    /// can drop rows for repos that have gone away.
    Discovered {
        roots: Vec<PathBuf>,
    },
    /// Tier 1 finished for one repo.
    Refs(Box<RepoStatus>),
    /// Tier 2 finished (or was served from cache) for one repo.
    Work(Box<RepoStatus>),
    Phase {
        name: &'static str,
        elapsed: Duration,
    },
    Done {
        elapsed: Duration,
    },
}

#[derive(Debug, Default, Clone)]
pub struct Timings {
    pub discovery: Duration,
    /// Time spent in the optional fetch phase. Zero when it didn't run, which
    /// is the usual case: fetching is opt-in everywhere.
    pub fetch: Duration,
    pub refs: Duration,
    pub work: Duration,
    pub total: Duration,
    pub repos: usize,
    pub work_scanned: usize,
    pub work_cached: usize,
    pub fetched: usize,
    pub fetch_failed: usize,
}

pub struct Fleet {
    pub repos: Vec<RepoStatus>,
    pub timings: Timings,
}

/// Walk the roots. Blocking filesystem work, so it goes on a blocking thread.
pub async fn discover_repos(cfg: Arc<Config>) -> Result<Vec<Discovered>> {
    tokio::task::spawn_blocking(move || {
        discover::discover(&cfg).map(|(repos, stats)| {
            tracing::debug!(
                dirs = stats.dirs_visited,
                repos = stats.repos_found,
                pruned = stats.pruned,
                "discovery complete"
            );
            repos
        })
    })
    .await?
}

/// What tier 2 did for a repo, so a sweep can report how much of the expensive
/// work it managed to avoid.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkOutcome {
    Scanned,
    Cached,
    Skipped,
}

/// Tier 1 for one repo. On failure the cached values are carried forward, since
/// a stale row beats a blank one.
async fn fill_refs(status: &mut RepoStatus, cfg: &Config, cached: Option<&RepoStatus>) -> bool {
    status.refs_probed_at = git::now_unix();
    match git::probe_refs(&status.root, cfg).await {
        Ok(refs) => {
            status.refs = Some(refs);
            true
        }
        Err(err) => {
            status.error = Some(format!("{err:#}"));
            if let Some(prev) = cached {
                status.refs = prev.refs.clone();
                status.work = prev.work.clone();
                status.work_key = prev.work_key.clone();
                status.work_probed_at = prev.work_probed_at;
            }
            false
        }
    }
}

/// Cached counts go stale even when the key still matches: editing a tracked
/// file touches neither HEAD nor the index, so the key alone would call an
/// edited-but-unstaged repo clean forever. The key is what makes a sweep cheap,
/// so keep it and put a ceiling on how long it is trusted.
fn work_expired(prev: &RepoStatus, cfg: &Config, now: i64) -> bool {
    match cfg.work_max_age() {
        Some(max_age) => now.saturating_sub(prev.work_probed_at) >= max_age.as_secs() as i64,
        None => false,
    }
}

/// Tier 2 for one repo, reusing the cache when HEAD and the index both match.
///
/// `force` skips that reuse. The watcher passes it, because a filesystem event
/// is direct evidence the working tree moved — evidence the key cannot carry.
async fn fill_work(
    status: &mut RepoStatus,
    cfg: &Config,
    cached: Option<&RepoStatus>,
    tier: Tier,
    force: bool,
    work_permit: &Semaphore,
) -> WorkOutcome {
    // A bare repo has no working tree, so `git status` can only ever fail
    // here. Bail before running it rather than recording an error against a
    // repo that is behaving exactly as intended.
    if status.is_bare() {
        return WorkOutcome::Skipped;
    }

    let head_sha = status
        .refs
        .as_ref()
        .and_then(|r| r.current_branch().map(|b| b.sha.clone()));
    let key = git::work_key(&status.root, head_sha);

    if !force {
        if let Some(prev) = cached {
            if prev.work.is_some()
                && prev.work_key.as_ref() == Some(&key)
                && !work_expired(prev, cfg, git::now_unix())
            {
                status.work = prev.work.clone();
                status.work_key = Some(key);
                status.work_probed_at = prev.work_probed_at;
                return WorkOutcome::Cached;
            }
        }
    }

    if tier == Tier::Refs {
        // Carry stale numbers forward, but leave the key mismatched so a later
        // full sweep knows they still need redoing.
        if let Some(prev) = cached {
            status.work = prev.work.clone();
            status.work_key = prev.work_key.clone();
            status.work_probed_at = prev.work_probed_at;
        }
        return WorkOutcome::Skipped;
    }

    let _permit = work_permit.acquire().await;
    match git::probe_work(&status.root, &cfg.status).await {
        Ok(work) => {
            status.work = Some(work);
            status.work_key = Some(key);
            status.work_probed_at = git::now_unix();
            WorkOutcome::Scanned
        }
        Err(err) => {
            status.error = Some(format!("{err:#}"));
            WorkOutcome::Skipped
        }
    }
}

/// True when a cached visibility check has outlived `visibility.interval` and
/// is worth repeating.
fn visibility_expired(info: &crate::model::VisibilityInfo, cfg: &Config, now: i64) -> bool {
    now.saturating_sub(info.checked_at) >= cfg.visibility_interval().as_secs() as i64
}

/// True when a cached visibility entry is still worth trusting as-is, so a
/// fresh check can be skipped. Only a real answer qualifies:
/// `CheckingDisabled` and `CheckFailed` are records of *not* having one, and
/// trusting those the same way a `Known` value is trusted would mean
/// re-enabling this feature -- or a transient failure clearing up -- doesn't
/// actually get retried until the interval runs out.
fn visibility_still_trusted(info: &crate::model::VisibilityInfo, cfg: &Config, now: i64) -> bool {
    matches!(info.status, crate::model::VisibilityStatus::Known(_))
        && !visibility_expired(info, cfg, now)
}

/// What to report after a provider call fails. Prefers a previously known
/// value over the fresh failure -- but, same reasoning as
/// [`visibility_still_trusted`], only a real one. Falling back to a cached
/// `CheckingDisabled` or `CheckFailed` here would hide a fresh, specific
/// failure reason behind a stale, less specific one.
fn visibility_fallback(
    cached: Option<&crate::model::VisibilityInfo>,
    reason: &str,
    now: i64,
) -> crate::model::VisibilityInfo {
    match cached.filter(|prev| matches!(prev.status, crate::model::VisibilityStatus::Known(_))) {
        Some(prev) => prev.clone(),
        None => crate::model::VisibilityInfo {
            status: crate::model::VisibilityStatus::CheckFailed(
                reason.lines().next().unwrap_or(reason).to_string(),
            ),
            checked_at: now,
        },
    }
}

/// Check a repo's visibility via whichever hosting provider its remote
/// belongs to (see [`crate::provider`]), reusing a cached value that's still
/// within `visibility.interval`. Always sets `status.visibility` to `Some` --
/// see [`VisibilityStatus`] for what each outcome means. Two of those
/// outcomes are free and computed regardless of `visibility.enabled`: no
/// remote at all, and a remote on a host nothing recognises. Both are facts
/// about the remote URL, not something that costs a network call to know, so
/// there's no reason to gate them behind the same flag that gates actual
/// provider calls.
async fn fill_visibility(
    status: &mut RepoStatus,
    cfg: &Config,
    cached: Option<&RepoStatus>,
    permit: &Semaphore,
) {
    use crate::model::VisibilityStatus;

    // "No remote" is a fact about a repo that was read. A repo that couldn't
    // be read at all hasn't established that or anything else, and saying it
    // has would be a claim nothing checked -- the same mistake this whole
    // column is built to avoid.
    let Some(refs) = status.refs.as_ref() else {
        status.visibility = Some(VisibilityInfo {
            status: VisibilityStatus::Unknown,
            checked_at: git::now_unix(),
        });
        return;
    };
    let Some(remote_url) = refs.remote_url.as_ref() else {
        status.visibility = Some(VisibilityInfo {
            status: VisibilityStatus::NoRemote,
            checked_at: git::now_unix(),
        });
        return;
    };
    let Some((provider, slug)) = provider::detect(remote_url) else {
        status.visibility = Some(VisibilityInfo {
            status: VisibilityStatus::Unsupported,
            checked_at: git::now_unix(),
        });
        return;
    };

    if !cfg.visibility.enabled {
        // The remote is real and checkable -- we just haven't asked. Keep a
        // previously *known* value (from before the flag was turned off)
        // rather than overwriting it with a generic "disabled".
        //
        // Only a known one, for the same reason `visibility_still_trusted`
        // and `visibility_fallback` insist on one: every other variant is a
        // record of not having an answer, and those go stale in a way a real
        // answer doesn't. A cached `no remote configured` from before the
        // remote could be read would otherwise outlive the thing it described
        // and keep being reported as fact.
        status.visibility = cached
            .and_then(|p| p.visibility.clone())
            .filter(|prev| matches!(prev.status, VisibilityStatus::Known(_)))
            .or(Some(VisibilityInfo {
                status: VisibilityStatus::CheckingDisabled,
                checked_at: git::now_unix(),
            }));
        return;
    }

    let now = git::now_unix();
    if let Some(prev) = cached.and_then(|p| p.visibility.as_ref()) {
        if visibility_still_trusted(prev, cfg, now) {
            status.visibility = Some(prev.clone());
            return;
        }
    }

    let _permit = permit.acquire().await;
    match provider::check(provider, &slug, cfg.visibility_timeout()).await {
        Ok(value) => {
            status.visibility = Some(VisibilityInfo {
                status: VisibilityStatus::Known(value),
                checked_at: git::now_unix(),
            });
        }
        Err(err) => {
            let reason = format!("{err:#}");
            tracing::debug!(
                repo = %status.root.display(),
                error = %reason,
                "visibility check failed"
            );
            status.visibility = Some(visibility_fallback(
                cached.and_then(|p| p.visibility.as_ref()),
                &reason,
                git::now_unix(),
            ));
        }
    }
}

/// Which repos a sweep's fetch phase should touch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Fetch {
    /// Leave the network alone. What every sweep does unless asked otherwise.
    Skip,
    /// Every discovered repo that has a remote.
    All,
    /// Only the repos in one group. `--group grav --fetch` is thirty fetches
    /// rather than five hundred, and the answer is the same for the rows
    /// you're going to be shown.
    Group(String),
}

impl Fetch {
    fn wanted(&self, d: &Discovered) -> bool {
        match self {
            Fetch::Skip => false,
            Fetch::All => true,
            Fetch::Group(group) => d.group == *group,
        }
    }
}

/// What a fetch phase managed to do.
#[derive(Clone, Copy, Debug, Default)]
pub struct FetchReport {
    /// Repos that had a remote and were fetched from.
    pub attempted: usize,
    /// How many of those failed -- unreachable, needed credentials, timed out.
    pub failed: usize,
}

/// Fetch every one of these repos that has a remote, bounded by
/// `remote.concurrency`, before anything gets probed.
///
/// The one network phase in a sweep, and the only reason "behind" ever means
/// anything: the counts themselves come from remote-tracking refs, which are
/// as stale as whatever last updated them. A repo with no remote is skipped
/// rather than fetched, since `git fetch` there is a process spawned to do
/// nothing.
///
/// Failures are counted and logged, never fatal. A remote that's unreachable,
/// wants a password, or times out leaves that repo's counts exactly as stale
/// as they already were, which is strictly better than abandoning the sweep.
pub async fn fetch_all(cfg: Arc<Config>, roots: Vec<PathBuf>) -> FetchReport {
    let with_remotes: Vec<PathBuf> = tokio::task::spawn_blocking(move || {
        roots
            .into_iter()
            .filter(|root| git::quick_remote_url(root).is_some())
            .collect()
    })
    .await
    .unwrap_or_default();

    if with_remotes.is_empty() {
        return FetchReport::default();
    }

    let timeout = cfg.remote_timeout();
    let limit = Arc::new(Semaphore::new(cfg.remote.concurrency.max(1)));
    let failed = Arc::new(AtomicUsize::new(0));
    let mut set = JoinSet::new();

    for root in &with_remotes {
        let root = root.clone();
        let limit = limit.clone();
        let failed = failed.clone();
        set.spawn(async move {
            let _permit = limit.acquire().await;
            if let Err(err) = git::fetch(&root, timeout).await {
                tracing::debug!(
                    repo = %root.display(),
                    error = %format!("{err:#}"),
                    "fetch failed"
                );
                failed.fetch_add(1, Ordering::Relaxed);
            }
        });
    }
    while set.join_next().await.is_some() {}

    FetchReport {
        attempted: with_remotes.len(),
        failed: failed.load(Ordering::Relaxed),
    }
}

/// Probe one repo end to end. Used for `drydock status <path>` and by the
/// watcher when a single repo changes; sweeps use the pipelined path below so
/// tier 1 and tier 2 can run at different concurrencies.
///
/// Pass `force` when something already told you the working tree moved. One
/// repo's scan costs milliseconds, so the callers that probe a single repo on
/// purpose have no reason to accept a cached answer. Visibility is exempt
/// from that: it has its own interval, checked separately below, since it
/// costs a real network round trip rather than a syscall.
pub async fn probe_one(
    d: &Discovered,
    cfg: &Config,
    cached: Option<&RepoStatus>,
    tier: Tier,
    force: bool,
) -> RepoStatus {
    let mut status = RepoStatus::new(d.root.clone(), d.group.clone(), d.name.clone());
    let unlimited = Semaphore::new(1);
    if fill_refs(&mut status, cfg, cached).await {
        fill_work(&mut status, cfg, cached, tier, force, &unlimited).await;
    }
    // Runs even when the refs probe failed, so the column reports `unknown`
    // rather than staying unset and rendering as a bare `-`. Carrying a
    // cached value forward is [`fill_visibility`]'s own business.
    fill_visibility(&mut status, cfg, cached, &unlimited).await;
    status
}

/// Run a full sweep. `tx` is optional: pass one to stream progress, or None to
/// just wait for the result.
///
/// Anything but [`Fetch::Skip`] inserts a network phase between discovery and
/// probing, so the "behind" counts this sweep reports were checked against the
/// remotes rather than read off whatever the last fetch left behind. It runs
/// first, and to completion, because tier 1 reads the remote-tracking refs it
/// updates.
pub async fn sweep(
    cfg: Arc<Config>,
    tier: Tier,
    fetch: Fetch,
    tx: Option<mpsc::UnboundedSender<Event>>,
) -> Result<Fleet> {
    let started = Instant::now();
    let mut timings = Timings::default();

    let t0 = Instant::now();
    // Every exit from here has to report Done. The dashboard treats a sweep as
    // still running until it hears that, and refuses to start another one while
    // it thinks one is in flight, so a silent early return here wedges every
    // future sweep for the life of the process.
    let discovered = match discover_repos(cfg.clone()).await {
        Ok(d) => d,
        Err(err) => {
            emit(
                &tx,
                Event::Done {
                    elapsed: started.elapsed(),
                },
            );
            return Err(err);
        }
    };
    timings.discovery = t0.elapsed();
    timings.repos = discovered.len();
    emit(
        &tx,
        Event::Discovered {
            roots: discovered.iter().map(|d| d.root.clone()).collect(),
        },
    );

    if fetch != Fetch::Skip {
        let t0 = Instant::now();
        let roots: Vec<PathBuf> = discovered
            .iter()
            .filter(|d| fetch.wanted(d))
            .map(|d| d.root.clone())
            .collect();
        let report = fetch_all(cfg.clone(), roots).await;
        timings.fetch = t0.elapsed();
        timings.fetched = report.attempted;
        timings.fetch_failed = report.failed;
        emit(
            &tx,
            Event::Phase {
                name: "fetch",
                elapsed: timings.fetch,
            },
        );
    }

    let cached = cache::load();
    let repos = sweep_repos(cfg.clone(), discovered, cached, tier, &tx, &mut timings).await;

    timings.total = started.elapsed();
    emit(
        &tx,
        Event::Done {
            elapsed: timings.total,
        },
    );

    if let Err(err) = cache::save(&repos) {
        tracing::warn!(error = %format!("{err:#}"), "could not write cache");
    }

    Ok(Fleet { repos, timings })
}

/// Probe an already-discovered set of repos.
///
/// One task per repo, each running tier 1 then tier 2, with a separate
/// semaphore per tier. Tier 1 gets to run wide so the table fills in quickly;
/// tier 2 is held to roughly one scan per core because it is syscall-bound and
/// oversubscribing it just adds contention. Doing both in the same task means
/// tier 1 is never paid for twice.
pub async fn sweep_repos(
    cfg: Arc<Config>,
    discovered: Vec<Discovered>,
    cached: HashMap<PathBuf, RepoStatus>,
    tier: Tier,
    tx: &Option<mpsc::UnboundedSender<Event>>,
    timings: &mut Timings,
) -> Vec<RepoStatus> {
    if discovered.is_empty() {
        return Vec::new();
    }
    let cached = Arc::new(cached);

    // Warm-start ordering: probe whatever the cache says was most recently
    // active first, so the top of the table is right within the first moment.
    let mut items = discovered;
    items.sort_by_key(|d| {
        std::cmp::Reverse(cached.get(&d.root).map(|r| r.activity_at()).unwrap_or(0))
    });

    let refs_sem = Arc::new(Semaphore::new(cfg.refs_concurrency()));
    let work_sem = Arc::new(Semaphore::new(cfg.work_concurrency()));
    let visibility_sem = Arc::new(Semaphore::new(cfg.visibility.concurrency.max(1)));
    let refs_ms = Arc::new(AtomicU64::new(0));
    let scanned = Arc::new(AtomicUsize::new(0));
    let from_cache = Arc::new(AtomicUsize::new(0));
    let started = Instant::now();

    let mut set: JoinSet<RepoStatus> = JoinSet::new();
    for d in items {
        let cfg = cfg.clone();
        let cached = cached.clone();
        let refs_sem = refs_sem.clone();
        let work_sem = work_sem.clone();
        let visibility_sem = visibility_sem.clone();
        let refs_ms = refs_ms.clone();
        let scanned = scanned.clone();
        let from_cache = from_cache.clone();
        let tx = tx.clone();

        set.spawn(async move {
            let previous = cached.get(&d.root);
            let mut status = RepoStatus::new(d.root.clone(), d.group.clone(), d.name.clone());

            let ok = {
                let _permit = refs_sem.acquire().await;
                fill_refs(&mut status, &cfg, previous).await
            };
            refs_ms.fetch_max(started.elapsed().as_millis() as u64, Ordering::Relaxed);
            emit(&tx, Event::Refs(Box::new(status.clone())));

            // A repo whose refs couldn't be read has no working tree worth
            // scanning, but it still gets a visibility verdict -- `unknown`,
            // or whatever was cached -- rather than an unset column.
            if ok {
                match fill_work(&mut status, &cfg, previous, tier, false, &work_sem).await {
                    WorkOutcome::Scanned => {
                        scanned.fetch_add(1, Ordering::Relaxed);
                    }
                    WorkOutcome::Cached => {
                        from_cache.fetch_add(1, Ordering::Relaxed);
                    }
                    WorkOutcome::Skipped => {}
                }
            }
            fill_visibility(&mut status, &cfg, previous, &visibility_sem).await;
            emit(&tx, Event::Work(Box::new(status.clone())));
            status
        });
    }

    let mut out = Vec::with_capacity(set.len());
    while let Some(joined) = set.join_next().await {
        match joined {
            Ok(status) => out.push(status),
            Err(err) => tracing::warn!(%err, "probe task failed"),
        }
    }

    timings.refs = Duration::from_millis(refs_ms.load(Ordering::Relaxed));
    timings.work = started.elapsed();
    timings.work_scanned = scanned.load(Ordering::Relaxed);
    timings.work_cached = from_cache.load(Ordering::Relaxed);
    emit(
        tx,
        Event::Phase {
            name: "refs",
            elapsed: timings.refs,
        },
    );
    emit(
        tx,
        Event::Phase {
            name: "work",
            elapsed: timings.work,
        },
    );

    out.sort_by(|a, b| a.root.cmp(&b.root));
    out
}

fn emit(tx: &Option<mpsc::UnboundedSender<Event>>, event: Event) {
    if let Some(tx) = tx {
        let _ = tx.send(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // `--group acme --fetch` should be that group's remotes, not the whole
    // tree's: the other rows aren't going to be printed.
    #[test]
    fn a_group_scoped_fetch_only_wants_that_group() {
        let repo = |group: &str| Discovered {
            root: PathBuf::from("/p").join(group).join("r"),
            group: group.to_string(),
            name: "r".into(),
        };
        let acme = repo("acme");
        let other = repo("other");

        let scoped = Fetch::Group("acme".into());
        assert!(scoped.wanted(&acme));
        assert!(!scoped.wanted(&other));

        assert!(Fetch::All.wanted(&other));
        assert!(!Fetch::Skip.wanted(&acme));
    }

    fn bare_status(is_bare: bool) -> RepoStatus {
        let mut status = RepoStatus::new(PathBuf::from("/nonexistent"), "g".into(), "r".into());
        status.refs = Some(crate::model::RefsInfo {
            head: crate::model::Head::Branch("main".into()),
            branches: Vec::new(),
            last_commit: None,
            stashes: 0,
            operation: None,
            newest_tag: None,
            described_tag: None,
            commits_since_tag: None,
            since_tag_subjects: Vec::new(),
            tags_orphaned: false,
            index_mtime: None,
            fetched_at: None,
            remote_url: None,
            changelog: None,
            is_bare,
            is_shallow: false,
        });
        status
    }

    // Issue #4: `git status` in a bare repo can only ever fail ("this
    // operation must be run in a work tree"), and recording that failure made
    // a perfectly healthy repo render as `error`. The scan is skipped
    // outright now, so there is nothing left to fail.
    #[tokio::test]
    async fn a_bare_repo_is_never_scanned_for_a_working_tree() {
        let cfg = Config::default();
        let mut status = bare_status(true);
        let permit = Semaphore::new(1);
        let outcome = fill_work(&mut status, &cfg, None, Tier::Full, true, &permit).await;
        assert_eq!(outcome, WorkOutcome::Skipped);
        assert!(status.work.is_none());
        assert_eq!(status.error, None, "a bare repo is not an error");
        assert_eq!(status.state_label(), "bare");
    }

    // That skip is guarded on `is_bare` alone, so a non-bare repo still has to
    // reach the scan. Here it fails, because the root does not exist -- and
    // that failure is exactly what should still be recorded.
    #[tokio::test]
    async fn a_normal_repo_still_reaches_the_working_tree_scan() {
        let cfg = Config::default();
        let mut status = bare_status(false);
        let permit = Semaphore::new(1);
        fill_work(&mut status, &cfg, None, Tier::Full, true, &permit).await;
        assert!(status.error.is_some(), "the failed scan should be recorded");
    }

    fn probed_at(when: i64) -> RepoStatus {
        let mut status = RepoStatus::new(PathBuf::from("/tmp/repo"), "group".into(), "repo".into());
        status.work_probed_at = when;
        status
    }

    fn with_max_age(max_age: &str) -> Config {
        let mut cfg = Config::default();
        cfg.status.max_age = max_age.into();
        cfg
    }

    #[test]
    fn cached_work_expires_once_it_outlives_max_age() {
        let cfg = with_max_age("1h");
        let prev = probed_at(1_000_000);
        assert!(!work_expired(&prev, &cfg, 1_000_000 + 3_599));
        assert!(work_expired(&prev, &cfg, 1_000_000 + 3_600));
    }

    #[test]
    fn an_empty_max_age_trusts_the_key_indefinitely() {
        assert!(!work_expired(&probed_at(0), &with_max_age(""), i64::MAX));
    }

    #[test]
    fn a_zero_max_age_rescans_every_sweep() {
        let cfg = with_max_age("0");
        assert!(work_expired(&probed_at(1_000_000), &cfg, 1_000_000));
    }

    fn visibility_info(status: crate::model::VisibilityStatus, checked_at: i64) -> VisibilityInfo {
        VisibilityInfo { status, checked_at }
    }

    #[test]
    fn a_known_value_within_the_interval_is_trusted() {
        let cfg = Config::default(); // 24h interval
        let info = visibility_info(
            crate::model::VisibilityStatus::Known(crate::model::Visibility::Public),
            1_000_000,
        );
        assert!(visibility_still_trusted(&info, &cfg, 1_000_000 + 3_600));
    }

    #[test]
    fn a_known_value_past_the_interval_is_not_trusted() {
        let cfg = Config::default(); // 24h interval
        let info = visibility_info(
            crate::model::VisibilityStatus::Known(crate::model::Visibility::Public),
            1_000_000,
        );
        assert!(!visibility_still_trusted(&info, &cfg, 1_000_000 + 86_400));
    }

    // The bug this guards against: a repo probed while checking was off (or
    // mid-failure) gets a cache entry that is *not itself an answer*. Without
    // this, turning checking back on -- or a transient failure clearing up --
    // wouldn't actually trigger a fresh check until the interval ran out,
    // because the interval logic alone can't tell "we know this" from "we
    // don't, yet" apart.
    #[test]
    fn checking_disabled_is_never_trusted_no_matter_how_fresh() {
        let cfg = Config::default();
        let info = visibility_info(crate::model::VisibilityStatus::CheckingDisabled, 1_000_000);
        assert!(!visibility_still_trusted(&info, &cfg, 1_000_000));
    }

    #[test]
    fn a_check_failure_is_never_trusted_no_matter_how_fresh() {
        let cfg = Config::default();
        let info = visibility_info(
            crate::model::VisibilityStatus::CheckFailed("timed out".into()),
            1_000_000,
        );
        assert!(!visibility_still_trusted(&info, &cfg, 1_000_000));
    }

    #[test]
    fn a_failed_check_falls_back_to_a_real_previous_value() {
        let known = visibility_info(
            crate::model::VisibilityStatus::Known(crate::model::Visibility::Private),
            1_000,
        );
        let out = visibility_fallback(Some(&known), "rate limited", 2_000);
        assert_eq!(out.status, known.status);
        // The fallback is a value worth trusting again, not the moment of
        // this failure -- so its timestamp is untouched, not bumped to now.
        assert_eq!(out.checked_at, 1_000);
    }

    #[test]
    fn a_failed_check_does_not_fall_back_to_a_placeholder() {
        for placeholder in [
            crate::model::VisibilityStatus::CheckingDisabled,
            crate::model::VisibilityStatus::CheckFailed("previous failure".into()),
        ] {
            let cached = visibility_info(placeholder, 1_000);
            let out = visibility_fallback(Some(&cached), "rate limited", 2_000);
            assert_eq!(
                out.status,
                crate::model::VisibilityStatus::CheckFailed("rate limited".into())
            );
            assert_eq!(out.checked_at, 2_000);
        }
    }

    #[test]
    fn a_failed_check_with_nothing_cached_reports_itself() {
        let out = visibility_fallback(None, "gh: not authenticated\nrun gh auth login", 2_000);
        assert_eq!(
            out.status,
            crate::model::VisibilityStatus::CheckFailed("gh: not authenticated".into())
        );
        assert_eq!(out.checked_at, 2_000);
    }

    fn status_with_remote(remote_url: Option<&str>) -> RepoStatus {
        let mut status = RepoStatus::new(PathBuf::from("/tmp/repo"), "group".into(), "repo".into());
        status.refs = Some(crate::model::RefsInfo {
            head: crate::model::Head::Branch("main".into()),
            branches: Vec::new(),
            last_commit: None,
            stashes: 0,
            operation: None,
            newest_tag: None,
            described_tag: None,
            commits_since_tag: None,
            since_tag_subjects: Vec::new(),
            tags_orphaned: false,
            index_mtime: None,
            fetched_at: None,
            remote_url: remote_url.map(String::from),
            changelog: None,
            is_bare: false,
            is_shallow: false,
        });
        status
    }

    fn enabled_visibility_cfg(enabled: bool) -> Config {
        let mut cfg = Config::default();
        cfg.visibility.enabled = enabled;
        cfg
    }

    // These two are free -- determined from the remote URL alone, no `gh`
    // call involved -- so they're expected to hold regardless of
    // `visibility.enabled`, unlike everything else `fill_visibility` does.

    #[tokio::test]
    async fn no_remote_is_known_for_free_either_way() {
        for enabled in [false, true] {
            let cfg = enabled_visibility_cfg(enabled);
            let mut status = status_with_remote(None);
            let permit = Semaphore::new(1);
            fill_visibility(&mut status, &cfg, None, &permit).await;
            assert_eq!(
                status.visibility.map(|v| v.status),
                Some(crate::model::VisibilityStatus::NoRemote)
            );
        }
    }

    // A repo whose refs probe failed with nothing cached has established
    // nothing -- least of all that it has no remote. Reporting "no remote
    // configured" there is a claim nothing checked, which is exactly what
    // this column exists not to do.
    #[tokio::test]
    async fn a_repo_that_could_not_be_read_is_unknown_rather_than_remoteless() {
        for enabled in [false, true] {
            let cfg = enabled_visibility_cfg(enabled);
            let mut status =
                RepoStatus::new(PathBuf::from("/tmp/repo"), "group".into(), "repo".into());
            status.error = Some("could not read the repo".into());
            assert!(status.refs.is_none());
            let permit = Semaphore::new(1);
            fill_visibility(&mut status, &cfg, None, &permit).await;
            assert_eq!(
                status.visibility.map(|v| v.status),
                Some(crate::model::VisibilityStatus::Unknown)
            );
        }
    }

    // The other half of that distinction: a repo that *was* read and genuinely
    // has no remote still says so.
    #[tokio::test]
    async fn a_readable_repo_with_no_remote_still_says_no_remote() {
        let cfg = enabled_visibility_cfg(true);
        let mut status = status_with_remote(None);
        let permit = Semaphore::new(1);
        fill_visibility(&mut status, &cfg, None, &permit).await;
        assert_eq!(
            status.visibility.map(|v| v.status),
            Some(crate::model::VisibilityStatus::NoRemote)
        );
    }

    #[tokio::test]
    async fn an_unrecognised_host_is_known_for_free_either_way() {
        for enabled in [false, true] {
            let cfg = enabled_visibility_cfg(enabled);
            let mut status = status_with_remote(Some("git@gitlab.com:owner/repo.git"));
            let permit = Semaphore::new(1);
            fill_visibility(&mut status, &cfg, None, &permit).await;
            assert_eq!(
                status.visibility.map(|v| v.status),
                Some(crate::model::VisibilityStatus::Unsupported)
            );
        }
    }

    #[tokio::test]
    async fn a_checkable_remote_says_so_plainly_when_checking_is_off() {
        let cfg = enabled_visibility_cfg(false);
        let mut status = status_with_remote(Some("git@github.com:owner/repo.git"));
        let permit = Semaphore::new(1);
        fill_visibility(&mut status, &cfg, None, &permit).await;
        assert_eq!(
            status.visibility.map(|v| v.status),
            Some(crate::model::VisibilityStatus::CheckingDisabled)
        );
    }

    #[tokio::test]
    async fn turning_checking_off_does_not_erase_a_previously_known_value() {
        let cfg = enabled_visibility_cfg(false);
        let mut status = status_with_remote(Some("git@github.com:owner/repo.git"));
        let mut cached = status_with_remote(Some("git@github.com:owner/repo.git"));
        cached.visibility = Some(crate::model::VisibilityInfo {
            status: crate::model::VisibilityStatus::Known(crate::model::Visibility::Public),
            checked_at: 1_000,
        });
        let permit = Semaphore::new(1);
        fill_visibility(&mut status, &cfg, Some(&cached), &permit).await;
        assert_eq!(
            status.visibility.map(|v| v.status),
            Some(crate::model::VisibilityStatus::Known(
                crate::model::Visibility::Public
            ))
        );
    }
}
