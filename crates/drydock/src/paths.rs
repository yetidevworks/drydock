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

const APP: &str = "drydock";

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
    Ok(cache_dir()?.join("state.json"))
}

pub fn log_file() -> Result<PathBuf> {
    Ok(cache_dir()?.join("drydock.log"))
}

/// Expand a leading `~` and make the path absolute.
pub fn expand(input: &str) -> PathBuf {
    let expanded = shellexpand::tilde(input);
    PathBuf::from(expanded.as_ref())
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
}
