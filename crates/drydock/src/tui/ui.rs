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

use super::{App, Mode};
use crate::fmt;
use crate::model::{ChangeKind, RepoStatus};
use crate::paths;

const ACCENT: Color = Color::Cyan;
const DIM: Color = Color::DarkGray;
const DIRTY: Color = Color::Yellow;
const UNPUSHED: Color = Color::Cyan;
const UNRELEASED: Color = Color::Magenta;
const TROUBLE: Color = Color::Red;
const CLEAN: Color = Color::Green;

/// Column widths, left to right. The repo name column absorbs whatever is left.
struct Columns {
    group: usize,
    name: usize,
    branch: usize,
    state: usize,
    changes: usize,
    ahead: usize,
    behind: usize,
    tag: usize,
    since_tag: usize,
    age: usize,
}

impl Columns {
    fn for_width(width: usize) -> Self {
        // Every column but the repo name is fixed, and the name absorbs the
        // remainder. That keeps the right-hand numbers in the same place as the
        // terminal resizes, which is what makes the table scannable.
        let group = 14;
        // Wide enough for "◆ unreleased" without an ellipsis.
        let state = 13;
        let changes = 12;
        let ahead = 6;
        let behind = 7;
        let tag = 14;
        let since_tag = 5;
        let age = 5;
        let fixed = group + state + changes + ahead + behind + tag + since_tag + age;

        // Split the leftover between the repo name and the branch. Branch names
        // like `codex/starvector-spike` deserve the room as much as repo names
        // do, and one enormously wide name column just looks like a mistake.
        // Never hand out more than there is: a floor that exceeds the budget
        // would push the age column off the right edge.
        let leftover = width.saturating_sub(fixed);
        let name = (leftover * 55 / 100).clamp(8, 46).min(leftover);
        let branch = leftover.saturating_sub(name);
        Self {
            group,
            name,
            branch,
            state,
            changes,
            ahead,
            behind,
            tag,
            since_tag,
            age,
        }
    }
}

/// How many repo rows fit, so the app can scroll by the right amount.
pub fn table_rows(area: Rect) -> usize {
    // title, filter bar, header, footer, plus the table's own border rows.
    area.height.saturating_sub(7).max(1) as usize
}

pub fn render(f: &mut Frame, app: &App) {
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
        _ => {}
    }
}

