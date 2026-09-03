//! Dashboard rendering.
//!
//! Rows are built as styled lines rather than handed to a table widget, because
//! the columns need to stay put across five hundred rows and the state column
//! carries colour that has to line up with the counts beside it.

use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
    Frame,
};

use crossterm::event::KeyModifiers;

use super::{App, Mode};
use crate::column::{Column, Width};
use crate::fmt;
use crate::model::{ChangeKind, ReleaseState, RepoStatus, Visibility, VisibilityStatus};
use crate::paths;
use crate::report::Align;

const ACCENT: Color = Color::Cyan;
const DIM: Color = Color::DarkGray;
const DIRTY: Color = Color::Yellow;
const UNPUSHED: Color = Color::Cyan;
const UNRELEASED: Color = Color::Magenta;
/// Commits waiting on the remote. Its own colour because it's its own axis:
/// unpushed is work you have that the remote doesn't, behind is work the
/// remote has that you don't, and rendering the second in the grey reserved
/// for "nothing to say" is how a repo twelve commits behind reads as quiet.
///
/// Light red rather than something softer, and bold with it, because behind
/// is the one count you can't work around: unpushed work is still in your
/// hands, but commits sitting on the remote block anything you do next until
/// you pull them. Near TROUBLE's red without being it — the two never appear
/// in the same column, and this is the same order of "deal with me first".
const BEHIND: Color = Color::LightRed;
const TROUBLE: Color = Color::Red;
const CLEAN: Color = Color::Green;
// Private isn't a warning state -- it's the one most repos should be in -- so
// it gets a colour of its own rather than the grey reserved for cells that
// hold no answer at all. Internal is half of each, and reads as such.
const PRIVATE: Color = Color::Blue;
const INTERNAL: Color = Color::Cyan;

/// The columns on screen, left to right, with the width each one resolved to
/// for this terminal. Which columns are in here comes from the config (see
/// [`crate::column`]); only the widths are worked out here.
struct TableLayout {
    columns: Vec<(Column, usize)>,
}

impl TableLayout {
    fn for_width(width: usize, branch_want: usize, columns: &[Column]) -> Self {
        // Every fixed column takes what it takes, the branch column takes only
        // what the branch names on screen need, and the repo name absorbs the
        // rest. That keeps the right-hand numbers in the same place as the
        // terminal resizes, which is what makes the table scannable.
        let fixed: usize = columns
            .iter()
            .filter_map(|c| match c.width() {
                Width::Fixed(w) => Some(w),
                _ => None,
            })
            .sum();
        let leftover = width.saturating_sub(fixed);

        // Nearly every repo sits on `main` or `develop`, so a fixed share of
        // the leftover left a wide strip of empty space next to truncated repo
        // names. Long branch names still get room, up to a cap, and never more
        // than half the leftover -- and never more than there is, or the age
        // column would be pushed off the right edge.
        let branch = if columns.contains(&Column::Branch) {
            branch_want.clamp(7, 24).min(leftover / 2)
        } else {
            0
        };
        let fill = leftover.saturating_sub(branch);

        let columns = columns
            .iter()
            .map(|c| {
                let w = match c.width() {
                    Width::Fixed(w) => w,
                    Width::Branch => branch,
                    Width::Fill => fill,
                };
                (*c, w)
            })
            .collect();
        Self { columns }
    }
}

/// How many repo rows fit, so the app can scroll by the right amount.
pub fn table_rows(area: Rect) -> usize {
    // title, filter bar, header, footer, plus the table's own border rows.
    area.height.saturating_sub(7).max(1) as usize
}

/// Screen row of the first repo in the table, so a click can be turned back
/// into a row. Counts down past the title, the filter bar, the table's top
/// border and the column header.
pub fn table_first_row(area: Rect) -> u16 {
    area.y + 4
}

/// Draws one frame and reports how far the overlay on top of it — if any —
/// can usefully be scrolled. Measured here because it's the only place that
/// knows both how many lines the pane came to and how many of them fit; the
/// input handlers clamp against it so the wheel can't spin off into blank
/// space below the content.
pub fn render(f: &mut Frame, app: &App) -> u16 {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // title and totals
            Constraint::Length(1), // active filters
            Constraint::Min(5),    // the table
            Constraint::Length(1), // key bar
            Constraint::Length(1), // message and progress
        ])
        .split(f.area());

    render_title(f, app, chunks[0]);
    render_filters(f, app, chunks[1]);
    render_table(f, app, chunks[2]);
    render_keys(f, app, chunks[3]);
    render_status(f, app, chunks[4]);

    match app.mode {
        Mode::Help => render_help(f, app, f.area()),
        Mode::Detail => render_detail(f, app, f.area()),
        Mode::Columns => {
            render_columns(f, app, f.area());
            0
        }
        _ => 0,
    }
}

/// How far a pane of `lines` can scroll inside `area` before it's showing
/// nothing but empty space. Two rows of that area are its own borders.
fn max_scroll(lines: usize, area: Rect) -> u16 {
    let visible = area.height.saturating_sub(2) as usize;
    lines.saturating_sub(visible).try_into().unwrap_or(u16::MAX)
}

