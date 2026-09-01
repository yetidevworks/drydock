//! The org-sync engine: clone missing checkouts and fast-forward existing
//! ones, one repo at a time.
//!
//! Three stages, so the interesting logic is a pure function tests can drive
//! without network or git. `plan` decides what would happen, `execute` does
//! it serially, and `sync_org` ties them together with the
//! degrade-to-update-only guard rail and the cache write.
//!
//! Two rules from the plan that this module lives by. Sync is *strictly
//! serial* — one git child at a time, no JoinSet, no semaphore — because it
//! runs against a fleet while the user may have editors open, and nothing
//! here may ever lose work. And orphans are only ever *reported*, never
//! removed: a repo on disk that the owner no longer lists was renamed or
//! deleted upstream, archived, or made private, and guessing which is how a
//! sync tool earns its rm -rf horror story.

use anyhow::{anyhow, Context, Result};
use globset::Glob;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::cache;
use crate::config::{Config, OrgConfig, OrgProvider};
use crate::git;
use crate::paths;
use crate::provider::OrgRepo;

/// A repo the config says not to touch: a fork, an archive, or a name
/// matching one of the `exclude` globs.
#[derive(Debug, Clone)]
pub struct SkippedRepo {
    pub name: String,
    pub reason: String,
}

/// What a sync would do, before anything runs. This is the `--dry-run` shape.
#[derive(Debug, Clone, Default)]
pub struct SyncPlan {
    pub to_clone: Vec<OrgRepo>,
    pub to_update: Vec<PathBuf>,
    pub orphans: Vec<PathBuf>,
    pub skipped: Vec<SkippedRepo>,
}

/// What happened to one repo. `Serialize` because `--json` and the TUI both
/// render outcomes verbatim.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum Action {
    Cloned,
    Updated,
    Current,
    Skipped,
    Orphaned,
    Error,
}

impl Action {
    pub fn label(&self) -> &'static str {
        match self {
            Action::Cloned => "cloned",
            Action::Updated => "updated",
            Action::Current => "current",
            Action::Skipped => "skipped",
            Action::Orphaned => "orphaned",
            Action::Error => "error",
        }
    }
}

/// One row of the report.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SyncOutcome {
    pub action: Action,
    pub name: String,
    pub detail: String,
}

/// What a running sync tells the dashboard: a progress tick before each unit
/// of work, then one outcome per repo — the same streaming shape the probe
/// channel uses, so the status line can show `cloning 7/31`.
#[derive(Debug, Clone)]
pub enum SyncEvent {
    Progress {
        done: usize,
        total: usize,
        label: String,
    },
    Repo(SyncOutcome),
}

/// The first line of an error chain, which is all a report row has room for.
/// Anyhow's display reads `context: cause`, so line one carries the context
/// a human wants.
fn first_line(err: &anyhow::Error) -> String {
    err.to_string().lines().next().unwrap_or("").to_string()
}

/// The meaningful last line of a pull's stdout: git prints the verdict
/// (`Fast-forward`, `Already up to date.`) last, after any fetch noise.
fn last_line(stdout: &str) -> String {
    stdout
        .lines()
        .rev()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .to_string()
}

/// Which URL to clone from, per the org's protocol.
fn clone_url<'a>(org: &OrgConfig, repo: &'a OrgRepo) -> &'a str {
    match org.protocol {
        crate::config::CloneProtocol::Ssh => &repo.ssh_url,
        crate::config::CloneProtocol::Https => &repo.https_url,
    }
}

/// Compile the org's exclude globs. An unparseable pattern is skipped with a
/// warning rather than failing the sync — same tolerance as `git.rs`'s tag
/// pattern compiler, same reason: a typo in one glob shouldn't take down a
/// whole org's sync.
fn compile_excludes(org: &OrgConfig) -> Vec<globset::GlobMatcher> {
    org.exclude
        .iter()
        .filter(|pattern| !pattern.is_empty() && *pattern != "*")
        .filter_map(|pattern| match Glob::new(pattern) {
            Ok(glob) => Some(glob.compile_matcher()),
            Err(err) => {
                tracing::warn!(%pattern, %err, "ignoring invalid org exclude glob");
                None
            }
        })
        .collect()
}

