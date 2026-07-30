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
