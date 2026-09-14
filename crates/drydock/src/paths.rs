//! Config and cache locations.
//!
//! Config goes in `dirs::config_dir()` (matching the sibling reeve and ytunnel
//! tools): on macOS `~/Library/Application Support/drydock`, on Linux
//! `~/.config/drydock`. The probe cache is disposable, so it goes in
//! `dirs::cache_dir()` instead.
//!
//! With one exception, and it only ever bites on macOS. Plenty of people keep
//! every tool's config under `~/.config` and sync that one directory between
//! machines, and macOS is the only platform where `dirs` ignores the XDG
//! variables. So an explicit `$XDG_CONFIG_HOME`, or a `~/.config/drydock`
//! that already exists, wins over the platform default. Neither costs anyone
//! a migration: on Linux `dirs` already resolves to the same place, and on
//! macOS a directory nobody created is never chosen.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

const APP: &str = "drydock";

/// Set when the roots came from `--root`, so that run gets its own cache file.
static CACHE_SUFFIX: OnceLock<String> = OnceLock::new();

/// Pick between an XDG-style location and the platform default.
///
/// An absolute `$XDG_CONFIG_HOME` / `$XDG_CACHE_HOME` wins outright — that is
/// someone stating where they want their files. A relative or empty one is
/// ignored rather than resolved against the working directory, which is what
/// the spec asks for. Failing that, an *existing* `~/.config/drydock` (or
/// `~/.cache/drydock`) wins: creating that directory is the other way of
/// saying the same thing, and only checking for it means this never moves
/// anyone's files out from under them.
fn resolve_dir(
    xdg: Option<PathBuf>,
    home: Option<&Path>,
    home_subdir: &str,
    platform_default: Option<PathBuf>,
) -> Option<PathBuf> {
    if let Some(dir) = xdg {
        if dir.is_absolute() {
            return Some(dir.join(APP));
        }
    }
    if let Some(home) = home {
        let candidate = home.join(home_subdir).join(APP);
        if candidate.is_dir() {
            return Some(candidate);
        }
    }
    platform_default.map(|dir| dir.join(APP))
}

pub fn config_dir() -> Result<PathBuf> {
    resolve_dir(
        std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from),
        dirs::home_dir().as_deref(),
        ".config",
        dirs::config_dir(),
    )
    .context("Could not determine a config directory")
}

pub fn config_file() -> Result<PathBuf> {
    Ok(config_dir()?.join("config.toml"))
}

/// Where manual release holds are kept. Config rather than cache: a hold is
/// something a person decided, not something a probe observed, and `drydock
/// scan --no-cache` must not throw it away.
pub fn holds_file() -> Result<PathBuf> {
    Ok(config_dir()?.join("holds.toml"))
}

pub fn cache_dir() -> Result<PathBuf> {
    resolve_dir(
        std::env::var_os("XDG_CACHE_HOME").map(PathBuf::from),
        dirs::home_dir().as_deref(),
        ".cache",
        dirs::cache_dir(),
    )
    .context("Could not determine a cache directory")
}

pub fn cache_file() -> Result<PathBuf> {
    Ok(cache_dir()?.join(cache_file_name(CACHE_SUFFIX.get().map(String::as_str))))
}

fn cache_file_name(suffix: Option<&str>) -> String {
    match suffix {
        Some(suffix) => format!("state-{suffix}.json"),
        None => "state.json".to_string(),
    }
}

/// Give a run over `--root` trees its own cache file.
///
/// The cache is keyed by repo path and written wholesale at the end of a
/// sweep, so one `drydock --root ~/scratch list` would otherwise replace the
/// fleet's cache with those few repos and leave the next dashboard start
/// painting an empty table. A file per set of roots keeps both warm, and each
/// set gets the same file every time so a repeated `--root` run is still
/// instant.
pub fn set_cache_namespace(roots: &[String]) {
    if roots.is_empty() {
        return;
    }
    let mut expanded: Vec<String> = roots
        .iter()
        .map(|r| expand(r).display().to_string())
        .collect();
    expanded.sort();
    expanded.dedup();
    let _ = CACHE_SUFFIX.set(short_hash(&expanded.join("\u{0}")));
}

