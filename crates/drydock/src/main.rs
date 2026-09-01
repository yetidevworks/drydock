mod cache;
mod cli;
mod column;
mod config;
mod discover;
mod filter;
mod fmt;
mod gh;
mod git;
mod gitea;
mod gitlab;
mod model;
mod org;
mod paths;
mod probe;
mod provider;
mod report;
mod tui;
mod watch;

use anyhow::{anyhow, Context, Result};
use clap::Parser;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;

use cli::{Cli, Commands, ConfigCommands, ListArgs, OrgCommands};
use filter::{Filter, MatchMode, Query, Sort};
use model::RepoStatus;
use probe::{Fetch, Tier};

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    init_tracing(cli.command.is_none());

    match cli.command {
        None => tui::run().await,
        Some(Commands::List(args)) => cmd_list(args).await,
        Some(Commands::Status { path, json }) => cmd_status(path, json).await,
        Some(Commands::Releasable {
            min_commits,
            include_changelog,
            json,
        }) => cmd_releasable(min_commits, include_changelog, json).await,
        Some(Commands::Scan {
            fast,
            no_cache,
            fetch,
        }) => cmd_scan(fast, no_cache, fetch).await,
        Some(Commands::Groups { json }) => cmd_groups(json).await,
        Some(Commands::Config(c)) => cmd_config(c),
        Some(Commands::Org(c)) => cmd_org(c).await,
        Some(Commands::TuiSnapshot {
            width,
            height,
            view,
        }) => {
            print!("{}", tui::snapshot(width, height, &view).await?);
            Ok(())
        }
    }
}

/// Start logging somewhere that won't wreck the output.
///
/// The dashboard owns the terminal, and stderr is not redirected while it
/// runs, so a single warning printed there lands on top of the frame and
/// scrolls the whole thing up. Under the dashboard the log goes to a file
/// instead, and to nowhere at all if that file cannot be opened. Every other
/// command is ordinary CLI output, where stderr is exactly right.
fn init_tracing(dashboard: bool) {
    let filter =
        tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into());
    if !dashboard {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(std::io::stderr)
            .init();
        return;
    }
    let file = paths::log_file().ok().and_then(|path| {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .ok()
    });
    match file {
        Some(file) => tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_ansi(false)
            .with_writer(std::sync::Mutex::new(file))
            .init(),
        None => tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(std::io::sink)
            .init(),
    }
}

/// Load config, reporting a bad config file rather than dying on it.
fn load_config() -> Arc<config::Config> {
    let (cfg, warning) = config::load_or_default();
    if let Some(warning) = warning {
        eprintln!("drydock: using defaults, config could not be read: {warning}");
    }
    Arc::new(cfg)
}

async fn gather(
    cfg: Arc<config::Config>,
    tier: Tier,
    no_cache: bool,
    fetch: Fetch,
) -> Result<Vec<RepoStatus>> {
    if no_cache {
        let _ = cache::clear();
    }
    announce_fetch(&fetch);
    let fleet = probe::sweep(cfg, tier, fetch, None).await?;
    print_scan_note(&fleet.timings);
    print_fetch_note(&fleet.timings);
    Ok(fleet.repos)
}

/// A fetch across a few hundred remotes is not instant and not free, so say
/// it's happening before it starts rather than leaving the terminal silent.
fn announce_fetch(fetch: &Fetch) {
    match fetch {
        Fetch::Skip => {}
        Fetch::All => eprintln!("drydock: fetching every repo with a remote..."),
        Fetch::Group(group) => eprintln!("drydock: fetching the {group} group..."),
    }
}

/// Say what the fetch phase managed, on stderr so it never contaminates piped
/// output. Failures are worth a word: a repo that couldn't be reached still
/// reports whatever its last fetch left, and silently passing that off as
/// checked is the thing this whole flag exists to stop.
fn print_fetch_note(timings: &probe::Timings) {
    if timings.fetched == 0 {
        return;
    }
    let failed = if timings.fetch_failed > 0 {
        format!(
            ", {} could not be reached and still show their last known counts",
            timings.fetch_failed
        )
    } else {
        String::new()
    };
    eprintln!(
        "drydock: fetched {} repos in {}{failed}",
        timings.fetched,
        fmt::duration(timings.fetch)
    );
}

