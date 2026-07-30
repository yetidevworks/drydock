//! The probe cache.
//!
//! Nothing in here is authoritative; it exists so that opening the dashboard
//! paints a full table immediately instead of an empty one, and so a restart
//! can skip working-tree scans for repos that haven't changed. A missing or
//! unreadable cache is a non-event.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

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
}

impl Default for Cache {
    fn default() -> Self {
        Self {
            version: CACHE_VERSION,
            saved_at: 0,
            repos: Vec::new(),
        }
    }
}

/// Load the cache, keyed by repo root for quick lookup during a sweep.
pub fn load() -> HashMap<PathBuf, RepoStatus> {
    let Ok(path) = paths::cache_file() else {
        return HashMap::new();
    };
    let Ok(body) = std::fs::read_to_string(&path) else {
        return HashMap::new();
    };
    match serde_json::from_str::<Cache>(&body) {
        Ok(cache) if cache.version == CACHE_VERSION => cache
            .repos
            .into_iter()
            .map(|r| (r.root.clone(), r))
            .collect(),
        Ok(_) => {
            tracing::debug!("ignoring cache written by a different version");
            HashMap::new()
        }
        Err(err) => {
            tracing::debug!(%err, "ignoring unreadable cache");
            HashMap::new()
        }
    }
}

pub fn save(repos: &[RepoStatus]) -> Result<()> {
    let dir = paths::cache_dir()?;
    std::fs::create_dir_all(&dir).with_context(|| format!("Creating {}", dir.display()))?;
    let path = paths::cache_file()?;
    let cache = Cache {
        version: CACHE_VERSION,
        saved_at: crate::git::now_unix(),
        repos: repos.to_vec(),
    };
    let body = serde_json::to_string(&cache).context("Serializing cache")?;
    // Write via a temp file so an interrupted save can't leave a truncated
    // cache behind.
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, body).with_context(|| format!("Writing {}", tmp.display()))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("Replacing {}", path.display()))?;
    Ok(())
}

pub fn clear() -> Result<()> {
    let path = paths::cache_file()?;
    if path.exists() {
        std::fs::remove_file(&path).with_context(|| format!("Removing {}", path.display()))?;
    }
    Ok(())
}
