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
        // `tea repos list --output json` looks tempting, but tea 0.15.1
        // answers with its own compact schema (`owner`/`name`/`type`/`ssh`)
        // instead of the API's repo objects — no clone URL, no archived or
        // fork flags — which silently gutted sync. The API search endpoint
        // answers with the real repo objects and handles users and orgs
        // alike through the `owner` parameter.
        let query = format!(
            "/repos/search?owner={owner}&limit={}&page={page}",
            PAGE_SIZE
        );
        let body = run_tea(&with_login(&["api", &query], login), timeout).await?;
        let page_repos = parse_tea(&body)?;

        // A page that adds nothing new ends the loop, so an instance that
        // ignores the page parameter cannot hang the sync in a spin.
        // The search endpoint's `owner` parameter is advisory on some
        // instances — it also answers with repos from orgs the account can
        // reach — so the exact namespace match happens here.
        let page_repos: Vec<OrgRepo> = page_repos
            .into_iter()
            .filter(|r| {
                r.owner_login
                    .as_deref()
                    .map_or(true, |o| o.eq_ignore_ascii_case(owner))
            })
            .collect();
        let fresh: Vec<OrgRepo> = page_repos
            .into_iter()
            .filter(|r: &OrgRepo| repos.iter().all(|k: &OrgRepo| k.name != r.name))
            .collect();
        let last_page = fresh.len() < PAGE_SIZE as usize;
        repos.extend(fresh);
        if last_page {
            return Ok(repos);
        }
        page += 1;
    }
}
#[derive(Deserialize)]
struct TeaRepo {
    name: String,
    /// The compact schema carries a plain string; the API carries an
    /// object with a `login`. Either way it pins the sync to exactly the
    /// registered owner — the search endpoint's `owner` parameter is not a
    /// strict filter on every instance.
    #[serde(default)]
    owner: Option<TeaOwner>,
    /// The API spells it `ssh_url`; tea's compact `repos list --output json`
    /// spells it `ssh`. One struct eats both shapes.
    #[serde(default, alias = "ssh")]
    ssh_url: String,
    #[serde(default, alias = "https_url")]
    clone_url: String,
    #[serde(default)]
    archived: bool,
    #[serde(default)]
    fork: bool,
}

/// `tea api /repos/search` answers with `{"ok":..,"data":[…]}`. Older or
/// alternate shapes — a bare API array — are tolerated by parsing on the
/// first bracket.
#[derive(Deserialize)]
#[serde(untagged)]
enum TeaOwner {
    Name(String),
    Object { login: String },
}

#[derive(Deserialize)]
struct TeaSearch {
    #[serde(default)]
    data: Vec<TeaRepo>,
}

/// Map tea's repo listing onto [`OrgRepo`]. The wrapper object is the
/// answer for an owner with zero repos too — `data: []` is empty and true,
/// not a parse failure. Absent optional fields default rather than failing
/// the whole listing: a repo missing one URL still has a name, and sync can
/// still do something with it.
fn parse_tea(body: &str) -> Result<Vec<OrgRepo>> {
    let payload =
        json_body(body).ok_or_else(|| anyhow!("no JSON in tea repos output: {body:?}"))?;
    let repos: Vec<TeaRepo> = if payload.trim_start().starts_with('{') {
        let search: TeaSearch = serde_json::from_str(payload)
            .with_context(|| format!("parsing tea api repos output: {payload:?}"))?;
        search.data
    } else {
        serde_json::from_str(payload)
            .with_context(|| format!("parsing tea repos output: {payload:?}"))?
    };
    Ok(repos
        .into_iter()
        .map(|r| OrgRepo {
            name: r.name,
            owner_login: r.owner.as_ref().map(|o| match o {
                TeaOwner::Name(n) => n.clone(),
                TeaOwner::Object { login } => login.clone(),
            }),
            ssh_url: r.ssh_url,
            https_url: r.clone_url,
            archived: r.archived,
            fork: r.fork,
        })
        .collect())
}

/// One `tea` login, as `tea logins` reports it. `name` is the login's own
/// label — the thing [`crate::config::OrgConfig::login`] stores to select
/// the instance later — and `host` is the URL's host, since that is what
/// identifies the instance to a human choosing where to register an org.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TeaLogin {
    pub name: String,
    pub host: String,
    pub user: Option<String>,
    pub is_default: bool,
}