/// A sweep across hundreds of repos takes a moment. Say what it cost, on
/// stderr, so it never contaminates piped output.
fn print_scan_note(timings: &probe::Timings) {
    tracing::debug!(
        repos = timings.repos,
        walk = ?timings.discovery,
        refs = ?timings.refs,
        work = ?timings.work,
        scanned = timings.work_scanned,
        cached = timings.work_cached,
        "sweep complete"
    );
}

fn build_query(args: &ListArgs) -> Result<Query> {
    let mut query = Query {
        match_mode: MatchMode::from_str(&args.match_mode).map_err(|e| anyhow!(e))?,
        sort: Sort::from_str(&args.sort).map_err(|e| anyhow!(e))?,
        reverse: args.reverse,
        group: args.group.clone(),
        search: args.search.clone().unwrap_or_default(),
        ..Query::default()
    };

    for (on, filter) in [
        (args.dirty, Filter::Dirty),
        (args.unpushed, Filter::Unpushed),
        (args.unreleased, Filter::Unreleased),
        (args.needs_release, Filter::NeedsRelease),
        (args.released, Filter::Released),
        (args.behind, Filter::Behind),
        (args.conflicted, Filter::Conflicted),
        (args.in_progress, Filter::InProgress),
        (args.detached, Filter::Detached),
        (args.no_remote, Filter::NoRemote),
        (args.no_upstream, Filter::NoUpstream),
        (args.stashed, Filter::Stashed),
        (args.clean, Filter::Clean),
        (args.errored, Filter::Error),
        (args.public, Filter::Public),
        (args.private, Filter::Private),
    ] {
        if on {
            query.filters.push(filter);
        }
    }
    for name in &args.filters {
        query
            .filters
            .push(Filter::from_str(name).map_err(|e| anyhow!(e))?);
    }
    if let Some(since) = &args.since {
        query.set_since(since).map_err(|e| anyhow!(e))?;
    }
    Ok(query)
}

/// What `--fetch` means for one `list` invocation. Narrowed to `--group` when
/// there is one: the rows outside it aren't going to be printed, so fetching
/// them is a few hundred network round trips spent on output nobody asked
/// for.
fn list_fetch(args: &ListArgs) -> Fetch {
    match (args.fetch, &args.group) {
        (false, _) => Fetch::Skip,
        (true, Some(group)) => Fetch::Group(group.clone()),
        (true, None) => Fetch::All,
    }
}

async fn cmd_list(args: ListArgs) -> Result<()> {
    let cfg = load_config();
    let query = build_query(&args)?;
    // Resolved before `cfg` is handed to the sweep, which takes ownership.
    let columns = cfg.columns();

    let repos: Vec<RepoStatus> = if args.cached {
        let mut repos: Vec<RepoStatus> = cache::load().into_values().collect();
        repos.sort_by(|a, b| a.root.cmp(&b.root));
        if repos.is_empty() {
            eprintln!("drydock: no cache yet, run `drydock scan` first");
        }
        repos
    } else {
        let tier = if args.fast { Tier::Refs } else { Tier::Full };
        gather(cfg, tier, args.no_cache, list_fetch(&args)).await?
    };

    let now = git::now_unix();
    let mut selected = query.apply(&repos, now);
    if let Some(limit) = args.limit {
        selected.truncate(limit);
    }

    if args.json {
        println!("{}", report::list_json(&selected, now)?);
        return Ok(());
    }

    if selected.is_empty() {
        println!("Nothing matched.");
    } else {
        print!(
            "{}",
            report::list_table(&selected, now, args.paths, &columns)
        );
    }
    let shown = selected.len();
    println!();
    println!("{}", report::summary(&repos, None));
    if shown != repos.len() {
        println!("Showing {shown} of {}.", repos.len());
    }
    Ok(())
}