fn render_title(f: &mut Frame, app: &App, area: Rect) {
    let dirty = app.repos.iter().filter(|r| r.flags().dirty).count();
    let unpushed = app.repos.iter().filter(|r| r.flags().unpushed).count();
    let unreleased = app.repos.iter().filter(|r| r.flags().unreleased).count();
    let roots: Vec<String> = app
        .cfg
        .root_paths()
        .iter()
        .map(|p| paths::contract(p))
        .collect();

    let mut spans = vec![
        Span::styled(
            " drydock ",
            Style::default()
                .fg(Color::Black)
                .bg(ACCENT)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(roots.join(" "), Style::default().fg(DIM)),
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
            format!("{unreleased} unreleased"),
            Style::default().fg(UNRELEASED),
        ),
    ];

    if app.watching {
        spans.push(Span::styled(" · live", Style::default().fg(CLEAN)));
    }
    if app.sweeping {
        let (done, total) = app.progress;
        spans.push(Span::styled(
            format!(" · scanning {done}/{total}"),
            Style::default().fg(DIM),
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
    let cols = Columns::for_width(inner.width as usize);
    let mut lines = vec![header_line(&cols)];

    let rows = inner.height.saturating_sub(1) as usize;
    let end = (app.scroll + rows).min(app.visible.len());
    for (offset, idx) in app.visible[app.scroll.min(app.visible.len())..end]
        .iter()
        .enumerate()
    {
        let selected = app.scroll + offset == app.selected;
        lines.push(repo_line(&app.repos[*idx], &cols, app.now, selected));
    }

    if app.visible.is_empty() {
        lines.push(Line::from(Span::styled(
            "  Nothing matches the current filters. Press a to clear them.",
            Style::default().fg(DIM),
        )));
    }

    f.render_widget(Paragraph::new(lines), inner);
}

fn header_line(cols: &Columns) -> Line<'static> {
    let style = Style::default().fg(DIM).add_modifier(Modifier::BOLD);
    Line::from(vec![
        Span::styled(pad("GROUP", cols.group), style),
        Span::styled(pad("REPO", cols.name), style),
        Span::styled(pad("BRANCH", cols.branch), style),
        Span::styled(pad("STATE", cols.state), style),
        Span::styled(pad("CHANGES", cols.changes), style),
        Span::styled(rpad("AHEAD", cols.ahead), style),
        Span::styled(rpad("BEHIND", cols.behind), style),
        Span::styled(pad("TAG", cols.tag), style),
        Span::styled(rpad("+TAG", cols.since_tag), style),
        Span::styled(rpad("AGE", cols.age), style),
    ])
}

fn repo_line(repo: &RepoStatus, cols: &Columns, now: i64, selected: bool) -> Line<'static> {
    let flags = repo.flags();
    let state_colour = if flags.error || flags.conflicted || flags.in_progress {
        TROUBLE
    } else if flags.dirty {
        DIRTY
    } else if flags.unpushed {
        UNPUSHED
    } else if flags.unreleased {
        UNRELEASED
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
    } else if flags.unreleased {
        "◆"
    } else if repo.work.is_none() {
        "·"
    } else {
        "✓"
    };

    let changes = repo
        .work
        .as_ref()
        .map(|w| fmt::changes(w.staged, w.unstaged, w.untracked, w.conflicts))
        .unwrap_or_else(|| "…".into());

    let tag = repo.tag_label();
    let mut spans = vec![
        Span::styled(pad(&repo.group, cols.group), base.fg(DIM)),
        Span::styled(
            pad(
                &fmt::truncate(&repo.name, cols.name.saturating_sub(1)),
                cols.name,
            ),
            if flags.clean() {
                base
            } else {
                base.add_modifier(Modifier::BOLD)
            },
        ),
        Span::styled(
            pad(
                &fmt::truncate(&repo.branch_label(), cols.branch.saturating_sub(1)),
                cols.branch,
            ),
            base.fg(if flags.detached { TROUBLE } else { Color::Blue }),
        ),
        Span::styled(
            pad(&format!("{marker} {}", repo.state_label()), cols.state),
            Style::default().fg(state_colour),
        ),
        Span::styled(
            pad(
                &fmt::truncate(&changes, cols.changes.saturating_sub(1)),
                cols.changes,
            ),
            base.fg(if flags.dirty { DIRTY } else { DIM }),
        ),
        Span::styled(
            rpad(&fmt::count(repo.unpushed_total()), cols.ahead),
            base.fg(if repo.unpushed_total() > 0 {
                UNPUSHED
            } else {
                DIM
            }),
        ),
        Span::styled(
            rpad(&fmt::count(repo.behind_total()), cols.behind),
            base.fg(DIM),
        ),
        Span::styled(
            pad(&fmt::truncate(&tag, cols.tag.saturating_sub(1)), cols.tag),
            base.fg(DIM),
        ),
        Span::styled(
            rpad(&fmt::count(repo.commits_since_tag()), cols.since_tag),
            base.fg(if repo.commits_since_tag() > 0 {
                UNRELEASED
            } else {
                DIM
            }),
        ),
        Span::styled(
            rpad(&fmt::age(repo.activity_at(), now), cols.age),
            base.fg(DIM),
        ),
    ];

    if selected {
        // Reverse the whole row rather than recolour it, so the state colours
        // stay readable under the cursor.
        for span in spans.iter_mut() {
            span.style = span.style.add_modifier(Modifier::REVERSED);
        }
    }
    Line::from(spans)
}

fn render_keys(f: &mut Frame, app: &App, area: Rect) {
    let keys: &[(&str, &str)] = match app.mode {
        Mode::Detail => &[
            ("esc", "back"),
            ("j/k", "scroll"),
            ("o", "editor"),
            ("t", "client"),
            ("y", "copy path"),
        ],
        Mode::Search => &[("esc", "cancel"), ("enter", "keep"), ("type", "to filter")],
        _ => &[
            ("j/k", "move"),
            ("⏎", "detail"),
            ("d", "dirty"),
            ("u", "unpushed"),
            ("r", "unreleased"),
            ("a", "clear"),
            ("s", "sort"),
            ("/", "search"),
            ("[ ]", "group"),
            ("1-4", "since"),
            ("o", "open"),
            ("?", "help"),
        ],
    };

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
            if app.timings.total > std::time::Duration::ZERO {
                parts.push(format!("last sweep {}", fmt::duration(app.timings.total)));
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

fn render_help(f: &mut Frame, app: &App, area: Rect) {
    let area = centred(area, 72, 92);
    f.render_widget(Clear, area);

    let sections: &[(&str, &[(&str, &str)])] = &[
        (
            "Moving around",
            &[
                ("j / k, ↑ / ↓", "move the selection"),
                ("ctrl-d / ctrl-u", "half a page"),
                ("home / end", "first and last row"),
                ("enter", "open the detail view"),
                ("q", "quit"),
            ],
        ),
        (
            "Narrowing the list",
            &[
                ("d", "uncommitted changes"),
                ("u", "commits not pushed"),
                ("r", "commits since the last tag"),
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
            "Handing off",
            &[
                ("o", "open in your editor"),
                ("t", "open in your git client"),
                ("T", "open a terminal there"),
                ("w", "open the remote in a browser"),
                ("y", "copy the path"),
                ("R / ctrl-r", "rescan now"),
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
    lines.push(Line::from(Span::styled(
        " Ahead and behind counts come from refs you have already fetched, so",
        Style::default().fg(DIM),
    )));
    lines.push(Line::from(Span::styled(
        " \"behind\" is only as fresh as your last fetch.",
        Style::default().fg(DIM),
    )));

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(ACCENT))
        .title(" keys · j/k to scroll · esc to close ")
        .title_alignment(Alignment::Center);
    f.render_widget(
        Paragraph::new(lines)
            .block(block)
            .scroll((app.detail_scroll, 0)),
        area,
    );
}

fn render_detail(f: &mut Frame, app: &App, area: Rect) {
    let Some(repo) = app.current() else { return };
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
                        Style::default().fg(if b.ahead > 0 { UNPUSHED } else { DIM }),
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

    f.render_widget(
        Paragraph::new(lines)
            .block(block)
            .scroll((app.detail_scroll, 0)),
        area,
    );
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
