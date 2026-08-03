mod cache;
mod cli;
mod config;
mod discover;
mod filter;
mod fmt;
mod git;
mod model;
mod paths;
mod probe;
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
use probe::Tier;

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
        Some(Commands::Scan { fast, no_cache }) => cmd_scan(fast, no_cache).await,
        Some(Commands::Groups { json }) => cmd_groups(json).await,
        Some(Commands::Config(c)) => cmd_config(c),
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

async fn gather(cfg: Arc<config::Config>, tier: Tier, no_cache: bool) -> Result<Vec<RepoStatus>> {
    if no_cache {
        let _ = cache::clear();
    }
    let fleet = probe::sweep(cfg, tier, None).await?;
    print_scan_note(&fleet.timings);
    Ok(fleet.repos)
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

async fn cmd_list(args: ListArgs) -> Result<()> {
    let cfg = load_config();
    let query = build_query(&args)?;

    let repos: Vec<RepoStatus> = if args.cached {
        let mut repos: Vec<RepoStatus> = cache::load().into_values().collect();
        repos.sort_by(|a, b| a.root.cmp(&b.root));
        if repos.is_empty() {
            eprintln!("drydock: no cache yet, run `drydock scan` first");
        }
        repos
    } else {
        let tier = if args.fast { Tier::Refs } else { Tier::Full };
        gather(cfg, tier, args.no_cache).await?
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
        print!("{}", report::list_table(&selected, now, args.paths));
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
    let status = probe::probe_one(&discovered, &cfg, cached.get(&root), Tier::Full).await;

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
    let repos = gather(cfg, Tier::Full, false).await?;
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

async fn cmd_scan(fast: bool, no_cache: bool) -> Result<()> {
    let cfg = load_config();
    if no_cache {
        let _ = cache::clear();
    }
    let tier = if fast { Tier::Refs } else { Tier::Full };
    let fleet = probe::sweep(cfg, tier, None).await?;
    println!("{}", report::summary(&fleet.repos, Some(&fleet.timings)));
    if fast {
        println!("Working trees were not scanned (--fast).");
    }
    Ok(())
}

async fn cmd_groups(json: bool) -> Result<()> {
    let cfg = load_config();
    let repos = gather(cfg, Tier::Full, false).await?;
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
