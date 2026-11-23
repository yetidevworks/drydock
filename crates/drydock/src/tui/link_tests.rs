use super::*;
use base64::{engine::general_purpose::STANDARD, Engine};
use std::{fs, os::unix::fs::PermissionsExt, path::Path};

fn fixture_git(root: &Path, args: &[&str]) -> String {
    fixture_git_at(root, args, "2000-01-01T12:00:00Z")
}

fn fixture_git_at(root: &Path, args: &[&str], date: &str) -> String {
    let output = std::process::Command::new("git")
        .env("GIT_COMMITTER_DATE", date)
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .unwrap();
    assert!(output.status.success(), "{args:?}: {output:?}");
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn link_fixture() -> (tempfile::TempDir, App) {
    let fixture = tempfile::tempdir().unwrap();
    fixture_git(fixture.path(), &["init", "-b", "main", "--template="]);
    fixture_git(fixture.path(), &["config", "user.name", "Fixture"]);
    fixture_git(
        fixture.path(),
        &["config", "user.email", "fixture@example.org"],
    );
    fixture_git(fixture.path(), &["config", "commit.gpgsign", "false"]);
    fixture_git(fixture.path(), &["config", "tag.gpgsign", "false"]);
    let mut app = App::new(Arc::new(Config::default()));
    app.repos = vec![RepoStatus::new(
        fixture.path().to_owned(),
        "test".into(),
        "fixture".into(),
    )];
    app.reindex();
    app.query.filters.clear();
    app.recompute();
    (fixture, app)
}

fn use_opener(app: &mut App, root: &Path, exit: u8) -> PathBuf {
    let opener = root.join(".git/browser opener");
    fs::write(
        &opener,
        format!(
            "#!/bin/sh\n[ \"$#\" -eq 1 ] || exit 91\nIFS= read -r line && exit 92\n\
             printf '%s' \"$1\" > \"$0.url\"\nprintf 'stdout'\nprintf 'stderr' >&2\nexit {exit}\n"
        ),
    )
    .unwrap();
    fs::set_permissions(&opener, fs::Permissions::from_mode(0o700)).unwrap();
    let url = opener.with_extension("url");
    app.link_opener = Some(opener);
    url
}

fn press(app: &mut App, key: char, tx: &mpsc::UnboundedSender<Input>) {
    handle_key(
        app,
        KeyEvent::new(KeyCode::Char(key), KeyModifiers::NONE),
        tx,
    );
}

async fn receive(rx: &mut mpsc::UnboundedReceiver<Input>) -> Input {
    tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("timed out awaiting TUI event")
        .expect("TUI event channel closed")
}

async fn finish_link(app: &mut App, rx: &mut mpsc::UnboundedReceiver<Input>) {
    loop {
        match receive(rx).await {
            Input::Picker(loaded) => {
                link_picker::on_loaded(app, loaded);
            }
            Input::Link(outcome) => {
                apply_link_outcome(app, outcome);
                break;
            }
            _ => panic!("missing Hosting Deep Link result"),
        }
    }
}

async fn finish_copy(app: &mut App, rx: &mut mpsc::UnboundedReceiver<Input>) {
    let Input::ClipboardStatus(status) = receive(rx).await else {
        panic!("missing clipboard result");
    };
    app.notify(status);
}

#[tokio::test]
async fn w_keeps_the_original_view_bindings() {
    let (_repo, mut app) = link_fixture();
    let (tx, mut rx) = mpsc::unbounded_channel();

    app.mode = Mode::Detail;
    press(&mut app, 'w', &tx);
    assert_eq!(app.active_message(), None);

    app.mode = Mode::Normal;
    press(&mut app, 'w', &tx);
    assert_eq!(app.active_message(), Some("That repo has no remote"));

    let (_history_repo, mut history_app) = link_fixture();
    history::open_now(&mut history_app, false).await;
    assert!(matches!(history_app.mode, Mode::History));
    press(&mut history_app, 'w', &tx);
    assert_eq!(
        history_app.active_message(),
        Some("That repo has no remote")
    );
    assert!(app.clipboard_request.is_none());
    assert!(history_app.clipboard_request.is_none());
    assert!(rx.try_recv().is_err());
}

#[test]
fn the_narrow_footer_keeps_navigation_order_and_only_shows_complete_hints() {
    let mut app = App::new(Arc::new(Config::default()));
    for (mode, width, expected) in [
        (
            Mode::Normal,
            60,
            "j/k move  ⏎ detail  space history  l links  d dirty",
        ),
        (
            Mode::Detail,
            60,
            "esc back  j/k scroll  l links  o finder  t client",
        ),
        (
            Mode::History,
            60,
            "space close  j/k commit  l links  J/K scroll",
        ),
        (Mode::Normal, 40, "j/k move  ⏎ detail  l links"),
    ] {
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(width, 20)).unwrap();
        app.mode = mode;
        draw(&mut terminal, &mut app).unwrap();
        let buffer = terminal.backend().buffer();
        let footer: String = (0..width).map(|x| buffer[(x, 18)].symbol()).collect();
        assert_eq!(footer.trim(), expected);
    }
}

