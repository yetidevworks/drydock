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
use crate::config::OrgProvider;
use crate::gitea;
use crate::gh;
use crate::gitlab;
use crate::model::Visibility;
use std::io::Read;
use std::time::{Duration, Instant};

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

/// The one shape every provider's listing maps into, so the org-sync engine
/// (org.rs, wave 2) never sees provider-specific JSON. Each CLI names these
/// fields differently (`sshUrl` vs `ssh_url_to_repo` vs `ssh_url`), and the
/// engine has no business caring; the per-provider modules do the translation
/// at the edge, right where the raw response is in hand.
#[derive(Clone, Debug)]
pub struct OrgRepo {
    pub name: String,
    pub ssh_url: String,
    pub https_url: String,
    pub archived: bool,
    pub fork: bool,
}

/// What the add-flow can offer, discovered from the tools themselves: one
/// probe per CLI (`gh`, `glab`, `tea`), each asked which hosts it is logged
/// in to. Probing *tool* auth rather than tokens is the gate by design:
/// drydock never reads, stores, or sends credentials, so what a tool can do
/// is exactly what drydock can do — an org on a host nothing is
/// authenticated to could be registered but never listed. A missing binary
/// or a not-logged-in tool therefore contributes no hosts rather than an
/// error; "no auth" is an honest answer the form enforces, not a failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthedHost {
    pub provider: OrgProvider,
    pub host: String,
    /// Account name on that host, when the tool reports it. Gitea's comes
    /// from the login's user; GitLab's degraded no-user entry is `None`.
    pub login: Option<String>,
    /// Gitea only: the `tea login` name, which is what [`crate::config::OrgConfig::login`]
    /// stores to select the instance later.
    pub name: Option<String>,
}

/// Probe every CLI once. Sync, deliberately: the add-form runs this once
/// when it opens and waits for the answer, and the async runners' error
/// handling has nothing useful to say about "not logged in" — that isn't a
/// failure, it's an empty contribution.
pub fn authenticated_hosts(timeout: Duration) -> Vec<AuthedHost> {
    let mut hosts = Vec::new();
    for (host, account) in gh::auth_status(timeout) {
        hosts.push(AuthedHost {
            provider: OrgProvider::GitHub,
            host,
            login: Some(account),
            name: None,
        });
    }
    for (host, account) in gitlab::auth_status(timeout) {
        hosts.push(AuthedHost {
            provider: OrgProvider::GitLab,
            host,
            login: (!account.is_empty()).then_some(account),
            name: None,
        });
    }
    for login in gitea::logins(timeout) {
        hosts.push(AuthedHost {
            provider: OrgProvider::Gitea,
            host: login.host,
            login: login.user,
            name: Some(login.name),
        });
    }
    hosts
}

/// The owners an authenticated account can register on `host`: its personal
/// namespace first, then the orgs/groups it belongs to, deduped. Dispatches
/// to the provider's module — GitHub needs only the timeout, GitLab names
/// its host (self-hosted instances ride `GITLAB_HOST`), and Gitea names the
/// `tea` login that selects the instance.
pub async fn list_owners(
    provider: OrgProvider,
    host: &str,
    login: &str,
    timeout: Duration,
) -> Result<Vec<String>> {
    match provider {
        OrgProvider::GitHub => gh::list_owners(timeout).await,
        OrgProvider::GitLab => gitlab::list_owners(host, timeout).await,
        OrgProvider::Gitea => gitea::list_owners(login, timeout).await,
    }
}

/// A probe's captured output. Both streams are kept because the CLIs don't
/// agree on which one they report on — `gh auth status` writes stdout, and
/// a stream change upstream must not blank a provider.
pub(crate) struct ProbeOutput {
    pub stdout: String,
    pub stderr: String,
}

/// Run a CLI tool synchronously, with the same rails as the async runners:
/// stdin closed so a credential prompt can never hang, colour/locale pinned
/// so output parses identically everywhere, and a hard deadline after which
/// the child is killed rather than left running. `Some` only when the tool
/// ran and exited zero; a missing binary, a non-zero exit (the CLIs' own way
/// of saying "not logged in"), or a timeout is `None`.
///
/// Output is drained *after* the child exits rather than concurrently, so a
/// tool writing more than the OS pipe buffer (~64 KiB) would stall into a
/// timeout. The auth probes print a handful of lines; if one ever grows
/// past that, this is the assumption to revisit.
pub(crate) fn run_probe(
    program: &str,
    args: &[&str],
    extra_env: &[(&str, &str)],
    timeout: Duration,
) -> Option<ProbeOutput> {
    let mut cmd = std::process::Command::new(program);
    cmd.args(args)
        .env("NO_COLOR", "1")
        .env("LC_ALL", "C")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    for (key, value) in extra_env {
        cmd.env(key, value);
    }

    let mut child = cmd.spawn().ok()?;
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(5)),
            Err(_) => return None,
        }
    };
    let mut out = ProbeOutput {
        stdout: String::new(),
        stderr: String::new(),
    };
    if let Some(mut pipe) = child.stdout.take() {
        let _ = pipe.read_to_string(&mut out.stdout);
    }
    if let Some(mut pipe) = child.stderr.take() {
        let _ = pipe.read_to_string(&mut out.stderr);
    }
    status.success().then_some(out)
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