async fn cmd_status(path: Option<String>, json: bool) -> Result<()> {
    let cfg = load_config();
    let start = match path {
        Some(p) => paths::expand(&p),
        None => std::env::current_dir().context("Reading the current directory")?,
    };
    let start = start.canonicalize().unwrap_or(start);
    let root = find_repo_root(&start)
        .ok_or_else(|| anyhow!("No git repo at or above {}", paths::contract(&start)))?;

    let (group, name) = split_for_display(&cfg, &root);
    let discovered = discover::Discovered {
        root: root.clone(),
        group,
        name,
    };
    let cached = cache::load();
    // Forced: one repo asked about by name is worth the scan, and a cached
    // "clean" for a tree the caller just edited is the wrong answer.
    let status = probe::probe_one(&discovered, &cfg, cached.get(&root), Tier::Full, true).await;

    let now = git::now_unix();
    if json {
        println!("{}", report::detail_json(&status, now)?);
    } else {
        print!("{}", report::detail(&status, now));
    }
    Ok(())
}

async fn cmd_releasable(min_commits: u32, include_changelog: bool, json: bool) -> Result<()> {
    let cfg = load_config();
    let repos = gather(cfg, Tier::Full, false, Fetch::Skip).await?;
    let now = git::now_unix();

    let mut selected: Vec<&RepoStatus> = repos
        .iter()
        .filter(|r| {
            let by_commits = r.commits_since_tag() >= min_commits.max(1);
            let by_changelog = include_changelog
                && r.refs
                    .as_ref()
                    .and_then(|refs| refs.changelog.as_ref())
                    .map(|c| !c.tagged)
                    .unwrap_or(false);
            by_commits || by_changelog
        })
        .collect();
    filter::sort_repos(&mut selected, Sort::Activity, false);

    if json {
        println!("{}", report::list_json(&selected, now)?);
        return Ok(());
    }

    if selected.is_empty() {
        println!("Nothing has commits past its last tag.");
        return Ok(());
    }

    let rows: Vec<Vec<String>> = selected
        .iter()
        .map(|r| {
            let changelog = r
                .refs
                .as_ref()
                .and_then(|refs| refs.changelog.as_ref())
                .map(|c| {
                    if c.tagged {
                        format!("{} (tagged)", c.version)
                    } else if c.unreleased_blocks > 1 {
                        let extra = c.unreleased_blocks - 1;
                        format!(
                            "{} (+{extra} block{})",
                            c.version,
                            if extra == 1 { "" } else { "s" }
                        )
                    } else {
                        format!("{} (untagged)", c.version)
                    }
                })
                .unwrap_or_else(|| "-".into());
            let notes = {
                let mut n = Vec::new();
                if r.flags().dirty {
                    n.push("dirty");
                }
                if r.refs.as_ref().map(|x| x.tag_off_branch()).unwrap_or(false) {
                    n.push("tag off branch");
                }
                if r.unpushed_total() > 0 {
                    n.push("unpushed");
                }
                if n.is_empty() {
                    "·".to_string()
                } else {
                    n.join(", ")
                }
            };
            vec![
                r.slug(),
                r.branch_label(),
                r.tag_label(),
                r.commits_since_tag().to_string(),
                changelog,
                fmt::age(r.activity_at(), now),
                notes,
            ]
        })
        .collect();

    print!(
        "{}",
        report::table(
            &[
                "REPO",
                "BRANCH",
                "LAST TAG",
                "COMMITS",
                "CHANGELOG",
                "AGE",
                "NOTES"
            ],
            &[
                report::Align::Left,
                report::Align::Left,
                report::Align::Left,
                report::Align::Right,
                report::Align::Left,
                report::Align::Right,
                report::Align::Left,
            ],
            &rows,
        )
    );
    println!();
    println!(
        "{} candidate{}.",
        selected.len(),
        if selected.len() == 1 { "" } else { "s" }
    );
    Ok(())
}