/// What the sync should do, given what the owner lists and what's on disk.
///
/// `disk` is the immediate subdirectories of the org's path that are
/// checkouts ([`disk_repos`]). Remote-only goes to `to_clone`, disk-only to
/// `orphans`, both to `to_update`, each bucket sorted so the report is stable
/// regardless of the order the provider listed in. Everything the config
/// filters out — forks, archives, exclude globs — lands in `skipped` with
/// the reason, so the report can say *why* a repo was passed over rather
/// than just omitting it.
///
/// Orphans are report-only, permanently. On-disk-but-unlisted usually means
/// the repo was renamed or deleted upstream, or went archived or private
/// since the last sync — but it can also mean a checkout the user keeps
/// there deliberately, and sync has no way to tell those apart. So it
/// reports and moves on; nothing is ever deleted.
pub fn plan(remote: &[OrgRepo], disk: &[PathBuf], cfg: &OrgConfig) -> SyncPlan {
    let globs = compile_excludes(cfg);

    // Match by directory name == repo name. Each disk path is claimed by at
    // most one remote; a name repeated on disk is an oddity discovery made,
    // and the first wins rather than double-updating.
    let mut disk_by_name: std::collections::HashMap<&str, &PathBuf> =
        std::collections::HashMap::with_capacity(disk.len());
    for path in disk {
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            disk_by_name.entry(name).or_insert(path);
        }
    }

    let mut plan = SyncPlan::default();
    let mut listed: std::collections::HashSet<&str> =
        std::collections::HashSet::with_capacity(remote.len());

    for repo in remote {
        if !cfg.include_forks && repo.fork {
            plan.skipped.push(SkippedRepo {
                name: repo.name.clone(),
                reason: "fork".into(),
            });
            continue;
        }
        if !cfg.include_archived && repo.archived {
            plan.skipped.push(SkippedRepo {
                name: repo.name.clone(),
                reason: "archived".into(),
            });
            continue;
        }
        if globs.iter().any(|glob| glob.is_match(&repo.name)) {
            plan.skipped.push(SkippedRepo {
                name: repo.name.clone(),
                reason: "excluded by pattern".into(),
            });
            continue;
        }
        listed.insert(repo.name.as_str());
        match disk_by_name.get(repo.name.as_str()) {
            Some(&path) => plan.to_update.push(path.clone()),
            None => plan.to_clone.push(repo.clone()),
        }
    }

    // A checkout whose name the (filtered) list doesn't carry is an orphan —
    // including one that was itself filtered out, which is exactly the
    // "archived-and-filtered, still on disk" case worth surfacing.
    for path in disk {
        let known = path
            .file_name()
            .and_then(|n| n.to_str())
            .map(|name| listed.contains(name))
            .unwrap_or(false);
        if !known {
            plan.orphans.push(path.clone());
        }
    }

    plan.to_clone.sort_by(|a, b| a.name.cmp(&b.name));
    plan.to_update.sort();
    plan.orphans.sort();
    plan
}

/// The immediate subdirectories of `path` that look like checkouts.
///
/// `.git` may be a directory (an ordinary clone) or a file (a worktree or
/// submodule pointer) — matching how `discover.rs` treats them, so sync and
/// discovery can never disagree about what a repo is. A missing directory is
/// an empty answer rather than an error: a first sync against an org whose
/// path nobody created yet is the normal case, and "nothing there yet" is
/// the truth about it.
pub fn disk_repos(path: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(path) else {
        return Vec::new();
    };
    let mut repos: Vec<PathBuf> = entries
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            if path.join(".git").exists() {
                Some(path)
            } else {
                None
            }
        })
        .collect();
    repos.sort();
    repos
}

/// Where this org's checkouts belong. An explicit `path` wins; otherwise the
/// first configured root plus the owner, which lands the checkouts in the
/// dashboard as a group for free. With no roots at all there is nothing to
/// resolve against, and that is an error rather than a guess — sync without
/// a destination is a config problem, and it should say so before touching
/// anything.
pub fn effective_path(cfg: &Config, org: &OrgConfig) -> Result<PathBuf> {
    match &org.path {
        Some(p) => Ok(paths::expand(p)),
        None => cfg
            .root_paths()
            .into_iter()
            .next()
            .map(|root| root.join(&org.owner))
            .ok_or_else(|| {
                anyhow!(
                    "org \"{}\" has no path set and no roots are configured",
                    org.owner
                )
            }),
    }
}

/// Make sure the org's checkouts will be found by discovery. The org's path
/// is where its checkouts live one level down (`<path>/<repo>`), so what
/// discovery needs is the path's PARENT as a scan root — that also makes the
/// owner read as a dashboard group, the same as the default path layout.
///
/// When the resolved path is already under a root (or the parent already is
/// one), this is a no-op. When the parent is `/`, fall back to adding the
/// path itself: an ungrouped row beats an invisible one.
///
/// Returns whether a root was added. Roots are stored `~`-contracted, and
/// appended in sorted-scan order is irrelevant, so just append. The only
/// error is [`effective_path`]'s — no roots and no path — which callers
/// already treat as their existing failure path.
pub fn ensure_scan_root(cfg: &mut Config, org: &OrgConfig) -> Result<bool> {
    let path = effective_path(cfg, org)?;
    let candidate = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() && parent != Path::new("/") => {
            parent.to_path_buf()
        }
        _ => path.clone(),
    };
    // Compared expanded: a root written as `~/Projects` and a candidate under
    // `$HOME` are the same directory to the sweep. `starts_with` is
    // component-wise, which is exactly the "under a root" relation discovery
    // walks — so `path.starts_with(root)` covers both "equals a root" and
    // "below one".
    let covered = cfg
        .root_paths()
        .iter()
        .any(|root| path.starts_with(root) || candidate.starts_with(root));
    if covered {
        return Ok(false);
    }
    cfg.roots.push(paths::contract(&candidate));
    Ok(true)
}