/// The scan roots as displayed, e.g. `~/Projects`.
fn roots_label(app: &App) -> String {
    app.cfg
        .root_paths()
        .iter()
        .map(|p| paths::contract(p))
        .collect::<Vec<_>>()
        .join(" ")
}

fn render_title(f: &mut Frame, app: &App, area: Rect) {
    let dirty = app.repos.iter().filter(|r| r.flags().dirty).count();
    let unpushed = app.repos.iter().filter(|r| r.flags().unpushed).count();
    let needs_release = app
        .repos
        .iter()
        .filter(|r| r.release_state() == ReleaseState::NeedsRelease)
        .count();
    let mut spans = vec![
        Span::styled(
            " drydock ",
            Style::default()
                .fg(Color::Black)
                .bg(ACCENT)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(roots_label(app), Style::default().fg(DIM)),
        Span::raw("  "),
        Span::styled(
            format!("{} repos", app.repos.len()),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw(" · "),
        Span::styled(format!("{dirty} dirty"), Style::default().fg(DIRTY)),
        Span::raw(" · "),
        Span::styled(
            format!("{unpushed} unpushed"),
            Style::default().fg(UNPUSHED),
        ),
        Span::raw(" · "),
        Span::styled(
            format!("{needs_release} need release"),
            Style::default().fg(UNRELEASED),
        ),
    ];

    // Behind, and never-checked, only appear when there are any. They're the
    // two counts that are usually zero, and a permanent "0 behind" would be
    // the same reassuring nothing the BEHIND column used to give.
    let behind = app.repos.iter().filter(|r| r.behind_total() > 0).count();
    if behind > 0 {
        spans.push(Span::raw(" · "));
        spans.push(Span::styled(
            format!("{behind} behind"),
            Style::default().fg(BEHIND).add_modifier(Modifier::BOLD),
        ));
    }
    let never = app.repos.iter().filter(|r| r.never_fetched()).count();
    if never > 0 {
        spans.push(Span::raw(" · "));
        spans.push(Span::styled(
            format!("{never} never fetched"),
            Style::default().fg(DIM),
        ));
    }

    if app.watching {
        spans.push(Span::styled(" · live", Style::default().fg(CLEAN)));
    }
    if let Some(note) = app.activity_note() {
        spans.push(Span::raw(" · "));
        spans.push(Span::styled(
            format!("{} {note}", app.spinner_frame()),
            Style::default().fg(ACCENT),
        ));
    }

    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_filters(f: &mut Frame, app: &App, area: Rect) {
    let mut spans = vec![Span::styled(" filter ", Style::default().fg(DIM))];

    if app.query.filters.is_empty() {
        spans.push(Span::styled("everything", Style::default().fg(DIM)));
    } else {
        for (i, filter) in app.query.filters.iter().enumerate() {
            if i > 0 {
                spans.push(Span::styled(
                    format!(" {} ", app.query.match_mode.label()),
                    Style::default().fg(DIM),
                ));
            }
            spans.push(Span::styled(
                format!(" {} ", filter.label()),
                Style::default().fg(Color::Black).bg(ACCENT),
            ));
        }
    }

    if let Some(since) = &app.query.since {
        spans.push(Span::styled("  since ", Style::default().fg(DIM)));
        spans.push(Span::styled(
            humanize_window(since.as_secs()),
            Style::default().fg(Color::White),
        ));
    }
    if let Some(group) = &app.query.group {
        spans.push(Span::styled("  group ", Style::default().fg(DIM)));
        spans.push(Span::styled(
            group.clone(),
            Style::default().fg(Color::White),
        ));
    }
    spans.push(Span::styled("  sort ", Style::default().fg(DIM)));
    spans.push(Span::styled(
        format!(
            "{}{}",
            app.query.sort.label(),
            if app.query.reverse { " (reversed)" } else { "" }
        ),
        Style::default().fg(Color::White),
    ));

    if app.mode == Mode::Search {
        spans.push(Span::styled("  /", Style::default().fg(ACCENT)));
        spans.push(Span::styled(
            format!("{}▏", app.search_input),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ));
    } else if !app.query.search.is_empty() {
        spans.push(Span::styled("  /", Style::default().fg(DIM)));
        spans.push(Span::styled(
            app.query.search.clone(),
            Style::default().fg(Color::White),
        ));
    }

    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_table(f: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(DIM))
        .title(Span::styled(
            if app.visible.len() == app.repos.len() {
                format!(" showing all {} ", app.repos.len())
            } else {
                format!(" showing {} of {} ", app.visible.len(), app.repos.len())
            },
            Style::default().fg(DIM),
        ));
    let inner = block.inner(area);
    f.render_widget(block, area);

    if inner.height < 2 {
        return;
    }
    // Size the branch column from every row that passes the filters, not just
    // the ones on screen, so it holds still while scrolling. Take a high
    // percentile rather than the longest name: nearly every repo is on `main`
    // or `develop`, and one `codex/some-experiment-branch` should truncate
    // rather than tax all 500 rows with a column of empty space.
    let mut branch_widths: Vec<usize> = app
        .visible
        .iter()
        .map(|idx| app.repos[*idx].branch_label().chars().count() + 1)
        .collect();
    branch_widths.sort_unstable();
    let branch_want = branch_widths
        .get(branch_widths.len().saturating_sub(1) * 95 / 100)
        .copied()
        .unwrap_or(0);
    let layout = TableLayout::for_width(inner.width as usize, branch_want, &app.columns);
    let mut lines = vec![header_line(&layout)];

    let rows = inner.height.saturating_sub(1) as usize;
    let end = (app.scroll + rows).min(app.visible.len());
    for (offset, idx) in app.visible[app.scroll.min(app.visible.len())..end]
        .iter()
        .enumerate()
    {
        let selected = app.scroll + offset == app.selected;
        lines.push(repo_line(&app.repos[*idx], &layout, app.now, selected));
    }

    if app.visible.is_empty() {
        lines.push(Line::from(""));
        // On a cold start there is nothing cached to show, so say what's
        // happening rather than leaving an empty box that reads as a hang.
        lines.push(match app.activity_note() {
            Some(note) => Line::from(vec![
                Span::styled(
                    format!("  {} ", app.spinner_frame()),
                    Style::default().fg(ACCENT),
                ),
                Span::styled(
                    format!("First scan of {} · {note}", roots_label(app)),
                    Style::default().fg(Color::White),
                ),
            ]),
            None => Line::from(Span::styled(
                "  Nothing matches the current filters. Press a to clear them.",
                Style::default().fg(DIM),
            )),
        });
    }

    f.render_widget(Paragraph::new(lines), inner);
}

fn header_line(layout: &TableLayout) -> Line<'static> {
    let style = Style::default().fg(DIM).add_modifier(Modifier::BOLD);
    let spans: Vec<Span<'static>> = layout
        .columns
        .iter()
        .map(|(col, w)| {
            let text = col.header(false);
            let cell = match col.align() {
                Align::Right => rpad(text, *w),
                Align::Left => pad(text, *w),
            };
            Span::styled(cell, style)
        })
        .collect();
    Line::from(spans)
}

fn repo_line(repo: &RepoStatus, layout: &TableLayout, now: i64, selected: bool) -> Line<'static> {
    let flags = repo.flags();
    let state_colour = if flags.error || flags.conflicted || flags.in_progress {
        TROUBLE
    } else if flags.dirty {
        DIRTY
    } else if flags.unpushed {
        UNPUSHED
    } else {
        CLEAN
    };

    let base = if flags.clean() {
        Style::default().fg(DIM)
    } else {
        Style::default()
    };

    let marker = if flags.error || flags.conflicted || flags.in_progress {
        "⚠"
    } else if flags.dirty {
        "●"
    } else if flags.unpushed {
        "↑"
    } else if repo.work.is_none() {
        "·"
    } else {
        "✓"
    };

    let mut spans: Vec<Span<'static>> = layout
        .columns
        .iter()
        .map(|(col, w)| {
            let w = *w;
            match col {
                Column::Group => Span::styled(pad(&repo.group, w), base.fg(DIM)),
                Column::Repo => Span::styled(
                    pad(&fmt::truncate(&repo.name, w.saturating_sub(1)), w),
                    if flags.clean() {
                        base
                    } else {
                        base.add_modifier(Modifier::BOLD)
                    },
                ),
                Column::Branch => Span::styled(
                    pad(&fmt::truncate(&repo.branch_label(), w.saturating_sub(1)), w),
                    base.fg(if flags.detached { TROUBLE } else { Color::Blue }),
                ),
                Column::State => Span::styled(
                    pad(&format!("{marker} {}", repo.state_label()), w),
                    Style::default().fg(state_colour),
                ),
                Column::Release => Span::styled(
                    pad(&release_cell(repo), w),
                    match repo.release_state() {
                        ReleaseState::NeedsRelease => Style::default().fg(UNRELEASED),
                        ReleaseState::Unreleased => base.fg(DIM),
                        ReleaseState::Released => base.fg(CLEAN),
                    },
                ),
                Column::Visibility => {
                    Span::styled(pad(&visibility_cell(repo), w), visibility_style(repo, base))
                }
                Column::VisibilityShort => Span::styled(
                    pad(repo.visibility_marker(), w),
                    visibility_style(repo, base),
                ),
                Column::Changes => {
                    let changes = repo
                        .work
                        .as_ref()
                        .map(|wk| fmt::changes(wk.staged, wk.unstaged, wk.untracked, wk.conflicts))
                        // `…` means "still scanning". A bare repo has nothing
                        // to scan, which is a different thing.
                        .unwrap_or_else(|| {
                            if repo.is_bare() {
                                "·".into()
                            } else {
                                "…".into()
                            }
                        });
                    Span::styled(
                        pad(&fmt::truncate(&changes, w.saturating_sub(1)), w),
                        base.fg(if flags.dirty { DIRTY } else { DIM }),
                    )
                }
                Column::Stashes => {
                    // Stashed work isn't a warning -- it's work you put down
                    // on purpose. It gets DIRTY's yellow because that's what
                    // it is, uncommitted work, but no bold: the row's real
                    // state is in STATE and CHANGES, and this shouldn't
                    // out-shout them.
                    match repo.stash_count() {
                        Some(n) if n > 0 => Span::styled(rpad(&n.to_string(), w), base.fg(DIRTY)),
                        Some(_) => Span::styled(rpad("·", w), base.fg(DIM)),
                        None => Span::styled(rpad("?", w), base.fg(DIM)),
                    }
                }
                // Both counts are the checked-out branch's, matching BRANCH
                // next door; `*` means another local branch has some too.
                // Only the branch's own count colours the cell -- a stale
                // side branch is a footnote, not a call to action on the row.
                Column::Ahead => {
                    let ahead = repo.branch_unpushed();
                    let text = fmt::marked(fmt::count(ahead), repo.other_branches_unpushed());
                    Span::styled(
                        rpad(&text, w),
                        base.fg(if ahead > 0 { UNPUSHED } else { DIM }),
                    )
                }
                Column::Behind => {
                    let behind = repo.branch_behind();
                    // `?`, not `·`: zero here means nothing was ever compared
                    // against a remote, which is not the same as in sync.
                    if behind > 0 {
                        let text = fmt::marked(fmt::count(behind), repo.other_branches_behind());
                        Span::styled(rpad(&text, w), base.fg(BEHIND).add_modifier(Modifier::BOLD))
                    } else if repo.never_fetched() && repo.behind_total() == 0 {
                        Span::styled(rpad("?", w), base.fg(DIM))
                    } else {
                        let text = fmt::marked("·".to_string(), repo.other_branches_behind());
                        Span::styled(rpad(&text, w), base.fg(DIM))
                    }
                }
                Column::Fetched => {
                    let text = crate::column::fetched_label(repo, now);
                    Span::styled(
                        rpad(&text, w),
                        base.fg(if text == "never" { BEHIND } else { DIM }),
                    )
                }
                Column::Tag => Span::styled(
                    pad(&fmt::truncate(&repo.tag_label(), w.saturating_sub(1)), w),
                    base.fg(DIM),
                ),
                Column::SinceTag => Span::styled(
                    rpad(&fmt::count(repo.commits_since_tag()), w),
                    base.fg(if repo.commits_since_tag() > 0 {
                        UNRELEASED
                    } else {
                        DIM
                    }),
                ),
                Column::Age => {
                    Span::styled(rpad(&fmt::age(repo.activity_at(), now), w), base.fg(DIM))
                }
            }
        })
        .collect();

    if selected {
        // Reverse the whole row rather than recolour it, so the state colours
        // stay readable under the cursor.
        for span in spans.iter_mut() {
            span.style = span.style.add_modifier(Modifier::REVERSED);
        }
    }
    Line::from(spans)
}

/// Green for "out in the open", the same reading as GitHub's own badge. Blue
/// for private, because it isn't a warning state — it's the one most repos
/// should be in, and greying it lumped it in with the cells that hold no
/// answer at all. Grey is kept for exactly those: not checked, no remote,
/// unsupported, unknown, failed.
fn visibility_style(repo: &RepoStatus, base: Style) -> Style {
    match repo.visibility.as_ref().map(|v| &v.status) {
        Some(VisibilityStatus::Known(Visibility::Public)) => base.fg(CLEAN),
        Some(VisibilityStatus::Known(Visibility::Private)) => base.fg(PRIVATE),
        Some(VisibilityStatus::Known(Visibility::Internal)) => base.fg(INTERNAL),
        Some(VisibilityStatus::Unsupported)
        | Some(VisibilityStatus::NoRemote)
        | Some(VisibilityStatus::Unknown)
        | Some(VisibilityStatus::CheckingDisabled)
        | Some(VisibilityStatus::CheckFailed(_))
        | None => base.fg(DIM),
    }
}

/// The long visibility cell: the shared marker, then a short form of the
/// label. Shortened only because the table is fixed-width — `status`,
/// `--json` and the detail view all show the full label, and for a real
/// failure the reason too. The VIS column drops the words entirely.
fn visibility_cell(repo: &RepoStatus) -> String {
    let label = match repo.visibility.as_ref().map(|v| &v.status) {
        Some(VisibilityStatus::Known(v)) => v.label(),
        Some(VisibilityStatus::Unsupported) => "unsupported",
        Some(VisibilityStatus::NoRemote) => "no remote",
        Some(VisibilityStatus::Unknown) => "unknown",
        Some(VisibilityStatus::CheckingDisabled) => "not checked",
        Some(VisibilityStatus::CheckFailed(_)) => "check failed",
        None => return "-".into(),
    };
    format!("{} {label}", repo.visibility_marker())
}

/// The release cell, with a marker so the column scans without reading words.
fn release_cell(repo: &RepoStatus) -> String {
    let state = repo.release_state();
    let marker = match state {
        ReleaseState::NeedsRelease => "◆",
        ReleaseState::Unreleased => "·",
        ReleaseState::Released => "✓",
    };
    format!("{marker} {}", state.label())
}

/// What the footer lists, which depends on what's held down.
///
/// Holding shift or ctrl swaps the row for what those keys would do right
/// now, so the modified bindings are discoverable by pressing the modifier
/// rather than by reading the help. Terminals that don't report a bare
/// modifier keep [`App::mods`] empty and always get the first row.
pub(super) fn key_hints(app: &App) -> &'static [(&'static str, &'static str)] {
    let shift = app.mods.contains(KeyModifiers::SHIFT);
    let ctrl = app.mods.contains(KeyModifiers::CONTROL);

    match app.mode {
        // Ctrl wins when both are down: its bindings are the same everywhere,
        // so showing them is never wrong, and a chord in progress is more
        // likely to be a ctrl one.
        _ if ctrl => &[
            ("^o", "terminal"),
            ("^f", "fetch all"),
            ("^r", "rescan"),
            ("^d/^u", "half page"),
            ("^c", "quit"),
        ],
        Mode::Detail if shift => &[("O", "editor"), ("T", "terminal")],
        Mode::Detail => &[
            ("esc", "back"),
            ("j/k", "scroll"),
            ("o", "finder"),
            ("t", "client"),
            ("y", "copy path"),
        ],
        Mode::Search => &[("esc", "cancel"), ("enter", "keep"), ("type", "to filter")],
        Mode::Columns if shift => &[("J/K", "move column"), ("C", "close")],
        _ if shift => &[
            ("O", "editor"),
            ("T", "terminal"),
            ("F", "fetch screen"),
            ("N", "unreleased"),
            ("S", "reverse sort"),
            ("C", "columns"),
            ("R", "rescan"),
        ],
        _ => &[
            ("j/k", "move"),
            ("⏎", "detail"),
            ("d", "dirty"),
            ("u", "unpushed"),
            ("r", "needs release"),
            ("a", "clear"),
            ("s", "sort"),
            ("/", "search"),
            ("[ ]", "group"),
            ("1-4", "since"),
            ("o", "finder"),
            ("^f", "fetch all"),
            ("?", "help"),
        ],
    }
}

