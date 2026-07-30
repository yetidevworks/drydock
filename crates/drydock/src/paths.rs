//! Config and cache locations.
//!
//! Config goes in `dirs::config_dir()` (matching the sibling reeve and ytunnel
//! tools): on macOS `~/Library/Application Support/drydock`, on Linux
//! `~/.config/drydock`. The probe cache is disposable, so it goes in
//! `dirs::cache_dir()` instead.

use anyhow::{Context, Result};
use std::path::PathBuf;

pub fn config_dir() -> Result<PathBuf> {
    let dir = dirs::config_dir()
        .context("Could not determine a config directory")?
        .join("drydock");
    Ok(dir)
}

pub fn config_file() -> Result<PathBuf> {
    Ok(config_dir()?.join("config.toml"))
}

pub fn cache_dir() -> Result<PathBuf> {
    let dir = dirs::cache_dir()
        .context("Could not determine a cache directory")?
        .join("drydock");
    Ok(dir)
}

pub fn cache_file() -> Result<PathBuf> {
    Ok(cache_dir()?.join("state.json"))
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
