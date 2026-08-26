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

/// Hosts whose repos `gh` can answer for.
///
/// `ssh.github.com` is the port-443 endpoint GitHub documents for networks
/// that block 22 — a different name for exactly the same repositories, and a
/// remote people on locked-down corporate networks really do have. The slug
/// is what `gh` looks up, so which of the two names got it here doesn't
/// matter once it's parsed.
fn is_github_host(host: &str) -> bool {
    let host = host.to_ascii_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host);
    host == "github.com" || host == "ssh.github.com"
}

/// Pull `owner/repo` out of a remote URL, if it points at github.com.
/// `None` for anything else, including other hosts and GitHub Enterprise
/// instances on their own domain — supporting those would mean threading a
/// `--hostname` through, which isn't worth it until someone needs it.
///
/// Parsed rather than prefix-matched, because the shapes people actually have
/// configured are more varied than a list of prefixes catches: an explicit
/// port (`ssh://git@github.com:22/…`), the port-443 host, `git://`, a
/// `user@` in an https URL, a trailing slash. Every one of those is a normal
/// remote, and each would have been reported as `unsupported` — honest, but
/// wrong.
///
/// A wiki's clone URL is `<repo>.wiki.git`, which isn't a repository the API
/// can look up on its own — a wiki's visibility is just its parent repo's.
/// So the `.wiki` suffix is stripped here, meaning every caller gets the
/// queryable slug for free rather than needing to know about this case.
pub fn github_slug(remote_url: &str) -> Option<String> {
    let url = remote_url.trim();

    let path = match url.split_once("://") {
        // A real URL: everything after the scheme is [user@]host[:port]/path.
        Some((scheme, after)) => {
            if !matches!(
                scheme.to_ascii_lowercase().as_str(),
                "ssh" | "https" | "http" | "git"
            ) {
                return None;
            }
            let (authority, path) = after.split_once('/')?;
            // Drop any `user@` and any `:port` before checking the host.
            let host = authority.rsplit('@').next()?.split(':').next()?;
            if !is_github_host(host) {
                return None;
            }
            path
        }
        // scp-style `[user@]host:owner/repo`, which has no scheme and can't
        // carry a port — that ambiguity is the reason `ssh://` URLs exist.
        None => {
            let (authority, path) = url.split_once(':')?;
            if !is_github_host(authority.rsplit('@').next()?) {
                return None;
            }
            path
        }
    };

    let path = path.trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let (owner, repo) = path.split_once('/')?;
    let repo = repo.strip_suffix(".wiki").unwrap_or(repo);
    // Anything deeper than `owner/repo` is a web URL, not a clone URL, and
    // guessing which two segments were meant is worse than saying nothing.
    if owner.is_empty() || repo.is_empty() || repo.contains('/') {
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

    // Every one of these is a normal remote somebody has configured, and every
    // one was reported as `unsupported` by the old prefix matching.
    #[test]
    fn parses_the_awkward_but_real_remote_shapes() {
        for url in [
            // The port-443 endpoint, for networks that block 22.
            "ssh://git@ssh.github.com:443/owner/repo.git",
            // An explicit port on the normal host.
            "ssh://git@github.com:22/owner/repo.git",
            // The read-only git protocol.
            "git://github.com/owner/repo.git",
            // Credentials embedded in an https remote.
            "https://someone@github.com/owner/repo.git",
            // A trailing slash, which used to yield "owner/repo/".
            "https://github.com/owner/repo/",
            "https://github.com/owner/repo.git/",
            // No user at all on an ssh URL.
            "ssh://github.com/owner/repo.git",
            "  https://github.com/owner/repo.git  ",
        ] {
            assert_eq!(
                github_slug(url).as_deref(),
                Some("owner/repo"),
                "failed on {url}"
            );
        }
    }

    // A web URL is not a clone URL, and picking two of its segments to call
    // owner and repo would be a guess. Saying nothing is the honest answer.
    #[test]
    fn rejects_paths_deeper_than_owner_slash_repo() {
        assert_eq!(github_slug("https://github.com/owner/repo/tree/main"), None);
    }

    // A lookalike host is not GitHub, and asking `gh` about it would return
    // someone else's repo of the same name.
    #[test]
    fn rejects_hosts_that_merely_end_in_github_com() {
        for url in [
            "https://notgithub.com/owner/repo.git",
            "https://github.com.evil.example/owner/repo.git",
            "git@github.example.com:owner/repo.git",
        ] {
            assert_eq!(github_slug(url), None, "accepted {url}");
        }
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
