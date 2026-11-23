use std::{ops::Range, path::PathBuf};

use super::{history::Entry, links::Target};
use crate::commits::Scope;

use ratatui::{
    buffer::{Buffer, CellWidth},
    layout::{Position, Rect},
    style::{Color, Modifier},
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum PaneKind {
    Table,
    Detail,
    Help,
    HistoryList,
    HistoryDiff,
    Links,
}

#[derive(PartialEq, Eq)]
pub(super) enum ContentKey {
    Table {
        scroll: usize,
        selected: usize,
    },
    Detail {
        root: PathBuf,
        scroll: u16,
    },
    Help {
        scroll: u16,
    },
    HistoryList {
        root: PathBuf,
        scope: Scope,
        scroll: usize,
        cursor: usize,
    },
    Links {
        root: PathBuf,
        cursor: usize,
    },
    HistoryDiff {
        root: PathBuf,
        entry: Option<Entry>,
        scroll: usize,
        wrap: bool,
    },
}

#[derive(Clone, PartialEq, Eq)]
pub(super) enum RowTarget {
    Repo(PathBuf),
    History(Entry),
}

#[derive(Clone, PartialEq, Eq)]
pub(super) struct LinkHit {
    pub(super) rect: Rect,
    pub(super) root: PathBuf,
    pub(super) target: Target,
}

/// Unhighlighted text from the pane that was actually painted on screen.
#[derive(PartialEq, Eq)]
pub(super) struct PaneSnapshot {
    key: ContentKey,
    pub(super) rect: Rect,
    rows: Vec<Row>,
    targets: Vec<Option<RowTarget>>,
    links: Vec<LinkHit>,
}

#[derive(PartialEq, Eq)]
struct Row {
    text: String,
    glyphs: Vec<Glyph>,
}

#[derive(PartialEq, Eq)]
struct Glyph {
    columns: Range<u16>,
    end_byte: usize,
}

#[derive(Clone, Copy)]
pub(super) enum Gesture {
    Pressed(Position),
    Selecting { anchor: Position, current: Position },
}

impl Gesture {
    pub(super) fn drag(&mut self, position: Position, rect: Rect) {
        let anchor = match *self {
            Self::Pressed(origin) if origin == position => return,
            Self::Pressed(origin) => origin,
            Self::Selecting { anchor, .. } => anchor,
        };
        let current = clamp(position, rect);
        *self = Self::Selecting { anchor, current };
    }
}

impl PaneSnapshot {
    pub(super) fn capture(buffer: &Buffer, rect: Rect, key: ContentKey) -> Option<Self> {
        let rect = rect.intersection(buffer.area);
        if rect.is_empty() {
            return None;
        }
        let rows = (rect.top()..rect.bottom())
            .map(|y| Row::capture(buffer, rect, y))
            .collect();
        Some(Self {
            key,
            rect,
            rows,
            targets: Vec::new(),
            links: Vec::new(),
        })
    }

    pub(super) fn with_targets(mut self, targets: Vec<Option<RowTarget>>) -> Self {
        self.targets = targets;
        self
    }

    pub(super) fn with_links(mut self, links: Vec<LinkHit>) -> Self {
        self.links = links
            .into_iter()
            .filter_map(|mut hit| {
                hit.rect = hit.rect.intersection(self.rect);
                (!hit.rect.is_empty()).then_some(hit)
            })
            .collect();
        self
    }

    pub(super) fn link_at(&self, pos: Position) -> Option<&LinkHit> {
        self.links.iter().find(|hit| hit.rect.contains(pos))
    }

    pub(super) fn kind(&self) -> PaneKind {
        match self.key {
            ContentKey::Table { .. } => PaneKind::Table,
            ContentKey::Detail { .. } => PaneKind::Detail,
            ContentKey::Help { .. } => PaneKind::Help,
            ContentKey::HistoryList { .. } => PaneKind::HistoryList,
            ContentKey::HistoryDiff { .. } => PaneKind::HistoryDiff,
            ContentKey::Links { .. } => PaneKind::Links,
        }
    }

    pub(super) fn target_at(&self, pos: Position) -> Option<&RowTarget> {
        if !self.rect.contains(pos) {
            return None;
        }
        self.targets.get((pos.y - self.rect.y) as usize)?.as_ref()
    }

    pub(super) fn highlight(&self, buffer: &mut Buffer, gesture: Gesture) {
        let Gesture::Selecting { anchor, current } = gesture else {
            return;
        };
        for (y, columns, _) in self.selected_rows(anchor, current) {
            for x in columns {
                let cell = &mut buffer[(x, y)];
                cell.set_fg(Color::White).set_bg(Color::Blue);
                cell.modifier.remove(Modifier::REVERSED);
            }
        }
    }

    pub(super) fn copy_text(&self, gesture: Gesture, release: Position) -> Option<String> {
        let Gesture::Selecting { anchor, .. } = gesture else {
            return None;
        };
        let current = clamp(release, self.rect);
        let mut text = String::new();
        for (i, (_, _, row)) in self.selected_rows(anchor, current).enumerate() {
            if i > 0 {
                text.push('\n');
            }
            text.push_str(row.trim_end_matches(' '));
        }
        Some(text)
    }

    fn selected_rows(
        &self,
        anchor: Position,
        current: Position,
    ) -> impl Iterator<Item = (u16, Range<u16>, &str)> {
        let (start, end) = ordered(anchor, current);
        (start.y..=end.y).map(move |y| {
            let left = if y == start.y {
                start.x
            } else {
                self.rect.left()
            };
            let right = if y == end.y {
                end.x
            } else {
                self.rect.right() - 1
            };
            let row = &self.rows[(y - self.rect.top()) as usize];
            let first = row.glyphs.partition_point(|g| g.columns.end <= left);
            let last = row.glyphs.partition_point(|g| g.columns.end <= right);
            let start_byte = if first == 0 {
                0
            } else {
                row.glyphs[first - 1].end_byte
            };
            let end_byte = row.glyphs[last].end_byte;
            let columns = row.glyphs[first].columns.start..row.glyphs[last].columns.end;
            (y, columns, &row.text[start_byte..end_byte])
        })
    }
}

fn clamp(position: Position, rect: Rect) -> Position {
    Position::new(
        position.x.clamp(rect.left(), rect.right() - 1),
        position.y.clamp(rect.top(), rect.bottom() - 1),
    )
}

impl Row {
    fn capture(buffer: &Buffer, rect: Rect, y: u16) -> Self {
        let mut row = Self {
            text: String::with_capacity(rect.width as usize),
            glyphs: Vec::with_capacity(rect.width as usize),
        };
        let mut x = rect.left();
        while x < rect.right() {
            let cell = &buffer[(x, y)];
            let end = x.saturating_add(cell.cell_width().max(1)).min(rect.right());
            row.text.push_str(cell.symbol());
            row.glyphs.push(Glyph {
                columns: x..end,
                end_byte: row.text.len(),
            });
            x = end;
        }
        row
    }
}

fn ordered(first: Position, second: Position) -> (Position, Position) {
    if (first.y, first.x) <= (second.y, second.x) {
        (first, second)
    } else {
        (second, first)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, style::Style, widgets::Paragraph, Terminal};

    #[test]
    fn a_selection_overrides_reversed_text_and_retains_other_modifiers() {
        let mut terminal = Terminal::new(TestBackend::new(8, 1)).unwrap();
        terminal
            .draw(|frame| {
                let rect = frame.area();
                frame.render_widget(
                    Paragraph::new("界👩‍💻x")
                        .style(Style::new().add_modifier(Modifier::REVERSED | Modifier::BOLD)),
                    rect,
                );
                let buffer = frame.buffer_mut();
                let key = ContentKey::Detail {
                    root: PathBuf::from("/repo"),
                    scroll: 0,
                };
                let snapshot = PaneSnapshot::capture(buffer, rect, key).unwrap();
                let mut gesture = Gesture::Pressed(Position::new(1, 0));
                gesture.drag(Position::new(3, 0), rect);
                snapshot.highlight(buffer, gesture);
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        for x in [0, 2] {
            let cell = &buffer[(x, 0)];
            assert_eq!((cell.fg, cell.bg), (Color::White, Color::Blue));
            assert_eq!(cell.modifier, Modifier::BOLD);
        }
        assert!(buffer[(4, 0)].modifier.contains(Modifier::REVERSED));
    }
}
