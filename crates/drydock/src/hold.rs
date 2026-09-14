//! Manual holds: "this commit state doesn't need a release".
//!
//! A repo reads as needing a release the moment anything lands past its newest
//! tag, and that is right far more often than it's wrong. But plenty of what
//! lands past a tag is a changelog line, a merge of a branch that was already
//! released, a README fix — work that is genuinely past the tag and genuinely
//! not worth cutting a version for. Across a few hundred repos those add up
//! until the needs-release list is mostly noise, which is the one thing a list
//! like that cannot afford to be.
//!
//! A hold takes one repo out of that list, and pins itself to the commit it
//! was placed at. That pin is the whole point: the hold covers *this* state
//! and nothing else, so the next commit lifts it on its own and the repo comes
//! back into the list without anyone having to remember it was ever held. A
//! hold that outlived the thing it described would be strictly worse than no
//! hold at all — it would hide real work while looking exactly like a repo
//! with nothing to ship.
//!
//! Holds live in the config directory rather than the cache, because they're
//! something a person decided rather than something a probe observed, and
//! clearing the cache must not throw them away.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::paths;

/// One repo's hold, pinned to the commit it was placed at.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Hold {
    /// HEAD when the hold was placed, as the short sha the rest of drydock
    /// works in. The only field matched on: everything else here is for
    /// saying what was held and when.
    pub sha: String,
    /// The branch HEAD was on, or `None` on a detached HEAD.
    pub branch: Option<String>,
    /// The tag the repo was sitting past.
    pub tag: Option<String>,
    /// When the hold was placed, unix seconds.
    pub at: i64,
    /// Why, if whoever placed it said.
    pub note: Option<String>,
}

impl Hold {
    /// True when this hold still describes what's checked out.
    pub fn covers(&self, head_sha: Option<&str>) -> bool {
        head_sha == Some(self.sha.as_str())
    }

    /// `1.0.3 (a1b2c3d)`, or just the sha when the repo had no reachable tag.
    pub fn label(&self) -> String {
        match &self.tag {
            Some(tag) => format!("{tag} ({})", self.sha),
            None => self.sha.clone(),
        }
    }
}

/// Every hold, keyed by repo root.
#[derive(Clone, Debug, Default)]
pub struct Holds {
    by_path: BTreeMap<PathBuf, Hold>,
}

impl Holds {
    /// The hold on this repo, if there is one. Says nothing about whether it
    /// still covers HEAD — that's [`Hold::covers`], and a lifted hold is still
    /// worth reporting.
    pub fn get(&self, root: &Path) -> Option<&Hold> {
        if self.by_path.is_empty() {
            return None;
        }
        if let Some(hold) = self.by_path.get(root) {
            return Some(hold);
        }
        // Only if the plain lookup missed, and only then: a repo reached
        // through a symlinked root is stored under the path it resolves to,
        // and this is what keeps that from silently never matching. Most
        // repos have no hold at all, so this costs one `realpath` on a miss
        // rather than one per repo per sweep.
        let real = root.canonicalize().ok()?;
        if real == root {
            return None;
        }
        self.by_path.get(&real)
    }

    pub fn set(&mut self, root: &Path, hold: Hold) {
        self.by_path.insert(key(root), hold);
    }

    pub fn remove(&mut self, root: &Path) -> Option<Hold> {
        self.by_path
            .remove(&key(root))
            .or_else(|| self.by_path.remove(root))
    }

    pub fn iter(&self) -> impl Iterator<Item = (&PathBuf, &Hold)> {
        self.by_path.iter()
    }

    pub fn is_empty(&self) -> bool {
        self.by_path.is_empty()
    }

    pub fn retain(&mut self, mut keep: impl FnMut(&Path, &Hold) -> bool) {
        self.by_path.retain(|path, hold| keep(path, hold));
    }
}

/// The path a hold is filed under: resolved where it can be, taken as written
/// where it can't. Writing and reading both go through this, so a repo named
/// one way on the command line and another by the scan lands in one entry.
fn key(root: &Path) -> PathBuf {
    root.canonicalize().unwrap_or_else(|_| root.to_path_buf())
}

/// Stamp the hold each of these repos is under, if any.
///
/// Called wherever repos come from — a sweep, the cache, a single probe — so
/// that [`crate::model::RepoStatus::release_state`] can answer without having
/// to reach for a file of its own.
pub fn apply(repos: &mut [crate::model::RepoStatus], holds: &Holds) {
    if holds.is_empty() {
        for repo in repos.iter_mut() {
            repo.hold = None;
        }
        return;
    }
    for repo in repos.iter_mut() {
        repo.hold = holds.get(&repo.root).cloned();
    }
}

// ---------------------------------------------------------------------------
// On disk
// ---------------------------------------------------------------------------

#[derive(Default, Serialize, Deserialize)]
struct HoldsFile {
    #[serde(default, rename = "hold")]
    holds: Vec<Entry>,
}

/// A hold as written out. Spelled out rather than flattened so the file stays
/// something you can edit by hand without guessing at the shape.
#[derive(Serialize, Deserialize)]
struct Entry {
    path: String,
    sha: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tag: Option<String>,
    at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    note: Option<String>,
}

