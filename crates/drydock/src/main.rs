mod cache;
mod cli;
mod column;
mod config;
mod discover;
mod filter;
mod fmt;
mod gh;
mod git;
mod hold;
mod model;
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

use cli::{Cli, Commands, ConfigCommands, ListArgs};
use filter::{Filter, MatchMode, Query, Sort};
use model::RepoStatus;
use probe::{Fetch, Tier};

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    init_tracing(cli.command.is_none());
    // Before anything reads or writes the cache, so a `--root` run doesn't
    // overwrite the fleet's.
    paths::set_cache_namespace(&cli.roots);
    let roots = cli.roots;

    match cli.command {
        None => tui::run(roots).await,
        Some(Commands::List(args)) => cmd_list(args, &roots).await,
        Some(Commands::Status { path, json }) => cmd_status(path, json, &roots).await,
        Some(Commands::Releasable {
            min_commits,
            include_changelog,
            json,
        }) => cmd_releasable(min_commits, include_changelog, json, &roots).await,
        Some(Commands::Scan {
            fast,
            no_cache,
            fetch,
        }) => cmd_scan(fast, no_cache, fetch, &roots).await,
        Some(Commands::Groups { json }) => cmd_groups(json, &roots).await,
        Some(Commands::Hold { path, note }) => cmd_hold(path, note, &roots).await,
        Some(Commands::Unhold { path }) => cmd_unhold(path, &roots).await,
        Some(Commands::Holds { prune, json }) => cmd_holds(prune, json, &roots).await,
        Some(Commands::Config(c)) => cmd_config(c, &roots),
        Some(Commands::TuiSnapshot {
            width,
            height,
            view,
        }) => {
            print!("{}", tui::snapshot(width, height, &view, roots).await?);
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
fn load_config(roots: &[String]) -> Arc<config::Config> {
    let (cfg, warning) = config::load_or_default(roots);
    if let Some(warning) = warning {
        eprintln!("drydock: using defaults, config could not be read: {warning}");
    }
    for root in config::missing_roots(roots) {
        eprintln!("drydock: --root {root} is not a directory");
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
        (args.held, Filter::Held),
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

async fn cmd_list(args: ListArgs, roots: &[String]) -> Result<()> {
    let cfg = load_config(roots);
    let query = build_query(&args)?;
    // Resolved before `cfg` is handed to the sweep, which takes ownership.
    let columns = cfg.columns();

    let repos: Vec<RepoStatus> = if args.cached {
        let mut repos: Vec<RepoStatus> = cache::load().into_values().collect();
        repos.sort_by(|a, b| a.root.cmp(&b.root));
        // The cache carries whatever holds were on these rows when it was
        // written, which is one `drydock hold` out of date the moment one is
        // placed. The file is the authority, so re-stamp from it.
        hold::apply(&mut repos, &hold::load());
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

async fn cmd_status(path: Option<String>, json: bool, roots: &[String]) -> Result<()> {
    let cfg = load_config(roots);
    let root = resolve_repo(path)?;

    let (group, name) = split_for_display(&cfg, &root);
    let discovered = discover::Discovered {
        root: root.clone(),
        group,
        name,
    };
    let cached = cache::load();
    // Forced: one repo asked about by name is worth the scan, and a cached
    // "clean" for a tree the caller just edited is the wrong answer.
    let mut status = probe::probe_one(&discovered, &cfg, cached.get(&root), Tier::Full, true).await;
    hold::apply(std::slice::from_mut(&mut status), &hold::load());

    let now = git::now_unix();
    if json {
        println!("{}", report::detail_json(&status, now)?);
    } else {
        print!("{}", report::detail(&status, now));
    }
    Ok(())
}

async fn cmd_releasable(
    min_commits: u32,
    include_changelog: bool,
    json: bool,
    roots: &[String],
) -> Result<()> {
    let cfg = load_config(roots);
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

async fn cmd_scan(fast: bool, no_cache: bool, fetch: bool, roots: &[String]) -> Result<()> {
    let cfg = load_config(roots);
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

async fn cmd_groups(json: bool, roots: &[String]) -> Result<()> {
    let cfg = load_config(roots);
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

/// Find a repo the way `status` does, so every command that takes a path
/// agrees about what "." means and about which root a hold is filed under.
fn resolve_repo(path: Option<String>) -> Result<PathBuf> {
    let start = match path {
        Some(p) => paths::expand(&p),
        None => std::env::current_dir().context("Reading the current directory")?,
    };
    let start = start.canonicalize().unwrap_or(start);
    find_repo_root(&start)
        .ok_or_else(|| anyhow!("No git repo at or above {}", paths::contract(&start)))
}

/// Probe one repo's refs. Tier 1 only: a hold is about commits and tags, and
/// nothing here needs a working-tree scan.
async fn probe_refs_only(cfg: &config::Config, root: &Path) -> RepoStatus {
    let (group, name) = split_for_display(cfg, root);
    let discovered = discover::Discovered {
        root: root.to_path_buf(),
        group,
        name,
    };
    probe::probe_one(&discovered, cfg, None, Tier::Refs, true).await
}

async fn cmd_hold(path: Option<String>, note: Option<String>, roots: &[String]) -> Result<()> {
    let cfg = load_config(roots);
    let root = resolve_repo(path)?;
    let status = probe_refs_only(&cfg, &root).await;

    let Some(sha) = status.head_sha().map(|s| s.to_string()) else {
        return Err(anyhow!("{} has no commits to hold", paths::contract(&root)));
    };
    // Holding a repo with nothing past its tag would place something that can
    // never suppress anything: the first commit to make it releasable is also
    // the one that lifts the hold.
    if status.release_state_raw() == model::ReleaseState::Released {
        println!(
            "{} is released, with nothing past {}. Nothing to hold.",
            status.slug(),
            status.tag_label()
        );
        return Ok(());
    }

    let mut holds = hold::load();
    let previous = holds.get(&root).cloned();
    holds.set(
        &root,
        hold::Hold {
            sha: sha.clone(),
            branch: status
                .refs
                .as_ref()
                .and_then(|r| r.head.branch().map(|b| b.to_string())),
            tag: status.refs.as_ref().and_then(|r| {
                r.described_tag
                    .as_ref()
                    .or(r.newest_tag.as_ref())
                    .map(|t| t.name.clone())
            }),
            at: git::now_unix(),
            note,
        },
    );
    let file = hold::save(&holds)?;

    let what = match previous {
        Some(prev) if prev.sha == sha => "Still holding",
        Some(_) => "Re-held at the current commit:",
        None => "Held",
    };
    println!(
        "{what} {} at {} ({} past {}).",
        status.slug(),
        sha,
        commits_note(status.commits_since_tag()),
        status.tag_label()
    );
    println!("The next commit lifts it. Written to {}", file.display());
    Ok(())
}

fn commits_note(count: u32) -> String {
    format!("{count} commit{}", if count == 1 { "" } else { "s" })
}

async fn cmd_unhold(path: Option<String>, _roots: &[String]) -> Result<()> {
    let root = resolve_repo(path)?;
    let mut holds = hold::load();
    match holds.remove(&root) {
        Some(held) => {
            hold::save(&holds)?;
            println!(
                "Lifted the hold on {} (placed at {}).",
                paths::contract(&root),
                held.label()
            );
        }
        None => println!("Nothing is holding {}.", paths::contract(&root)),
    }
    Ok(())
}

/// What one held repo is doing right now. Probing is the only way to know
/// whether a hold still covers HEAD, and there are only ever a handful of
/// these, so each one is asked directly rather than run through a sweep.
struct HeldRepo {
    root: PathBuf,
    hold: hold::Hold,
    /// `None` when the repo is no longer on disk.
    status: Option<RepoStatus>,
}

impl HeldRepo {
    fn active(&self) -> bool {
        match &self.status {
            Some(status) => self.hold.covers(status.head_sha()),
            None => false,
        }
    }

    fn state(&self) -> &'static str {
        match &self.status {
            None => "repo is gone",
            Some(status) if self.hold.covers(status.head_sha()) => "holding",
            Some(_) => "lifted, HEAD moved",
        }
    }
}

async fn cmd_holds(prune: bool, json: bool, roots: &[String]) -> Result<()> {
    let cfg = load_config(roots);
    let holds = hold::load();
    if holds.is_empty() {
        if json {
            println!("[]");
        } else {
            println!("Nothing is held. Press h in the dashboard, or run `drydock hold <path>`.");
        }
        return Ok(());
    }

    let mut held: Vec<HeldRepo> = Vec::new();
    for (root, hold) in holds.iter() {
        let status = if root.is_dir() {
            Some(probe_refs_only(&cfg, root).await)
        } else {
            None
        };
        held.push(HeldRepo {
            root: root.clone(),
            hold: hold.clone(),
            status,
        });
    }

    let now = git::now_unix();
    if json {
        #[derive(serde::Serialize)]
        struct Row<'a> {
            path: &'a Path,
            sha: &'a str,
            branch: Option<&'a str>,
            tag: Option<&'a str>,
            at: i64,
            note: Option<&'a str>,
            active: bool,
            state: &'a str,
            head_sha: Option<&'a str>,
        }
        let rows: Vec<Row> = held
            .iter()
            .map(|h| Row {
                path: &h.root,
                sha: &h.hold.sha,
                branch: h.hold.branch.as_deref(),
                tag: h.hold.tag.as_deref(),
                at: h.hold.at,
                note: h.hold.note.as_deref(),
                active: h.active(),
                state: h.state(),
                head_sha: h.status.as_ref().and_then(|s| s.head_sha()),
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }

    let rows: Vec<Vec<String>> = held
        .iter()
        .map(|h| {
            vec![
                h.status
                    .as_ref()
                    .map(|s| s.slug())
                    .unwrap_or_else(|| paths::contract(&h.root)),
                h.hold.branch.clone().unwrap_or_else(|| "·".into()),
                h.hold.tag.clone().unwrap_or_else(|| "·".into()),
                h.hold.sha.clone(),
                fmt::age(h.hold.at, now),
                h.state().to_string(),
                h.hold.note.clone().unwrap_or_else(|| "·".into()),
            ]
        })
        .collect();

    print!(
        "{}",
        report::table(
            &["REPO", "BRANCH", "TAG", "COMMIT", "HELD", "STATE", "NOTE"],
            &[
                report::Align::Left,
                report::Align::Left,
                report::Align::Left,
                report::Align::Left,
                report::Align::Right,
                report::Align::Left,
                report::Align::Left,
            ],
            &rows,
        )
    );

    let live = held.iter().filter(|h| h.active()).count();
    let spent = held.len() - live;
    println!();
    println!(
        "{live} holding, {spent} lifted{}.",
        if spent > 0 && !prune {
            " — `drydock holds --prune` forgets the lifted ones"
        } else {
            ""
        }
    );

    if prune && spent > 0 {
        let mut holds = holds;
        let keep: Vec<PathBuf> = held
            .iter()
            .filter(|h| h.active())
            .map(|h| h.root.clone())
            .collect();
        holds.retain(|path, _| keep.iter().any(|k| k == path));
        hold::save(&holds)?;
        println!("Pruned {spent}.");
    }
    Ok(())
}

fn cmd_config(command: ConfigCommands, roots: &[String]) -> Result<()> {
    match command {
        ConfigCommands::Path => {
            println!("config  {}", paths::config_file()?.display());
            println!("holds   {}", paths::holds_file()?.display());
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
            let (cfg, warning) = config::load_or_default(roots);
            if let Some(warning) = warning {
                eprintln!("drydock: showing defaults, config could not be read: {warning}");
            }
            print!("{}", toml::to_string_pretty(&cfg)?);
            Ok(())
        }
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
