//! Listing a Gitea/Forgejo owner's repositories via the `tea` CLI.
//!
//! The same shape as [`crate::gh`]: no HTTP client, no token handling of our
//! own, just riding on whatever `tea login add` already set up. The CLI also
//! keeps its own notion of which instance a login points at, which is why
//! there is no host parameter here — selecting an instance *is* selecting a
//! login, and an empty one means tea's default rather than "all instances".

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use std::time::Duration;
use tokio::process::Command;

use crate::provider::OrgRepo;

/// Page size for `tea repos list`. The API honours whatever it's given, so
/// paging until a short page arrives is the loop that ends reliably whether
/// the instance is Gitea or one of its forks.
const PAGE_SIZE: u32 = 100;

/// One `tea` invocation, with the safety rails applied. Stdin is closed so a
/// credential prompt can never hang waiting for input, a timed-out process is
/// reaped rather than left running, and colour/locale are pinned so output
/// parses identically everywhere.
async fn run_tea(args: &[&str], timeout: Duration) -> Result<String> {
    let mut cmd = Command::new("tea");
    cmd.args(args)
        .env("NO_COLOR", "1")
        .env("LC_ALL", "C")
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);

    let output = tokio::time::timeout(timeout, cmd.output())
        .await
        .map_err(|_| {
            anyhow!(
                "tea {} timed out after {}s",
                args.join(" "),
                timeout.as_secs()
            )
        })?
        .with_context(|| format!("running tea {}", args.join(" ")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(anyhow!(
            "tea {} failed: {}",
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

/// List every repo `owner` has on the instance `login` names. An empty
/// `login` is deliberately not an error: it means tea's own default login,
/// which is how the vast majority of single-instance users run tea — adding
/// `-l` with an empty value would override that default with nothing.
pub async fn list_owner(owner: &str, login: &str, timeout: Duration) -> Result<Vec<OrgRepo>> {
    let mut repos = Vec::new();
    let mut page = 1u32;
    loop {
        let mut args: Vec<String> = vec![
            "repos".into(),
            "list".into(),
            "--owner".into(),
            owner.into(),
            "--output".into(),
            "json".into(),
            "--limit".into(),
            PAGE_SIZE.to_string(),
            "--page".into(),
            page.to_string(),
        ];
        if !login.is_empty() {
            args.push("-l".into());
            args.push(login.into());
        }
        let args: Vec<&str> = args.iter().map(String::as_str).collect();

        let body = run_tea(&args, timeout).await?;
        let page_repos = parse_tea(&body)?;
        let last_page = page_repos.len() < PAGE_SIZE as usize;
        repos.extend(page_repos);
        if last_page {
            return Ok(repos);
        }
        page += 1;
    }
}

#[derive(Deserialize)]
struct TeaRepo {
    name: String,
    #[serde(default)]
    ssh_url: String,
    #[serde(default)]
    clone_url: String,
    #[serde(default)]
    archived: bool,
    #[serde(default)]
    fork: bool,
}

/// Map `tea repos list --output json` output onto [`OrgRepo`]. Absent
/// optional fields default rather than failing the whole listing: a repo
/// missing one URL still has a name, and sync can still do something with it.
fn parse_tea(body: &str) -> Result<Vec<OrgRepo>> {
    let repos: Vec<TeaRepo> = serde_json::from_str(body)
        .with_context(|| format!("parsing tea repos list output: {body:?}"))?;
    Ok(repos
        .into_iter()
        .map(|r| OrgRepo {
            name: r.name,
            ssh_url: r.ssh_url,
            https_url: r.clone_url,
            archived: r.archived,
            fork: r.fork,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tea_repo_list_output() {
        let body = r#"[
            {"name":"fleet","ssh_url":"git@git.example.com:otter/fleet.git","clone_url":"https://git.example.com/otter/fleet.git"},
            {"name":"old-site","ssh_url":"git@git.example.com:otter/old-site.git","clone_url":"https://git.example.com/otter/old-site.git","archived":true},
            {"name":"mirror","ssh_url":"git@git.example.com:otter/mirror.git","clone_url":"https://git.example.com/otter/mirror.git","fork":true}
        ]"#;
        let repos = parse_tea(body).expect("fixture should parse");
        assert_eq!(repos.len(), 3);
        assert_eq!(repos[0].name, "fleet");
        assert_eq!(repos[0].ssh_url, "git@git.example.com:otter/fleet.git");
        assert_eq!(
            repos[0].https_url,
            "https://git.example.com/otter/fleet.git"
        );
        assert!(!repos[0].archived);
        assert!(!repos[0].fork);
        assert!(repos[1].archived);
        assert!(repos[2].fork);
    }

    #[test]
    fn parses_repos_missing_optional_fields() {
        let repos = parse_tea(r#"[{"name":"bare"}]"#).expect("fixture should parse");
        assert_eq!(repos.len(), 1);
        assert_eq!(repos[0].name, "bare");
        assert_eq!(repos[0].ssh_url, "");
        assert!(!repos[0].archived);
    }

    #[test]
    fn rejects_non_json_tea_output() {
        assert!(parse_tea("not json at all").is_err());
    }

    #[test]
    fn rejects_empty_tea_output() {
        assert!(parse_tea("").is_err());
    }
}
