//! Hosting-provider detection and dispatch.
//!
//! Visibility is a property the *host* tracks, not `git`, so checking it
//! means first figuring out which host a remote points at and then asking
//! that host's own tool. Right now the only host this recognises is GitHub,
//! via `gh` (see [`crate::gh`]). A remote on any other host — GitLab,
//! Bitbucket, self-hosted, anything — makes [`detect`] return `None`, which
//! the caller reports as `VisibilityStatus::Unsupported` rather than
//! guessing at a value nothing actually confirmed.
//!
//! Adding a second provider is meant to be a small, local change: GitLab via
//! `glab` is the natural next one, since it mirrors `gh`'s shape closely. It
//! would need a new module alongside `gh.rs` with its own slug parser and
//! its own shell-out function, a new [`Provider`] variant, and one more arm
//! each in [`detect`] and [`check`]. Nothing in the probing pipeline
//! (`probe::fill_visibility`) needs to change — it already only knows about
//! `Provider` and [`crate::model::Visibility`], not `gh` specifically.
//! Bitbucket is a harder case: unlike GitHub and GitLab, there's no
//! equivalent first-party CLI riding on credentials the user already set up
//! elsewhere, so supporting it would mean this tool handling API tokens on
//! its own — a different shape of feature, not just another arm here.

use anyhow::Result;
use std::time::Duration;

use crate::gh;
use crate::model::Visibility;

/// A hosting provider this tool knows how to ask about visibility.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Provider {
    GitHub,
}

/// Figure out which provider a remote URL belongs to, and the slug to ask
/// that provider's tool about. `None` means no known provider recognises the
/// URL — a different host, or something unparseable.
pub fn detect(remote_url: &str) -> Option<(Provider, String)> {
    gh::github_slug(remote_url).map(|slug| (Provider::GitHub, slug))
}

/// Ask the given provider for one repo's visibility.
pub async fn check(provider: Provider, slug: &str, timeout: Duration) -> Result<Visibility> {
    match provider {
        Provider::GitHub => gh::visibility(slug, timeout).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_github_remotes() {
        assert_eq!(
            detect("git@github.com:owner/repo.git"),
            Some((Provider::GitHub, "owner/repo".into()))
        );
    }

    #[test]
    fn other_hosts_are_unrecognised() {
        assert_eq!(detect("git@gitlab.com:owner/repo.git"), None);
        assert_eq!(detect("git@bitbucket.org:owner/repo.git"), None);
        assert_eq!(detect("https://git.example.com/owner/repo.git"), None);
    }
}