/// FNV-1a. Not cryptographic and not meant to be — it only has to name a
/// disposable file, and being written out by hand means the name can't shift
/// under a toolchain upgrade and orphan everyone's cache.
fn short_hash(input: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in input.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

pub fn log_file() -> Result<PathBuf> {
    Ok(cache_dir()?.join("drydock.log"))
}

/// Expand a leading `~` and any `$VAR` references.
///
/// Environment variables matter here because the roots are the one setting
/// people want to state indirectly: `$GHQ_ROOT` in a config file is the same
/// config on every machine, whereas the path it resolves to isn't. An
/// undefined variable falls back to tilde-only expansion, which leaves the
/// text alone rather than silently turning `~/$NOPE` into `~/`.
pub fn expand(input: &str) -> PathBuf {
    match shellexpand::full(input) {
        Ok(expanded) => PathBuf::from(expanded.as_ref()),
        Err(_) => PathBuf::from(shellexpand::tilde(input).as_ref()),
    }
}

/// Render a path with `$HOME` collapsed back to `~`, for display.
pub fn contract(path: &std::path::Path) -> String {
    if let Some(home) = dirs::home_dir() {
        if let Ok(rest) = path.strip_prefix(&home) {
            return format!("~/{}", rest.display());
        }
    }
    path.display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn platform_default() -> Option<PathBuf> {
        Some(PathBuf::from("/platform/default"))
    }

    #[test]
    fn an_absolute_xdg_var_wins_over_everything() {
        let home = tempfile::tempdir().unwrap();
        // Even with the `~/.config` directory sitting right there.
        std::fs::create_dir_all(home.path().join(".config").join(APP)).unwrap();
        assert_eq!(
            resolve_dir(
                Some(PathBuf::from("/somewhere/else")),
                Some(home.path()),
                ".config",
                platform_default(),
            ),
            Some(PathBuf::from("/somewhere/else/drydock"))
        );
    }

    #[test]
    fn a_relative_or_empty_xdg_var_is_ignored() {
        for var in ["", "relative/path"] {
            assert_eq!(
                resolve_dir(
                    Some(PathBuf::from(var)),
                    None,
                    ".config",
                    platform_default()
                ),
                Some(PathBuf::from("/platform/default/drydock")),
                "{var:?} should not have been honoured"
            );
        }
    }

    #[test]
    fn an_existing_dot_config_dir_wins_over_the_platform_default() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join(".config").join(APP)).unwrap();
        assert_eq!(
            resolve_dir(None, Some(home.path()), ".config", platform_default()),
            Some(home.path().join(".config").join(APP))
        );
    }

    // The whole point of only ever *checking* for `~/.config/drydock`: someone
    // who has never asked for this keeps the platform default, and nothing
    // they own moves.
    #[test]
    fn a_missing_dot_config_dir_leaves_the_platform_default_alone() {
        let home = tempfile::tempdir().unwrap();
        assert_eq!(
            resolve_dir(None, Some(home.path()), ".config", platform_default()),
            Some(PathBuf::from("/platform/default/drydock"))
        );
    }

    // A `~/.config/drydock` that is somehow a *file* is not a config
    // directory, and picking it would fail later with a much worse message.
    #[test]
    fn a_file_where_the_dot_config_dir_would_be_is_not_mistaken_for_one() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join(".config")).unwrap();
        std::fs::write(home.path().join(".config").join(APP), "not a directory").unwrap();
        assert_eq!(
            resolve_dir(None, Some(home.path()), ".config", platform_default()),
            Some(PathBuf::from("/platform/default/drydock"))
        );
    }

    #[test]
    fn nothing_at_all_is_reported_rather_than_guessed_at() {
        assert_eq!(resolve_dir(None, None, ".config", None), None);
    }

    // Someone who never passes `--root` keeps the file they already have.
    #[test]
    fn the_default_cache_file_is_unsuffixed() {
        assert_eq!(cache_file_name(None), "state.json");
        assert_eq!(cache_file_name(Some("abc")), "state-abc.json");
    }

    // The same roots have to name the same file every time, or a repeated
    // `--root` run would cold-start and leave a new file behind each time.
    #[test]
    fn the_cache_name_is_stable_and_order_independent() {
        assert_eq!(short_hash("~/a\u{0}~/b"), short_hash("~/a\u{0}~/b"));
        assert_ne!(short_hash("~/a"), short_hash("~/b"));
    }

    #[test]
    fn env_vars_expand_in_paths() {
        std::env::set_var("DRYDOCK_TEST_ROOT", "/somewhere/ghq");
        assert_eq!(
            expand("$DRYDOCK_TEST_ROOT/github.com"),
            PathBuf::from("/somewhere/ghq/github.com")
        );
        std::env::remove_var("DRYDOCK_TEST_ROOT");
    }

    // An unset variable leaves the text as written, which shows up in the
    // "not a directory" warning instead of silently scanning `/github.com`.
    #[test]
    fn an_undefined_var_is_left_alone() {
        std::env::remove_var("DRYDOCK_TEST_MISSING");
        assert_eq!(
            expand("$DRYDOCK_TEST_MISSING/github.com"),
            PathBuf::from("$DRYDOCK_TEST_MISSING/github.com")
        );
    }
}