#[tokio::test]
async fn detail_picker_copies_the_repo_path_when_no_remote_is_configured() {
    let (fixture, mut app) = link_fixture();
    app.mode = Mode::Detail;
    let (tx, mut rx) = mpsc::unbounded_channel();
    let clipboard = clipboard::Clipboard::start(true, vec![], tx.clone());
    let mut output = Vec::new();

    press(&mut app, 'l', &tx);
    press(&mut app, 'r', &tx);
    finish_link(&mut app, &mut rx).await;
    send_clipboard(&mut app, &mut output, &clipboard);
    finish_copy(&mut app, &mut rx).await;

    let path = fixture.path().to_string_lossy();
    assert_eq!(
        output,
        format!("\x1b]52;c;{}\x07", STANDARD.encode(path.as_bytes())).as_bytes()
    );
    assert_eq!(
        app.active_message(),
        Some(format!("No web remote available — sent {path} to terminal clipboard").as_str())
    );
}

#[tokio::test]
async fn normal_and_detail_picker_open_the_tracking_repository_homepage() {
    let (fixture, mut app) = link_fixture();
    let root = fixture.path();
    fixture_git(
        root,
        &[
            "remote",
            "add",
            "origin",
            "https://github.com/upstream/project.git",
        ],
    );
    fixture_git(
        root,
        &[
            "remote",
            "add",
            "fork",
            "ssh://notgit@gitlab.example.org:2222/group/team/project.git",
        ],
    );
    fixture_git(root, &["config", "branch.main.remote", "fork"]);
    fixture_git(root, &["config", "branch.main.merge", "refs/heads/topic"]);
    let opened = use_opener(&mut app, root, 0);
    let (tx, mut rx) = mpsc::unbounded_channel();

    let url = "https://gitlab.example.org/group/team/project";
    for mode in [Mode::Normal, Mode::Detail] {
        app.mode = mode;
        press(&mut app, 'l', &tx);
        press(&mut app, 'r', &tx);
        finish_link(&mut app, &mut rx).await;
        assert_eq!(fs::read_to_string(&opened).unwrap(), url);
        assert_eq!(app.active_message(), Some(format!("Opened {url}").as_str()));
        fs::remove_file(&opened).unwrap();
    }
}