async fn cmd_scan(fast: bool, no_cache: bool, fetch: bool) -> Result<()> {
    let cfg = load_config();
    if no_cache {
        let _ = cache::clear();
    }
    let fetch = if fetch { Fetch::All } else { Fetch::Skip };
    announce_fetch(&fetch);
    let tier = if fast { Tier::Refs } else { Tier::Full };
    let fetched = fetch != Fetch::Skip;
    let fleet = probe::sweep(cfg, tier, fetch, None).await?;
    println!("{}", report::summary(&fleet.repos, Some(&fleet.timings)));
    if fast {
        println!("Working trees were not scanned (--fast).");
    }
    print_fetch_note(&fleet.timings);
    if !fetched {
        let never = fleet.repos.iter().filter(|r| r.never_fetched()).count();
        if never > 0 {
            println!(
                "{never} repos have never fetched, so their behind counts are unchecked. \
                 Run `drydock scan --fetch` to check them."
            );
        }
    }
    Ok(())
}

async fn cmd_groups(json: bool) -> Result<()> {
    let cfg = load_config();
    let repos = gather(cfg, Tier::Full, false, Fetch::Skip).await?;
    if json {
        let now = git::now_unix();
        let views: Vec<_> = repos.iter().map(|r| report::view(r, now)).collect();
        println!("{}", serde_json::to_string_pretty(&views)?);
        return Ok(());
    }
    print!("{}", report::groups_table(&repos));
    println!();
    println!("{}", report::summary(&repos, None));
    Ok(())
}

fn cmd_config(command: ConfigCommands) -> Result<()> {
    match command {
        ConfigCommands::Path => {
            println!("config  {}", paths::config_file()?.display());
            println!("cache   {}", paths::cache_file()?.display());
            Ok(())
        }
        ConfigCommands::Init { force } => {
            let path = paths::config_file()?;
            if path.exists() && !force {
                println!(
                    "{} already exists. Pass --force to overwrite it.",
                    path.display()
                );
                return Ok(());
            }
            let written = config::save(&config::Config::default())?;
            println!("Wrote {}", written.display());
            Ok(())
        }
        ConfigCommands::Show => {
            let (cfg, warning) = config::load_or_default();
            if let Some(warning) = warning {
                eprintln!("drydock: showing defaults, config could not be read: {warning}");
            }
            print!("{}", toml::to_string_pretty(&cfg)?);
            Ok(())
        }
    }
}

async fn cmd_org(command: OrgCommands) -> Result<()> {
    match command {
        OrgCommands::Add {
            owner,
            provider,
            host,
            path,
            login,
            protocol,
            include_forks,
            include_archived,
            root,
        } => org_add(
            owner,
            provider,
            host,
            path,
            login,
            protocol,
            include_forks,
            include_archived,
            root,
        ),
        OrgCommands::List { json } => org_list(json),
        OrgCommands::Remove { owner } => org_remove(&owner),
        OrgCommands::Sync {
            owner,
            dry_run,
            json,
        } => org_sync(owner, dry_run, json).await,
    }
}

/// Read a `--provider` name. `OrgProvider::from_host` matches hostnames, so
/// the bare names get their own small table rather than a fake host lookup —
/// and anything else is refused with the valid spellings, since a typo'd
/// provider would point the wrong CLI at somebody's instance.
fn parse_provider(name: &str) -> Result<config::OrgProvider> {
    match name.trim().to_ascii_lowercase().as_str() {
        "github" => Ok(config::OrgProvider::GitHub),
        "gitlab" => Ok(config::OrgProvider::GitLab),
        "gitea" => Ok(config::OrgProvider::Gitea),
        other => Err(anyhow!(
            "unknown provider \"{other}\"; expected github, gitlab, or gitea"
        )),
    }
}
/// Read a `--protocol` name. Case-insensitive because people type `SSH`.
fn parse_protocol(name: &str) -> Result<config::CloneProtocol> {
    match name.trim().to_ascii_lowercase().as_str() {
        "ssh" => Ok(config::CloneProtocol::Ssh),
        "https" => Ok(config::CloneProtocol::Https),
        other => Err(anyhow!(
            "unknown protocol \"{other}\"; expected ssh or https"
        )),
    }
}

/// The hostname each provider's hosted instance lives at, for when the user
/// named a provider but not a host.
fn default_host(provider: config::OrgProvider) -> String {
    match provider {
        config::OrgProvider::GitHub => "github.com".into(),
        config::OrgProvider::GitLab => "gitlab.com".into(),
        config::OrgProvider::Gitea => "gitea.com".into(),
    }
}

