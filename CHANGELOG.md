# 0.1.8

## 08/26/2026

1. [](#new)
    * Config and cache locations now honour `XDG_CONFIG_HOME` and `XDG_CACHE_HOME`, and on macOS an existing `~/.config/drydock` is used in preference to `~/Library/Application Support`. Nothing moves on its own: a directory nobody created is never chosen, so anyone who hasn't asked for this keeps the platform default. Config and cache resolve independently, so `~/.config/drydock` with no `~/.cache/drydock` puts the config where you want it and leaves the cache where macOS expects it.
2. [](#improved)
    * The dashboard's columns are now configurable, and the `C` key opens a picker to set them: space toggles a column, `J`/`K` reorder, `a` resets, `esc` saves to `[ui] columns`. The plain `drydock list` table reads the same setting. Every column's header, alignment, width and cell renderer now live together in one place rather than in two parallel `const` arrays plus a struct of widths that had to be edited in lockstep.
    * The VISIBILITY column is now shown only when `visibility.enabled` is on. It was unconditional, which cost fifteen fixed characters of table width on every row whether or not checking was happening — enough to cut repo names to four characters at 120 columns — while every cell read "checking off".
3. [](#bugfix)
    * The VISIBILITY column no longer claims `no remote configured` for a repo that couldn't be read at all. That is a fact about a repo which *was* read, and asserting it for one that wasn't is the kind of unchecked claim the column is built to avoid. Those now report `unknown`.
    * GitHub remotes are now parsed rather than prefix-matched, so several ordinary shapes stop being reported as `unsupported`: `ssh://git@ssh.github.com:443/…` (the port-443 endpoint for networks that block 22), an explicit port, `git://`, credentials embedded in an https URL, and a trailing slash. A URL deeper than `owner/repo` is rejected rather than guessed at.
    * Bare repos no longer report as errored. `git status` in a bare repo can only ever fail — "this operation must be run in a work tree" — and recording that failure made a repo behaving exactly as intended render as `error` on every sweep. The working-tree scan is skipped outright for bare repos now, and they report a `bare` state instead.
    * A bare repo's worktrees are now found without turning on `follow_nested_repos`. In the common bare-plus-worktrees layout the bare repo is what discovery hits first, and pruning there hid the worktrees — the only things in the layout with a working tree to report on. A bare repo has no working tree, so nothing can be nested inside one, which is the only case that setting is guarding against.

# 0.1.7

## 08/21/2026

1. [](#improved)
    * The branch column now sizes itself to the branch names in the filtered list, and the repo name column keeps the rest. Nearly every repo sits on `main` or `develop`, so the old fixed 55/45 split of the leftover space left a wide strip of empty space beside truncated repo names. Sizing is to a high percentile rather than the longest name, so a stray `codex/some-long-experiment` truncates instead of costing every row.

# 0.1.6

## 08/13/2026

1. [](#bugfix)
    * Fixed a repo with unstaged edits showing as clean indefinitely. The
      working-tree scan is skipped whenever HEAD and `.git/index` both match
      the cached probe, and editing a tracked file touches neither, so a repo
      edited but never staged kept reporting the counts from its last scan —
      across restarts, since the cache is on disk. Repos flagged by the watcher
      now always rescan, `drydock status` never accepts a cached answer, and
      cached counts expire after an hour by default (`status.max_age`).

# 0.1.5

## 08/03/2026

1. [](#bugfix)
    * Fixed the dashboard being pushed off the top of the screen, taking the
      title and filter bars with it, until the window was resized. Warnings
      were logged to stderr, which is not redirected while the dashboard owns
      the terminal, so any one of them printed straight over the frame and
      scrolled it. Under the dashboard the log now goes to `drydock.log` in the
      cache directory, and the one warning you are likely to hit is shown in
      the status bar as well. Every other command still logs to stderr.

# 0.1.4

## 08/03/2026

1. [](#bugfix)
    * Fixed the dashboard silently stopping its periodic sweeps. A sweep that
      failed before it started walking never reported that it had finished, and
      the dashboard refuses to start a sweep while it believes one is running,
      so a single failure wedged every sweep after it for the life of the
      process. A dashboard left open for days sat on the data it started with,
      with nothing on screen to say so. A failed sweep now always reports back,
      and a sweep still claiming to be in flight after three minutes is
      presumed dead so the next one runs regardless.

1. [](#improved)
    * The status bar says how long ago the last sweep was, not just how long it
      took, so stale data is visible instead of looking exactly like fresh data.

# 0.1.3

## 07/30/2026

1. [](#new)
    * Release state is now its own axis with three values, in its own `RELEASE`
      column: `unreleased` (no tags at all), `released` (tagged with nothing
      since), and `needs release` (commits or uncommitted changes past the tag).
      It sits alongside the working state rather than competing with it, so a
      repo can be dirty and released, or clean and still needing a release.
    * New `--needs-release` and `--released` filters, and `N` in the dashboard
      for never-released repos.
    * Mouse support in the dashboard. Click a row to select it, and the wheel
      moves the selection the same way `j` and `k` do, so the selected row
      never scrolls off screen. Over the detail view the wheel scrolls the
      pane instead.

1. [](#improved)
    * Tags left over from a repo's previous life no longer count as releases.
      A theme rewritten from scratch keeps its old tags pointing at commits no
      branch can reach any more, and those were reading as a release the
      current work had run past. When no branch anywhere can reach the newest
      tag, the repo reads as never released. Shallow clones are exempt, since
      their history is truncated and the check would be wrong.
    * `--unreleased` and the `r` key changed meaning. `--unreleased` now means
      never tagged, and `r` toggles needs-release, which is the actionable one.
    * `groups` gained a `NEEDS RELEASE` column, and JSON output gained a
      `release_state` field.

# 0.1.2

## 07/30/2026

1. [](#bugfix)
    * A merge commit that is the only thing following a tag, and that genuinely
      changes the tree, is no longer reported as released. Discounting merges
      fixes the git-flow back-merge, but on its own it could hide work that
      still needed releasing.

# 0.1.1

## 07/30/2026

1. [](#bugfix)
    * Git-flow back-merges no longer count as unreleased work. After a release,
      `develop` carries a "Merge tag 'x.y.z' into develop" commit the tag cannot
      reach, which reported one commit to release when there was nothing to
      release. Merge commits are now excluded from commits-since-tag.

# 0.1.0

## 07/30/2026

1. [](#new)
    * Initial release.
    * Live TUI dashboard over every git repo under your scan roots, with
      composable filters, sorting, fuzzy search, group scoping and a detail view.
    * Two-tier probing: refs and tags on every sweep, working-tree scans cached
      against HEAD and the index so they only rerun when something moved.
    * Filesystem watcher re-probes individual repos as they change, so the
      dashboard stays current without repeated full sweeps.
    * Release intelligence: commits since the nearest reachable tag with their
      subjects, git-flow aware tag reporting, and detection of `CHANGELOG.md`
      blocks that have stacked up above the newest tag.
    * `list`, `status`, `releasable`, `scan`, `groups` and `config` commands,
      all with `--json` for scripting, plus `list --cached` for instant output.
    * Optional, off-by-default periodic `git fetch` so behind counts can be kept
      fresh, with `f` and `F` for on-demand fetches.
