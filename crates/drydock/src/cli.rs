//! The command line. With no subcommand the dashboard opens, matching the
//! reeve and ytunnel convention.

use clap::{Args, Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "drydock",
    version,
    about = "What's uncommitted, unpushed, and unreleased across every repo you own.",
    long_about = None
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Print the fleet as a table (or JSON), then exit.
    List(ListArgs),

    /// Show everything known about one repo. Defaults to the current directory.
    Status {
        /// Repo path. Defaults to the current directory.
        path: Option<String>,
        #[arg(long)]
        json: bool,
    },

    /// Repos with commits since their last tag, newest activity first.
    Releasable {
        /// Only show repos at least this many commits past their last tag.
        #[arg(long, default_value_t = 1)]
        min_commits: u32,
        /// Include repos whose changelog is ahead of their newest tag even when
        /// no commits follow the tag.
        #[arg(long)]
        include_changelog: bool,
        #[arg(long)]
        json: bool,
    },

    /// Walk the roots, probe everything, and refresh the cache.
    Scan {
        /// Skip working-tree scans. Much faster, but no change counts.
        #[arg(long)]
        fast: bool,
        /// Ignore the existing cache and re-probe everything.
        #[arg(long)]
        no_cache: bool,
        /// Fetch every repo with a remote first, so "behind" counts reflect
        /// the remotes rather than your last fetch. Network traffic against
        /// every remote you own.
        #[arg(long)]
        fetch: bool,
    },

    /// Group summary: repo counts and how many need attention in each.
    Groups {
        #[arg(long)]
        json: bool,
    },

    /// Register an owner, list registrations, or sync a fleet of checkouts.
    #[command(subcommand)]
    Org(OrgCommands),

    /// Show where config and cache live, or write a starter config.
    #[command(subcommand)]
    Config(ConfigCommands),

    /// Render the dashboard once to plain text. Useful for checking layout
    /// without a terminal.
    #[command(hide = true)]
    TuiSnapshot {
        #[arg(long, default_value_t = 140)]
        width: u16,
        #[arg(long, default_value_t = 40)]
        height: u16,
        /// Which overlay to render: none, help, detail, or columns.
        #[arg(long, default_value = "none")]
        view: String,
    },
}

#[derive(Subcommand, Debug)]
pub enum ConfigCommands {
    /// Print the config and cache paths.
    Path,
    /// Write a config file containing the current defaults.
    Init {
        /// Overwrite an existing config.
        #[arg(long)]
        force: bool,
    },
    /// Print the effective config.
    Show,
}