/// Ask `tea` for its configured logins. Sync, run once when the add-flow
/// opens — see [`crate::provider::authenticated_hosts`] for why. JSON
/// output is tried first because parsing JSON beats parsing tables, but
/// the `--output json` flag isn't accepted by every tea (0.15 rejects it
/// outright), so a failed or unparseable attempt falls back to the table —
/// the one shape every tea version prints. A missing binary or an empty
/// config is simply no logins: the form's "no auth, can't add it" case,
/// not an error.
pub fn logins(timeout: Duration) -> Vec<TeaLogin> {
    if let Some(out) =
        crate::provider::run_probe("tea", &["logins", "--output", "json"], &[], timeout)
    {
        if let Some(parsed) = parse_logins_json(&out.stdout) {
            return parsed;
        }
    }
    match crate::provider::run_probe("tea", &["logins"], &[], timeout) {
        Some(out) => parse_logins_table(&out.stdout),
        None => Vec::new(),
    }
}

/// Parse the array newer `tea logins --output json` prints. `None` when the
/// body isn't JSON at all — the caller's cue to fall back to the table.
/// Keys are read tolerantly: the login struct has shuffled field names
/// across tea versions, and the default flag has travelled between them
/// (`default`/`is_default`/`active`); any one being true is trusted. When
/// nothing is marked, the first entry is — the order tea itself writes the
/// config in.
fn parse_logins_json(body: &str) -> Option<Vec<TeaLogin>> {
    let value: serde_json::Value = serde_json::from_str(json_body(body)?).ok()?;
    let entries = value.as_array()?;
    let mut logins: Vec<TeaLogin> = entries
        .iter()
        .filter_map(|entry| {
            let name = entry.get("name")?.as_str()?.trim();
            if name.is_empty() {
                return None;
            }
            let url = entry.get("url")?.as_str()?;
            let user = entry
                .get("user")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|u| !u.is_empty())
                .map(str::to_string);
            let is_default = ["default", "is_default", "active"]
                .iter()
                .any(|k| entry.get(*k).and_then(serde_json::Value::as_bool) == Some(true));
            Some(TeaLogin {
                name: name.to_string(),
                host: url_host(url)?,
                user,
                is_default,
            })
        })
        .collect();
    if !logins.is_empty() && !logins.iter().any(|l| l.is_default) {
        logins[0].is_default = true;
    }
    Some(logins)
}

/// Dispatch on which table shape `tea logins` printed: the box-drawn grid
/// of recent versions, or the plain space-aligned table of older ones.
fn parse_logins_table(body: &str) -> Vec<TeaLogin> {
    if body.contains('│') {
        parse_box_table(body)
    } else {
        parse_plain_table(body)
    }
}

/// The box-drawn table current tea prints: a bordered grid whose header row
/// names the columns, including an explicit DEFAULT column. Cells are split
/// on the border character, so alignment never matters.
fn parse_box_table(body: &str) -> Vec<TeaLogin> {
    let mut logins: Vec<TeaLogin> = Vec::new();
    let mut header: Option<Vec<String>> = None;
    for line in body.lines() {
        // Border rows (┌─┬─┐, ├─┼─┤, └─┴─┘) carry no `│` cells and are
        // skipped by construction.
        if !line.contains('│') {
            continue;
        }
        let cells: Vec<&str> = line.split('│').map(str::trim).collect();
        // The fragments outside the outermost borders are always empty.
        let cells = &cells[1..cells.len().saturating_sub(1)];
        let first = cells.first().copied().unwrap_or("");
        if first.eq_ignore_ascii_case("NAME") {
            header = Some(cells.iter().map(|c| c.to_ascii_uppercase()).collect());
            continue;
        }
        let Some(header) = &header else { continue };
        let col = |name: &str| header.iter().position(|h| h == name);
        let (Some(name_i), Some(url_i)) = (col("NAME"), col("URL")) else {
            continue;
        };
        let name = cells.get(name_i).copied().unwrap_or("").trim();
        if name.is_empty() {
            continue;
        }
        let user = col("USER")
            .and_then(|i| cells.get(i))
            .map(|u| u.trim())
            .filter(|u| !u.is_empty())
            .map(str::to_string);
        let is_default = col("DEFAULT")
            .and_then(|i| cells.get(i))
            .map(|d| d.eq_ignore_ascii_case("true") || *d == "*")
            .unwrap_or(false);
        let Some(host) = col("URL").and_then(|_| url_host(cells.get(url_i).copied().unwrap_or("")))
        else {
            continue;
        };
        logins.push(TeaLogin {
            name: name.to_string(),
            host,
            user,
            is_default,
        });
    }
    // tea marks the default where it can, but 0.15 prints `false` for a
    // sole login — an unmarked list means the first entry.
    if !logins.is_empty() && !logins.iter().any(|l| l.is_default) {
        logins[0].is_default = true;
    }
    logins
}