#[tokio::test]
async fn history_picker_copies_the_root_for_an_unpushed_commit_on_another_branch() {
    let (fixture, mut app) = link_fixture();
    let root = fixture.path();
    fixture_git(
        root,
        &[
            "remote",
            "add",
            "origin",
            "https://github.com/upstream/project.git",
        ],
    );
    fixture_git(
        root,
        &["remote", "add", "fork", "git@github.com:mine/project.git"],
    );
    fixture_git(root, &["commit", "--allow-empty", "-m", "Pushed base"]);
    fixture_git(root, &["update-ref", "refs/remotes/fork/main", "HEAD"]);
    fixture_git(root, &["branch", "--set-upstream-to", "fork/main", "main"]);
    fixture_git(root, &["checkout", "-b", "other"]);
    fixture_git(
        root,
        &["commit", "--allow-empty", "-m", "Local side branch"],
    );
    fixture_git(root, &["checkout", "main"]);
    app.repos[0].refs = Some(git::probe_refs(root, &app.cfg).await.unwrap());
    let opened = use_opener(&mut app, root, 0);
    history::open_now(&mut app, false).await;

    let (history_tx, mut history_rx) = mpsc::unbounded_channel();
    press(&mut app, 'a', &history_tx);
    let Input::History(loaded) = receive(&mut history_rx).await else {
        panic!("missing History log");
    };
    history::on_loaded(&mut app, *loaded, &history_tx);
    press(&mut app, 'k', &history_tx);

    let (tx, mut rx) = mpsc::unbounded_channel();
    let clipboard = clipboard::Clipboard::start(
        true,
        vec![clipboard::Tool("sh", &["-c", "cat >/dev/null"])],
        tx.clone(),
    );
    let mut output = Vec::new();
    press(&mut app, 'l', &tx);
    press(&mut app, 'c', &tx);
    finish_link(&mut app, &mut rx).await;
    assert!(!opened.exists(), "a local commit must not start a browser");
    assert!(!app.active_message().unwrap_or_default().contains("copied"));
    send_clipboard(&mut app, &mut output, &clipboard);
    finish_copy(&mut app, &mut rx).await;

    let path = root.to_string_lossy();
    assert_eq!(
        output,
        format!("\x1b]52;c;{}\x07", STANDARD.encode(path.as_bytes())).as_bytes()
    );
    assert_eq!(
        app.active_message(),
        Some(format!("Not on remote yet — copied {path}").as_str())
    );
}

#[tokio::test]
async fn history_picker_opens_a_commit_reachable_from_the_selected_remote() {
    let (fixture, mut app) = link_fixture();
    let root = fixture.path();
    fixture_git(
        root,
        &["remote", "add", "mirror", "git@github.com:mine/project.git"],
    );
    fixture_git(root, &["commit", "--allow-empty", "-m", "Selected commit"]);
    let sha = fixture_git(root, &["rev-parse", "HEAD"]);
    fixture_git(root, &["commit", "--allow-empty", "-m", "Remote tip"]);
    fixture_git(
        root,
        &["update-ref", "refs/remotes/mirror/team/topic", "HEAD"],
    );
    fixture_git(root, &["checkout", "--detach", &sha]);
    let opened = use_opener(&mut app, root, 0);
    history::open_now(&mut app, false).await;
    let (tx, mut rx) = mpsc::unbounded_channel();

    for (remote, url) in [
        (
            "git@github.com:mine/project.git",
            format!("https://github.com/mine/project/commit/{sha}"),
        ),
        (
            "http://login:secret@gitlab.example.org:8080/group/project.git",
            format!("http://gitlab.example.org:8080/group/project/-/commit/{sha}"),
        ),
        (
            "ssh://git@github.com:2222/mine/项目 #?%25.git",
            format!("https://github.com/mine/%E9%A1%B9%E7%9B%AE%20%23%3F%25/commit/{sha}"),
        ),
    ] {
        fixture_git(root, &["remote", "set-url", "mirror", remote]);
        press(&mut app, 'l', &tx);
        press(&mut app, 'c', &tx);
        finish_link(&mut app, &mut rx).await;
        assert_eq!(fs::read_to_string(&opened).unwrap(), url);
        assert_eq!(app.active_message(), Some(format!("Opened {url}").as_str()));
        fs::remove_file(&opened).unwrap();
    }
}

#[tokio::test]
async fn history_picker_on_uncommitted_changes_opens_the_history_repos_homepage() {
    let (fixture, mut app) = link_fixture();
    let root = fixture.path();
    fixture_git(
        root,
        &[
            "remote",
            "add",
            "origin",
            "https://forge.example.org/team/project.git",
        ],
    );
    fixture_git(root, &["commit", "--allow-empty", "-m", "Initial commit"]);
    fs::write(root.join("uncommitted"), "local change").unwrap();
    app.repos[0].work = Some(git::probe_work(root, &app.cfg.status).await.unwrap());
    let opened = use_opener(&mut app, root, 0);
    history::open_now(&mut app, false).await;

    // The dashboard may reselect or remove the underlying row while History stays open.
    app.retain_roots(&[]);
    app.recompute();
    let (tx, mut rx) = mpsc::unbounded_channel();
    press(&mut app, 'l', &tx);
    press(&mut app, 'r', &tx);
    finish_link(&mut app, &mut rx).await;
    assert_eq!(
        fs::read_to_string(&opened).unwrap(),
        "https://forge.example.org/team/project"
    );
}

