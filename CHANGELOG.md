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