/// One registered source: an instance plus an owner, remembered so its
/// checkouts can be brought up to date on demand. Mirrors `ConfigCommands` —
/// a handful of small verbs under one roof.
#[derive(Subcommand, Debug)]
pub enum OrgCommands {
    /// Remember an owner so `org sync` can keep its checkouts current.
    Add {
        /// The organization or user to list — both list the same way.
        owner: String,
        /// github, gitlab, or gitea. Inferred from your authenticated CLI
        /// when omitted.
        #[arg(long)]
        provider: Option<String>,
        /// Instance hostname. Inferred from your authenticated CLI when
        /// omitted.
        #[arg(long)]
        host: Option<String>,
        /// Where the checkouts live. Defaults to the first configured root
        /// plus the owner.
        #[arg(long)]
        path: Option<String>,
        /// Gitea only: which `tea` login to use. Empty means tea's default.
        #[arg(long)]
        login: Option<String>,
        /// How to clone: ssh or https. Defaults to ssh.
        #[arg(long)]
        protocol: Option<String>,
        /// Also sync forked repos.
        #[arg(long)]
        include_forks: bool,
        /// Also sync archived repos.
        #[arg(long)]
        include_archived: bool,
    },
    /// List the registered owners.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Forget an owner. Config only — checkouts on disk are left alone.
    Remove {
        /// Every registration with this owner is removed.
        owner: String,
    },
    /// Clone what is missing and fast-forward what is present, one repo at a
    /// time. Orphans are reported, never deleted.
    Sync {
        /// Which owner to sync. Omitted means every enabled registration.
        owner: Option<String>,
        /// Print the plan without cloning or updating anything.
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Args, Debug, Clone)]
pub struct ListArgs {
    /// Repos with uncommitted changes.
    #[arg(long)]
    pub dirty: bool,
    /// Repos with commits their upstream doesn't have.
    #[arg(long)]
    pub unpushed: bool,
    /// Repos that have never been released: no tags at all.
    #[arg(long)]
    pub unreleased: bool,
    /// Repos with commits or uncommitted changes past their last tag.
    #[arg(long)]
    pub needs_release: bool,
    /// Repos tagged with nothing since.
    #[arg(long)]
    pub released: bool,
    /// Repos whose upstream is ahead. Only as fresh as your last fetch, so
    /// pair it with --fetch to check the remotes now.
    #[arg(long)]
    pub behind: bool,
    /// Repos with merge conflicts.
    #[arg(long)]
    pub conflicted: bool,
    /// Repos with a merge, rebase or cherry-pick in progress.
    #[arg(long)]
    pub in_progress: bool,
    /// Repos on a detached HEAD.
    #[arg(long)]
    pub detached: bool,
    /// Repos with no remote configured.
    #[arg(long)]
    pub no_remote: bool,
    /// Repos whose current branch has no upstream.
    #[arg(long)]
    pub no_upstream: bool,
    /// Repos with stash entries.
    #[arg(long)]
    pub stashed: bool,
    /// Repos with nothing outstanding.
    #[arg(long)]
    pub clean: bool,
    /// Repos that could not be probed.
    #[arg(long)]
    pub errored: bool,
    /// Repos checked and found public. Only meaningful with visibility.enabled.
    #[arg(long)]
    pub public: bool,
    /// Repos checked and found private or internal.
    #[arg(long)]
    pub private: bool,

    /// Add a filter by name, repeatable.
    #[arg(long = "filter", value_name = "NAME")]
    pub filters: Vec<String>,

    /// Whether several filters widen (`any`) or narrow (`all`) the result.
    #[arg(long, default_value = "any", value_name = "any|all")]
    pub match_mode: String,

    /// Only repos touched within this window, e.g. 1h, 1d, 1w, 1mo.
    #[arg(long, value_name = "DURATION")]
    pub since: Option<String>,

    /// Only repos in this group (the folder directly under the scan root).
    #[arg(long, short)]
    pub group: Option<String>,

    /// Fuzzy match against group, name and branch.
    #[arg(long, short = 'S', value_name = "TEXT")]
    pub search: Option<String>,

    /// Sort key: activity, name, group, changes, unpushed, behind, since-tag,
    /// state, visibility.
    #[arg(long, default_value = "activity")]
    pub sort: String,

    /// Reverse the sort.
    #[arg(long, short)]
    pub reverse: bool,

    /// Show at most this many rows.
    #[arg(long, short = 'n')]
    pub limit: Option<usize>,

    /// Machine-readable output.
    #[arg(long)]
    pub json: bool,

    /// Skip working-tree scans. Much faster, but no change counts.
    #[arg(long)]
    pub fast: bool,

    /// Ignore the existing cache and re-probe everything.
    #[arg(long)]
    pub no_cache: bool,

    /// Print the last known state without probing anything. Instant, and as
    /// stale as your last scan.
    #[arg(long)]
    pub cached: bool,

    /// Fetch every repo with a remote before probing, so "behind" counts
    /// reflect the remotes rather than your last fetch. Network traffic
    /// against every remote you own, bounded by `remote.concurrency`.
    #[arg(long, conflicts_with = "cached")]
    pub fetch: bool,

    /// Print full paths instead of group/name.
    #[arg(long)]
    pub paths: bool,
}
