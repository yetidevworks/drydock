//! Synchronous clipboard support: native tools locally, OSC 52 over SSH or as a fallback.

use std::env;
use std::io::{self, Write};
use std::process::{Command, Stdio};

// Stay below xterm's 100,000-byte OSC 52 limit after base64 expansion.
const OSC52_MAX_BYTES: usize = 74_994;

pub(super) fn copy(text: &str) -> String {
    let ssh = ["SSH_TTY", "SSH_CONNECTION", "SSH_CLIENT"]
        .iter()
        .any(|name| env::var_os(name).is_some_and(|value| !value.is_empty()));
    let tools: &[(&str, &[&str])] = if ssh {
        &[]
    } else if cfg!(target_os = "macos") {
        &[("pbcopy", &[])]
    } else if cfg!(target_os = "linux") {
        &[
            ("wl-copy", &["--type", "text/plain;charset=utf-8"]),
            ("xclip", &["-selection", "clipboard", "-in"]),
            ("xsel", &["--clipboard", "--input"]),
        ]
    } else {
        &[]
    };
    if tools
        .iter()
        .any(|(program, args)| run_tool(program, args, text).is_ok())
    {
        return format!("Copied {text}");
    }
    let available =
        env::var("TERM").is_ok_and(|term| !matches!(term.as_str(), "" | "dumb" | "linux"));
    // OSC 52 has no acknowledgement: report sending, not confirmed clipboard access.
    match write_osc52(&mut io::stdout().lock(), text, available) {
        Ok(()) => format!("Sent {text} to terminal clipboard"),
        Err(err) => format!("Could not copy: {err}"),
    }
}

fn run_tool(program: &str, args: &[&str], text: &str) -> io::Result<()> {
    // Helpers fork: discard their output so inherited pipes cannot hold up the caller.
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let mut stdin = child.stdin.take().expect("clipboard tool has piped stdin");
    let written = stdin.write_all(text.as_bytes());
    drop(stdin);
    let status = child.wait()?; // Reap the child even when writing failed.
    written?;
    if !status.success() {
        return Err(io::Error::other(format!("{program} exited with {status}")));
    }
    Ok(())
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

    #[test]
    fn base64_matches_the_rfc_4648_vectors() {
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
    fn osc52_writes_one_sequence_and_nothing_when_it_cannot() {
        let mut out = Vec::new();
        write_osc52(&mut out, "/tmp/repo", true).unwrap();
        assert_eq!(out, b"\x1b]52;c;L3RtcC9yZXBv\x07");

        let mut out = Vec::new();
        assert!(write_osc52(&mut out, "/tmp/repo", false).is_err());
        let too_big = "a".repeat(OSC52_MAX_BYTES + 1);
        assert!(write_osc52(&mut out, &too_big, true).is_err());
        assert!(out.is_empty(), "a refused copy writes nothing");
    }
}
