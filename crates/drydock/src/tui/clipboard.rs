use std::{
    env,
    io::{self, Write},
    process::Stdio,
};

use base64::{display::Base64Display, engine::general_purpose::STANDARD};
use tokio::{io::AsyncWriteExt, process::Command, sync::mpsc};

use super::Input;

const OSC52_MAX_BYTES: usize = 74_994;

pub(super) struct Request {
    text: String,
    label: String,
    reason: Option<&'static str>,
}

impl Request {
    pub(super) fn new(text: String, label: String) -> Self {
        Self {
            text,
            label,
            reason: None,
        }
    }

    pub(super) fn with_reason(text: String, reason: &'static str) -> Self {
        Self {
            label: text.clone(),
            text,
            reason: Some(reason),
        }
    }

    fn success(&self, local: bool) -> String {
        let label = &self.label;
        match (self.reason, local) {
            (None, true) => format!("Copied {label}"),
            (None, false) => format!("Sent {label} to terminal clipboard"),
            (Some(reason), true) => format!("{reason} — copied {label}"),
            (Some(reason), false) => format!("{reason} — sent {label} to terminal clipboard"),
        }
    }
}

pub(super) struct Clipboard {
    terminal_available: bool,
    requests: mpsc::UnboundedSender<Pending>,
}

struct Pending {
    request: Request,
    terminal: Result<(), String>,
}

#[derive(Clone, Copy)]
pub(super) struct Tool(pub(super) &'static str, pub(super) &'static [&'static str]);

enum ToolError {
    Missing,
    Failed(String),
}

impl Clipboard {
    pub(super) fn for_terminal(events: mpsc::UnboundedSender<Input>) -> Self {
        let terminal_available =
            env::var("TERM").is_ok_and(|term| !matches!(term.as_str(), "" | "dumb" | "linux"));
        let mut tools = Vec::new();
        if cfg!(target_os = "macos") {
            tools.push(Tool("pbcopy", &[]));
        } else if cfg!(target_os = "linux") {
            if env::var_os("WAYLAND_DISPLAY").is_some_and(|value| !value.is_empty()) {
                tools.push(Tool("wl-copy", &["--type", "text/plain;charset=utf-8"]));
            }
            if env::var_os("DISPLAY").is_some_and(|value| !value.is_empty()) {
                tools.extend([
                    Tool("xclip", &["-selection", "clipboard", "-in"]),
                    Tool("xsel", &["--clipboard", "--input"]),
                ]);
            }
        }
        Self::start(terminal_available, tools, events)
    }

    pub(super) fn start(
        terminal_available: bool,
        tools: Vec<Tool>,
        events: mpsc::UnboundedSender<Input>,
    ) -> Self {
        let (requests, mut pending) = mpsc::unbounded_channel::<Pending>();
        // Serialise local writes so a slow tool cannot overwrite a later copy.
        tokio::spawn(async move {
            while let Some(Pending { request, terminal }) = pending.recv().await {
                let status = match copy_local(&tools, &request.text).await {
                    Ok(true) => request.success(true),
                    Err(reason) => format!("Could not copy: {reason}"),
                    Ok(false) => match terminal {
                        Ok(()) => request.success(false),
                        Err(reason) => format!("Could not copy: {reason}"),
                    },
                };
                let _ = events.send(Input::ClipboardStatus(status));
            }
        });
        Self {
            terminal_available,
            requests,
        }
    }

    pub(super) fn submit(&self, backend: &mut impl Write, request: Request) {
        // OSC 52 has no acknowledgement. Only the local tool can confirm a copy.
        let terminal = send_terminal(backend, &request.text, self.terminal_available);
        let _ = self.requests.send(Pending { request, terminal });
    }
}

async fn copy_local(tools: &[Tool], text: &str) -> Result<bool, String> {
    let mut failure = None;
    for &tool in tools {
        match run_tool(tool, text).await {
            Ok(()) => return Ok(true),
            Err(ToolError::Missing) => {}
            Err(ToolError::Failed(reason)) => failure = Some(reason),
        }
    }
    failure.map_or(Ok(false), Err)
}

async fn run_tool(Tool(program, args): Tool, text: &str) -> Result<(), ToolError> {
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
    // Reap the child even when its input pipe failed.
    let status = child
        .wait()
        .await
        .map_err(|err| ToolError::Failed(format!("could not wait for {program}: {err}")))?;
    if !status.success() {
        return Err(ToolError::Failed(format!("{program} exited with {status}")));
    }
    written.map_err(|err| ToolError::Failed(format!("could not write to {program}: {err}")))
}

fn send_terminal(backend: &mut impl Write, text: &str, available: bool) -> Result<(), String> {
    if text.len() > OSC52_MAX_BYTES {
        return Err(format!(
            "terminal clipboard limit is {OSC52_MAX_BYTES} bytes"
        ));
    }
    if !available {
        return Err("terminal clipboard unavailable".to_string());
    }
    let payload = Base64Display::new(text.as_bytes(), &STANDARD);
    let sequence = format!("\x1b]52;c;{payload}\x07");
    backend
        .write_all(sequence.as_bytes())
        .and_then(|()| backend.flush())
        .map_err(|err| format!("terminal clipboard write failed: {err}"))
}