/// The plain aligned table older tea prints: `Name  URL  SSH Host  User`
/// with columns padded by spaces. Parsed by the header's column offsets so
/// an empty SSH Host cell — the common case for https-only instances —
/// can't shift the fields that follow. The default login is marked with a
/// trailing `*` when tea marks it at all; when nothing is marked, the first
/// row is treated as the default, matching the config's first entry.
fn parse_plain_table(body: &str) -> Vec<TeaLogin> {
    const HEADERS: [&str; 4] = ["Name", "URL", "SSH Host", "User"];
    let mut logins: Vec<TeaLogin> = Vec::new();
    let mut cols: Vec<usize> = Vec::new();
    for line in body.lines() {
        if cols.is_empty() {
            // The header is the first line all four column names line up
            // in order on; anything else is preamble.
            let Some(offsets) = header_offsets(line, &HEADERS) else {
                continue;
            };
            cols = offsets;
            continue;
        }
        let cell = |i: usize| -> Option<&str> {
            let start = *cols.get(i)?;
            let end = cols.get(i + 1).copied().unwrap_or(line.len());
            line.get(start..end).map(str::trim)
        };
        let Some(name) = cell(0) else { continue };
        let name = name.trim();
        if name.is_empty() || name.starts_with('-') {
            continue; // separator rows, if any
        }
        let (is_default, name) = match name.strip_suffix('*') {
            Some(stripped) => (true, stripped),
            None => (false, name),
        };
        let Some(url) = cell(1).filter(|u| !u.is_empty()) else {
            continue;
        };
        let Some(host) = url_host(url) else { continue };
        logins.push(TeaLogin {
            name: name.to_string(),
            host,
            user: cell(3).filter(|u| !u.is_empty()).map(str::to_string),
            is_default,
        });
    }
    if !logins.is_empty() && !logins.iter().any(|l| l.is_default) {
        logins[0].is_default = true;
    }
    logins
}

/// Start offset of each header name, found in order, so plain-table rows
/// can be sliced by column. `None` when the line isn't a header at all.
fn header_offsets(header: &str, names: &[&str]) -> Option<Vec<usize>> {
    let mut offsets = Vec::new();
    let mut from = 0;
    for name in names {
        let pos = header[from..].find(name)? + from;
        offsets.push(pos);
        from = pos + name.len();
    }
    Some(offsets)
}

/// The JSON payload inside a body that may carry chatter: `tea api` prints
/// notice lines (e.g. "NOTE: no login matched this repository…") ahead of
/// the response on stdout in non-interactive mode. Parsing starts at the
/// first character that can open a JSON value; nothing before it matters.
fn json_body(body: &str) -> Option<&str> {
    Some(&body[body.find(['{', '['])?..])
}

/// The host half of a login's URL, e.g. `https://git.example.com:3000/` →
/// `git.example.com:3000`. Lowercased, like every other host this tool
/// compares; the port is kept because a non-standard port identifies a
/// self-hosted instance as much as the name does.
fn url_host(url: &str) -> Option<String> {
    let rest = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    let authority = rest.split(['/', '?']).next()?;
    let authority = authority.rsplit('@').next()?.trim();
    (!authority.is_empty()).then(|| authority.to_ascii_lowercase())
}

/// List the owners the selected login can register on its instance: the
/// account itself first, then the organizations it belongs to. `/user` is
/// asked explicitly because the repo listing takes an owner but nothing
/// else reports the personal namespace — and the form needs it as a choice
/// like any other.
pub async fn list_owners(login: &str, timeout: Duration) -> Result<Vec<String>> {
    let me_body = run_tea(&with_login(&["api", "/user"], login), timeout).await?;
    let me = parse_user(&me_body)?;
    let orgs_body = run_tea(&with_login(&["api", "/user/orgs"], login), timeout).await?;
    let mut owners = vec![me];
    owners.extend(parse_orgs(&orgs_body)?);
    // An account can appear in its own org list (or twice over); the form
    // doesn't need the duplicate.
    let mut deduped: Vec<String> = Vec::new();
    for name in owners {
        if !deduped.contains(&name) {
            deduped.push(name);
        }
    }
    Ok(deduped)
}

