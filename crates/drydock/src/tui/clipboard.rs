//! Cross-platform clipboard support.
//!
//! Copies straight to OSC 52 when running over SSH so text reaches the local
//! terminal's clipboard instead of a remote display server. Locally, prefers
//! the platform's clipboard tool (`pbcopy`, `wl-copy`, `xclip`, `xsel`) and
//! falls back to OSC 52 when no tool is available or none succeeds.

use std::env;
use std::io::{self, Write};
use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use super::Input;

/// Xterm's 100,000-byte OSC 52 payload limit, in raw bytes before base64
/// expansion (74,994 -> 99,992 base64 bytes).
const OSC52_MAX_BYTES: usize = 74_994;

/// A local clipboard tool finishes in milliseconds when its display server is
/// alive; cap how long a wedged X11 or Wayland socket can stall a copy before
/// falling back to OSC 52.
const TOOL_TIMEOUT: Duration = Duration::from_secs(3);

/// Total time quitting will wait for an in-flight local clipboard tool before
/// cancelling it and falling back to OSC 52.
const FLUSH_TIMEOUT: Duration = Duration::from_millis(500);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Tool(&'static str, &'static [&'static str]);

enum ToolError {
    Missing,
    Failed(String),
}

struct Env {
    macos: bool,
    linux: bool,
    ssh: bool,
    wayland: bool,
    x11: bool,
    osc52: bool,
}

impl Env {
    fn current() -> Self {
        Self::from_lookup(
            cfg!(target_os = "macos"),
            cfg!(target_os = "linux"),
            |name| env::var(name).ok(),
        )
    }

    fn from_lookup(macos: bool, linux: bool, lookup: impl Fn(&str) -> Option<String>) -> Self {
        let present = |name: &str| lookup(name).is_some_and(|v| !v.is_empty());
        let ssh = present("SSH_TTY") || present("SSH_CONNECTION") || present("SSH_CLIENT");
        let osc52 = lookup("TERM").is_some_and(|t| !matches!(t.as_str(), "" | "dumb" | "linux"));
        Self {
            macos,
            linux,
            ssh,
            wayland: present("WAYLAND_DISPLAY"),
            x11: present("DISPLAY"),
            osc52,
        }
    }

    fn local_tools(&self) -> Vec<Tool> {
        if self.ssh {
            return Vec::new();
        }
        let mut tools = Vec::new();
        if self.macos {
            tools.push(Tool("pbcopy", &[]));
        } else if self.linux {
            if self.wayland {
                tools.push(Tool("wl-copy", &["--type", "text/plain;charset=utf-8"]));
            }
            if self.x11 {
                tools.extend([
                    Tool("xclip", &["-selection", "clipboard", "-in"]),
                    Tool("xsel", &["--clipboard", "--input"]),
                ]);
            }
        }
        tools
    }
}

/// The result of trying local clipboard tools on the background worker.
pub(super) struct Outcome {
    seq: u64,
    result: LocalResult,
}

enum LocalResult {
    Copied(String),
    Fallback {
        text: String,
        local_error: Option<String>,
    },
}

pub(super) struct Clipboard {
    osc52: bool,
    /// Monotonic sequence number for issued copies. Stale outcomes from an
    /// earlier copy are dropped so a delayed OSC 52 fallback can never
    /// overwrite a newer copy that already succeeded.
    latest: u64,
    /// Text of the latest copy while its local tool is still in flight, so
    /// quitting under a stalled tool can cancel and fall back to OSC 52 once.
    pending: Option<String>,
    /// Present only when local tools exist for this environment; over SSH or on
    /// a headless session, [`copy`] writes OSC 52 directly without a worker.
    local: Option<mpsc::UnboundedSender<(u64, String)>>,
    worker: Option<JoinHandle<()>>,
}

impl Clipboard {
    pub(super) fn for_terminal(events: mpsc::UnboundedSender<Input>) -> Self {
        let env = Env::current();
        Self::start(env.osc52, env.local_tools(), events)
    }

    fn start(osc52: bool, tools: Vec<Tool>, events: mpsc::UnboundedSender<Input>) -> Self {
        let (local, worker) = if tools.is_empty() {
            (None, None)
        } else {
            let (tx, mut rx) = mpsc::unbounded_channel::<(u64, String)>();
            // Serialise local writes so a slower copy cannot overwrite a newer one.
            let worker = tokio::spawn(async move {
                while let Some(mut next) = rx.recv().await {
                    // If newer copies queued up while the previous tool ran,
                    // skip straight to the latest one.
                    while let Ok(newer) = rx.try_recv() {
                        next = newer;
                    }
                    let (seq, text) = next;
                    let result = match copy_local(&tools, &text).await {
                        Ok(true) => LocalResult::Copied(text),
                        Ok(false) => LocalResult::Fallback {
                            text,
                            local_error: None,
                        },
                        Err(err) => LocalResult::Fallback {
                            text,
                            local_error: Some(err),
                        },
                    };
                    if events
                        .send(Input::Clipboard(Outcome { seq, result }))
                        .is_err()
                    {
                        break;
                    }
                }
            });
            (Some(tx), Some(worker))
        };
        Self {
            osc52,
            latest: 0,
            pending: None,
            local,
            worker,
        }
    }

    /// Issue a copy. Writes OSC 52 immediately and returns a status line when
    /// no local tool is configured (for example over SSH), or queues the local
    /// worker and returns `None` until it reports back.
    pub(super) fn copy(&mut self, backend: &mut impl Write, text: String) -> Option<String> {
        self.latest += 1;
        if let Some(local) = &self.local {
            self.pending = Some(text.clone());
            let _ = local.send((self.latest, text));
            return None;
        }
        Some(self.copy_via_osc52(backend, &text, None))
    }

    /// Complete a background local copy, falling back to OSC 52 on the event
    /// loop thread so writes never race with terminal drawing. Stale outcomes
    /// from superseded copies are ignored.
    pub(super) fn finish(&mut self, backend: &mut impl Write, outcome: Outcome) -> Option<String> {
        if outcome.seq != self.latest {
            return None;
        }
        self.pending = None;
        let status = match outcome.result {
            LocalResult::Copied(text) => format!("Copied {text}"),
            LocalResult::Fallback { text, local_error } => {
                self.copy_via_osc52(backend, &text, local_error)
            }
        };
        Some(status)
    }

    /// Finish any in-flight local copy (and its OSC 52 fallback, if needed)
    /// before leaving the alternate screen, bounded so a stalled display
    /// server cannot delay quitting.
    pub(super) async fn flush(
        self,
        backend: &mut impl Write,
        events: &mut mpsc::UnboundedReceiver<Input>,
    ) {
        self.flush_within(backend, events, FLUSH_TIMEOUT).await;
    }

    async fn flush_within(
        mut self,
        backend: &mut impl Write,
        events: &mut mpsc::UnboundedReceiver<Input>,
        limit: Duration,
    ) {
        drop(self.local.take());
        self.drain_events(backend, events);
        if let Some(mut worker) = self.worker.take() {
            if self.pending.is_none() {
                worker.abort();
                return;
            }
            if tokio::time::timeout(limit, &mut worker).await.is_err() {
                worker.abort();
            }
            self.drain_events(backend, events);
        }
        if let Some(text) = self.pending.take() {
            let _ = write_osc52(backend, &text, self.osc52);
        }
    }

    fn drain_events(
        &mut self,
        backend: &mut impl Write,
        events: &mut mpsc::UnboundedReceiver<Input>,
    ) {
        while let Ok(input) = events.try_recv() {
            if let Input::Clipboard(outcome) = input {
                let _ = self.finish(backend, outcome);
            }
        }
    }

    fn copy_via_osc52(
        &self,
        backend: &mut impl Write,
        text: &str,
        local_error: Option<String>,
    ) -> String {
        // OSC 52 has no acknowledgement, so be honest that it was sent to the
        // terminal rather than confirmed by a local pasteboard tool.
        match write_osc52(backend, text, self.osc52) {
            Ok(()) => format!("Sent {text} to terminal clipboard"),
            Err(osc_err) => format!("Could not copy: {}", local_error.unwrap_or(osc_err)),
        }
    }
}

async fn copy_local(tools: &[Tool], text: &str) -> Result<bool, String> {
    let mut failure = None;
    for &tool in tools {
        match tokio::time::timeout(TOOL_TIMEOUT, run_tool(tool, text)).await {
            Ok(Ok(())) => return Ok(true),
            Ok(Err(ToolError::Missing)) => {}
            Ok(Err(ToolError::Failed(reason))) => failure = Some(reason),
            Err(_) => failure = Some(format!("{} timed out", tool.0)),
        }
    }
    failure.map_or(Ok(false), Err)
}

async fn run_tool(Tool(program, args): Tool, text: &str) -> Result<(), ToolError> {
    // Redirect stdout and stderr to null: xclip and wl-copy fork a background
    // helper that inherits them, and piped descriptors would hang `wait`.
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|err| match err.kind() {
            io::ErrorKind::NotFound => ToolError::Missing,
            _ => ToolError::Failed(format!("could not start {program}: {err}")),
        })?;
    let mut stdin = child.stdin.take().expect("clipboard tool has piped stdin");
    let written = stdin.write_all(text.as_bytes()).await;
    drop(stdin);
    // Reap the child even when writing stdin failed.
    let status = child
        .wait()
        .await
        .map_err(|err| ToolError::Failed(format!("could not wait for {program}: {err}")))?;
    if !status.success() {
        return Err(ToolError::Failed(format!("{program} exited with {status}")));
    }
    written.map_err(|err| ToolError::Failed(format!("could not write to {program}: {err}")))
}

fn write_osc52(backend: &mut impl Write, text: &str, available: bool) -> Result<(), String> {
    if text.len() > OSC52_MAX_BYTES {
        return Err(format!(
            "terminal clipboard limit is {OSC52_MAX_BYTES} bytes"
        ));
    }
    if !available {
        return Err("terminal clipboard is unavailable".to_string());
    }
    let payload = encode_base64(text.as_bytes());
    write!(backend, "\x1b]52;c;{payload}\x07")
        .and_then(|()| backend.flush())
        .map_err(|err| format!("terminal clipboard write failed: {err}"))
}

fn encode_base64(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((n >> 18) & 0x3f) as usize] as char);
        out.push(TABLE[((n >> 12) & 0x3f) as usize] as char);
        out.push(if chunk.len() > 1 {
            TABLE[((n >> 6) & 0x3f) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[(n & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_from(macos: bool, linux: bool, vars: &[(&str, &str)]) -> Env {
        Env::from_lookup(macos, linux, |name| {
            vars.iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| (*v).to_string())
        })
    }

    #[test]
    fn base64_matches_rfc_4648_vectors() {
        for (input, expected) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
            ("中文", "5Lit5paH"),
        ] {
            assert_eq!(encode_base64(input.as_bytes()), expected, "{input:?}");
        }
    }

    #[test]
    fn ssh_skips_local_tools_even_when_display_or_macos_is_available() {
        let mac_ssh = env_from(
            true,
            false,
            &[("SSH_CONNECTION", "1 2 3 4"), ("TERM", "xterm-256color")],
        );
        assert!(mac_ssh.local_tools().is_empty());
        assert!(mac_ssh.osc52);

        let linux_ssh = env_from(
            false,
            true,
            &[
                ("SSH_TTY", "/dev/pts/0"),
                ("WAYLAND_DISPLAY", "wayland-0"),
                ("DISPLAY", ":0"),
                ("TERM", "tmux-256color"),
            ],
        );
        assert!(linux_ssh.local_tools().is_empty());
        assert!(linux_ssh.osc52);
    }

    #[test]
    fn local_tool_order_matches_the_session() {
        let mac = env_from(true, false, &[("TERM", "xterm-256color")]);
        assert_eq!(
            mac.local_tools().iter().map(|t| t.0).collect::<Vec<_>>(),
            ["pbcopy"]
        );

        let wayland_and_x11 = env_from(
            false,
            true,
            &[
                ("WAYLAND_DISPLAY", "wayland-0"),
                ("DISPLAY", ":0"),
                ("TERM", "foot"),
            ],
        );
        assert_eq!(
            wayland_and_x11
                .local_tools()
                .iter()
                .map(|t| t.0)
                .collect::<Vec<_>>(),
            ["wl-copy", "xclip", "xsel"]
        );

        let x11_only = env_from(false, true, &[("DISPLAY", ":0")]);
        assert_eq!(
            x11_only
                .local_tools()
                .iter()
                .map(|t| t.0)
                .collect::<Vec<_>>(),
            ["xclip", "xsel"]
        );
        assert!(!x11_only.osc52, "missing TERM disables OSC 52");

        let linux_vt = env_from(false, true, &[("TERM", "linux")]);
        assert!(linux_vt.local_tools().is_empty());
        assert!(!linux_vt.osc52);
    }

    #[test]
    fn osc52_writes_a_valid_sequence_and_checks_limits() {
        let mut out = Vec::new();
        write_osc52(&mut out, "/tmp/repo", true).unwrap();
        assert_eq!(out, b"\x1b]52;c;L3RtcC9yZXBv\x07");

        let mut out = Vec::new();
        assert!(write_osc52(&mut out, "hi", false).is_err());
        assert!(out.is_empty());

        let too_big = "a".repeat(OSC52_MAX_BYTES + 1);
        assert!(write_osc52(&mut out, &too_big, true).is_err());
        assert!(out.is_empty());
    }

    #[tokio::test]
    async fn ssh_copies_directly_via_osc52() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut clipboard = Clipboard::start(true, Vec::new(), tx);
        let mut backend = Vec::new();

        let status = clipboard.copy(&mut backend, "/tmp/repo".into());
        assert_eq!(
            status.as_deref(),
            Some("Sent /tmp/repo to terminal clipboard")
        );
        assert_eq!(backend, b"\x1b]52;c;L3RtcC9yZXBv\x07");
        assert!(rx.try_recv().is_err(), "no background worker needed");
    }

    #[tokio::test]
    async fn a_working_local_tool_copies_without_writing_osc52() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let tool = Tool("sh", &["-c", "test \"$(cat)\" = \"/tmp/repo\""]);
        let mut clipboard = Clipboard::start(true, vec![tool], tx);
        let mut backend = Vec::new();

        assert_eq!(clipboard.copy(&mut backend, "/tmp/repo".into()), None);
        let Some(Input::Clipboard(outcome)) = rx.recv().await else {
            panic!("expected clipboard outcome");
        };
        let status = clipboard.finish(&mut backend, outcome);
        assert_eq!(status.as_deref(), Some("Copied /tmp/repo"));
        assert!(backend.is_empty(), "local copy should not emit OSC 52");
    }

    #[tokio::test]
    async fn a_missing_or_failing_local_tool_falls_back_to_osc52() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let tools = vec![
            Tool("drydock-no-such-clipboard-tool", &[]),
            Tool("false", &[]),
        ];
        let mut clipboard = Clipboard::start(true, tools.clone(), tx.clone());
        let mut backend = Vec::new();

        clipboard.copy(&mut backend, "/tmp/repo".into());
        let Some(Input::Clipboard(outcome)) = rx.recv().await else {
            panic!("expected clipboard outcome");
        };
        let status = clipboard.finish(&mut backend, outcome);
        assert_eq!(
            status.as_deref(),
            Some("Sent /tmp/repo to terminal clipboard")
        );
        assert_eq!(backend, b"\x1b]52;c;L3RtcC9yZXBv\x07");

        // When OSC 52 is also unavailable, report the local tool's failure.
        let mut no_osc52 = Clipboard::start(false, tools, tx);
        let mut backend = Vec::new();
        no_osc52.copy(&mut backend, "/tmp/repo".into());
        let Some(Input::Clipboard(outcome)) = rx.recv().await else {
            panic!("expected clipboard outcome");
        };
        let status = no_osc52.finish(&mut backend, outcome).unwrap();
        assert!(
            status.starts_with("Could not copy: false exited"),
            "{status}"
        );
        assert!(backend.is_empty());
    }

    // If an earlier copy fails locally and queues an OSC 52 fallback, a newer
    // copy that succeeds locally must not be overwritten when the stale
    // fallback reaches the UI loop.
    #[tokio::test]
    async fn a_stale_fallback_does_not_overwrite_a_newer_local_copy() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let tool = Tool("sh", &["-c", "test \"$(cat)\" = \"/tmp/new\""]);
        let mut clipboard = Clipboard::start(true, vec![tool], tx);
        let mut backend = Vec::new();

        clipboard.copy(&mut backend, "/tmp/old".into());
        let Some(Input::Clipboard(stale)) = rx.recv().await else {
            panic!("expected stale fallback outcome");
        };

        clipboard.copy(&mut backend, "/tmp/new".into());
        let Some(Input::Clipboard(fresh)) = rx.recv().await else {
            panic!("expected fresh local outcome");
        };

        assert_eq!(clipboard.finish(&mut backend, stale), None);
        assert!(
            backend.is_empty(),
            "superseded fallback must not write OSC 52"
        );
        assert_eq!(
            clipboard.finish(&mut backend, fresh).as_deref(),
            Some("Copied /tmp/new")
        );
        assert!(backend.is_empty());
    }

    #[tokio::test]
    async fn flushing_on_exit_writes_a_pending_fallback() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let tools = vec![Tool("drydock-no-such-clipboard-tool", &[])];
        let mut clipboard = Clipboard::start(true, tools, tx);
        let mut backend = Vec::new();

        clipboard.copy(&mut backend, "/tmp/repo".into());
        clipboard.flush(&mut backend, &mut rx).await;
        assert_eq!(backend, b"\x1b]52;c;L3RtcC9yZXBv\x07");
    }

    #[tokio::test]
    async fn flushing_on_exit_bounds_a_stalled_tool_and_falls_back_to_latest() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let tools = vec![Tool("sleep", &["10"]), Tool("sleep", &["10"])];
        let mut clipboard = Clipboard::start(true, tools, tx);
        let mut backend = Vec::new();

        clipboard.copy(&mut backend, "old".into());
        clipboard.copy(&mut backend, "new".into());

        let started = std::time::Instant::now();
        clipboard
            .flush_within(&mut backend, &mut rx, Duration::from_millis(50))
            .await;

        assert!(
            started.elapsed() < Duration::from_secs(1),
            "flush took {:?}",
            started.elapsed()
        );
        assert_eq!(backend, b"\x1b]52;c;bmV3\x07");
    }
}