/// Ask the resolved provider for the owner's raw repo list. No filtering
/// happens here — forks, archives, and globs belong to the planner, and the
/// raw list is what orphan detection needs.
pub async fn list_for(org: &OrgConfig, timeout: Duration) -> Result<Vec<OrgRepo>> {
    match org.resolved_provider() {
        OrgProvider::GitHub => crate::gh::list_owner(&org.owner, timeout).await,
        OrgProvider::GitLab => {
            crate::gitlab::list_owner(&org.owner, &org.host, org.include_subgroups, timeout).await
        }
        OrgProvider::Gitea => crate::gitea::list_owner(&org.owner, &org.login, timeout).await,
    }
}

/// Run a plan, strictly serially.
///
/// One repo at a time, one sorted pass: clones and updates interleaved by
/// name, each with its own timeout from the config. This is deliberately not
/// `spawn_fetch`'s semaphore-and-JoinSet shape; `remote.concurrency` stays
/// for dashboard fetches only.
///
/// A pull that fails — diverged, dirty, detached, no upstream — is a skip
/// with a reason, never an error and never lost work; that is the whole
/// safety story of `--ff-only`. Only a failed *clone* is an Error, and
/// `git::clone` has already removed the partial directory it may have left.
///
/// Orphans and config-level skips are reported without progress ticks: they
/// are not work, and the report explains them outright.
pub async fn execute(
    org: &OrgConfig,
    plan: &SyncPlan,
    cfg: &Config,
    tx: Option<&tokio::sync::mpsc::UnboundedSender<SyncEvent>>,
) -> Vec<SyncOutcome> {
    let mut outcomes = Vec::new();

    // Send failures are irrelevant here: a dropped channel just means the
    // dashboard went away, and the sync should finish regardless.
    let emit = |event: SyncEvent| {
        if let Some(tx) = tx {
            let _ = tx.send(event);
        }
    };

    match effective_path(cfg, org) {
        Err(err) => {
            // One config-level error, not one per repo: the path resolves the
            // same way for every repo, and stamping the same complaint onto
            // each row helps nobody. Orphans and skips still report — they
            // need only names, not the resolved path.
            let outcome = SyncOutcome {
                action: Action::Error,
                name: org.owner.clone(),
                detail: format!("org path could not be resolved: {}", first_line(&err)),
            };
            emit(SyncEvent::Repo(outcome.clone()));
            outcomes.push(outcome);
        }
        Ok(root) => {
            enum Work {
                Clone(OrgRepo),
                Update(PathBuf),
            }
            let mut work: Vec<(String, Work)> = plan
                .to_clone
                .iter()
                .map(|repo| (repo.name.clone(), Work::Clone(repo.clone())))
                .chain(plan.to_update.iter().map(|path| {
                    let name = path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or_default()
                        .to_string();
                    (name, Work::Update(path.clone()))
                }))
                .collect();
            work.sort_by(|a, b| a.0.cmp(&b.0));
            let total = work.len() + plan.orphans.len() + plan.skipped.len();

            for (done, (name, item)) in work.into_iter().enumerate() {
                emit(SyncEvent::Progress {
                    done,
                    total,
                    label: format!(
                        "{} {}",
                        match item {
                            Work::Clone(_) => "cloning",
                            Work::Update(_) => "updating",
                        },
                        name
                    ),
                });

                let outcome = match item {
                    Work::Clone(repo) => {
                        let url = clone_url(org, &repo);
                        match git::clone(url, &root.join(&repo.name), cfg.remote_timeout()).await {
                            Ok(()) => SyncOutcome {
                                action: Action::Cloned,
                                name: repo.name.clone(),
                                detail: format!("cloned from {}", url),
                            },
                            Err(err) => SyncOutcome {
                                action: Action::Error,
                                name: repo.name,
                                detail: first_line(&err),
                            },
                        }
                    }
                    Work::Update(path) => match git::pull_ff(&path, cfg.remote_timeout()).await {
                        Ok(stdout) => {
                            let summary = last_line(&stdout);
                            let (action, detail) = if summary.contains("Already up to date") {
                                (Action::Current, summary)
                            } else {
                                (Action::Updated, summary)
                            };
                            SyncOutcome {
                                action,
                                name,
                                detail,
                            }
                        }
                        Err(err) => SyncOutcome {
                            action: Action::Skipped,
                            name,
                            detail: first_line(&err),
                        },
                    },
                };

                emit(SyncEvent::Repo(outcome.clone()));
                outcomes.push(outcome);
            }
        }
    }

    // Orphans: reported, never touched.
    for path in &plan.orphans {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string_lossy().into_owned());
        let outcome = SyncOutcome {
            action: Action::Orphaned,
            name,
            detail: format!("on disk, not in {}'s repo list", org.owner),
        };
        emit(SyncEvent::Repo(outcome.clone()));
        outcomes.push(outcome);
    }

    // Config-level skips: the report explains why each was passed over.
    for skip in &plan.skipped {
        let outcome = SyncOutcome {
            action: Action::Skipped,
            name: skip.name.clone(),
            detail: skip.reason.clone(),
        };
        emit(SyncEvent::Repo(outcome.clone()));
        outcomes.push(outcome);
    }

    outcomes
}

