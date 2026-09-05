//! Terminal escape-sequence helpers: OSC 8 hyperlinks and OSC 52 clipboard.
//!
//! Both travel as plain bytes over SSH, so they act on the user's LOCAL
//! terminal/clipboard — exactly what a remote TUI needs. Control characters
//! are zero-width in unicode-width, so embedding these in ratatui spans does
//! not skew layout (keep each sequence inside one span and off wrapped lines).

use base64ct::{Base64, Encoding};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Multiplexer {
    None,
    Tmux,
    Screen,
}

/// Detect whether wftui is running inside tmux or GNU screen.
pub fn detect_multiplexer() -> Multiplexer {
    if std::env::var_os("TMUX").is_some() {
        Multiplexer::Tmux
    } else if std::env::var_os("STY").is_some()
        || std::env::var("TERM").is_ok_and(|t| t.starts_with("screen"))
    {
        Multiplexer::Screen
    } else {
        Multiplexer::None
    }
}

/// Detect whether the session is running over an SSH connection.
pub fn is_remote_session() -> bool {
    std::env::var_os("SSH_CONNECTION").is_some()
        || std::env::var_os("SSH_CLIENT").is_some()
        || std::env::var_os("SSH_TTY").is_some()
}

/// Wrap `label` in an OSC 8 hyperlink pointing at `url`.
pub fn hyperlink(url: &str, label: &str) -> String {
    let seq = format!("\x1b]8;;{url}\x1b\\{label}\x1b]8;;\x1b\\");
    deliver_in_mux(&seq, detect_multiplexer())
}

/// Set terminal window and tab title via OSC 0.
pub fn set_title(title: &str) -> String {
    let seq = format!("\x1b]0;{title}\x07");
    deliver_in_mux(&seq, detect_multiplexer())
}

/// OSC 52 clipboard write (base64 payload, BEL terminator).
pub fn set_clipboard(value: &str) -> String {
    let b64 = Base64::encode_string(value.as_bytes());
    let raw = format!("\x1b]52;c;{b64}\x07");
    match detect_multiplexer() {
        Multiplexer::Tmux => {
            // Dual-delivery: send raw OSC 52 (for tmux native set-clipboard)
            // PLUS DCS-wrapped OSC 52 (for tmux allow-passthrough outer terminal).
            let dcs = deliver_in_mux(&raw, Multiplexer::Tmux);
            format!("{raw}{dcs}")
        }
        Multiplexer::Screen => deliver_in_mux(&raw, Multiplexer::Screen),
        Multiplexer::None => raw,
    }
}

/// Deliver escape sequence through terminal multiplexer DCS passthrough if needed.
pub fn deliver_in_mux(seq: &str, mux: Multiplexer) -> String {
    match mux {
        Multiplexer::Tmux => {
            let escaped = seq.replace('\x1b', "\x1b\x1b");
            format!("\x1bPtmux;\x1b{escaped}\x1b\\")
        }
        Multiplexer::Screen => {
            let escaped = seq.replace('\x1b', "\x1b\x1b");
            const CHUNK_SIZE: usize = 76;
            if escaped.len() <= CHUNK_SIZE {
                format!("\x1bP{escaped}\x1b\\")
            } else {
                let chunks: Vec<String> = escaped
                    .as_bytes()
                    .chunks(CHUNK_SIZE)
                    .map(|c| String::from_utf8_lossy(c).to_string())
                    .collect();
                format!("\x1bP{}\x1b\\", chunks.join("\x1b\\\x1bP"))
            }
        }
        Multiplexer::None => seq.to_string(),
    }
}

