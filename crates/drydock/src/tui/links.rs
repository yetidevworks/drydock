use std::{
    path::{Path, PathBuf},
    process::Stdio,
};

use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use tokio::process::Command;
use url::Url;

use crate::git::{run_git, run_git_local};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Target {
    Repository,
    Commit(String),
    Branch(String),
    Tag(String),
    Releases,
    Reviews,
    Issues,
    Automation,
    Compare { tag: String },
}

pub(super) enum Unavailable {
    NoWebRemote,
    UnsupportedHost,
    UnsupportedCommitHost,
    UnverifiedCommit,
    NotOnRemote,
}

impl Unavailable {
    pub(super) fn message(&self) -> &'static str {
        match self {
            Self::NoWebRemote => "No web remote available",
            Self::UnsupportedHost => "Deep links are unavailable for this Git host",
            Self::UnsupportedCommitHost => "Commit links are unavailable for this Git host",
            Self::UnverifiedCommit => "Could not verify the commit on the remote",
            Self::NotOnRemote => "Not on remote yet",
        }
    }
}

pub(super) enum Outcome {
    BrowserStatus(String),
    CopyPath { root: PathBuf, reason: Unavailable },
}

enum Forge {
    GitHub,
    GitLab,
    Unknown,
}

const PATH_SEGMENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

struct WebBase {
    url: Url,
    forge: Forge,
    remote: String,
}

impl WebBase {
    async fn load(root: &Path) -> Option<Self> {
        let branch = symbolic_branch(root).await;
        Self::load_for_branch(root, branch.as_deref()).await
    }

    async fn load_for_branch(root: &Path, branch: Option<&str>) -> Option<Self> {
        let remote = selected_remote(root, branch).await?;
        let raw = run_git(root, &["remote", "get-url", "--", &remote])
            .await
            .ok()?;
        let url = web_base(&raw)?;
        let forge = match url.host_str()? {
            "github.com" => Forge::GitHub,
            host if host.contains("gitlab") => Forge::GitLab,
            _ => Forge::Unknown,
        };
        Some(Self { url, forge, remote })
    }

    fn deep_link(&self, segments: &[&str]) -> Option<Url> {
        let mut url = self.url.clone();
        let mut path = url.path().to_owned();
        match self.forge {
            Forge::GitHub => {}
            Forge::GitLab => path.push_str("/-"),
            Forge::Unknown => return None,
        }
        for segment in segments.iter().flat_map(|segment| segment.split('/')) {
            path.push('/');
            path.extend(utf8_percent_encode(segment, PATH_SEGMENT));
        }
        url.set_path(&path);
        Some(url)
    }
}

pub(super) fn platform_opener() -> Option<PathBuf> {
    let program = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(target_os = "linux") {
        "xdg-open"
    } else {
        return None;
    };
    Some(PathBuf::from(program))
}

async fn symbolic_branch(root: &Path) -> Option<String> {
    let head = run_git(root, &["symbolic-ref", "--quiet", "HEAD"])
        .await
        .ok()?;
    head.trim_end()
        .strip_prefix("refs/heads/")
        .map(str::to_owned)
}

async fn selected_remote(root: &Path, branch: Option<&str>) -> Option<String> {
    let remotes = run_git(root, &["remote"]).await.ok()?;
    let tracking = match branch {
        Some(branch) => run_git(
            root,
            &["config", "--get", &format!("branch.{branch}.remote")],
        )
        .await
        .ok(),
        None => None,
    };
    tracking
        .as_deref()
        .map(str::trim_end)
        .filter(|name| remotes.lines().any(|remote| remote == *name))
        .or_else(|| remotes.lines().find(|name| *name == "origin"))
        .or_else(|| remotes.lines().next())
        .map(str::to_owned)
}

/// Validate the authority before parsing. A browser URL parser can repair
/// extra slashes, but a remote must explicitly name its host.
fn valid_authority(authority: &str) -> bool {
    !authority.is_empty()
        && !authority
            .chars()
            .any(|c| c.is_whitespace() || matches!(c, '/' | '?' | '#'))
}