/// Turn a listing result plus the disk state into outcomes, without the
/// listing itself — the seam that keeps the degrade path testable without
/// network (a failed listing is just an `Err` handed in).
///
/// The guard rail: a *failed* listing degrades to update-only over whatever
/// is already on disk, with one Error row leading the report. An empty
/// remote must never read as "the whole org is orphans". A listing that
/// succeeds empty is trusted as real — an owner with zero repos is not an
/// error, and the orphans it implies are honest.
async fn finish_sync(
    org: &OrgConfig,
    cfg: &Config,
    tx: Option<&tokio::sync::mpsc::UnboundedSender<SyncEvent>>,
    remote: Result<Vec<OrgRepo>>,
) -> Vec<SyncOutcome> {
    let root = match effective_path(cfg, org) {
        Ok(root) => root,
        Err(err) => {
            return vec![SyncOutcome {
                action: Action::Error,
                name: org.owner.clone(),
                detail: format!("org path could not be resolved: {}", first_line(&err)),
            }];
        }
    };
    let disk = disk_repos(&root);

    match remote {
        Ok(list) => {
            let plan = plan(&list, &disk, org);
            execute(org, &plan, cfg, tx).await
        }
        Err(err) => {
            // Degraded: the remote truth is unknown, so nothing on disk may
            // be accused of being an orphan. Update what is there and lead
            // the report with the failure.
            let degraded = SyncPlan {
                to_update: disk,
                ..Default::default()
            };
            let mut outcomes = vec![SyncOutcome {
                action: Action::Error,
                name: org.owner.clone(),
                detail: format!("listing failed: {}", first_line(&err)),
            }];
            outcomes.extend(execute(org, &degraded, cfg, tx).await);
            outcomes
        }
    }
}

/// Remember how a sync went, for the dashboard's LAST SYNC column. The cache
/// is display data, so `save_org_states` already treats a failed write as a
/// log line rather than an error.
fn record_state(org: &OrgConfig, outcomes: &[SyncOutcome]) {
    let count = |action: Action| outcomes.iter().filter(|o| o.action == action).count();
    let last_error = outcomes
        .iter()
        .find(|o| o.action == Action::Error)
        .map(|o| o.detail.clone());
    cache::save_org_states(vec![cache::OrgSyncState {
        provider: org.resolved_provider().as_str().to_string(),
        host: org.host.clone(),
        owner: org.owner.clone(),
        last_sync_at: git::now_unix(),
        cloned: count(Action::Cloned),
        updated: count(Action::Updated),
        current: count(Action::Current),
        skipped: count(Action::Skipped),
        orphans: count(Action::Orphaned),
        errors: count(Action::Error),
        last_error,
    }]);
}

/// One org, end to end: resolve the path, list, plan, execute serially, and
/// record the outcome in the cache.
pub async fn sync_org(
    org: &OrgConfig,
    cfg: &Config,
    tx: Option<&tokio::sync::mpsc::UnboundedSender<SyncEvent>>,
) -> Result<Vec<SyncOutcome>> {
    effective_path(cfg, org)?;
    let remote = list_for(org, cfg.remote_timeout()).await;
    let outcomes = finish_sync(org, cfg, tx, remote).await;
    record_state(org, &outcomes);
    Ok(outcomes)
}