/// `-l` selects the tea login; an empty login is tea's own default and is
/// deliberately omitted, mirroring [`list_owner`].
fn with_login<'a>(args: &[&'a str], login: &'a str) -> Vec<&'a str> {
    let mut args = args.to_vec();
    if !login.is_empty() {
        args.push("-l");
        args.push(login);
    }
    args
}

#[derive(Deserialize)]
struct TeaUser {
    #[serde(default)]
    login: Option<String>,
    #[serde(default)]
    user_name: Option<String>,
}

/// The authenticated account's name from `tea api /user`. `user_name` is
/// the field older instances report under; neither present is an error —
/// an ownerless listing would be a silent wrong answer.
fn parse_user(body: &str) -> Result<String> {
    let payload =
        json_body(body).ok_or_else(|| anyhow!("no JSON in tea api /user output: {body:?}"))?;
    let user: TeaUser = serde_json::from_str(payload)
        .with_context(|| format!("parsing tea api /user output: {payload:?}"))?;
    user.login
        .or(user.user_name)
        .map(|n| n.trim().to_string())
        .filter(|n| !n.is_empty())
        .ok_or_else(|| anyhow!("user response carries no login name"))
}

#[derive(Deserialize)]
struct TeaOrg {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    user_name: Option<String>,
}