fn org_add(
    owner: String,
    provider: Option<String>,
    host: Option<String>,
    path: Option<String>,
    login: Option<String>,
    protocol: Option<String>,
    include_forks: bool,
    include_archived: bool,
    add_root: bool,
) -> Result<()> {
    // Provider and host resolve each other: an explicit provider takes the
    // hosted default host, a bare host must be a known instance, and with
    // neither the assumption is github.com — the common case gets to be no
    // flags at all. An inferred provider stays unset in the config, so a
    // hand-edited config keeps the same minimal shape `org add` writes.
    let trimmed_provider = provider.as_deref().map(str::trim).filter(|p| !p.is_empty());
    let trimmed_host = host.as_deref().map(str::trim).filter(|h| !h.is_empty());
    let (resolved, host) = match (trimmed_provider, trimmed_host) {
        (Some(name), explicit) => {
            let provider = parse_provider(name)?;
            let host = explicit
                .map(str::to_string)
                .unwrap_or_else(|| default_host(provider));
            (Some(provider), host)
        }
        (None, Some(hostname)) => {
            // The provider is inferred again at load time, so here it only
            // has to exist — which is what turns an unknown host into an
            // error instead of a silently misresolved org.
            if config::OrgProvider::from_host(hostname).is_none() {
                return Err(anyhow!(
                    "host \"{hostname}\" is not a known instance; \
                     pass --provider github, gitlab, or gitea"
                ));
            }
            (None, hostname.to_string())
        }
        (None, None) => (Some(config::OrgProvider::GitHub), "github.com".into()),
    };
    let protocol = match protocol.as_deref() {
        Some(name) => parse_protocol(name)?,
        None => config::CloneProtocol::Ssh,
    };

    let mut cfg = config::load()?;
    let org = config::OrgConfig {
        provider: resolved,
        host: host.clone(),
        owner: owner.clone(),
        path,
        login: login.unwrap_or_default(),
        protocol,
        include_forks,
        include_archived,
        ..config::OrgConfig::default()
    };

    // The checkout path is needed for `--root` and for the confirmation line,
    // and resolving it here means `org add` fails loudly rather than leaving
    // a registration that sync cannot place anywhere.
    let checkout = org::effective_path(&cfg, &org)?;
    if add_root {
        if let Some(parent) = checkout.parent() {
            // Compared expanded: a root written as `~/Projects` and a parent
            // under `$HOME` are the same directory to the sweep.
            if !cfg.root_paths().contains(&parent.to_path_buf()) {
                cfg.roots.push(paths::contract(parent));
            }
        }
    }

    cfg.orgs.push(org);
    let problems = cfg.org_problems();
    if !problems.is_empty() {
        // Nothing has been saved: a bad registration should not leave the
        // config file worse than the user left it.
        for problem in &problems {
            eprintln!("drydock: {problem}");
        }
        return Err(anyhow!("org add failed validation; config not saved"));
    }

    let written = config::save(&cfg)?;
    println!(
        "Registered {} on {} — checkouts under {}. Config written to {}.",
        owner,
        host,
        paths::contract(&checkout),
        written.display(),
    );
    Ok(())
}

fn org_list(json: bool) -> Result<()> {
    let cfg = config::load()?;
    if json {
        println!("{}", serde_json::to_string_pretty(&cfg.orgs)?);
        return Ok(());
    }
    if cfg.orgs.is_empty() {
        println!("No orgs registered. Try `drydock org add <owner>`.");
        return Ok(());
    }
    let rows: Vec<Vec<String>> = cfg
        .orgs
        .iter()
        .map(|org| {
            // A path that will not resolve still deserves its row; sync is
            // what reports the problem in detail.
            let path = org::effective_path(&cfg, org)
                .map(|p| paths::contract(&p))
                .unwrap_or_else(|_| "—".into());
            vec![
                org.resolved_provider().as_str().to_string(),
                org.owner.clone(),
                org.host.clone(),
                path,
                if org.enabled { "yes" } else { "no" }.to_string(),
            ]
        })
        .collect();
    print!(
        "{}",
        report::table(
            &["PROVIDER", "OWNER", "HOST", "PATH", "ENABLED"],
            &[report::Align::Left; 5],
            &rows,
        )
    );
    Ok(())
}