/// Build the plan without running it — the `--dry-run` backing.
///
/// A failed listing is an error here rather than a degraded plan: a dry run
/// that printed "everything is orphans" because `gh` hiccuped would be more
/// alarming than one that simply refused. `list_for` is async while this is
/// not, so it is parked on the ambient runtime when there is one (the CLI
/// always runs inside tokio), or on a throwaway runtime when there isn't.
pub fn plan_only(org: &OrgConfig, cfg: &Config) -> Result<(SyncPlan, PathBuf)> {
    let root = effective_path(cfg, org)?;
    let timeout = cfg.remote_timeout();

    let remote = match tokio::runtime::Handle::try_current() {
        Ok(handle) => tokio::task::block_in_place(|| handle.block_on(list_for(org, timeout)))?,
        Err(_) => {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .context("Building a runtime for listing")?;
            rt.block_on(list_for(org, timeout))?
        }
    };

    let disk = disk_repos(&root);
    Ok((plan(&remote, &disk, org), root))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(name: &str) -> OrgRepo {
        OrgRepo {
            name: name.to_string(),
            ssh_url: String::new(),
            https_url: String::new(),
            archived: false,
            fork: false,
        }
    }

    fn org() -> OrgConfig {
        OrgConfig {
            host: "github.com".into(),
            owner: "acme".into(),
            ..OrgConfig::default()
        }
    }

    fn cfg_with_root(dir: &Path) -> Config {
        Config {
            roots: vec![dir.display().to_string()],
            ..Default::default()
        }
    }

    // ------------------------------------------------------------------
    // Planner
    // ------------------------------------------------------------------

    #[test]
    fn plan_buckets_by_presence_on_disk() {
        let remote = vec![repo("alpha"), repo("beta"), repo("gamma")];
        let disk = vec![PathBuf::from("/r/beta"), PathBuf::from("/r/delta")];
        let plan = plan(&remote, &disk, &org());

        assert_eq!(
            plan.to_clone
                .iter()
                .map(|r| r.name.as_str())
                .collect::<Vec<_>>(),
            vec!["alpha", "gamma"]
        );
        assert_eq!(plan.to_update, vec![PathBuf::from("/r/beta")]);
        assert_eq!(plan.orphans, vec![PathBuf::from("/r/delta")]);
        assert!(plan.skipped.is_empty());
    }

    #[test]
    fn forks_archives_and_globs_are_skipped_with_reasons() {
        let mut forked = repo("forky");
        forked.fork = true;
        let mut archived = repo("old-thing");
        archived.archived = true;

        let mut cfg = org();
        // "[bad" is unparseable; the sync must tolerate it rather than fail.
        cfg.exclude = vec!["sandbox-*".into(), "[bad".into()];

        let remote = vec![repo("kept"), forked, archived, repo("sandbox-x")];
        let plan = plan(&remote, &[], &cfg);

        assert_eq!(plan.to_clone.len(), 1);
        assert_eq!(plan.to_clone[0].name, "kept");
        let skipped: Vec<(&str, &str)> = plan
            .skipped
            .iter()
            .map(|s| (s.name.as_str(), s.reason.as_str()))
            .collect();
        assert_eq!(
            skipped,
            vec![
                ("forky", "fork"),
                ("old-thing", "archived"),
                ("sandbox-x", "excluded by pattern"),
            ]
        );
    }

    #[test]
    fn include_flags_keep_would_be_skips() {
        let mut forked = repo("forky");
        forked.fork = true;
        let mut cfg = org();
        cfg.include_forks = true;
        let plan = plan(&[forked], &[], &cfg);
        assert!(plan.skipped.is_empty());
        assert_eq!(plan.to_clone.len(), 1);
    }
    #[test]
    fn buckets_come_out_sorted_by_name() {
        let remote = vec![repo("zulu"), repo("alpha"), repo("mike")];
        let disk = vec![
            PathBuf::from("/r/zulu"),
            PathBuf::from("/r/beta"),
            PathBuf::from("/r/yank"),
        ];
        let plan = plan(&remote, &disk, &org());

        assert_eq!(
            plan.to_clone
                .iter()
                .map(|r| r.name.as_str())
                .collect::<Vec<_>>(),
            vec!["alpha", "mike"]
        );
        assert_eq!(plan.to_update, vec![PathBuf::from("/r/zulu")]);
        assert_eq!(
            plan.orphans,
            vec![PathBuf::from("/r/beta"), PathBuf::from("/r/yank")]
        );
    }

    // ------------------------------------------------------------------
    // Disk discovery
    // ------------------------------------------------------------------

    #[test]
    fn disk_repos_finds_checkouts_and_skips_plain_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // A real checkout, a worktree (its `.git` is a file), and things
        // that are not repos at all.
        std::fs::create_dir_all(root.join("real/.git")).unwrap();
        std::fs::create_dir_all(root.join("worktree")).unwrap();
        std::fs::write(root.join("worktree/.git"), "gitdir: ../real/.git").unwrap();
        std::fs::create_dir_all(root.join("plain")).unwrap();
        std::fs::write(root.join("loose.txt"), "").unwrap();

        assert_eq!(
            disk_repos(root),
            vec![root.join("real"), root.join("worktree")]
        );
    }

    #[test]
    fn a_missing_disk_dir_is_an_empty_answer_not_an_error() {
        assert!(disk_repos(Path::new("/nonexistent/no/such/dir")).is_empty());
    }

    // ------------------------------------------------------------------
    // Path resolution
    // ------------------------------------------------------------------

    #[test]
    fn effective_path_prefers_the_org_path_over_the_first_root() {
        let cfg = cfg_with_root(Path::new("/somewhere"));
        let mut org = org();
        org.path = Some("~/custom/spot".into());
        assert_eq!(
            effective_path(&cfg, &org).unwrap(),
            paths::expand("~/custom/spot")
        );
    }

    #[test]
    fn effective_path_falls_back_to_the_first_root_plus_owner() {
        let cfg = cfg_with_root(Path::new("/somewhere"));
        assert_eq!(
            effective_path(&cfg, &org()).unwrap(),
            PathBuf::from("/somewhere/acme")
        );
    }

    #[test]
    fn effective_path_without_any_root_is_an_error() {
        let cfg = Config {
            roots: Vec::new(),
            ..Config::default()
        };
        assert!(effective_path(&cfg, &org()).is_err());
    }

    // ------------------------------------------------------------------
    // Execution against `file://` remotes — no network anywhere.
    // ------------------------------------------------------------------

    /// One-commit source repo to clone from; see the same helper in git.rs.
    fn source_repo(dir: &Path, name: &str) -> PathBuf {
        let src = dir.join(name);
        std::fs::create_dir_all(&src).unwrap();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(&src)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@e")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@e")
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}: {out:?}");
        };
        git(&["init", "-q", "-b", "main", "."]);
        std::fs::write(src.join("f.txt"), "one").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-qm", "one"]);
        src
    }

    fn cfg_with_timeout(dir: &Path) -> Config {
        let mut cfg = cfg_with_root(dir);
        // A tight remote timeout keeps a wedged child from hanging a test.
        cfg.remote.timeout = "30s".into();
        cfg
    }

    fn remote_with_file_urls(dir: &Path, names: &[&str]) -> Vec<OrgRepo> {
        names
            .iter()
            .map(|name| {
                let mut r = repo(name);
                let url = format!("file://{}", dir.join(name).display());
                r.ssh_url = url.clone();
                r.https_url = url;
                r
            })
            .collect()
    }

    #[tokio::test]
    async fn execute_clones_then_reports_current_on_the_second_run() {
        let dir = tempfile::tempdir().unwrap();
        let sources = tempfile::tempdir().unwrap();
        source_repo(sources.path(), "alpha");
        let remote = remote_with_file_urls(sources.path(), &["alpha"]);
        let cfg = cfg_with_timeout(dir.path());
        let fleet = effective_path(&cfg, &org()).unwrap();

        let first = plan(&remote, &[], &org());
        let outcomes = execute(&org(), &first, &cfg, None).await;
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].action, Action::Cloned);
        assert!(fleet.join("alpha/.git").exists(), "the clone happened");

        let second = plan(&remote, &disk_repos(&fleet), &org());
        let outcomes = execute(&org(), &second, &cfg, None).await;
        assert_eq!(outcomes[0].action, Action::Current, "{:?}", outcomes[0]);
    }

    #[tokio::test]
    async fn a_clone_that_is_behind_comes_back_updated() {
        let dir = tempfile::tempdir().unwrap();
        let src = source_repo(dir.path(), "src");
        let work = tempfile::tempdir().unwrap();

        let mut alpha = repo("alpha");
        alpha.ssh_url = format!("file://{}", src.display());
        alpha.https_url = alpha.ssh_url.clone();
        let cfg = cfg_with_timeout(work.path());
        let fleet = effective_path(&cfg, &org()).unwrap();

        let first = plan(&[alpha.clone()], &[], &org());
        execute(&org(), &first, &cfg, None).await;

        // Move the source forward so the clone is behind.
        std::fs::write(src.join("f.txt"), "two").unwrap();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(&src)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@e")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@e")
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}: {out:?}");
        };
        git(&["add", "-A"]);
        git(&["commit", "-qm", "two"]);

        let second = plan(&[alpha], &[fleet.join("alpha")], &org());
        let outcomes = execute(&org(), &second, &cfg, None).await;
        assert_eq!(outcomes[0].action, Action::Updated, "{:?}", outcomes[0]);
        assert!(
            !outcomes[0].detail.is_empty(),
            "the update detail should carry git's summary: {:?}",
            outcomes[0]
        );
    }

    // The promise that makes running sync against a fleet safe: a repo it
    // cannot fast-forward comes back as a skip, byte-identical.
    #[tokio::test]
    async fn a_diverged_clone_is_skipped_and_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let src = source_repo(dir.path(), "src");
        let work = tempfile::tempdir().unwrap();

        let mut alpha = repo("alpha");
        alpha.ssh_url = format!("file://{}", src.display());
        alpha.https_url = alpha.ssh_url.clone();

        let cfg = cfg_with_timeout(work.path());
        let fleet = effective_path(&cfg, &org()).unwrap();
        let first = plan(&[alpha.clone()], &[], &org());
        execute(&org(), &first, &cfg, None).await;
        let checkout = fleet.join("alpha");

        let git = |args: &[&str], cwd: &Path| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(cwd)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@e")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@e")
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}: {out:?}");
        };
        // Diverge: a local commit *and* an upstream commit.
        std::fs::write(checkout.join("local.txt"), "local").unwrap();
        git(&["add", "-A"], &checkout);
        git(&["commit", "-qm", "local"], &checkout);
        std::fs::write(src.join("upstream.txt"), "upstream").unwrap();
        git(&["add", "-A"], &src);
        git(&["commit", "-qm", "upstream"], &src);

        let before = std::fs::read_to_string(checkout.join("local.txt")).unwrap();
        let second = plan(&[alpha], &[checkout.clone()], &org());
        let outcomes = execute(&org(), &second, &cfg, None).await;
        assert_eq!(outcomes[0].action, Action::Skipped, "{:?}", outcomes[0]);
        assert_eq!(
            std::fs::read_to_string(checkout.join("local.txt")).unwrap(),
            before,
            "the diverged tree must be byte-identical after the skip"
        );
    }

    #[tokio::test]
    async fn orphans_are_reported_and_never_touched() {
        let dir = tempfile::tempdir().unwrap();
        let src = source_repo(dir.path(), "src");
        let work = tempfile::tempdir().unwrap();

        // A checkout on disk the remote list does not name: clone one, then
        // list only a different repo.
        let cfg = cfg_with_timeout(work.path());
        let mut listed = repo("listed");
        listed.ssh_url = format!("file://{}", src.display());
        listed.https_url = listed.ssh_url.clone();
        let first = plan(&[listed], &[], &org());
        execute(&org(), &first, &cfg, None).await;

        let stray = work.path().join("stray");
        std::fs::create_dir_all(stray.join(".git")).unwrap();

        let mut other = repo("other");
        other.ssh_url = format!("file://{}", src.display());
        other.https_url = other.ssh_url.clone();
        let second = plan(&[other], &disk_repos(work.path()), &org());
        let outcomes = execute(&org(), &second, &cfg, None).await;

        let orphan = outcomes
            .iter()
            .find(|o| o.action == Action::Orphaned)
            .expect("the stray checkout should be reported");
        assert_eq!(orphan.name, "stray");
        assert_eq!(orphan.detail, "on disk, not in acme's repo list");
        assert!(stray.join(".git").exists(), "orphans are never removed");
    }

    #[tokio::test]
    async fn a_failed_clone_leaves_no_directory_and_reports_an_error() {
        let _dir = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();

        let mut repo = repo("doomed");
        repo.ssh_url = "file:///nonexistent/no-such-repo".into();
        repo.https_url = repo.ssh_url.clone();

        let plan = plan(&[repo], &[], &org());
        let outcomes = execute(&org(), &plan, &cfg_with_timeout(work.path()), None).await;

        assert_eq!(outcomes[0].action, Action::Error, "{:?}", outcomes[0]);
        assert!(
            !work.path().join("doomed").exists(),
            "a partial clone must not survive as a checkout"
        );
    }

    /// A strictly serial execution produces events that never interleave:
    /// each Progress tick is immediately followed by its own repo's outcome,
    /// and `done` climbs one at a time from zero.
    #[tokio::test]
    async fn progress_events_arrive_strictly_one_at_a_time() {
        let dir = tempfile::tempdir().unwrap();
        let sources = tempfile::tempdir().unwrap();
        for name in ["alpha", "beta", "gamma"] {
            source_repo(sources.path(), name);
        }
        let cfg = cfg_with_timeout(dir.path());
        let remote = remote_with_file_urls(sources.path(), &["alpha", "beta", "gamma"]);
        let plan = plan(&remote, &[], &org());

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let outcomes = execute(&org(), &plan, &cfg, Some(&tx)).await;
        drop(tx);

        assert_eq!(outcomes.len(), 3);
        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            events.push(event);
        }

        // 3 × (Progress + Repo), in exactly that alternating shape.
        assert_eq!(events.len(), 6);
        for (i, pair) in events.chunks(2).enumerate() {
            match (&pair[0], &pair[1]) {
                (
                    SyncEvent::Progress {
                        done,
                        total,
                        label: _,
                    },
                    SyncEvent::Repo(outcome),
                ) => {
                    assert_eq!(*done, i, "progress must climb one at a time");
                    assert_eq!(*total, 3);
                    assert_eq!(outcome.name, name_in_label(&pair[0]));
                    let expected = format!(
                        "{} {}",
                        if outcome.action == Action::Cloned {
                            "cloning"
                        } else {
                            "updating"
                        },
                        outcome.name
                    );
                    assert_eq!(label_of(&pair[0]), expected);
                }
                other => panic!("events must alternate Progress/Repo, got {other:?}"),
            }
        }
    }

    fn label_of(event: &SyncEvent) -> String {
        match event {
            SyncEvent::Progress { label, .. } => label.clone(),
            _ => panic!("not a progress event"),
        }
    }

    fn name_in_label(event: &SyncEvent) -> String {
        label_of(event).split(' ').nth(1).unwrap_or("").to_string()
    }

    // ------------------------------------------------------------------
    // The degrade path
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn a_failed_listing_degrades_to_update_only_with_one_error_row() {
        let dir = tempfile::tempdir().unwrap();
        let src = source_repo(dir.path(), "src");
        let work = tempfile::tempdir().unwrap();

        // Something already on disk, so the degraded plan has work to do.
        let mut repo = repo("alpha");
        repo.ssh_url = format!("file://{}", src.display());
        repo.https_url = repo.ssh_url.clone();
        let cfg = cfg_with_timeout(work.path());
        let plan = plan(&[repo], &[], &org());
        execute(&org(), &plan, &cfg, None).await;

        // The listing failed: the sync must NOT conclude the org is empty
        // and report the checkout as an orphan.
        let outcomes = finish_sync(&org(), &cfg, None, Err(anyhow!("gh failed: boom"))).await;

        assert_eq!(outcomes[0].action, Action::Error);
        assert_eq!(outcomes[0].detail, "listing failed: gh failed: boom");
        assert!(
            !outcomes.iter().any(|o| o.action == Action::Orphaned),
            "a failed listing must never read as an empty org: {:?}",
            outcomes
        );
        assert!(
            outcomes.iter().any(|o| o.action == Action::Current),
            "the on-disk checkout should still be updated: {:?}",
            outcomes
        );
    }

    #[tokio::test]
    async fn an_empty_but_successful_listing_is_trusted() {
        let dir = tempfile::tempdir().unwrap();
        let src = source_repo(dir.path(), "src");
        let work = tempfile::tempdir().unwrap();

        let mut repo = repo("alpha");
        repo.ssh_url = format!("file://{}", src.display());
        repo.https_url = repo.ssh_url.clone();
        let cfg = cfg_with_timeout(work.path());
        let fleet = effective_path(&cfg, &org()).unwrap();
        let plan = plan(&[repo], &[], &org());
        execute(&org(), &plan, &cfg, None).await;

        // The owner really lists nothing now. The checkout is an orphan —
        // honestly reported, honestly untouched.
        let outcomes = finish_sync(&org(), &cfg, None, Ok(Vec::new())).await;
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].action, Action::Orphaned);
        assert!(fleet.join("alpha").exists());
    }

    #[tokio::test]
    async fn a_failed_path_resolution_reports_one_error_and_no_work() {
        let org = OrgConfig {
            host: "github.com".into(),
            owner: "acme".into(),
            ..OrgConfig::default()
        };
        // No roots, no path: nothing can run.
        let cfg = Config {
            roots: Vec::new(),
            ..Config::default()
        };
        let mut repo = repo("alpha");
        repo.ssh_url = "file:///nowhere".into();
        repo.https_url = repo.ssh_url.clone();
        let plan = plan(&[repo], &[], &org);

        let outcomes = execute(&org, &plan, &cfg, None).await;
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].action, Action::Error);
        assert_eq!(outcomes[0].name, "acme");
        assert!(outcomes[0]
            .detail
            .starts_with("org path could not be resolved"));
    }

    // ------------------------------------------------------------------
    // Scan-root wiring
    // ------------------------------------------------------------------

    #[test]
    fn path_under_an_existing_root_is_a_noop() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = cfg_with_root(dir.path());
        let org = OrgConfig {
            host: "github.com".into(),
            owner: "acme".into(),
            // The parent of this path is the root itself.
            path: Some(format!("{}/acme", dir.path().display())),
            ..OrgConfig::default()
        };

        assert_eq!(ensure_scan_root(&mut cfg, &org).unwrap(), false);
        assert_eq!(cfg.roots, vec![dir.path().display().to_string()]);
    }

    #[test]
    fn path_outside_the_roots_adds_the_parent() {
        let mut cfg = Config {
            roots: vec!["~/elsewhere".into()],
            ..Config::default()
        };
        let org = OrgConfig {
            host: "github.com".into(),
            owner: "acme".into(),
            path: Some("~/org-checkouts/acme".into()),
            ..OrgConfig::default()
        };

        assert_eq!(ensure_scan_root(&mut cfg, &org).unwrap(), true);
        // Stored `~`-contracted, so a hand-edited config stays readable.
        assert_eq!(
            cfg.roots,
            vec!["~/elsewhere".to_string(), "~/org-checkouts".to_string()]
        );
    }

    #[test]
    fn adding_the_same_parent_twice_is_idempotent() {
        let mut cfg = Config {
            roots: vec!["~/elsewhere".into()],
            ..Config::default()
        };
        let org = OrgConfig {
            host: "github.com".into(),
            owner: "acme".into(),
            path: Some("~/org-checkouts/acme".into()),
            ..OrgConfig::default()
        };

        assert_eq!(ensure_scan_root(&mut cfg, &org).unwrap(), true);
        assert_eq!(ensure_scan_root(&mut cfg, &org).unwrap(), false);
        assert_eq!(cfg.roots.len(), 2);
    }

    #[test]
    fn a_second_org_under_a_newly_added_parent_is_a_noop() {
        let mut cfg = Config {
            roots: vec!["~/elsewhere".into()],
            ..Config::default()
        };
        let first = OrgConfig {
            host: "github.com".into(),
            owner: "acme".into(),
            path: Some("~/org-checkouts/acme".into()),
            ..OrgConfig::default()
        };
        let second = OrgConfig {
            host: "github.com".into(),
            owner: "other".into(),
            path: Some("~/org-checkouts/other".into()),
            ..OrgConfig::default()
        };

        assert_eq!(ensure_scan_root(&mut cfg, &first).unwrap(), true);
        assert_eq!(ensure_scan_root(&mut cfg, &second).unwrap(), false);
        assert_eq!(
            cfg.roots,
            vec!["~/elsewhere".to_string(), "~/org-checkouts".to_string()]
        );
    }

    #[test]
    fn a_root_level_path_falls_back_to_the_path_itself() {
        let mut cfg = Config {
            roots: vec!["/elsewhere".into()],
            ..Config::default()
        };
        let org = OrgConfig {
            host: "github.com".into(),
            owner: "acme".into(),
            // Parent is `/`: the org path itself becomes the root, because
            // an ungrouped row still beats an invisible one. Only a
            // single-component absolute path has `/` for a parent.
            path: Some("/srv".into()),
            ..OrgConfig::default()
        };

        assert_eq!(ensure_scan_root(&mut cfg, &org).unwrap(), true);
        assert_eq!(
            cfg.roots,
            vec!["/elsewhere".to_string(), "/srv".to_string()]
        );
    }

    #[test]
    fn no_roots_and_no_path_is_an_error() {
        let mut cfg = Config {
            roots: Vec::new(),
            ..Config::default()
        };
        let org = OrgConfig {
            host: "github.com".into(),
            owner: "acme".into(),
            ..OrgConfig::default()
        };

        assert!(ensure_scan_root(&mut cfg, &org).is_err());
        assert!(cfg.roots.is_empty());
    }
}
