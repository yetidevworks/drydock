//! The probe cache.
//!
//! Nothing in here is authoritative; it exists so that opening the dashboard
//! paints a full table immediately instead of an empty one, and so a restart
//! can skip working-tree scans for repos that haven't changed. A missing or
//! unreadable cache is a non-event.
//!
//! Two independent writers share the one file: the repo sweep (`save`) and
//! the org-sync engine (`save_org_states`). Each writes only its own half and
//! *carries the other half through* — `save` preserves the org states already
//! in the file, `save_org_states` preserves the repos. Neither can be the
//! sole owner, because neither knows when the other last ran, and dropping
//! the other's half would quietly reset the dashboard's last-sync column
//! (or the whole probe cache) every time the other feature ran.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::model::RepoStatus;
use crate::paths;

/// Bump when the stored structs change shape in a way that makes old entries
/// misleading rather than merely incomplete.
const CACHE_VERSION: u32 = 1;

#[derive(Debug, Serialize, Deserialize)]
pub struct Cache {
    pub version: u32,
    pub saved_at: i64,
    pub repos: Vec<RepoStatus>,
    /// Org-sync outcomes, last run per registered owner. Additive with a
    /// default, so cache files written before org sync existed still load —
    /// which is why `CACHE_VERSION` did not move.
    #[serde(default)]
    pub orgs: Vec<OrgSyncState>,
}

impl Default for Cache {
    fn default() -> Self {
        Self {
            version: CACHE_VERSION,
            saved_at: 0,
            repos: Vec::new(),
            orgs: Vec::new(),
        }
    }
}

/// What one registered org's most recent sync looked like: when it ran and
/// how each bucket came out. This is display data for the org manager, not a
/// queue or a lock — nothing here gates a future sync.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrgSyncState {
    pub provider: String,
    pub host: String,
    pub owner: String,
    pub last_sync_at: i64,
    pub cloned: usize,
    pub updated: usize,
    pub current: usize,
    pub skipped: usize,
    pub orphans: usize,
    pub errors: usize,
    pub last_error: Option<String>,
}

/// Read the cache file if it is readable *and* written by this version. A
/// missing, unreadable, or foreign-version cache is a non-event rather than
/// an error — the cache is disposable by design.
fn read_cache_file(path: &Path) -> Option<Cache> {
    let Ok(body) = std::fs::read_to_string(path) else {
        return None;
    };
    match serde_json::from_str::<Cache>(&body) {
        Ok(cache) if cache.version == CACHE_VERSION => Some(cache),
        Ok(_) => {
            tracing::debug!("ignoring cache written by a different version");
            None
        }
        Err(err) => {
            tracing::debug!(%err, "ignoring unreadable cache");
            None
        }
    }
}

