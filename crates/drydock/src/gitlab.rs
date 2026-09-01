//! Listing a GitLab owner's repositories via the `glab` CLI.
//!
//! The same shape as [`crate::gh`]: no HTTP client, no token handling of our
//! own, just riding on whatever `glab auth login` already set up. What GitLab
//! adds is one decision GitHub doesn't have: a namespace can be a user or a
//! group, and the two are listed through different flags. So this asks the
//! API which kind the owner is first, then pages through the matching list
//! shape — guessing would mean an empty result or an API error on half of
//! all owners.
//!
//! `GITLAB_HOST` is exported into every child so self-hosted instances work
//! through the same login `glab auth login --hostname` set up; for
//! gitlab.com the variable simply restates the default.

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use std::time::Duration;
use tokio::process::Command;

use crate::provider::OrgRepo;

/// Page size for `glab repo list`. 100 is the API's maximum, so paging until
/// a short page arrives is the fewest round trips the CLI allows.
const PAGE_SIZE: u32 = 100;

/// One `glab` invocation against `host`, with the safety rails applied.
/// Stdin is closed so a credential prompt can never hang waiting for input, a
/// timed-out process is reaped rather than left running, and colour/locale
/// are pinned so output parses identically everywhere.
async fn run_glab(args: &[&str], host: &str, timeout: Duration) -> Result<String> {
    let mut cmd = Command::new("glab");
    cmd.args(args)
        .env("GITLAB_HOST", host)
        .env("NO_COLOR", "1")
        .env("LC_ALL", "C")
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);

    let output = tokio::time::timeout(timeout, cmd.output())
        .await
        .map_err(|_| {
            anyhow!(
                "glab {} timed out after {}s",
                args.join(" "),
                timeout.as_secs()
            )
        })?
        .with_context(|| format!("running glab {}", args.join(" ")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(anyhow!(
            "glab {} failed: {}",
            args.join(" "),
            if stderr.is_empty() {
                "no output".to_string()
            } else {
                stderr.lines().next().unwrap_or("").to_string()
            }
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// List every project `owner` has on `host`. `include_subgroups` only means
/// anything for a group owner; a user's namespace has no subgroups, and the
/// flag is simply not offered there.
pub async fn list_owner(
    owner: &str,
    host: &str,
    include_subgroups: bool,
    timeout: Duration,
) -> Result<Vec<OrgRepo>> {
    let body = run_glab(&["api", &format!("namespaces/{owner}")], host, timeout).await?;
    let kind = parse_glab_namespace(&body)?;

    let mut repos = Vec::new();
    let mut page = 1u32;
    loop {
        let page_arg = page.to_string();
        let per_page_arg = PAGE_SIZE.to_string();
        let mut args: Vec<&str> = vec!["repo", "list"];
        if kind == "group" {
            args.push("--group");
            args.push(owner);
            if include_subgroups {
                args.push("--include-subgroups");
            }
        } else {
            args.push("--user");
            args.push(owner);
        }
        args.extend([
            "--output",
            "json",
            "--per-page",
            &per_page_arg,
            "--page",
            &page_arg,
        ]);

        let body = run_glab(&args, host, timeout).await?;
        let page_repos = parse_glab(&body)?;
        let last_page = page_repos.len() < PAGE_SIZE as usize;
        repos.extend(page_repos);
        if last_page {
            return Ok(repos);
        }
        page += 1;
    }
}

#[derive(Deserialize)]
struct GlabNamespace {
    #[serde(default)]
    kind: Option<String>,
}

/// Read the `kind` off a namespace lookup. Both listing shapes below depend
/// on it, so an answer without one is an error rather than a guess — the API
/// always sends it, and its absence means something else went wrong.
fn parse_glab_namespace(body: &str) -> Result<String> {
    let ns: GlabNamespace = serde_json::from_str(body)
        .with_context(|| format!("parsing glab namespace output: {body:?}"))?;
    ns.kind
        .ok_or_else(|| anyhow!("namespace response carries no \"kind\""))
}

#[derive(Deserialize)]
struct GlabProject {
    name: String,
    #[serde(default)]
    ssh_url_to_repo: String,
    #[serde(default)]
    http_url_to_repo: String,
    #[serde(default)]
    archived: bool,
    // Presence is the whole signal: a project that has never been forked
    // omits the field, and what it was forked *from* is of no interest here.
    forked_from_project: Option<serde_json::Value>,
}

/// Map `glab repo list --output json` output onto [`OrgRepo`]. Absent
/// optional fields default rather than failing the whole listing: a project
/// missing one URL still has a name, and sync can still do something with it.
fn parse_glab(body: &str) -> Result<Vec<OrgRepo>> {
    let projects: Vec<GlabProject> = serde_json::from_str(body)
        .with_context(|| format!("parsing glab repo list output: {body:?}"))?;
    Ok(projects
        .into_iter()
        .map(|p| OrgRepo {
            name: p.name,
            ssh_url: p.ssh_url_to_repo,
            https_url: p.http_url_to_repo,
            archived: p.archived,
            fork: p.forked_from_project.is_some(),
        })
        .collect())
}

/// Ask `glab` which hosts it is logged in to, as (host, username) pairs.
/// Sync, run once when the add-flow opens — the same reasoning as `gh`'s
/// probe: "not logged in" is an empty answer, not a failure. A degraded
/// parse (exit zero, nothing recognisable) yields one `gitlab.com` entry
/// with no user: a parse-shape change upstream must not blank the provider,
/// because the default-host listing still works.
pub fn auth_status(timeout: Duration) -> Vec<(String, String)> {
    let Some(out) = crate::provider::run_probe("glab", &["auth", "status"], &[], timeout) else {
        return Vec::new();
    };
    let mut pairs = parse_auth_status(&out.stdout);
    if pairs.is_empty() {
        pairs = parse_auth_status(&out.stderr);
    }
    if pairs.is_empty() {
        vec![("gitlab.com".to_string(), String::new())]
    } else {
        pairs
    }
}

/// Pull (host, username) pairs out of `glab auth status` output. The output
/// is host-named blocks — the host on its own line (sometimes with a
/// trailing colon), then `✓ Logged in as <user>` under it — so the parser
/// tracks the current block header and anchors on the words, not the
/// markers, which vary with colour and version. A user line under no
/// header falls back to `gitlab.com`, glab's own default when it has
/// nothing else to name.
fn parse_auth_status(out: &str) -> Vec<(String, String)> {
    let mut pairs: Vec<(String, String)> = Vec::new();
    let mut host = "gitlab.com".to_string();
    for line in out.lines() {
        let trimmed = line.trim();
        if let Some(i) = trimmed.find("Logged in as ") {
            let user = trimmed[i + "Logged in as ".len()..].trim();
            if !user.is_empty() && !pairs.iter().any(|(h, _)| *h == host) {
                pairs.push((host.clone(), user.to_string()));
            }
        } else if !line.starts_with(char::is_whitespace) {
            // A block header candidate: a bare hostname, optionally with a
            // trailing colon. Anything else (a version banner, say) has
            // spaces and can't be a host.
            let name = trimmed.trim_end_matches(':');
            if name.contains('.') && !name.contains(char::is_whitespace) {
                host = name.to_string();
            }
        }
    }
    pairs
}

/// List the owners the authenticated account can register on `host`: the
/// user's own namespace first, then the groups, deduped. Namespaces is the
/// one endpoint that answers both in a single shape — asking `user/orgs`
/// and `groups` separately would need two paging loops for no extra
/// information.
///
/// Paged manually through the query string, like every other GitLab
/// listing here: the API's default page of 20 would silently truncate a
/// group-heavy account's choices.
pub async fn list_owners(host: &str, timeout: Duration) -> Result<Vec<String>> {
    let mut owners: Vec<String> = Vec::new();
    let mut page = 1u32;
    loop {
        let page_arg = page.to_string();
        let body = run_glab(
            &["api", &format!("namespaces?per_page={PAGE_SIZE}&page={page_arg}")],
            host,
            timeout,
        )
        .await?;
        let page_owners = parse_owners(&body)?;
        let last_page = page_owners.len() < PAGE_SIZE as usize;
        // User-before-group ordering happens inside parse_owners per page;
        // across pages the personal namespace always lands on page one, so
        // the top of the list stays personal-first. A page that adds
        // nothing new ends the loop even if the host ignored the page
        // parameter — a paging loop that can't terminate would hang the
        // form.
        let before = owners.len();
        for name in page_owners {
            if !owners.contains(&name) {
                owners.push(name);
            }
        }
        if last_page || owners.len() == before {
            return Ok(owners);
        }
        page += 1;
    }
}

#[derive(Deserialize)]
struct GlabOwnerNamespace {
    path: String,
    #[serde(default)]
    kind: Option<String>,
}

/// Turn `glab api namespaces` output into owner names: the user's own
/// namespace (`kind: "user"`) first, then the groups, deduped, because the
/// form offers them top-down and the personal namespace is the most common
/// choice. A namespace without a `kind` is treated as a group rather than
/// failing the whole listing.
fn parse_owners(body: &str) -> Result<Vec<String>> {
    let namespaces: Vec<GlabOwnerNamespace> = serde_json::from_str(body)
        .with_context(|| format!("parsing glab namespaces output: {body:?}"))?;
    let mut owners: Vec<String> = Vec::new();
    for user_only in [true, false] {
        for ns in &namespaces {
            let is_user = ns.kind.as_deref() == Some("user");
            if is_user == user_only && !owners.contains(&ns.path) {
                owners.push(ns.path.clone());
            }
        }
    }
    Ok(owners)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Covers both flags the planner filters on later, so parse_gh's GitLab
    // twin is proven to carry them through rather than drop them.
    #[test]
    fn parses_glab_project_list_output() {
        let body = r#"[
            {"name":"fleet","ssh_url_to_repo":"git@gitlab.com:acme/fleet.git","http_url_to_repo":"https://gitlab.com/acme/fleet.git"},
            {"name":"old-site","ssh_url_to_repo":"git@gitlab.com:acme/old-site.git","http_url_to_repo":"https://gitlab.com/acme/old-site.git","archived":true},
            {"name":"mirror","ssh_url_to_repo":"git@gitlab.com:acme/mirror.git","http_url_to_repo":"https://gitlab.com/acme/mirror.git","forked_from_project":{"id":42,"name":"upstream"}}
        ]"#;
        let repos = parse_glab(body).expect("fixture should parse");
        assert_eq!(repos.len(), 3);
        assert_eq!(repos[0].name, "fleet");
        assert_eq!(repos[0].ssh_url, "git@gitlab.com:acme/fleet.git");
        assert_eq!(repos[0].https_url, "https://gitlab.com/acme/fleet.git");
        assert!(!repos[0].archived);
        assert!(!repos[0].fork);
        assert!(repos[1].archived);
        assert!(!repos[1].fork);
        assert!(repos[2].fork);
    }

    #[test]
    fn parses_repos_missing_optional_fields() {
        let repos = parse_glab(r#"[{"name":"bare"}]"#).expect("fixture should parse");
        assert_eq!(repos.len(), 1);
        assert_eq!(repos[0].name, "bare");
        assert_eq!(repos[0].ssh_url, "");
        assert!(!repos[0].archived);
        assert!(!repos[0].fork);
    }

    #[test]
    fn rejects_non_json_glab_output() {
        assert!(parse_glab("not json at all").is_err());
    }

    #[test]
    fn rejects_empty_glab_output() {
        assert!(parse_glab("").is_err());
    }

    #[test]
    fn reads_namespace_kinds() {
        assert_eq!(
            parse_glab_namespace(r#"{"id":1,"kind":"group","path":"acme"}"#).unwrap(),
            "group"
        );
        assert_eq!(
            parse_glab_namespace(r#"{"id":2,"kind":"user","path":"otter"}"#).unwrap(),
            "user"
        );
    }

    #[test]
    fn rejects_namespace_without_kind() {
        assert!(parse_glab_namespace(r#"{"id":3}"#).is_err());
    }

    #[test]
    fn rejects_non_json_namespace_output() {
        assert!(parse_glab_namespace("").is_err());
    }

    #[test]
    fn parses_glab_auth_status_blocks() {
        let out = "\
gitlab.com:
  ✓ Logged in as crueber
  ✓ GitLab CLI 1.40.0

git.mycompany.com
  ✓ Logged in as ops";
        assert_eq!(
            parse_auth_status(out),
            vec![
                ("gitlab.com".to_string(), "crueber".to_string()),
                ("git.mycompany.com".to_string(), "ops".to_string())
            ]
        );
    }

    // A user line under no host header is glab's degraded shape; the
    // default host is the only name that doesn't invent one.
    #[test]
    fn falls_back_to_the_default_host_without_a_header() {
        assert_eq!(
            parse_auth_status("  ✓ Logged in as crueber"),
            vec![("gitlab.com".to_string(), "crueber".to_string())]
        );
    }

    #[test]
    fn parses_nothing_from_non_auth_status_text() {
        assert!(parse_auth_status("").is_empty());
        assert!(parse_auth_status("error: not authenticated").is_empty());
    }

    #[test]
    fn parses_namespaces_users_first_then_groups() {
        let body = r#"[
            {"id":3,"path":"fleet","kind":"group"},
            {"id":1,"path":"crueber","kind":"user"},
            {"id":2,"path":"acme","kind":"group"},
            {"id":4,"path":"fleet","kind":"group"}
        ]"#;
        assert_eq!(
            parse_owners(body).expect("fixture should parse"),
            vec!["crueber", "fleet", "acme"]
        );
    }

    // A namespace without a kind still names an owner — dropping it would
    // hide a real option from the form for no good reason.
    #[test]
    fn keeps_namespaces_without_a_kind() {
        let body = r#"[{"id":1,"path":"crueber","kind":"user"},{"id":2,"path":"mystery"}]"#;
        assert_eq!(
            parse_owners(body).expect("fixture should parse"),
            vec!["crueber", "mystery"]
        );
    }

    #[test]
    fn rejects_non_json_namespaces_output() {
        assert!(parse_owners("not json at all").is_err());
    }

    #[test]
    fn rejects_empty_namespaces_output() {
        assert!(parse_owners("").is_err());
    }
}