/// Copy text to system clipboard (Wayland wl-copy, X11 xclip/xsel, macOS pbcopy, tmux buffer).
pub fn copy_to_system_clipboard(value: &str) {
    if detect_multiplexer() == Multiplexer::Tmux
        && let Ok(mut c) = std::process::Command::new("tmux")
            .args(["load-buffer", "-w", "-"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
    {
        if let Some(mut stdin) = c.stdin.take() {
            use std::io::Write;
            let _ = stdin.write_all(value.as_bytes());
        }
        let _ = c.wait();
    }

    #[cfg(target_os = "macos")]
    {
        if let Ok(mut c) = std::process::Command::new("pbcopy")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            if let Some(mut stdin) = c.stdin.take() {
                use std::io::Write;
                let _ = stdin.write_all(value.as_bytes());
            }
            let _ = c.wait();
        }
    }

    #[cfg(target_os = "linux")]
    {
        if std::env::var_os("WAYLAND_DISPLAY").is_some()
            && let Ok(mut c) = std::process::Command::new("wl-copy")
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
        {
            if let Some(mut stdin) = c.stdin.take() {
                use std::io::Write;
                let _ = stdin.write_all(value.as_bytes());
            }
            let _ = c.wait();
        }
        if std::env::var_os("DISPLAY").is_some() {
            let tools = [
                ("xclip", vec!["-selection", "clipboard"]),
                ("xsel", vec!["--clipboard", "--input"]),
            ];
            for (cmd, args) in tools {
                if let Ok(mut c) = std::process::Command::new(cmd)
                    .args(&args)
                    .stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn()
                {
                    if let Some(mut stdin) = c.stdin.take() {
                        use std::io::Write;
                        let _ = stdin.write_all(value.as_bytes());
                    }
                    if c.wait().is_ok() {
                        break;
                    }
                }
            }
        }
    }
}

/// Read text from system clipboard if available.
pub fn read_from_system_clipboard() -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        if let Ok(output) = std::process::Command::new("pbpaste").output()
            && output.status.success()
            && let Ok(text) = String::from_utf8(output.stdout)
            && !text.is_empty()
        {
            return Some(text);
        }
    }

    #[cfg(target_os = "linux")]
    {
        if std::env::var_os("WAYLAND_DISPLAY").is_some()
            && let Ok(output) = std::process::Command::new("wl-paste")
                .arg("--no-newline")
                .output()
            && output.status.success()
            && let Ok(text) = String::from_utf8(output.stdout)
            && !text.is_empty()
        {
            return Some(text);
        }
        if std::env::var_os("DISPLAY").is_some() {
            let tools = [
                ("xclip", vec!["-selection", "clipboard", "-o"]),
                ("xsel", vec!["--clipboard", "--output"]),
            ];
            for (cmd, args) in tools {
                if let Ok(output) = std::process::Command::new(cmd).args(&args).output()
                    && output.status.success()
                    && let Ok(text) = String::from_utf8(output.stdout)
                    && !text.is_empty()
                {
                    return Some(text);
                }
            }
        }
    }

    if detect_multiplexer() == Multiplexer::Tmux
        && let Ok(output) = std::process::Command::new("tmux")
            .args(["save-buffer", "-"])
            .output()
        && output.status.success()
        && let Ok(text) = String::from_utf8(output.stdout)
        && !text.is_empty()
    {
        return Some(text);
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hyperlink_wraps_label_with_reset() {
        let s = deliver_in_mux(
            "\x1b]8;;https://x.example/a\x1b\\click\x1b]8;;\x1b\\",
            Multiplexer::None,
        );
        assert!(s.starts_with("\x1b]8;;https://x.example/a\x1b\\"));
        assert!(s.contains("click"));
        assert!(s.ends_with("\x1b]8;;\x1b\\"));
    }

    #[test]
    fn clipboard_payload_is_base64() {
        let s = deliver_in_mux(
            &format!(
                "\x1b]52;c;{}\x07",
                Base64::encode_string("https://windowsforum.com/x".as_bytes())
            ),
            Multiplexer::None,
        );
        let payload = s
            .strip_prefix("\x1b]52;c;")
            .expect("osc52 header")
            .strip_suffix('\x07')
            .expect("bel terminator");
        let decoded = Base64::decode_vec(payload).unwrap();
        assert_eq!(
            String::from_utf8(decoded).unwrap(),
            "https://windowsforum.com/x"
        );
    }

    #[test]
    fn tmux_passthrough_doubles_escapes() {
        let raw = "\x1b]52;c;QUJD\x07";
        let wrapped = deliver_in_mux(raw, Multiplexer::Tmux);
        assert!(wrapped.starts_with("\x1bPtmux;\x1b"));
        assert!(wrapped.ends_with("\x1b\\"));
        assert!(wrapped.contains("\x1b\x1b]52;c;QUJD\x07"));
        assert_eq!(deliver_in_mux(raw, Multiplexer::None), raw);
    }

    #[test]
    fn screen_passthrough_chunks_long_payloads() {
        let long_payload = "A".repeat(100);
        let wrapped = deliver_in_mux(&long_payload, Multiplexer::Screen);
        assert!(wrapped.starts_with("\x1bP"));
        assert!(wrapped.ends_with("\x1b\\"));
        assert!(wrapped.contains("\x1b\\\x1bP"));
    }

    #[test]
    fn terminal_title_generation() {
        let title_seq = set_title("wftui - Windows Forums");
        assert!(title_seq.contains("wftui - Windows Forums"));
    }
}