/// Write the whole cache atomically. The version and timestamp are stamped
/// here so every writer produces the same shape of file.
fn write_cache_file(path: &Path, mut cache: Cache) -> Result<()> {
    cache.version = CACHE_VERSION;
    cache.saved_at = crate::git::now_unix();
    let body = serde_json::to_string(&cache).context("Serializing cache")?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("Creating {}", dir.display()))?;
    }
    // Write via a temp file so an interrupted save can't leave a truncated
    // cache behind.
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, body).with_context(|| format!("Writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("Replacing {}", path.display()))?;
    Ok(())
}

/// The identity of an org state: provider, host, owner — the same triple the
/// config uses to name a registration.
fn org_key(state: &OrgSyncState) -> (String, String, String) {
    (
        state.provider.clone(),
        state.host.clone(),
        state.owner.clone(),
    )
}

/// Load the cache, keyed by repo root for quick lookup during a sweep.
pub fn load() -> HashMap<PathBuf, RepoStatus> {
    match paths::cache_file() {
        Ok(path) => read_cache_file(&path)
            .map(|cache| {
                cache
                    .repos
                    .into_iter()
                    .map(|r| (r.root.clone(), r))
                    .collect()
            })
            .unwrap_or_default(),
        Err(_) => HashMap::new(),
    }
}

pub fn save(repos: &[RepoStatus]) -> Result<()> {
    match paths::cache_file() {
        Ok(path) => save_repos_at(&path, repos),
        Err(err) => Err(err).context("Could not determine a cache directory"),
    }
}

/// Replace the repo half of the cache, carrying any org states through
/// untouched. The read-modify-write is what keeps the two writers from
/// erasing each other; see the module doc.
fn save_repos_at(path: &Path, repos: &[RepoStatus]) -> Result<()> {
    let mut cache = read_cache_file(path).unwrap_or_default();
    cache.repos = repos.to_vec();
    write_cache_file(path, cache)
}

/// Load the org-sync states, keyed by (provider, host, owner).
pub fn load_org_states() -> HashMap<(String, String, String), OrgSyncState> {
    match paths::cache_file() {
        Ok(path) => load_org_states_at(&path),
        Err(_) => HashMap::new(),
    }
}

fn load_org_states_at(path: &Path) -> HashMap<(String, String, String), OrgSyncState> {
    read_cache_file(path)
        .map(|cache| {
            cache
                .orgs
                .into_iter()
                .map(|state| (org_key(&state), state))
                .collect()
        })
        .unwrap_or_default()
}

/// Record the latest sync outcome per org. States merge by
/// (provider, host, owner): an upsert replaces its own key and leaves the
/// rest alone, and the repo half of the cache rides through untouched — the
/// sweep's data is not this writer's to drop.
///
/// The cache is display data, so a failed write is logged rather than
/// surfaced: the next sync rewrites the whole state anyway.
pub fn save_org_states(upserts: Vec<OrgSyncState>) {
    let Ok(path) = paths::cache_file() else {
        return;
    };
    if let Err(err) = save_org_states_at(&path, upserts) {
        tracing::debug!(%err, "could not save org sync states");
    }
}

fn save_org_states_at(path: &Path, upserts: Vec<OrgSyncState>) -> Result<()> {
    let mut cache = read_cache_file(path).unwrap_or_default();
    for upsert in upserts {
        let key = org_key(&upsert);
        match cache.orgs.iter_mut().find(|state| org_key(state) == key) {
            Some(existing) => *existing = upsert,
            None => cache.orgs.push(upsert),
        }
    }
    write_cache_file(path, cache)
}

pub fn clear() -> Result<()> {
    let path = paths::cache_file()?;
    if path.exists() {
        std::fs::remove_file(&path).with_context(|| format!("Removing {}", path.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::RepoStatus;

    // The public fns resolve the real cache dir, which a test must not write
    // to; these drive the same code paths against a tempdir instead.
    fn state(provider: &str, host: &str, owner: &str) -> OrgSyncState {
        OrgSyncState {
            provider: provider.into(),
            host: host.into(),
            owner: owner.into(),
            last_sync_at: 42,
            cloned: 1,
            updated: 2,
            current: 3,
            skipped: 4,
            orphans: 5,
            errors: 6,
            last_error: None,
        }
    }

    #[test]
    fn org_states_round_trip_through_the_cache_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        save_org_states_at(&path, vec![state("github", "github.com", "acme")]).unwrap();

        let loaded = load_org_states_at(&path);
        assert_eq!(loaded.len(), 1);
        let got = &loaded[&("github".into(), "github.com".into(), "acme".into())];
        assert_eq!(got.owner, "acme");
        assert_eq!(got.updated, 2);
        assert_eq!(got.last_sync_at, 42);
    }

    // The two writers share one file; neither may drop the other's half.
    #[test]
    fn saving_org_states_preserves_repos_written_earlier() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let repo = RepoStatus::new(dir.path().join("r"), "g".into(), "r1".into());
        save_repos_at(&path, &[repo]).unwrap();

        save_org_states_at(&path, vec![state("gitea", "g.example.com", "otter")]).unwrap();

        let cache = read_cache_file(&path).unwrap();
        assert_eq!(cache.repos.len(), 1, "repos must survive the org write");
        assert_eq!(cache.repos[0].name, "r1");
        assert_eq!(cache.orgs.len(), 1);
        assert_eq!(cache.orgs[0].owner, "otter");
    }

    #[test]
    fn an_upsert_replaces_its_own_key_and_leaves_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        save_org_states_at(
            &path,
            vec![
                state("github", "github.com", "acme"),
                state("gitlab", "gitlab.com", "acme"),
            ],
        )
        .unwrap();

        let mut replacement = state("github", "github.com", "acme");
        replacement.cloned = 99;
        replacement.last_error = Some("listing failed: boom".into());
        save_org_states_at(&path, vec![replacement]).unwrap();

        let loaded = load_org_states_at(&path);
        assert_eq!(loaded.len(), 2, "upsert must not displace other orgs");
        assert_eq!(
            loaded[&("github".into(), "github.com".into(), "acme".into())].cloned,
            99
        );
        assert_eq!(
            loaded[&("gitlab".into(), "gitlab.com".into(), "acme".into())].cloned,
            1
        );
    }

    // Org sync is additive on a version-1 file: an older cache without the
    // `orgs` field at all still loads, which is why CACHE_VERSION stayed put.
    #[test]
    fn a_cache_file_without_org_states_still_loads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let repo = RepoStatus::new(dir.path().join("r"), "g".into(), "r1".into());
        // Write through the current writer minus the orgs field, the way a
        // pre-org-sync version of drydock would have left it.
        let old = Cache {
            version: CACHE_VERSION,
            saved_at: 7,
            repos: vec![repo],
            orgs: Vec::new(),
        };
        let mut body = serde_json::to_string(&old).unwrap();
        // Crude but faithful: strip the empty `orgs` key the current struct
        // would have written, leaving the shape an older binary produced.
        body = body.replace(",\"orgs\":[]", "");
        std::fs::write(&path, body).unwrap();

        let loaded = load_org_states_at(&path);
        assert!(loaded.is_empty());
        let cache = read_cache_file(&path).unwrap();
        assert_eq!(cache.repos.len(), 1);
        assert_eq!(cache.saved_at, 7);
    }

    #[test]
    fn a_missing_cache_file_is_an_empty_answer() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        assert!(load_org_states_at(&path).is_empty());
    }
}