/// The owners the config knows about, for error messages that point at the
/// right spelling. Sorted and deduplicated, since the same owner may sit on
/// more than one host.
fn configured_owners(cfg: &config::Config) -> Vec<String> {
    let mut owners: Vec<String> = cfg.orgs.iter().map(|o| o.owner.clone()).collect();
    owners.sort();
    owners.dedup();
    owners
}

fn org_remove(owner: &str) -> Result<()> {
    let mut cfg = config::load()?;
    // Snapshot the owners before the orgs are drained out of the config —
    // the not-found error below still has to be able to list them.
    let owners = configured_owners(&cfg);
    let (removed, kept): (Vec<config::OrgConfig>, Vec<config::OrgConfig>) =
        cfg.orgs.drain(..).partition(|org| org.owner == owner);
    if removed.is_empty() {
        if owners.is_empty() {
            return Err(anyhow!("No orgs are registered."));
        }
        return Err(anyhow!(
            "No org registered for owner \"{owner}\". Registered owners: {}.",
            owners.join(", ")
        ));
    }
    for org in &removed {
        println!(
            "Removed {} on {} for owner \"{}\"",
            org.resolved_provider().as_str(),
            org.host,
            org.owner
        );
    }
    cfg.orgs = kept;
    config::save(&cfg)?;
    // Saying so is the whole point: `remove` forgets a registration, it does
    // not rm -rf a fleet of checkouts.
    println!("Config only — existing checkouts on disk were not touched.");
    Ok(())
}

async fn org_sync(owner: Option<String>, dry_run: bool, json: bool) -> Result<()> {
    let cfg = config::load()?;
    let targets: Vec<&config::OrgConfig> = match owner {
        Some(name) => {
            let matches: Vec<&config::OrgConfig> =
                cfg.orgs.iter().filter(|o| o.owner == name).collect();
            if matches.is_empty() {
                let owners = configured_owners(&cfg);
                if owners.is_empty() {
                    return Err(anyhow!("No orgs are registered."));
                }
                return Err(anyhow!(
                    "No org registered for owner \"{name}\". Registered owners: {}.",
                    owners.join(", ")
                ));
            }
            matches
        }
        None => {
            if cfg.orgs.is_empty() {
                return Err(anyhow!(
                    "No orgs are registered. Try `drydock org add <owner>`."
                ));
            }
            let enabled: Vec<&config::OrgConfig> = cfg.orgs.iter().filter(|o| o.enabled).collect();
            if enabled.is_empty() {
                return Err(anyhow!(
                    "Every registered org is disabled. Enable one in the config or name an owner."
                ));
            }
            enabled
        }
    };

    for org in targets {
        if dry_run {
            let (plan, _root) = org::plan_only(org, &cfg)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&org_plan_view(&plan))?);
            } else {
                print_org_plan(org, &plan);
            }
        } else {
            // No event channel: the CLI prints one report at the end rather
            // than streaming, and per-repo failures are rows in it, not
            // reasons to abort the rest of the batch.
            let outcomes = org::sync_org(org, &cfg, None).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&outcomes)?);
            } else {
                print_org_report(&outcomes);
            }
        }
    }
    Ok(())
}

