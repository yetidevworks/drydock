//! Repository visibility via the `gh` CLI.
//!
//! This is the one probe in the whole tool that isn't `git`. Visibility is a
//! property the host tracks, not something recorded anywhere under `.git`, so
//! there is no way to answer "is this public or private" without asking
//! GitHub. This shells out to `gh` the same way [`crate::git::fetch`] shells
//! out to `git`: no HTTP client, no token handling of our own, just riding on
//! whatever `gh auth login` already set up. Only github.com remotes are
//! recognised for now; anything else is skipped rather than guessed at.

use anyhow::{anyhow, Context, Result};
use std::time::Duration;
use tokio::process::Command;

use crate::model::Visibility;

/// Pull `owner/repo` out of a remote URL, if it points at github.com.
/// `None` for anything else, including other hosts and GitHub Enterprise
/// instances on their own domain — supporting those would mean threading a
/// `--hostname` through, which isn't worth it until someone needs it.
///
/// A wiki's clone URL is `<repo>.wiki.git`, which isn't a repository the API
/// can look up on its own — a wiki's visibility is just its parent repo's.
/// So the `.wiki` suffix is stripped here, meaning every caller gets the
/// queryable slug for free rather than needing to know about this case.
pub fn github_slug(remote_url: &str) -> Option<String> {
    let rest = remote_url
        .strip_prefix("git@github.com:")
        .or_else(|| remote_url.strip_prefix("ssh://git@github.com/"))
        .or_else(|| remote_url.strip_prefix("https://github.com/"))
        .or_else(|| remote_url.strip_prefix("http://github.com/"))?;
    let rest = rest.strip_suffix(".git").unwrap_or(rest);
    let (owner, repo) = rest.split_once('/')?;
    let repo = repo.strip_suffix(".wiki").unwrap_or(repo);
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some(format!("{owner}/{repo}"))
}

/// Ask `gh` for one repo's visibility. Errors — `gh` missing, not
/// authenticated, repo not found, timeout — are the caller's to decide how to
/// treat; this never guesses at a value.
pub async fn visibility(slug: &str, timeout: Duration) -> Result<Visibility> {
    let mut cmd = Command::new("gh");
    cmd.args([
        "repo",
        "view",
        slug,
        "--json",
        "visibility",
        "--jq",
        ".visibility",
    ])
    .env("GH_PROMPT_DISABLED", "1")
    .env("NO_COLOR", "1")
    .env("LC_ALL", "C")
    .stdin(std::process::Stdio::null())
    .kill_on_drop(true);

    let output = tokio::time::timeout(timeout, cmd.output())
        .await
        .map_err(|_| anyhow!("gh repo view timed out after {}s", timeout.as_secs()))?
        .context("running gh repo view")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(anyhow!(
            "gh repo view failed: {}",
            if stderr.is_empty() {
                "no output".to_string()
            } else {
                stderr.lines().next().unwrap_or("").to_string()
            }
        ));
    }

    let raw = String::from_utf8_lossy(&output.stdout).trim().to_string();
    raw.parse::<Visibility>()
        .map_err(|_| anyhow!("unrecognised visibility {raw:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_common_github_remote_shapes() {
        assert_eq!(
            github_slug("git@github.com:owner/repo.git").as_deref(),
            Some("owner/repo")
        );
        assert_eq!(
            github_slug("ssh://git@github.com/owner/repo.git").as_deref(),
            Some("owner/repo")
        );
        assert_eq!(
            github_slug("https://github.com/owner/repo.git").as_deref(),
            Some("owner/repo")
        );
        assert_eq!(
            github_slug("https://github.com/owner/repo").as_deref(),
            Some("owner/repo")
        );
    }

    #[test]
    fn rejects_non_github_and_malformed_remotes() {
        assert_eq!(github_slug("git@gitlab.com:owner/repo.git"), None);
        assert_eq!(github_slug("git@github.com:owner-only.git"), None);
        assert_eq!(github_slug("not a url"), None);
    }

    #[test]
    fn wiki_remotes_resolve_to_their_parent_repo() {
        assert_eq!(
            github_slug("git@github.com:owner/repo.wiki.git").as_deref(),
            Some("owner/repo")
        );
    }
}