fn render_keys(f: &mut Frame, app: &App, area: Rect) {
    let keys = key_hints(app);

    let mut spans = vec![Span::raw(" ")];
    for (key, what) in keys {
        spans.push(Span::styled(
            (*key).to_string(),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(format!(" {what}  "), Style::default().fg(DIM)));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_status(f: &mut Frame, app: &App, area: Rect) {
    let text = match app.active_message() {
        Some(message) => Span::styled(format!(" {message}"), Style::default().fg(Color::White)),
        None => {
            let mut parts = Vec::new();
            if let Some(note) = app.activity_note() {
                parts.push(note);
            }
            // How long ago, not just how long it took. Without this a dashboard
            // whose sweeps have stopped looks exactly like one that is up to
            // date, which is the worst way for this tool to fail.
            if let Some(at) = app.last_sweep_at {
                parts.push(format!(
                    "swept {} ago in {}",
                    fmt::age(at, app.now),
                    fmt::duration(app.timings.total)
                ));
            }
            if app.timings.work_cached > 0 {
                parts.push(format!("{} from cache", app.timings.work_cached));
            }
            if let Some(repo) = app.current() {
                let (at, source) = repo.activity();
                parts.push(format!(
                    "{} · {} ago via {}",
                    paths::contract(&repo.root),
                    fmt::age(at, app.now),
                    source.label()
                ));
            }
            Span::styled(
                format!(" {}", parts.join("  ·  ")),
                Style::default().fg(DIM),
            )
        }
    };
    f.render_widget(Paragraph::new(Line::from(text)), area);
}

// ---------------------------------------------------------------------------
// Overlays
// ---------------------------------------------------------------------------

fn centred(area: Rect, width_pct: u16, height_pct: u16) -> Rect {
    let w = area.width * width_pct / 100;
    let h = area.height * height_pct / 100;
    Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    }
}

/// One legend row: the glyph, the colour the table draws it in, and what it
/// means in that column.
type Marker = (&'static str, Color, &'static str);
/// One column's worth of legend: its header, then its markers.
type MarkerGroup = (&'static str, Vec<Marker>);

/// The marker legend for `?`, grouped by the column each glyph belongs to and
/// limited to the columns actually on screen — a legend for a column you've
/// turned off is dead weight.
///
/// The same glyph means different things in different columns (`●` is dirty
/// in STATE and public in VISIBILITY, `·` is "nothing" nearly everywhere), so
/// grouping is what makes it readable rather than a flat list of collisions.
/// Colours are the table's own: printed in plain white this would teach half
/// of what a cell says.
pub fn marker_legend(app: &App) -> Vec<MarkerGroup> {
    let mut groups: Vec<MarkerGroup> = Vec::new();
    let shown = |c: Column| app.columns.contains(&c);

    if shown(Column::State) {
        groups.push((
            "STATE",
            vec![
                ("⚠", TROUBLE, "conflict, operation in progress, or error"),
                ("●", DIRTY, "uncommitted changes"),
                ("↑", UNPUSHED, "commits not pushed to the upstream"),
                ("✓", CLEAN, "nothing outstanding"),
                ("·", DIM, "not scanned yet, or a bare repo"),
            ],
        ));
    }
    if shown(Column::Release) {
        groups.push((
            "RELEASE",
            vec![
                ("◆", UNRELEASED, "commits or changes past the newest tag"),
                ("✓", CLEAN, "tagged, with nothing since"),
                ("·", DIM, "no tags at all"),
            ],
        ));
    }
    if shown(Column::Changes) {
        groups.push((
            "CHANGES",
            vec![
                ("!", DIRTY, "conflicted files"),
                ("+", DIRTY, "staged"),
                ("~", DIRTY, "unstaged"),
                ("?", DIRTY, "untracked"),
                ("·", DIM, "a clean working tree"),
            ],
        ));
    }
    if shown(Column::Visibility) || shown(Column::VisibilityShort) {
        groups.push((
            "VISIBILITY",
            vec![
                ("●", CLEAN, "public"),
                ("⊘", PRIVATE, "private"),
                ("◐", INTERNAL, "internal, on GitHub Enterprise"),
                ("!", DIM, "a check failed — press ⏎ for the reason"),
                ("·", DIM, "no answer: not checked, no remote, unknown host"),
            ],
        ));
    }
    groups
}

fn render_help(f: &mut Frame, app: &App, area: Rect) -> u16 {
    let area = centred(area, 72, 92);
    f.render_widget(Clear, area);

    let sections: &[(&str, &[(&str, &str)])] = &[
        (
            "Moving around",
            &[
                ("j / k, ↑ / ↓", "move the selection"),
                ("ctrl-d / ctrl-u", "half a page"),
                ("home / end", "first and last row"),
                ("click", "select that row"),
                ("wheel", "move the selection, or scroll the detail view"),
                ("enter", "open the detail view"),
                ("q", "quit"),
            ],
        ),
        (
            "Narrowing the list",
            &[
                ("d", "uncommitted changes"),
                ("u", "commits not pushed"),
                ("r", "needs a release: commits or changes past the tag"),
                ("N", "never released: no tags at all"),
                ("b", "behind the upstream"),
                ("c / i", "conflicts / operation in progress"),
                ("x / e", "detached HEAD / probe errors"),
                ("n", "nothing outstanding"),
                ("&", "switch between matching any and all filters"),
                ("a", "clear every filter"),
                ("/", "search by name, group or branch"),
                ("[ / ]", "step through groups"),
                ("1 2 3 4", "touched in the last hour, day, week, month"),
                ("0", "any age"),
            ],
        ),
        (
            "Ordering",
            &[("s", "cycle the sort key"), ("S", "reverse the sort")],
        ),
        (
            "Checking the remotes",
            &[
                ("f", "fetch the selected repo"),
                ("F", "fetch everything on screen"),
                ("ctrl-f", "fetch every repo in the fleet, filters ignored"),
            ],
        ),
        (
            "Handing off",
            &[
                ("o", "show the folder in Finder"),
                ("O", "open in your editor"),
                ("T / ctrl-o", "open a terminal there"),
                ("t", "open in your git client"),
                ("w", "open the remote in a browser"),
                ("y", "copy the path"),
                ("R / ctrl-r", "rescan now"),
                ("C", "choose which columns to show"),
            ],
        ),
    ];

    let mut lines = Vec::new();
    for (title, entries) in sections {
        lines.push(Line::from(Span::styled(
            format!(" {title}"),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )));
        for (key, what) in *entries {
            lines.push(Line::from(vec![
                Span::styled(format!("   {:<16}", key), Style::default().fg(Color::White)),
                Span::styled((*what).to_string(), Style::default().fg(DIM)),
            ]));
        }
        lines.push(Line::from(""));
    }
    let legend = marker_legend(app);
    if !legend.is_empty() {
        lines.push(Line::from(Span::styled(
            " Markers",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )));
        for (i, (column, entries)) in legend.into_iter().enumerate() {
            if i > 0 {
                lines.push(Line::from(""));
            }
            lines.push(Line::from(Span::styled(
                format!("   {column}"),
                Style::default().fg(Color::White),
            )));
            for (marker, colour, what) in entries {
                lines.push(Line::from(vec![
                    Span::styled(format!("     {marker:<14}"), Style::default().fg(colour)),
                    Span::styled(what.to_string(), Style::default().fg(DIM)),
                ]));
            }
        }
        lines.push(Line::from(""));
    }

    for note in [
        " Ahead and behind counts come from refs you have already fetched, so",
        " \"behind\" is only as fresh as your last fetch. A `?` there means",
        " nothing has ever fetched that repo, so its behind count has never",
        " been checked against anything -- press f, F or ctrl-f to check it.",
    ] {
        lines.push(Line::from(Span::styled(note, Style::default().fg(DIM))));
    }

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(ACCENT))
        .title(" keys · j/k to scroll · esc to close ")
        .title_alignment(Alignment::Center);
    let scrollable = max_scroll(lines.len(), area);
    f.render_widget(
        Paragraph::new(lines)
            .block(block)
            .scroll((app.detail_scroll.min(scrollable), 0)),
        area,
    );
    scrollable
}

/// The column picker. Shows every column, the ones on screen first in the
/// order they're drawn, then the ones that aren't. The table behind keeps
/// redrawing as this changes, so the effect of every toggle is visible before
/// it's committed.
fn render_columns(f: &mut Frame, app: &App, area: Rect) {
    let area = centred(area, 72, 80);
    f.render_widget(Clear, area);

    let rows = app.picker_rows();
    let shown = app.columns.len();
    let mut lines: Vec<Line> = Vec::new();

    for (i, (column, on)) in rows.iter().enumerate() {
        // The one divider in the list: everything above it is on screen, in
        // render order, and everything below is not.
        if i == shown && shown < rows.len() {
            lines.push(Line::from(Span::styled(
                "   ── not shown ──",
                Style::default().fg(DIM),
            )));
        }

        let selected = i == app.column_cursor;
        let box_glyph = if *on { "◉" } else { "○" };
        let mut style = if *on {
            Style::default().fg(Color::White)
        } else {
            Style::default().fg(DIM)
        };
        if selected {
            style = style.add_modifier(Modifier::REVERSED);
        }
        // REPO can't be turned off, so say why rather than letting someone
        // press space at it and wonder.
        let note = if column.toggleable() {
            column.describe()
        } else {
            "always shown"
        };
        lines.push(Line::from(vec![
            Span::styled(format!("  {box_glyph} {:<12}", column.header(false)), style),
            Span::styled(format!("  {note}"), Style::default().fg(DIM)),
        ]));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        " Order is the order they're drawn, left to right.",
        Style::default().fg(DIM),
    )));
    if !app.cfg.visibility.enabled {
        lines.push(Line::from(Span::styled(
            " VISIBILITY needs visibility.enabled in the config to hold a value;",
            Style::default().fg(DIM),
        )));
        lines.push(Line::from(Span::styled(
            " showing it with checking off gives a column of \"checking off\".",
            Style::default().fg(DIM),
        )));
    }

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(ACCENT))
        .title(" columns · space toggles · J/K reorders · a resets · esc saves ")
        .title_alignment(Alignment::Center);
    f.render_widget(Paragraph::new(lines).block(block), area);
}

