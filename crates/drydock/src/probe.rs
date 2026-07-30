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
use crate::model::RepoStatus;

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
    pub refs: Duration,
    pub work: Duration,
    pub total: Duration,
    pub repos: usize,
    pub work_scanned: usize,
    pub work_cached: usize,
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

/// Tier 2 for one repo, reusing the cache when HEAD and the index both match.
async fn fill_work(
    status: &mut RepoStatus,
    cfg: &Config,
    cached: Option<&RepoStatus>,
    tier: Tier,
    work_permit: &Semaphore,
) -> WorkOutcome {
    let head_sha = status
        .refs
        .as_ref()
        .and_then(|r| r.current_branch().map(|b| b.sha.clone()));
    let key = git::work_key(&status.root, head_sha);

    if let Some(prev) = cached {
        if prev.work.is_some() && prev.work_key.as_ref() == Some(&key) {
            status.work = prev.work.clone();
            status.work_key = Some(key);
            status.work_probed_at = prev.work_probed_at;
            return WorkOutcome::Cached;
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

/// Probe one repo end to end. Used for `drydock status <path>` and by the
/// watcher when a single repo changes; sweeps use the pipelined path below so
/// tier 1 and tier 2 can run at different concurrencies.
pub async fn probe_one(
    d: &Discovered,
    cfg: &Config,
    cached: Option<&RepoStatus>,
    tier: Tier,
) -> RepoStatus {
    let mut status = RepoStatus::new(d.root.clone(), d.group.clone(), d.name.clone());
    if !fill_refs(&mut status, cfg, cached).await {
        return status;
    }
    let unlimited = Semaphore::new(1);
    fill_work(&mut status, cfg, cached, tier, &unlimited).await;
    status
}

/// Run a full sweep. `tx` is optional: pass one to stream progress, or None to
/// just wait for the result.
pub async fn sweep(
    cfg: Arc<Config>,
    tier: Tier,
    tx: Option<mpsc::UnboundedSender<Event>>,
) -> Result<Fleet> {
    let started = Instant::now();
    let mut timings = Timings::default();

    let t0 = Instant::now();
    let discovered = discover_repos(cfg.clone()).await?;
    timings.discovery = t0.elapsed();
    timings.repos = discovered.len();
    emit(
        &tx,
        Event::Discovered {
            roots: discovered.iter().map(|d| d.root.clone()).collect(),
        },
    );

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
            if !ok {
                return status;
            }

            match fill_work(&mut status, &cfg, previous, tier, &work_sem).await {
                WorkOutcome::Scanned => {
                    scanned.fetch_add(1, Ordering::Relaxed);
                }
                WorkOutcome::Cached => {
                    from_cache.fetch_add(1, Ordering::Relaxed);
                }
                WorkOutcome::Skipped => {}
            }
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