/// Show what a sync would do, without doing it. One bucket per line, with
/// orphans labelled as reported-only — the label is the reassurance.
fn print_org_plan(org: &config::OrgConfig, plan: &org::SyncPlan) {
    println!(
        "{} on {} — plan (dry run, nothing was touched)",
        org.owner, org.host
    );
    if !plan.to_clone.is_empty() {
        println!("CLONE {}:", plan.to_clone.len());
        for repo in &plan.to_clone {
            println!("  {}", repo.name);
        }
    }
    if !plan.to_update.is_empty() {
        println!("UPDATE {}:", plan.to_update.len());
        for path in &plan.to_update {
            println!("  {}", paths::contract(path));
        }
    }
    if !plan.orphans.is_empty() {
        println!(
            "ORPHAN {} (on disk, not listed — never touched):",
            plan.orphans.len()
        );
        for path in &plan.orphans {
            println!("  {}", paths::contract(path));
        }
    }
    if !plan.skipped.is_empty() {
        println!("SKIP {}:", plan.skipped.len());
        for skipped in &plan.skipped {
            println!("  {} — {}", skipped.name, skipped.reason);
        }
    }
    if plan.to_clone.is_empty()
        && plan.to_update.is_empty()
        && plan.orphans.is_empty()
        && plan.skipped.is_empty()
    {
        println!("Nothing to do.");
    }
    println!();
}

/// One row per repo plus the counts. Built on `report::table` so the column
/// padding matches every other command's output.
fn print_org_report(outcomes: &[org::SyncOutcome]) {
    if outcomes.is_empty() {
        println!("Nothing to do.");
        return;
    }
    let rows: Vec<Vec<String>> = outcomes
        .iter()
        .map(|o| {
            vec![
                o.name.clone(),
                o.action.label().to_string(),
                o.detail.clone(),
            ]
        })
        .collect();
    print!(
        "{}",
        report::table(
            &["NAME", "ACTION", "DETAIL"],
            &[report::Align::Left; 3],
            &rows,
        )
    );
    let count = |action: org::Action| outcomes.iter().filter(|o| o.action == action).count();
    println!(
        "{} cloned, {} updated, {} current, {} skipped, {} orphans, {} errors",
        count(org::Action::Cloned),
        count(org::Action::Updated),
        count(org::Action::Current),
        count(org::Action::Skipped),
        count(org::Action::Orphaned),
        count(org::Action::Error),
    );
    println!();
}

/// `--json` view of a sync plan. `SyncPlan` stays a plain struct for the
/// engine and tests, so the CLI renders the names and paths it actually
/// means rather than dragging serialization into `org.rs` for one command.
#[derive(serde::Serialize)]
struct OrgPlanView<'a> {
    to_clone: Vec<&'a str>,
    to_update: Vec<String>,
    orphans: Vec<String>,
    skipped: Vec<OrgSkipView<'a>>,
}

#[derive(serde::Serialize)]
struct OrgSkipView<'a> {
    name: &'a str,
    reason: &'a str,
}

fn org_plan_view(plan: &org::SyncPlan) -> OrgPlanView<'_> {
    OrgPlanView {
        to_clone: plan.to_clone.iter().map(|r| r.name.as_str()).collect(),
        to_update: plan.to_update.iter().map(|p| paths::contract(p)).collect(),
        orphans: plan.orphans.iter().map(|p| paths::contract(p)).collect(),
        skipped: plan
            .skipped
            .iter()
            .map(|s| OrgSkipView {
                name: &s.name,
                reason: &s.reason,
            })
            .collect(),
    }
}

/// Walk up from a path looking for a checkout.
fn find_repo_root(start: &Path) -> Option<PathBuf> {
    let mut current = if start.is_dir() {
        start.to_path_buf()
    } else {
        start.parent()?.to_path_buf()
    };
    loop {
        if current.join(".git").exists() {
            return Some(current);
        }
        current = current.parent()?.to_path_buf();
    }
}

/// Work out the group and name for a repo found outside a normal sweep, so a
/// one-off `status` call labels it the same way the table would.
fn split_for_display(cfg: &config::Config, root: &Path) -> (String, String) {
    for scan_root in cfg.root_paths() {
        if let Ok(rel) = root.strip_prefix(&scan_root) {
            let parts: Vec<String> = rel
                .components()
                .map(|c| c.as_os_str().to_string_lossy().to_string())
                .collect();
            return match parts.len() {
                0 => (String::new(), root.display().to_string()),
                1 => (String::new(), parts[0].clone()),
                _ => (parts[0].clone(), parts[1..].join("/")),
            };
        }
    }
    (
        String::new(),
        root.file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| root.display().to_string()),
    )
}