fn web_base(remote: &str) -> Option<Url> {
    let remote = remote.strip_suffix('\n').unwrap_or(remote);
    if remote.chars().any(|c| c.is_control() || c == '\\') {
        return None;
    }
    let mut url = if let Some((scheme, rest)) = remote.split_once("://") {
        let (authority, path) = rest.split_once('/')?;
        if !valid_authority(authority) {
            return None;
        }
        if scheme.eq_ignore_ascii_case("ssh") {
            let mut url = Url::parse(&format!("https://{authority}/")).ok()?;
            url.set_port(None).ok()?;
            url.set_path(&format!("/{path}"));
            url
        } else if scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https") {
            Url::parse(remote).ok()?
        } else {
            return None;
        }
    } else {
        let (authority, path) = remote.split_once(':')?;
        if !valid_authority(authority) {
            return None;
        }
        let mut url = Url::parse(&format!("https://{authority}/")).ok()?;
        url.path_segments_mut()
            .ok()?
            .pop_if_empty()
            .extend(path.trim_matches('/').split('/'));
        url
    };
    let path = url.path().trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    if path.is_empty() || !url.has_host() {
        return None;
    }
    let path = format!("/{path}");
    url.set_username("").ok()?;
    url.set_password(None).ok()?;
    url.set_query(None);
    url.set_fragment(None);
    url.set_path(&path);
    Some(url)
}

pub(super) async fn resolve(root: &Path, target: &Target) -> Result<Url, Unavailable> {
    let base = WebBase::load(root).await.ok_or(Unavailable::NoWebRemote)?;
    resolve_with_base(root, &base, target).await
}

pub(super) struct Preview {
    pub(super) branch: Option<String>,
    pub(super) copy_paths: Vec<Target>,
}

pub(super) async fn preview(root: &Path, mut targets: Vec<Target>) -> Preview {
    let symbolic = symbolic_branch(root).await;
    let base = WebBase::load_for_branch(root, symbolic.as_deref()).await;
    let branch = match symbolic {
        Some(branch)
            if exact_ref(root, &format!("refs/heads/{branch}"))
                .await
                .is_ok() =>
        {
            Some(branch)
        }
        _ => None,
    };
    targets.extend(branch.iter().cloned().map(Target::Branch));
    let mut copy_paths = Vec::new();
    if let Some(base) = base {
        for target in targets {
            if resolve_with_base(root, &base, &target).await.is_err() {
                copy_paths.push(target);
            }
        }
    } else {
        copy_paths = targets;
    }
    Preview { branch, copy_paths }
}

async fn resolve_with_base(
    root: &Path,
    base: &WebBase,
    target: &Target,
) -> Result<Url, Unavailable> {
    match target {
        Target::Repository => Ok(base.url.clone()),
        Target::Commit(sha) => {
            let url = base
                .deep_link(&["commit", sha])
                .ok_or(Unavailable::UnsupportedCommitHost)?;
            require_remote_commit(root, &base.remote, sha).await?;
            Ok(url)
        }
        Target::Branch(branch) => {
            if matches!(base.forge, Forge::Unknown) {
                return Err(Unavailable::UnsupportedHost);
            }
            let branch = remote_branch(root, &base.remote, branch).await?;
            base.deep_link(&["tree", &branch])
                .ok_or(Unavailable::UnsupportedHost)
        }
        Target::Tag(tag) => {
            let url = match base.forge {
                Forge::GitLab => base.deep_link(&["releases", tag]),
                _ => base.deep_link(&["releases", "tag", tag]),
            }
            .ok_or(Unavailable::UnsupportedHost)?;
            let sha = tag_commit(root, tag).await?;
            require_remote_commit(root, &base.remote, &sha).await?;
            Ok(url)
        }
        Target::Releases => base
            .deep_link(&["releases"])
            .ok_or(Unavailable::UnsupportedHost),
        Target::Reviews => base
            .deep_link(&[match base.forge {
                Forge::GitLab => "merge_requests",
                _ => "pulls",
            }])
            .ok_or(Unavailable::UnsupportedHost),
        Target::Issues => base
            .deep_link(&["issues"])
            .ok_or(Unavailable::UnsupportedHost),
        Target::Automation => base
            .deep_link(&[match base.forge {
                Forge::GitLab => "pipelines",
                _ => "actions",
            }])
            .ok_or(Unavailable::UnsupportedHost),
        Target::Compare { tag } => {
            let (head, tag_sha) =
                tokio::try_join!(revision_commit(root, "HEAD"), tag_commit(root, tag))?;
            let url = base
                .deep_link(&["compare", &format!("{tag}...{head}")])
                .ok_or(Unavailable::UnsupportedHost)?;
            tokio::try_join!(
                require_remote_commit(root, &base.remote, &head),
                require_remote_commit(root, &base.remote, &tag_sha)
            )?;
            Ok(url)
        }
    }
}

