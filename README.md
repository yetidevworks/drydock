# drydock

What's uncommitted, unpushed, and unreleased across every repo you own.

A drydock is where vessels sit while work is done on them, before they go back
out. If you keep a few hundred checkouts on disk and lose track of which ones
have work sitting in them, this tells you, in one screen, live.

```
 drydock  ~/Projects  557 repos · 134 dirty · 26 unpushed · 193 unreleased · live
 filter  dirty  any  unpushed   since 1w  sort activity
┌ showing 17 of 557 ────────────────────────────────────────────────────────────────────────┐
│GROUP         REPO                     BRANCH             STATE        CHANGES  AHEAD  +TAG│
│skulkworks    magpie                   main               ● dirty      +1 ~12 ?8    ·     1│
│grav          grav-skeleton-sandbox    master             ● dirty      +2 ~11 ?12   3     ·│
│grav          grav-plugin-form         develop            ● dirty      ~5           ·     1│
│skulkworks    vectorfox-mas            mas                ↑ unpushed   ·            1    11│
│grav          grav                     develop            ◆ unreleased ·            ·     6│
└───────────────────────────────────────────────────────────────────────────────────────────┘
 j/k move  ⏎ detail  d dirty  u unpushed  r unreleased  a clear  s sort  / search  ? help
```

## What it answers

- **What did I touch recently?** Filter to the last hour, day, week or month.
  A clean repo's activity is its newest commit; a dirty repo's is when you last
  saved a file, which is usually much more recent.
- **What have I not committed?** Staged, unstaged, untracked and conflicted
  counts, per repo.
- **What have I not pushed?** Across every local branch, not just the one that
  happens to be checked out.
- **What's worth releasing?** Commits since the last tag, with the actual commit
  subjects, plus whether `CHANGELOG.md` has run ahead of the newest tag.

## Install

```sh
make install          # into ~/.local/bin
# or
cargo install --path crates/drydock
```

## Use

```sh
drydock                                   # the live dashboard
drydock list --dirty --since 1d           # a table, then exit
drydock list --unpushed --group grav --json
drydock releasable --min-commits 3        # what's worth a release pass
drydock status .                          # everything about one repo
drydock scan                              # refresh the cache
drydock groups                            # per-group tallies
drydock config init                       # write a config file
```

`drydock list --cached` prints the last known state with no probing at all,
which is instant and suitable for a status line.

### Filters

`--dirty`, `--unpushed`, `--unreleased`, `--behind`, `--conflicted`,
`--in-progress`, `--detached`, `--no-remote`, `--no-upstream`, `--stashed`,
`--clean`, `--errored`.

Several filters **widen** the result by default: `--dirty --unpushed` means
"either". Pass `--match all` (or press `&` in the dashboard) to require all of
them instead.

## How it works, and why it's fast

Roughly 550 repos, warm cache, on an M-series Mac:

| | |
|---|---|
| Walk the tree | ~0.9s |
| Refs, tags, tracking (tier 1) | ~1.4s |
| Full sweep, working trees from cache | **~2.2s** |
| Full sweep, cold | ~6.2s |
| `list --cached` | ~5ms |

The split matters. Reading refs and tags is cheap. Scanning working trees is
not: `git status` across that many repos costs about 40 seconds of syscall time,
which is ~85% of the total. So:

- **Tier 1** (refs, tags, tracking, stash count, in-progress operations) runs on
  every sweep.
- **Tier 2** (the working-tree scan) is cached against HEAD and the index mtime,
  and only reruns where something actually moved.
- A **filesystem watcher** then re-probes individual repos as they change, which
  costs milliseconds. That's what makes leaving the dashboard open all day
  reasonable rather than a background CPU tax.

Two implementation notes worth knowing:

- It shells out to `git` rather than linking a git library, so the numbers match
  exactly what you see on the command line, including whatever per-repo config
  is in play.
- Every invocation passes `--no-optional-locks`. Without it, polling hundreds of
  repos would take `index.lock` and rewrite indexes constantly, fighting your
  editor and any GUI client you have open.

### Ahead, behind, and the network

Ahead and behind counts come from remote-tracking refs you have already fetched,
so no network access is involved and they are safe to recompute constantly. That
also means **"behind" is only as fresh as your last fetch**.

Fetching is off by default, because it is real traffic against every remote you
own and a remote that wants credentials can hang. Press `f` to fetch the
selected repo or `F` to fetch everything on screen, or set `remote.fetch = true`
to have it happen on a timer.

## Configuration

`drydock config init` writes commented defaults to
`~/Library/Application Support/drydock/config.toml` (`~/.config/drydock` on
Linux). The cache lives under `~/Library/Caches/drydock`.

Highlights:

```toml
roots = ["~/Projects"]     # each immediate subdirectory becomes a "group"
max_depth = 4
follow_nested_repos = false # keeps submodules and vendored checkouts out
exclude = ["riffle-testbed/**"]

[refresh]
interval = "5m"            # backstop sweep; the watcher handles the rest
watch = true
debounce = "1s"

[release]
# Requires a digit, so marker tags like `latest` aren't mistaken for releases.
tag_pattern = "*[0-9]*"

[remote]
fetch = false              # see "Ahead, behind, and the network" above

[ui]
editor_command = ["zed", "{path}"]
git_client_command = ["open", "-a", "Tower", "{path}"]
```

### A note on tags and git-flow

With git-flow, tags land on `master` while work carries on on `develop`, so the
newest tag by date often isn't an ancestor of `HEAD`. drydock reports the nearest
*reachable* tag as the primary number and flags the discrepancy rather than
quietly picking one. A tag shown in parentheses, like `(1.0.9)`, means the newest
tag isn't reachable from the current branch.

## Licence

MIT