/// Organization names from `tea api /user/orgs`. Versions disagree about
/// which key carries the owner path — `name`, `username`, `user_name` —
/// and only the path lists correctly, so all three are tried before an
/// entry is dropped.
fn parse_orgs(body: &str) -> Result<Vec<String>> {
    let payload =
        json_body(body).ok_or_else(|| anyhow!("no JSON in tea api /user/orgs output: {body:?}"))?;
    let orgs: Vec<TeaOrg> = serde_json::from_str(payload)
        .with_context(|| format!("parsing tea api /user/orgs output: {payload:?}"))?;
    Ok(orgs
        .into_iter()
        .filter_map(|o| o.name.or(o.username).or(o.user_name))
        .map(|n| n.trim().to_string())
        .filter(|n| !n.is_empty())
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

    #[test]
    fn parses_tea_logins_json() {
        let body = r#"[
            {"name":"codeberg","url":"https://codeberg.org","user":"otter","active":true},
            {"name":"packden","url":"https://git.packden.us","user":"crueber","active":false}
        ]"#;
        let logins = parse_logins_json(body).expect("fixture should parse");
        assert_eq!(logins.len(), 2);
        assert_eq!(logins[0].name, "codeberg");
        assert_eq!(logins[0].host, "codeberg.org");
        assert_eq!(logins[0].user.as_deref(), Some("otter"));
        assert!(logins[0].is_default);
        assert!(!logins[1].is_default);
    }

    // No login marked default means the first entry is, since that's the
    // order tea writes the config in.
    #[test]
    fn json_logins_without_a_default_mark_the_first() {
        let body = r#"[{"name":"packden","url":"https://git.packden.us"}]"#;
        let logins = parse_logins_json(body).expect("fixture should parse");
        assert_eq!(logins.len(), 1);
        assert!(logins[0].is_default);
        assert_eq!(logins[0].user, None);
    }

    // An empty config is valid JSON and means no logins — distinct from a
    // body that was never JSON, which sends the caller to the table shape.
    #[test]
    fn json_logins_distinguish_empty_from_unparseable() {
        assert_eq!(parse_logins_json("[]").map(|l| l.is_empty()), Some(true));
        assert_eq!(parse_logins_json(r#"{"logins":[]}"#), None);
        assert_eq!(parse_logins_json("nope"), None);
        assert_eq!(parse_logins_json(""), None);
    }

    // The exact table current tea prints — including the DEFAULT column
    // reading `false` for a sole login, which is why the first-row rule
    // exists.
    #[test]
    fn parses_the_box_table_tea_prints() {
        let body = "\
┌────────────────┬────────────────────────┬────────────────┬─────────┬─────────┐
│      NAME      │          URL           │    SSH HOST    │  USER   │ DEFAULT │
├────────────────┼────────────────────────┼────────────────┼─────────┼─────────┤
│ git.packden.us │ https://git.packden.us │ git.packden.us │ crueber │ false   │
│ codeberg       │ https://codeberg.org   │ codeberg.org   │ otter   │ true    │
└────────────────┴────────────────────────┴────────────────┴─────────┴─────────┘";
        let logins = parse_logins_table(body);
        assert_eq!(logins.len(), 2);
        assert_eq!(logins[0].name, "git.packden.us");
        assert_eq!(logins[0].host, "git.packden.us");
        assert_eq!(logins[0].user.as_deref(), Some("crueber"));
        assert!(!logins[0].is_default);
        assert!(logins[1].is_default);
    }

    #[test]
    fn box_table_without_a_default_marks_the_first() {
        let body = "\
┌────────────┬────────────────────────┬──────────┬────────┬─────────┐
│    NAME    │          URL           │ SSH HOST │  USER  │ DEFAULT │
├────────────┼────────────────────────┼──────────┼────────┼─────────┤
│ git.packden.us │ https://git.packden.us │      │ crueber│ false   │
└────────────┴────────────────────────┴──────────┴────────┴─────────┘";
        let logins = parse_logins_table(body);
        assert_eq!(logins.len(), 1);
        assert!(logins[0].is_default);
        assert_eq!(logins[0].user.as_deref(), Some("crueber"));
    }

    // The older plain shape: columns padded by spaces, the SSH Host cell
    // often empty, the default marked with a trailing `*`.
    #[test]
    fn parses_the_plain_table_older_tea_prints() {
        let body = "\
Name        URL                      SSH Host        User
codeberg    https://codeberg.org                     otter
company*    https://git.company.com  git.company.com jill";
        let logins = parse_logins_table(body);
        assert_eq!(logins.len(), 2);
        assert_eq!(logins[0].name, "codeberg");
        assert_eq!(logins[0].host, "codeberg.org");
        assert_eq!(logins[0].user.as_deref(), Some("otter"));
        assert!(!logins[0].is_default);
        assert_eq!(logins[1].name, "company");
        assert_eq!(logins[1].host, "git.company.com");
        assert_eq!(logins[1].user.as_deref(), Some("jill"));
        assert!(logins[1].is_default);
    }

    #[test]
    fn plain_table_without_a_default_marks_the_first() {
        let body = "\
Name      URL                    SSH Host  User
solo      https://git.example.com          sam";
        let logins = parse_logins_table(body);
        assert_eq!(logins.len(), 1);
        assert!(logins[0].is_default);
    }

    #[test]
    fn parses_no_logins_from_empty_output() {
        assert!(parse_logins_table("").is_empty());
        assert!(parse_logins_table("no logins configured").is_empty());
    }

    // Real `tea api /user` output, chatter line and all: the NOTE line is
    // why parsing starts at the first JSON value rather than the first byte.
    #[test]
    fn parses_the_user_out_of_annotated_api_output() {
        let body = "NOTE: no login matched this repository, falling back to login 'git.packden.us'.\n\
                    {\"id\":1,\"login\":\"crueber\",\"login_name\":\"\",\"full_name\":\"Christopher Rueber\"}";
        assert_eq!(parse_user(body).expect("fixture should parse"), "crueber");
        assert_eq!(
            parse_user(r#"{"id":2,"user_name":"otter"}"#).expect("should parse"),
            "otter"
        );
    }

    #[test]
    fn rejects_user_output_without_a_login_name() {
        assert!(parse_user(r#"{"id":3}"#).is_err());
        assert!(parse_user("chatter only").is_err());
        assert!(parse_user("").is_err());
    }

    // Versions disagree about which key carries the owner path; all three
    // spellings must resolve.
    #[test]
    fn parses_org_names_from_any_of_the_key_shapes() {
        let body = r#"[
            {"id":1,"name":"acme","username":"acme"},
            {"id":2,"username":"fleet"},
            {"id":3,"user_name":"legacy"},
            {"id":4,"full_name":"Display Only"}
        ]"#;
        assert_eq!(
            parse_orgs(body).expect("fixture should parse"),
            vec!["acme", "fleet", "legacy"]
        );
    }

    #[test]
    fn rejects_non_json_org_output() {
        assert!(parse_orgs("not json at all").is_err());
        assert!(parse_orgs("").is_err());
    }

    #[test]
    fn appends_the_login_flag_only_when_one_is_named() {
        assert_eq!(with_login(&["api", "/user"], ""), vec!["api", "/user"]);
        assert_eq!(
            with_login(&["api", "/user"], "packden"),
            vec!["api", "/user", "-l", "packden"]
        );
    }

    #[test]
    fn extracts_hosts_from_login_urls() {
        assert_eq!(
            url_host("https://git.example.com"),
            Some("git.example.com".into())
        );
        assert_eq!(
            url_host("https://git.example.com:3000/"),
            Some("git.example.com:3000".into())
        );
        assert_eq!(
            url_host("http://GIT.Example.COM/path"),
            Some("git.example.com".into())
        );
        assert_eq!(url_host(""), None);
    }

    // tea 0.15.1's `repos list --output json` — the real captured output. A
    // compact schema with an `ssh` field and no https URL, archived or fork:
    // the shape that silently gutted sync before the api-based listing.
    #[test]
    fn the_compact_tea_schema_loses_only_what_tea_loses() {
        let body = r#"[
          {"owner":"crueber","name":"eth-reward-calc","type":"source",
           "ssh":"ssh://git@git.packden.us:2288/crueber/eth-reward-calc.git"},
          {"owner":"crueber","name":"dotfiles","type":"mirror",
           "ssh":"ssh://git@git.packden.us:2288/crueber/dotfiles.git"}
        ]"#;
        let repos = parse_tea(body).unwrap();
        assert_eq!(repos.len(), 2);
        assert_eq!(repos[0].name, "eth-reward-calc");
        assert_eq!(
            repos[0].ssh_url,
            "ssh://git@git.packden.us:2288/crueber/eth-reward-calc.git"
        );
        assert!(
            repos[0].https_url.is_empty(),
            "tea compact has no https URL"
        );
        assert!(!repos[1].archived);
    }

    // `tea api /repos/search` — the shape sync actually uses now: a wrapper
    // object whose `data` carries the real API repo objects, chatter line
    // and all.
    #[test]
    fn the_api_wrapper_is_parsed_with_full_fields() {
        let body = concat!(
            "NOTE: some builds print chatter before the payload\n",
            r#"{"ok":true,"data":[{""#,
            r#"""#,
        );
        let _ = body; // replaced below — kept for readability of the real one
        let body = r#"NOTE: chatter
{"ok":true,"data":[{"id":63,"owner":{"login":"crueber"},"name":"ai-rag","full_name":"crueber/ai-rag","ssh_url":"ssh://git@git.packden.us:2288/crueber/ai-rag.git","clone_url":"https://git.packden.us/crueber/ai-rag.git","archived":false,"fork":false},{"id":64,"name":"boxes","ssh_url":"ssh://git@git.packden.us:2288/acme/boxes.git","clone_url":"https://git.packden.us/acme/boxes.git","archived":true,"fork":true}]}"#;
        let repos = parse_tea(body).unwrap();
        assert_eq!(repos.len(), 2);
        assert_eq!(repos[0].name, "ai-rag");
        assert_eq!(
            repos[0].ssh_url,
            "ssh://git@git.packden.us:2288/crueber/ai-rag.git"
        );
        assert_eq!(
            repos[0].https_url,
            "https://git.packden.us/crueber/ai-rag.git"
        );
        assert!(!repos[0].archived);
        assert!(repos[1].archived && repos[1].fork);
    }

    #[test]
    fn an_empty_data_array_is_a_real_empty_answer() {
        let repos = parse_tea(r#"{"ok":true,"data":[]}"#).unwrap();
        assert!(repos.is_empty());
    }

    #[test]
    fn a_bare_api_array_still_parses() {
        let repos =
            parse_tea(r#"[{"name":"r","ssh_url":"ssh://x/r.git","clone_url":"https://x/r.git"}]"#)
                .unwrap();
        assert_eq!(repos.len(), 1);
        assert_eq!(repos[0].https_url, "https://x/r.git");
    }

    #[test]
    fn the_owner_login_is_carried_from_both_shapes() {
        let compact = parse_tea(
            r#"[{"owner":"crueber","name":"r1","type":"source","ssh":"ssh://x/crueber/r1.git"}]"#,
        )
        .unwrap();
        assert_eq!(compact[0].owner_login.as_deref(), Some("crueber"));

        let api = parse_tea(
            r#"{"ok":true,"data":[{"name":"r2","owner":{"login":"mhs"},"ssh_url":"ssh://x/mhs/r2.git","clone_url":"https://x/mhs/r2.git"}]}"#,
        )
        .unwrap();
        assert_eq!(api[0].owner_login.as_deref(), Some("mhs"));
    }
    #[test]
    fn no_json_at_all_is_an_error() {
        assert!(parse_tea("totally not json").is_err());
    }
}