async fn exact_ref(root: &Path, reference: &str) -> Result<String, Unavailable> {
    run_git_local(root, &["show-ref", "--verify", "--hash", "--", reference])
        .await
        .map(|sha| sha.trim_end().to_owned())
        .map_err(|_| Unavailable::NotOnRemote)
}

async fn revision_commit(root: &Path, revision: &str) -> Result<String, Unavailable> {
    let revision = format!("{revision}^{{commit}}");
    run_git_local(
        root,
        &["rev-parse", "--verify", "--end-of-options", &revision],
    )
    .await
    .map(|sha| sha.trim_end().to_owned())
    .map_err(|_| Unavailable::NotOnRemote)
}

async fn tag_commit(root: &Path, tag: &str) -> Result<String, Unavailable> {
    let tag_oid = exact_ref(root, &format!("refs/tags/{tag}")).await?;
    revision_commit(root, &tag_oid).await
}

/// Resolve a local branch's upstream source name on the selected remote.
async fn remote_branch(root: &Path, remote: &str, branch: &str) -> Result<String, Unavailable> {
    let branch_ref = format!("refs/heads/{branch}");
    let upstream = run_git_local(
        root,
        &[
            "for-each-ref",
            "--format=%(refname)%09%(upstream:remotename)%09%(upstream:remoteref)%09%(upstream)",
            "--",
            &branch_ref,
        ],
    )
    .await
    .map_err(|_| Unavailable::NotOnRemote)?;
    let prefix = format!("{branch_ref}\t");
    let upstream = upstream
        .lines()
        .find_map(|line| line.strip_prefix(&prefix))
        .ok_or(Unavailable::NotOnRemote)?;
    let (upstream_remote, upstream) = upstream.split_once('\t').ok_or(Unavailable::NotOnRemote)?;
    let (upstream_ref, tracking_ref) = upstream.split_once('\t').ok_or(Unavailable::NotOnRemote)?;
    let remote_prefix = format!("refs/remotes/{remote}/");
    if upstream_remote.is_empty() {
        exact_ref(root, &format!("{remote_prefix}{branch}")).await?;
        return Ok(branch.to_owned());
    }
    if upstream_remote != remote || !tracking_ref.starts_with(&remote_prefix) {
        return Err(Unavailable::NotOnRemote);
    }
    let branch = upstream_ref
        .strip_prefix("refs/heads/")
        .ok_or(Unavailable::NotOnRemote)?;
    exact_ref(root, tracking_ref).await?;
    Ok(branch.to_owned())
}

async fn require_remote_commit(root: &Path, remote: &str, sha: &str) -> Result<(), Unavailable> {
    if sha.len() != 40 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(Unavailable::UnverifiedCommit);
    }
    let pattern = format!("refs/remotes/{remote}/");
    let refs = run_git_local(
        root,
        &[
            "for-each-ref",
            "--count=1",
            "--format=%(refname)",
            "--contains",
            sha,
            "--",
            &pattern,
        ],
    )
    .await
    .map_err(|_| Unavailable::UnverifiedCommit)?;
    if refs.trim().is_empty() {
        return Err(Unavailable::NotOnRemote);
    }
    Ok(())
}

pub(super) async fn activate(root: PathBuf, target: Target, opener: Option<PathBuf>) -> Outcome {
    match resolve(&root, &target).await {
        Ok(url) => Outcome::BrowserStatus(open_url(opener.as_deref(), url).await),
        Err(reason) => Outcome::CopyPath { root, reason },
    }
}

async fn open_url(opener: Option<&Path>, url: Url) -> String {
    let Some(opener) = opener else {
        return "Could not open browser: no opener for this platform".into();
    };
    match Command::new(opener)
        .arg(url.as_str())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .status()
        .await
    {
        Ok(status) if status.success() => format!("Opened {url}"),
        Ok(status) => format!("Could not open {url}: opener exited with {status}"),
        Err(err) => format!("Could not open {url}: {err}"),
    }
}
