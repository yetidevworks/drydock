use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::Span,
    widgets::{Block, Clear},
    Frame,
};
use std::path::PathBuf;
use tokio::{sync::mpsc::UnboundedSender, task::JoinHandle};

use super::{
    activate_link,
    links::{self, Preview, Target},
    selection::{ContentKey, LinkHit, PaneSnapshot},
    ui::{ACCENT, DIM},
    App, Input, Mode,
};

pub(super) struct Picker {
    root: PathBuf,
    entries: Vec<Entry>,
    cursor: usize,
    generation: u64,
    copy_paths: Vec<Target>,
    task: JoinHandle<()>,
}

impl Drop for Picker {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub(super) struct Loaded {
    root: PathBuf,
    generation: u64,
    preview: Preview,
}

struct Entry {
    shortcut: &'static str,
    label: String,
    target: Option<Target>,
}

impl Entry {
    fn branch(branch: Option<&str>) -> Self {
        Self::new(
            "b",
            branch.map_or_else(|| "Branch".into(), |branch| format!("Branch: {branch}")),
            branch.map(|branch| Target::Branch(branch.into())),
        )
    }

    fn new(shortcut: &'static str, label: impl Into<String>, target: Option<Target>) -> Self {
        Self {
            shortcut,
            label: label.into(),
            target,
        }
    }
}

impl Picker {
    pub(super) fn move_cursor(&mut self, down: bool) {
        let next = if down {
            (self.cursor + 1..self.entries.len()).find(|&i| self.entries[i].target.is_some())
        } else {
            (0..self.cursor)
                .rev()
                .find(|&i| self.entries[i].target.is_some())
        };
        if let Some(i) = next {
            self.cursor = i;
        }
    }
}

pub(super) fn open(app: &mut App, tx: &UnboundedSender<Input>) {
    let repo = super::handoff_repo(app);
    let root = app
        .history
        .as_ref()
        .map(|view| view.root.clone())
        .or_else(|| repo.map(|repo| repo.root.clone()));
    let Some(root) = root else { return };
    let refs = repo.and_then(|repo| repo.refs.as_ref());
    let tag = refs.and_then(|refs| refs.described_tag.as_ref());
    let branch = refs.and_then(|refs| refs.head.branch());
    let commit_or_compare = match app.mode {
        Mode::History => Entry::new(
            "c",
            "Commit",
            app.history
                .as_ref()
                .and_then(|view| view.selected_commit())
                .map(|commit| Target::Commit(commit.sha.clone())),
        ),
        _ => Entry::new(
            "c",
            "Compare +TAG",
            tag.map(|tag| Target::Compare {
                tag: tag.name.clone(),
            }),
        ),
    };
    let entries = vec![
        Entry::new("w/r", "Repository", Some(Target::Repository)),
        commit_or_compare,
        Entry::branch(branch),
        match tag {
            Some(tag) => Entry::new(
                "t",
                format!("Tag: {}", tag.name),
                Some(Target::Tag(tag.name.clone())),
            ),
            None => Entry::new("t", "Releases", Some(Target::Releases)),
        },
        Entry::new("p", "Pull / Merge Requests", Some(Target::Reviews)),
        Entry::new("i", "Issues", Some(Target::Issues)),
        Entry::new("a", "Actions / Pipelines", Some(Target::Automation)),
    ];
    let targets = entries
        .iter()
        .filter_map(|entry| entry.target.clone())
        .filter(|target| !matches!(target, Target::Branch(_)))
        .collect();
    app.link_picker_generation = app.link_picker_generation.wrapping_add(1);
    let generation = app.link_picker_generation;
    let task_root = root.clone();
    let tx = tx.clone();
    let task = tokio::spawn(async move {
        let preview = links::preview(&task_root, targets).await;
        let _ = tx.send(Input::Picker(Loaded {
            root: task_root,
            generation,
            preview,
        }));
    });
    app.link_picker = Some(Picker {
        root,
        entries,
        cursor: 0,
        generation,
        copy_paths: Vec::new(),
        task,
    });
}

pub(super) fn on_loaded(app: &mut App, loaded: Loaded) -> bool {
    let Some(picker) = app
        .link_picker
        .as_mut()
        .filter(|picker| picker.root == loaded.root && picker.generation == loaded.generation)
    else {
        return false;
    };
    let Preview { branch, copy_paths } = loaded.preview;
    if let Some(entry) = picker
        .entries
        .iter_mut()
        .find(|entry| entry.shortcut == "b")
    {
        *entry = Entry::branch(branch.as_deref());
    }
    if picker.entries[picker.cursor].target.is_none() {
        picker.cursor = 0;
    }
    picker.copy_paths = copy_paths;
    true
}

pub(super) fn handle_key(app: &mut App, key: KeyEvent, tx: &UnboundedSender<Input>) {
    if !key.modifiers.is_empty() {
        if key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('c') {
            app.should_quit = true;
        }
        return;
    }
    let Some(picker) = app.link_picker.as_mut() else {
        return;
    };
    let target = match key.code {
        KeyCode::Esc | KeyCode::Char('q' | 'l') => {
            app.link_picker = None;
            return;
        }
        KeyCode::Down | KeyCode::Char('j') => {
            picker.move_cursor(true);
            return;
        }
        KeyCode::Up | KeyCode::Char('k') => {
            picker.move_cursor(false);
            return;
        }
        KeyCode::Enter => picker.entries[picker.cursor].target.as_ref(),
        KeyCode::Char(c) => picker
            .entries
            .iter()
            .find(|entry| {
                entry
                    .shortcut
                    .split('/')
                    .any(|shortcut| shortcut.starts_with(c))
            })
            .and_then(|entry| entry.target.as_ref()),
        _ => None,
    };
    if let Some(target) = target {
        let root = picker.root.clone();
        let target = target.clone();
        app.link_picker = None;
        activate_link(app, root, target, tx);
    }
}

pub(super) fn render(f: &mut Frame, picker: &Picker) -> Option<PaneSnapshot> {
    let area = f.area();
    let width = area.width.min(48);
    let height = area.height.min(picker.entries.len() as u16 + 2);
    let popup = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    );
    let block = Block::bordered()
        .border_style(Style::default().fg(ACCENT))
        .title(" Link Picker ");
    let body = block.inner(popup);
    f.render_widget(Clear, popup);
    f.render_widget(block, popup);
    let buffer = f.buffer_mut();
    let scroll = (picker.cursor + 1).saturating_sub(usize::from(body.height));
    let mut hits = Vec::new();
    for (y, (index, entry)) in
        (body.top()..body.bottom()).zip(picker.entries.iter().enumerate().skip(scroll))
    {
        let style = if entry.target.is_none() {
            Style::default().fg(DIM)
        } else if index == picker.cursor {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default()
        };
        let span = Span::styled(format!(" {:<4} {}", entry.shortcut, entry.label), style);
        let (end, _) = buffer.set_span(body.x, y, &span, body.width);
        if let Some(target) = &entry.target {
            hits.push(LinkHit {
                rect: Rect::new(body.x, y, end - body.x, 1),
                root: picker.root.clone(),
                target: target.clone(),
            });
            if picker.copy_paths.contains(target) {
                let suffix = Span::styled(" · copy path", Style::default().fg(DIM));
                buffer.set_span(end, y, &suffix, body.right() - end);
            }
        }
    }
    PaneSnapshot::capture(
        buffer,
        body,
        ContentKey::Links {
            root: picker.root.clone(),
            cursor: picker.cursor,
        },
    )
    .map(|pane| pane.with_links(hits))
}
