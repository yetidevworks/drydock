# 1.1.2

## 09/03/2026

1. [](#bugfix)
    * **AHEAD and BEHIND now report the branch you have checked out**, the one BRANCH names in the same row, rather than every local branch summed. A topic branch abandoned a month ago and left tracking `origin/master` made a perfectly up-to-date `master` read as 128 behind: a number true of the repo and false of every branch the row mentioned. On my own tree that was 14 rows of 580, one of them claiming 1391 behind on a branch that was in sync. Thanks to @nick4eva for the report and the fix.
    * When another local branch is ahead or behind on its own, the cell picks up a `*`, so `·*` is "this branch is in sync, some other one isn't". Select the repo to see which, in the per-branch table. Only the checked-out branch's own count colours the cell.
2. [](#improved)
    * `--unpushed`, `--behind`, both sort keys and the header counts still work from the repo-wide totals. "Does this repo need attention anywhere" is the right question for a filter, even where it's the wrong one for a column.
    * `--json` gains `branch_ahead` and `branch_behind`, the per-branch pair the columns show. `ahead` and `behind` keep the repo-wide meaning they have always had: 1.0 promised those fields wouldn't change incompatibly without a major version, and quietly redefining a number is the one break a script can't notice.

# 1.1.1

## 09/01/2026

1. [](#new)
    * **A `STASH` column**, off by default, showing how many stash entries are parked on each repo. The count was already read on every sweep — stash entries are reflog lines, so it's the line count of `logs/refs/stash` and no process is spawned for it — and it was already in the detail pane, `--json` and the `--stashed` filter. The one thing you couldn't do was see it across the fleet without selecting each repo in turn. Turn it on with `C`, or `"stashes"` in `[ui] columns`. It's off by default because most repos have no stashes, and a column of `·` still charges six characters of every row to the repo name.
    * A `stashes` sort key, so `s` in the dashboard and `--sort stashes` bring the deepest piles to the top. It works whether or not the column is showing, which is the point if you'd rather not spend the width: press `s` when you want to find parked work, and leave the table as it was the rest of the time.
2. [](#improved)
    * `STASH` reads `?` for a repo nothing has probed yet and `·` for one with no stashes, the same distinction `CHANGES` and `BEHIND` already draw. A bare repo gets the real zero rather than the shrug — its stash reflog is read off disk like anyone else's and simply isn't there.

# 1.1.0

## 08/30/2026

1. [](#new)
    * **`--fetch` on `drydock list` and `drydock scan`.** Behind counts are read from remote-tracking refs, so they were only ever as fresh as your last fetch, and nothing outside the dashboard could refresh them — `drydock list --behind` reported whatever was on disk. `--fetch` runs a network phase before anything is probed, bounded by `remote.concurrency` and capped per repo by `remote.timeout`, and says how many repos it reached. On `list` it narrows to `--group` when you pass one, so `--group acme --behind --fetch` is thirty fetches rather than five hundred.
    * **`ctrl-f` fetches the whole fleet** in the dashboard. `F` fetches what's on screen, which is bounded by what you're looking at — but the repos worth knowing about are the ones you *aren't* looking at, filtered out or scrolled past. `ctrl-f` is to the remotes what `ctrl-r` is to the disk, and it's in the footer beside it.
    * A `FETCHED` column, off by default, showing how long since each repo last heard from its remote, and `never` when it never has. The detail view says the same for the repo you're on. Both read git's own `FETCH_HEAD`, so a `git pull` you ran yourself in a terminal counts. `fetched_at` and `never_fetched` are in `--json` too.
    * **`o` shows a repo's folder in Finder**, with the editor moved to `O` — `o` is the one you reach for most. `ctrl-o` opens a terminal there, the same as `T`. The new `[ui] file_manager_command` is a `{path}` template like the other three, so `open -R {path}` reveals the folder in its parent instead, and it's whatever you use on a machine that isn't a Mac.
    * **The footer hints follow the modifier you're holding.** Hold shift and `o finder` becomes `O editor`, alongside `T terminal`, `F fetch screen` and the rest of the uppercase row; hold ctrl and you get `^o terminal`, `^f fetch all`, `^r rescan`, `^d/^u half page`, `^c quit`. Needs a terminal that reports a bare modifier — the kitty keyboard protocol, so kitty, Ghostty, WezTerm, iTerm2 3.5+, foot — and anywhere else the footer stays exactly as it was. Turning that protocol on also means a shifted letter can arrive as its unshifted codepoint with a flag beside it, and that key repeats stop counting as presses, so both are folded back into the shape the bindings are written in.
    * A `visibility` sort key that groups the table by what each repo's visibility check found — a failed check first, then the private repos, then the public ones, then everything with no checked answer — which the `s` key only stops on once visibility checking is turned on, while `--sort visibility` and `default_sort = "visibility"` are honoured either way.
2. [](#improved)
    * **A repo nothing has ever fetched now shows `?` in BEHIND, not `·`.** Zero there reads as "in sync", and for a repo that has never fetched that's a claim nobody checked — the count is zero because there were no remote-tracking refs to compare against, not because the remote had nothing new. The header counts them alongside the rest: `57 behind · 59 never fetched`.
    * **The behind count is bold light red now** rather than the grey used for "nothing to say". It's its own axis: unpushed work is still in your hands, but commits sitting on the remote block whatever you do next until you pull them, and a repo twelve commits behind used to read as quiet as a clean one. Branch tracking lines in the detail view pick up the same colour.
    * `f` and `F` are in the `?` help now, under a "Checking the remotes" section with `ctrl-f`. They were documented only in the README, which is a poor place for the keys that decide whether BEHIND means anything.
    * A fetch no longer passes `--no-tags`. Tags come along by git's ordinary auto-follow, because the release half of the table is built on them: a tag pushed from another machine never arrived, so a repo went on reporting `needs release` for work that had already been released. Local tags are still never pruned — a tag you've cut but not pushed is exactly what `needs release` exists to find.

# 1.0.0

## 08/26/2026

1. [](#new)
    * **drydock is 1.0.** The commands, the config keys and the `--json` fields are settled now, and won't change incompatibly without another major version.
    * Config and cache locations now honour `XDG_CONFIG_HOME` and `XDG_CACHE_HOME`, and on macOS an existing `~/.config/drydock` is used in preference to `~/Library/Application Support`. Nothing moves on its own: a directory nobody created is never chosen, so anyone who hasn't asked for this keeps the platform default. Config and cache resolve independently, so `~/.config/drydock` with no `~/.cache/drydock` puts the config where you want it and leaves the cache where macOS expects it.
2. [](#improved)
    * The dashboard's columns are now configurable, and the `C` key opens a picker to set them: space toggles a column, `J`/`K` reorder, `a` resets, `esc` saves to `[ui] columns`. Turning VISIBILITY on there turns visibility checking on with it and starts a sweep, since a column that can only say "checking off" isn't what anyone was asking for. Turning it off leaves checking alone, because `--public`, `--private` and `--json` still read it. The plain `drydock list` table reads the same setting. Every column's header, alignment, width and cell renderer now live together in one place rather than in two parallel `const` arrays plus a struct of widths that had to be edited in lockstep.
    * `?` help now carries a legend for every marker in the table — STATE, RELEASE, CHANGES and VISIBILITY — in their real colours. Grouped by column, because the same glyph means different things in different ones (`●` is uncommitted changes in STATE and public in VISIBILITY), and each group only appears when its column is on screen.
    * The scroll wheel works in the help and detail panes, which is what most people reach for before finding `j`/`k`, and moves the cursor in the column picker. Both panes now stop at the end of their content instead of scrolling on into blank space — easy to overshoot with a wheel, and the old `j`/`k` scroll was unbounded too.
    * A `VIS` column: the same value as VISIBILITY, rendered as just its marker. Four characters instead of fifteen, which at 120 columns is the difference between `drydock` and `dryd…` in the repo name. Pick it in the `C` picker or use `"visibility_short"` in `[ui] columns`; asking for both forms gives whichever was listed first.
    * Private repos are now blue rather than grey, and internal repos cyan with a `◐` of their own. Private isn't a warning state — it's the one most repos should be in — and greying it lumped it in with the cells that hold no answer at all. Grey is now kept for exactly those. A check that was attempted and failed gets a `!`, since it's the only non-answer you can act on.
    * The table's wording for a repo that hasn't been checked is now `not checked` rather than `checking off`, which read as a verb phrase and said nothing about the flag that controls it.
    * The VISIBILITY column is now shown only when `visibility.enabled` is on. It was unconditional, which cost fifteen fixed characters of table width on every row whether or not checking was happening — enough to cut repo names to four characters at 120 columns — while every cell read "checking off".
3. [](#bugfix)
    * Linked worktrees now report their remote. A worktree's git directory holds its own HEAD, index and refs but no `config` — that file is shared, and reached through the `commondir` pointer beside them — so reading it from the worktree's own directory found nothing and every linked worktree showed `no remote configured` whatever its remote was. `core.bare` is still read per checkout, since a worktree of a bare repo has a working tree of its own.
    * A cached visibility that wasn't an answer is no longer carried forward when checking is off. Only a real `public`/`private` result survives the flag being turned off; a stale `no remote configured` used to outlive the condition it described and keep being reported as fact. Matches what the interval and failure-fallback paths already did.
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