#[tokio::test]
async fn picker_reports_opener_spawn_and_exit_failures() {
    let (fixture, mut app) = link_fixture();
    let root = fixture.path();
    fixture_git(
        root,
        &["remote", "add", "origin", "git@github.com:mine/project.git"],
    );
    let opened = use_opener(&mut app, root, 7);
    let (tx, mut rx) = mpsc::unbounded_channel();

    press(&mut app, 'l', &tx);
    press(&mut app, 'r', &tx);
    finish_link(&mut app, &mut rx).await;
    assert_eq!(
        fs::read_to_string(&opened).unwrap(),
        "https://github.com/mine/project"
    );
    assert_eq!(
        app.active_message(),
        Some("Could not open https://github.com/mine/project: opener exited with exit status: 7")
    );

    app.link_opener = Some(root.join("missing-browser"));
    press(&mut app, 'l', &tx);
    press(&mut app, 'r', &tx);
    finish_link(&mut app, &mut rx).await;
    assert!(app
        .active_message()
        .unwrap()
        .starts_with("Could not open https://github.com/mine/project: "));
}

#[tokio::test]
async fn picker_rejects_local_and_malformed_remotes_and_reports_copy_failure() {
    let (fixture, mut app) = link_fixture();
    let root = fixture.path();
    let opened = use_opener(&mut app, root, 0);
    let (tx, mut rx) = mpsc::unbounded_channel();
    let clipboard = clipboard::Clipboard::start(false, vec![], tx.clone());
    let mut output = Vec::new();

    fixture_git(root, &["remote", "add", "origin", "/tmp/local-repo.git"]);
    for remote in [
        "/tmp/local-repo.git",
        "https:///github.com/repo.git",
        "file:///tmp/local-repo.git",
    ] {
        fixture_git(root, &["remote", "set-url", "origin", remote]);
        press(&mut app, 'l', &tx);
        press(&mut app, 'r', &tx);
        finish_link(&mut app, &mut rx).await;
        assert!(!opened.exists(), "the opener received {remote}");
        send_clipboard(&mut app, &mut output, &clipboard);
        finish_copy(&mut app, &mut rx).await;
        assert_eq!(
            app.active_message(),
            Some("Could not copy: terminal clipboard unavailable")
        );
    }
    assert!(output.is_empty());
}