fn render_detail(f: &mut Frame, app: &App, area: Rect) -> u16 {
    let Some(repo) = app.current() else { return 0 };
    let area = centred(area, 84, 86);
    f.render_widget(Clear, area);

    let mut lines: Vec<Line> = Vec::new();
    let label = |text: &str| Span::styled(format!("  {:<14}", text), Style::default().fg(DIM));

    lines.push(Line::from(vec![
        label("path"),
        Span::raw(paths::contract(&repo.root)),
    ]));
    lines.push(Line::from(vec![
        label("state"),
        Span::styled(
            repo.state_label().to_string(),
            Style::default().add_modifier(Modifier::BOLD),
        ),
    ]));

    if let Some(v) = &repo.visibility {
        let mut spans = vec![
            label("visibility"),
            Span::styled(
                v.status.label().to_string(),
                visibility_style(repo, Style::default()),
            ),
        ];
        match &v.status {
            // A real provider call happened for both of these, so there's a
            // real "checked ... ago" to report. CheckFailed also gets its
            // reason spelled out here, unlike the table cell -- this is the
            // one place with room for it.
            VisibilityStatus::Known(_) => {
                spans.push(Span::styled(
                    format!("  (checked {} ago)", fmt::age(v.checked_at, app.now)),
                    Style::default().fg(DIM),
                ));
            }
            VisibilityStatus::CheckFailed(reason) => {
                spans.push(Span::styled(
                    format!("  ({} ago): {reason}", fmt::age(v.checked_at, app.now)),
                    Style::default().fg(DIM),
                ));
            }
            // Unsupported, NoRemote and CheckingDisabled all come from
            // reading the remote URL or the config, not a real provider
            // call, so there's nothing that was actually "checked".
            VisibilityStatus::Unsupported
            | VisibilityStatus::NoRemote
            | VisibilityStatus::Unknown
            | VisibilityStatus::CheckingDisabled => {}
        }
        lines.push(Line::from(spans));
    }

    let (at, source) = repo.activity();
    lines.push(Line::from(vec![
        label("activity"),
        Span::raw(format!(
            "{} ago ({})",
            fmt::age(at, app.now),
            source.label()
        )),
    ]));

    if let Some(refs) = &repo.refs {
        lines.push(Line::from(vec![
            label("head"),
            Span::styled(refs.head.label(), Style::default().fg(Color::Blue)),
        ]));
        lines.push(Line::from(vec![
            label("remote"),
            Span::raw(
                refs.remote_url
                    .clone()
                    .unwrap_or_else(|| "(none)".to_string()),
            ),
        ]));
        if refs.remote_url.is_some() {
            let (text, colour) = match refs.fetched_at {
                Some(at) => (format!("{} ago", fmt::age(at, app.now)), DIM),
                None => (
                    "never — this repo's behind count has never been checked".to_string(),
                    BEHIND,
                ),
            };
            lines.push(Line::from(vec![
                label("fetched"),
                Span::styled(text, Style::default().fg(colour)),
            ]));
        }
        if refs.stashes > 0 {
            lines.push(Line::from(vec![
                label("stashes"),
                Span::raw(refs.stashes.to_string()),
            ]));
        }
        if let Some(op) = refs.operation {
            lines.push(Line::from(vec![
                label("in progress"),
                Span::styled(op.label().to_string(), Style::default().fg(TROUBLE)),
            ]));
        }

        match (&refs.described_tag, refs.commits_since_tag) {
            (Some(tag), Some(count)) => {
                lines.push(Line::from(vec![
                    label("last tag"),
                    Span::raw(format!("{} ({} ago)", tag.name, fmt::age(tag.at, app.now))),
                ]));
                lines.push(Line::from(vec![
                    label("since tag"),
                    Span::styled(
                        format!("{count} commit{}", if count == 1 { "" } else { "s" }),
                        Style::default().fg(if count > 0 { UNRELEASED } else { DIM }),
                    ),
                ]));
            }
            _ => lines.push(Line::from(vec![
                label("last tag"),
                Span::styled("none reachable".to_string(), Style::default().fg(DIM)),
            ])),
        }
        if refs.tag_off_branch() {
            if let Some(newest) = &refs.newest_tag {
                lines.push(Line::from(vec![
                    label(""),
                    Span::styled(
                        format!(
                            "newest tag {} is not an ancestor of HEAD (normal with git-flow)",
                            newest.name
                        ),
                        Style::default().fg(DIM),
                    ),
                ]));
            }
        }
        if let Some(cl) = &refs.changelog {
            let note = if cl.tagged {
                "matches a tag".to_string()
            } else if cl.unreleased_blocks > 1 {
                format!("{} unreleased blocks stacked up", cl.unreleased_blocks)
            } else {
                "not tagged yet".to_string()
            };
            lines.push(Line::from(vec![
                label("changelog"),
                Span::raw(format!("{} ", cl.version)),
                Span::styled(
                    format!("({note})"),
                    Style::default().fg(if cl.tagged { DIM } else { UNRELEASED }),
                ),
            ]));
        }

        // Branches, most recently committed first.
        let mut branches: Vec<_> = refs.branches.iter().collect();
        branches.sort_by_key(|b| std::cmp::Reverse(b.committed_at));
        if !branches.is_empty() {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "  branches",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            )));
            for b in branches.iter().take(12) {
                let tracking = match (&b.upstream, b.gone) {
                    (_, true) => "upstream gone".to_string(),
                    (None, _) => "no upstream".to_string(),
                    (Some(u), _) => {
                        let mut s = u.clone();
                        if b.ahead > 0 {
                            s.push_str(&format!(" ↑{}", b.ahead));
                        }
                        if b.behind > 0 {
                            s.push_str(&format!(" ↓{}", b.behind));
                        }
                        s
                    }
                };
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("    {:<26}", fmt::truncate(&b.name, 25)),
                        Style::default().fg(Color::Blue),
                    ),
                    Span::styled(
                        format!("{:<34}", fmt::truncate(&tracking, 33)),
                        Style::default().fg(if b.ahead > 0 {
                            UNPUSHED
                        } else if b.behind > 0 {
                            BEHIND
                        } else {
                            DIM
                        }),
                    ),
                    Span::styled(
                        format!("{:>5}  ", fmt::age(b.committed_at, app.now)),
                        Style::default().fg(DIM),
                    ),
                    Span::raw(fmt::truncate(&b.subject, 40)),
                ]));
            }
        }

        if !refs.since_tag_subjects.is_empty() {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                format!(
                    "  commits since {}",
                    refs.described_tag
                        .as_ref()
                        .map(|t| t.name.as_str())
                        .unwrap_or("the last tag")
                ),
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            )));
            for subject in refs.since_tag_subjects.iter().take(20) {
                lines.push(Line::from(vec![
                    Span::styled("    · ", Style::default().fg(DIM)),
                    Span::raw(fmt::truncate(subject, 100)),
                ]));
            }
        }
    }

    if let Some(work) = &repo.work {
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled(
                "  changed files  ",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                fmt::changes(work.staged, work.unstaged, work.untracked, work.conflicts),
                Style::default().fg(DIRTY),
            ),
        ]));
        for file in work.files.iter().take(30) {
            let (marker, colour) = match file.kind {
                ChangeKind::Staged => ("+", CLEAN),
                ChangeKind::Unstaged => ("~", DIRTY),
                ChangeKind::Untracked => ("?", DIM),
                ChangeKind::Conflicted => ("!", TROUBLE),
            };
            lines.push(Line::from(vec![
                Span::styled(format!("    {marker} "), Style::default().fg(colour)),
                Span::raw(fmt::truncate(&file.path, 90)),
            ]));
        }
        if work.truncated || work.files.len() > 30 {
            lines.push(Line::from(Span::styled(
                "    … more",
                Style::default().fg(DIM),
            )));
        }
    }

    if let Some(err) = &repo.error {
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            label("error"),
            Span::styled(err.clone(), Style::default().fg(TROUBLE)),
        ]));
    }

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(ACCENT))
        .title(format!(" {} ", repo.slug()))
        .title_alignment(Alignment::Center);

    let scrollable = max_scroll(lines.len(), area);
    f.render_widget(
        Paragraph::new(lines)
            .block(block)
            .scroll((app.detail_scroll.min(scrollable), 0)),
        area,
    );
    scrollable
}

// ---------------------------------------------------------------------------

fn pad(text: &str, width: usize) -> String {
    let len = text.chars().count();
    if len >= width {
        fmt::truncate(text, width)
    } else {
        format!("{text}{}", " ".repeat(width - len))
    }
}

fn rpad(text: &str, width: usize) -> String {
    let len = text.chars().count();
    if len >= width {
        fmt::truncate(text, width)
    } else {
        format!("{}{text} ", " ".repeat(width - len - 1))
    }
}

fn humanize_window(secs: u64) -> String {
    match secs {
        s if s < 3_600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h", s / 3_600),
        s if s < 604_800 => format!("{}d", s / 86_400),
        s if s < 2_592_000 => format!("{}w", s / 604_800),
        s => format!("{}mo", s / 2_592_000),
    }
}