/// Load every hold. A missing file is the normal state and reads as none; an
/// unreadable one is logged and treated the same way, because a dashboard that
/// refuses to open over a hand-edited file is worse than one that opens and
/// says nothing is held.
pub fn load() -> Holds {
    let Ok(path) = paths::holds_file() else {
        return Holds::default();
    };
    let Ok(body) = std::fs::read_to_string(&path) else {
        return Holds::default();
    };
    match toml::from_str::<HoldsFile>(&body) {
        Ok(file) => parse(file),
        Err(err) => {
            tracing::warn!(%err, path = %path.display(), "ignoring unreadable holds file");
            Holds::default()
        }
    }
}

fn parse(file: HoldsFile) -> Holds {
    let mut holds = Holds::default();
    for entry in file.holds {
        let root = paths::expand(&entry.path);
        holds.by_path.insert(
            key(&root),
            Hold {
                sha: entry.sha,
                branch: entry.branch,
                tag: entry.tag,
                at: entry.at,
                note: entry.note,
            },
        );
    }
    holds
}

pub fn save(holds: &Holds) -> Result<PathBuf> {
    let dir = paths::config_dir()?;
    std::fs::create_dir_all(&dir).with_context(|| format!("Creating {}", dir.display()))?;
    let path = paths::holds_file()?;

    let file = HoldsFile {
        holds: holds
            .by_path
            .iter()
            .map(|(root, hold)| Entry {
                path: root.display().to_string(),
                sha: hold.sha.clone(),
                branch: hold.branch.clone(),
                tag: hold.tag.clone(),
                at: hold.at,
                note: hold.note.clone(),
            })
            .collect(),
    };

    let body = format!(
        "{HEADER}{}",
        toml::to_string_pretty(&file).context("Serializing holds")?
    );
    // Same temp-file dance the cache does: an interrupted write must not be
    // able to leave a half-file that reads as "nothing is held".
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, body).with_context(|| format!("Writing {}", tmp.display()))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("Replacing {}", path.display()))?;
    Ok(path)
}

const HEADER: &str = "\
# drydock holds. Each entry takes one repo out of \"needs release\" for exactly
# the commit it names: when that repo's HEAD moves, the hold lifts itself and
# the repo comes back into the list. Written by `drydock hold` and the `h` key
# in the dashboard; `drydock holds` prints what's here.

";

#[cfg(test)]
mod tests {
    use super::*;

    fn hold(sha: &str) -> Hold {
        Hold {
            sha: sha.into(),
            branch: Some("develop".into()),
            tag: Some("1.0.3".into()),
            at: 1_700_000_000,
            note: None,
        }
    }

    // The pin is the feature: a hold covers the commit it was placed at and
    // nothing else, so the next commit lifts it without anyone remembering.
    #[test]
    fn a_hold_covers_only_the_commit_it_was_placed_at() {
        let held = hold("a1b2c3d");
        assert!(held.covers(Some("a1b2c3d")));
        assert!(!held.covers(Some("9999999")));
        assert!(!held.covers(None));
    }

    #[test]
    fn holds_round_trip_through_the_file_format() {
        let mut holds = Holds::default();
        let root = PathBuf::from("/tmp/drydock-test/repo");
        holds.set(&root, hold("a1b2c3d"));
        holds.set(
            &PathBuf::from("/tmp/drydock-test/other"),
            Hold {
                note: Some("changelog only".into()),
                ..hold("beefbee")
            },
        );

        let file = HoldsFile {
            holds: holds
                .iter()
                .map(|(path, h)| Entry {
                    path: path.display().to_string(),
                    sha: h.sha.clone(),
                    branch: h.branch.clone(),
                    tag: h.tag.clone(),
                    at: h.at,
                    note: h.note.clone(),
                })
                .collect(),
        };
        let body = toml::to_string_pretty(&file).unwrap();
        let back = parse(toml::from_str::<HoldsFile>(&body).unwrap());

        assert_eq!(back.iter().count(), 2);
        assert_eq!(back.get(&root).map(|h| h.sha.as_str()), Some("a1b2c3d"));
        assert_eq!(
            back.get(Path::new("/tmp/drydock-test/other"))
                .and_then(|h| h.note.as_deref()),
            Some("changelog only")
        );
    }

    // A repo nothing has held is the overwhelmingly common case, and it has to
    // stay a plain map miss rather than turning into a filesystem call each.
    #[test]
    fn an_empty_set_holds_nothing() {
        let holds = Holds::default();
        assert!(holds.is_empty());
        assert!(holds.get(Path::new("/tmp/whatever")).is_none());
    }

    #[test]
    fn removing_a_hold_by_the_path_it_was_set_with_works() {
        let mut holds = Holds::default();
        let root = PathBuf::from("/tmp/drydock-test/repo");
        holds.set(&root, hold("a1b2c3d"));
        assert!(holds.remove(&root).is_some());
        assert!(holds.is_empty());
    }
}