#[tokio::test]
async fn history_picker_copies_the_root_for_a_commit_on_an_unknown_forge() {
    let (fixture, mut app) = link_fixture();
    let root = fixture.path();
    fixture_git(
        root,
        &[
            "remote",
            "add",
            "origin",
            "https://forge.example.org/team/project.git",
        ],
    );
    fixture_git(root, &["commit", "--allow-empty", "-m", "Initial commit"]);
    fixture_git(root, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
    let opened = use_opener(&mut app, root, 0);
    history::open_now(&mut app, false).await;
    let (tx, mut rx) = mpsc::unbounded_channel();
    let clipboard = clipboard::Clipboard::start(true, vec![], tx.clone());
    let mut output = Vec::new();

    press(&mut app, 'l', &tx);
    press(&mut app, 'c', &tx);
    finish_link(&mut app, &mut rx).await;
    assert!(!opened.exists());
    send_clipboard(&mut app, &mut output, &clipboard);
    finish_copy(&mut app, &mut rx).await;
    let path = root.to_string_lossy();
    assert_eq!(
        output,
        format!("\x1b]52;c;{}\x07", STANDARD.encode(path.as_bytes())).as_bytes()
    );
    assert_eq!(
        app.active_message(),
        Some(format!("Commit links are unavailable for this Git host — sent {path} to terminal clipboard").as_str())
    );
}

fn dashboard() -> Terminal<ratatui::backend::TestBackend> {
    Terminal::new(ratatui::backend::TestBackend::new(100, 20)).unwrap()
}

fn frame_text(terminal: &Terminal<ratatui::backend::TestBackend>) -> String {
    terminal
        .backend()
        .buffer()
        .content
        .chunks(100)
        .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn link_picker_opens_the_homepage_and_returns_to_detail() {
    let (fixture, mut app) = link_fixture();
    let root = fixture.path();
    fixture_git(
        root,
        &["remote", "add", "origin", "git@github.com:mine/project.git"],
    );
    fixture_git(root, &["commit", "--allow-empty", "-m", "Initial commit"]);
    for index in 0..24 {
        fs::write(root.join(format!("local-{index:02}")), "local change").unwrap();
    }
    app.repos[0].refs = Some(git::probe_refs(root, &app.cfg).await.unwrap());
    app.repos[0].work = Some(git::probe_work(root, &app.cfg.status).await.unwrap());
    let opened = use_opener(&mut app, root, 0);
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut terminal = dashboard();
    handle_key(
        &mut app,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        &tx,
    );
    draw(&mut terminal, &mut app).unwrap();
    press(&mut app, 'j', &tx);
    draw(&mut terminal, &mut app).unwrap();
    let scroll = app.detail_scroll;
    assert!(scroll > 0);
    let detail_body = |terminal: &Terminal<ratatui::backend::TestBackend>| {
        frame_text(terminal)
            .lines()
            .take(18)
            .collect::<Vec<_>>()
            .join("\n")
    };
    let detail = detail_body(&terminal);
    let selected = app.selected_root.clone();

    press(&mut app, 'l', &tx);
    draw(&mut terminal, &mut app).unwrap();
    assert!(frame_text(&terminal).contains("Link Picker"));
    assert!(app.mode == Mode::Detail);

    press(&mut app, 'r', &tx);
    finish_link(&mut app, &mut rx).await;
    assert_eq!(
        fs::read_to_string(opened).unwrap(),
        "https://github.com/mine/project"
    );
    draw(&mut terminal, &mut app).unwrap();
    assert!(!frame_text(&terminal).contains("Link Picker"));
    assert_eq!(app.detail_scroll, scroll);
    assert_eq!(app.selected_root, selected);
    assert!(app.mode == Mode::Detail);
    assert_eq!(detail_body(&terminal), detail);
}

#[tokio::test]
async fn picker_compares_the_described_tag_to_the_full_remote_reachable_head() {
    let (fixture, mut app) = link_fixture();
    let root = fixture.path();
    fixture_git(
        root,
        &["remote", "add", "origin", "git@github.com:mine/project.git"],
    );
    fixture_git(root, &["commit", "--allow-empty", "-m", "Release"]);
    fixture_git(root, &["tag", "release/v1.0.0#%猫"]);
    fixture_git(root, &["checkout", "-b", "other"]);
    fixture_git(root, &["commit", "--allow-empty", "-m", "Other branch"]);
    fixture_git_at(
        root,
        &["tag", "-a", "-m", "Newer release", "v2.0.0"],
        "2000-02-01T12:00:00Z",
    );
    fixture_git(root, &["checkout", "main"]);
    fixture_git(root, &["commit", "--allow-empty", "-m", "New work"]);
    fixture_git(root, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
    let head = fixture_git(root, &["rev-parse", "HEAD"]);
    app.repos[0].refs = Some(git::probe_refs(root, &app.cfg).await.unwrap());
    assert_eq!(
        app.repos[0]
            .refs
            .as_ref()
            .unwrap()
            .described_tag
            .as_ref()
            .unwrap()
            .name,
        "release/v1.0.0#%猫"
    );
    assert_eq!(
        app.repos[0]
            .refs
            .as_ref()
            .unwrap()
            .newest_tag
            .as_ref()
            .unwrap()
            .name,
        "v2.0.0"
    );
    let opened = use_opener(&mut app, root, 0);
    let (tx, mut rx) = mpsc::unbounded_channel();

    press(&mut app, 'l', &tx);
    press(&mut app, 'c', &tx);
    finish_link(&mut app, &mut rx).await;
    assert_eq!(
        fs::read_to_string(opened).unwrap(),
        format!("https://github.com/mine/project/compare/release/v1.0.0%23%25%E7%8C%AB...{head}")
    );
}

fn mouse(
    terminal: &mut Terminal<ratatui::backend::TestBackend>,
    app: &mut App,
    kind: MouseEventKind,
    position: Position,
    tx: &mpsc::UnboundedSender<Input>,
) -> bool {
    handle_terminal_event(
        terminal,
        app,
        TermEvent::Mouse(MouseEvent {
            kind,
            column: position.x,
            row: position.y,
            modifiers: KeyModifiers::NONE,
        }),
        tx,
    )
    .unwrap()
}

fn locate(terminal: &Terminal<ratatui::backend::TestBackend>, text: &str) -> Position {
    for (y, row) in frame_text(terminal).lines().enumerate() {
        if let Some(start) = row.find(text) {
            let column = unicode_width::UnicodeWidthStr::width(&row[..start]);
            return Position::new(column as u16, y as u16);
        }
    }
    panic!("missing {text:?} on screen:\n{}", frame_text(terminal));
}

#[tokio::test]
async fn picker_click_opens_the_tracking_branch_and_only_hovered_text_is_underlined() {
    let (fixture, mut app) = link_fixture();
    let root = fixture.path();
    fixture_git(
        root,
        &[
            "remote",
            "add",
            "origin",
            "git@github.com:upstream/project.git",
        ],
    );
    fixture_git(
        root,
        &[
            "remote",
            "add",
            "fork",
            "ssh://git@gitlab.example.org:2222/mine/project.git",
        ],
    );
    fixture_git(root, &["commit", "--allow-empty", "-m", "Remote work"]);
    fixture_git(
        root,
        &["update-ref", "refs/remotes/fork/team/topic#%猫", "HEAD"],
    );
    fixture_git(
        root,
        &["branch", "--set-upstream-to", "fork/team/topic#%猫", "main"],
    );
    fixture_git(root, &["commit", "--allow-empty", "-m", "Local work"]);
    app.repos[0].refs = Some(git::probe_refs(root, &app.cfg).await.unwrap());
    let opened = use_opener(&mut app, root, 0);
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut terminal = dashboard();

    press(&mut app, 'l', &tx);
    draw(&mut terminal, &mut app).unwrap();
    let branch = locate(&terminal, "Branch:");
    assert!(mouse(
        &mut terminal,
        &mut app,
        MouseEventKind::Moved,
        branch,
        &tx
    ));
    draw(&mut terminal, &mut app).unwrap();
    let underlined = ratatui::style::Modifier::UNDERLINED;
    assert!(terminal.backend().buffer()[(branch.x, branch.y)]
        .modifier
        .contains(underlined));
    assert!(!terminal.backend().buffer()[(0, branch.y)]
        .modifier
        .contains(underlined));
    assert!(!mouse(
        &mut terminal,
        &mut app,
        MouseEventKind::Moved,
        Position::new(branch.x + 1, branch.y),
        &tx
    ));

    mouse(
        &mut terminal,
        &mut app,
        MouseEventKind::Down(MouseButton::Left),
        branch,
        &tx,
    );
    mouse(
        &mut terminal,
        &mut app,
        MouseEventKind::Up(MouseButton::Left),
        branch,
        &tx,
    );
    finish_link(&mut app, &mut rx).await;
    assert_eq!(
        fs::read_to_string(opened).unwrap(),
        "https://gitlab.example.org/mine/project/-/tree/team/topic%23%25%E7%8C%AB"
    );
}

#[tokio::test]
async fn picker_shortcuts_open_the_forges_release_reviews_issues_and_automation_pages() {
    let (fixture, mut app) = link_fixture();
    let root = fixture.path();
    fixture_git(
        root,
        &["remote", "add", "origin", "git@github.com:mine/project.git"],
    );
    fixture_git(root, &["commit", "--allow-empty", "-m", "Published"]);
    fixture_git(root, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
    let opened = use_opener(&mut app, root, 0);
    let (tx, mut rx) = mpsc::unbounded_channel();

    press(&mut app, 'l', &tx);
    press(&mut app, 't', &tx);
    finish_link(&mut app, &mut rx).await;
    assert_eq!(
        fs::read_to_string(&opened).unwrap(),
        "https://github.com/mine/project/releases"
    );
    fixture_git(root, &["tag", "-a", "-m", "Release", "v1.0.0"]);
    app.repos[0].refs = Some(git::probe_refs(root, &app.cfg).await.unwrap());

    for (remote, base, suffixes) in [
        (
            "git@github.com:mine/project.git",
            "https://github.com/mine/project",
            ["releases/tag/v1.0.0", "pulls", "issues", "actions"],
        ),
        (
            "https://gitlab.example.org/mine/project.git",
            "https://gitlab.example.org/mine/project/-",
            ["releases/v1.0.0", "merge_requests", "issues", "pipelines"],
        ),
    ] {
        fixture_git(root, &["remote", "set-url", "origin", remote]);
        for (key, suffix) in ['t', 'p', 'i', 'a'].into_iter().zip(suffixes) {
            press(&mut app, 'l', &tx);
            press(&mut app, key, &tx);
            finish_link(&mut app, &mut rx).await;
            assert_eq!(
                fs::read_to_string(&opened).unwrap(),
                format!("{base}/{suffix}")
            );
        }
    }
}

#[tokio::test]
async fn picker_marks_local_branches_and_copies_the_root_if_the_exact_ref_is_missing() {
    let (fixture, mut app) = link_fixture();
    let root = fixture.path();
    fixture_git(
        root,
        &["remote", "add", "origin", "git@github.com:mine/project.git"],
    );
    fixture_git(root, &["commit", "--allow-empty", "-m", "Local only"]);
    fixture_git(
        root,
        &["update-ref", "refs/remotes/origin/main/child", "HEAD"],
    );
    app.repos[0].refs = Some(git::probe_refs(root, &app.cfg).await.unwrap());
    let opened = use_opener(&mut app, root, 0);
    let (tx, mut rx) = mpsc::unbounded_channel();
    let clipboard = clipboard::Clipboard::start(true, vec![], tx.clone());
    let mut terminal = dashboard();

    press(&mut app, 'l', &tx);
    // The preview arrives asynchronously. Dispatch it through the picker
    // handler before asserting what the rendered frame tells the user.
    let Input::Picker(loaded) = receive(&mut rx).await else {
        panic!("missing Link Picker preview");
    };
    link_picker::on_loaded(&mut app, loaded);
    draw(&mut terminal, &mut app).unwrap();
    assert!(frame_text(&terminal)
        .lines()
        .any(|row| row.contains("Branch: main") && row.contains("copy path")));

    // Compare has no base tag, so j skips it and Enter selects the branch.
    press(&mut app, 'j', &tx);
    handle_key(
        &mut app,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        &tx,
    );
    finish_link(&mut app, &mut rx).await;
    assert!(!opened.exists());
    let mut output = Vec::new();
    send_clipboard(&mut app, &mut output, &clipboard);
    finish_copy(&mut app, &mut rx).await;
    let path = root.to_string_lossy();
    assert_eq!(
        output,
        format!("\x1b]52;c;{}\x07", STANDARD.encode(path.as_bytes())).as_bytes()
    );
    assert_eq!(
        app.active_message(),
        Some(format!("Not on remote yet — sent {path} to terminal clipboard").as_str())
    );
}

#[tokio::test]
async fn picker_is_modal_and_returns_to_the_same_history_commit_and_scroll() {
    let (fixture, mut app) = link_fixture();
    let root = fixture.path();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut terminal = dashboard();
    draw(&mut terminal, &mut app).unwrap();

    fs::write(
        root.join("many-lines"),
        (0..100)
            .map(|line| format!("line {line:03}\n"))
            .collect::<String>(),
    )
    .unwrap();
    fixture_git(root, &["add", "many-lines"]);
    fixture_git(root, &["commit", "-m", "Oldest commit"]);
    let oldest = fixture_git(root, &["rev-parse", "HEAD"]);
    for index in 1..=20 {
        fixture_git(
            root,
            &[
                "commit",
                "--allow-empty",
                "-m",
                &format!("Newer commit {index:02}"),
            ],
        );
    }
    fixture_git(
        root,
        &["remote", "add", "origin", "git@github.com:mine/project.git"],
    );
    fixture_git(root, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
    let opened = use_opener(&mut app, root, 0);
    press(&mut app, ' ', &tx);
    for _ in 0..2 {
        let Input::History(loaded) = receive(&mut rx).await else {
            panic!("missing History content");
        };
        history::on_loaded(&mut app, *loaded, &tx);
    }
    handle_key(
        &mut app,
        KeyEvent::new(KeyCode::End, KeyModifiers::NONE),
        &tx,
    );
    let Input::History(loaded) = receive(&mut rx).await else {
        panic!("missing commit change");
    };
    history::on_loaded(&mut app, *loaded, &tx);
    handle_key(
        &mut app,
        KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE),
        &tx,
    );
    draw(&mut terminal, &mut app).unwrap();
    let history_body = |terminal: &Terminal<ratatui::backend::TestBackend>| {
        frame_text(terminal)
            .lines()
            .take(18)
            .collect::<Vec<_>>()
            .join("\n")
    };
    let before = history_body(&terminal);
    assert!(before.contains("Oldest commit"));
    assert!(before.contains("line 0"), "{before}");

    for close in [KeyCode::Esc, KeyCode::Char('q'), KeyCode::Char('l')] {
        press(&mut app, 'l', &tx);
        draw(&mut terminal, &mut app).unwrap();
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL),
            &tx,
        );
        draw(&mut terminal, &mut app).unwrap();
        let commit = locate(&terminal, "c    Commit");
        let outside = Position::new(5, 4);
        mouse(
            &mut terminal,
            &mut app,
            MouseEventKind::Down(MouseButton::Left),
            outside,
            &tx,
        );
        mouse(
            &mut terminal,
            &mut app,
            MouseEventKind::Up(MouseButton::Left),
            outside,
            &tx,
        );
        mouse(
            &mut terminal,
            &mut app,
            MouseEventKind::Down(MouseButton::Left),
            commit,
            &tx,
        );
        mouse(
            &mut terminal,
            &mut app,
            MouseEventKind::Drag(MouseButton::Left),
            Position::new(commit.x + 1, commit.y),
            &tx,
        );
        mouse(
            &mut terminal,
            &mut app,
            MouseEventKind::Up(MouseButton::Left),
            commit,
            &tx,
        );
        press(&mut app, 'j', &tx);
        assert!(app.mode == Mode::History);
        handle_key(&mut app, KeyEvent::new(close, KeyModifiers::NONE), &tx);
        draw(&mut terminal, &mut app).unwrap();
        assert_eq!(history_body(&terminal), before);
        assert!(!opened.exists());
    }

    press(&mut app, 'l', &tx);
    press(&mut app, 'c', &tx);
    finish_link(&mut app, &mut rx).await;
    assert_eq!(
        fs::read_to_string(opened).unwrap(),
        format!("https://github.com/mine/project/commit/{oldest}")
    );
    draw(&mut terminal, &mut app).unwrap();
    assert_eq!(history_body(&terminal), before);
}

#[tokio::test]
async fn stale_picker_previews_cannot_replace_a_reopened_pickers_availability() {
    let (fixture, mut app) = link_fixture();
    let root = fixture.path();
    fixture_git(
        root,
        &["remote", "add", "origin", "git@github.com:mine/project.git"],
    );
    fixture_git(root, &["commit", "--allow-empty", "-m", "Local work"]);
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut terminal = dashboard();

    press(&mut app, 'l', &tx);
    let Input::Picker(first) = receive(&mut rx).await else {
        panic!("missing first Link Picker preview");
    };
    press(&mut app, 'q', &tx);
    fixture_git(root, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
    press(&mut app, 'l', &tx);
    let Input::Picker(second) = receive(&mut rx).await else {
        panic!("missing reopened Link Picker preview");
    };
    link_picker::on_loaded(&mut app, second);
    assert!(!link_picker::on_loaded(&mut app, first));
    draw(&mut terminal, &mut app).unwrap();
    assert!(frame_text(&terminal).contains("Branch: main"));
    assert!(!frame_text(&terminal).contains("copy path"));
}

#[test]
fn link_picker_shortcut_preserves_search_help_and_columns_behavior() {
    let (_fixture, mut app) = link_fixture();
    let (tx, _rx) = mpsc::unbounded_channel();

    press(&mut app, '/', &tx);
    press(&mut app, 'l', &tx);
    assert!(app.mode == Mode::Search);
    assert_eq!(app.search_input, "l");
    handle_key(
        &mut app,
        KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
        &tx,
    );
    press(&mut app, '?', &tx);
    press(&mut app, 'l', &tx);
    assert!(app.mode == Mode::Help);
    assert!(app.link_picker.is_none());
    handle_key(
        &mut app,
        KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
        &tx,
    );
    press(&mut app, 'C', &tx);
    press(&mut app, 'l', &tx);
    assert!(app.mode == Mode::Columns);
    assert!(app.link_picker.is_none());
}
