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

/// Deliver `seq` to the terminal, dual-delivering (raw + DCS-wrapped) when
/// the multiplexer is (or might be) GNU screen.
///
/// `TERM` starting with `screen` plus no `$TMUX` is also exactly what a local
/// tmux reached over ssh looks like from here (ssh forwards `TERM`, never
/// `TMUX`), and a bare, non-`tmux;`-prefixed DCS is invisible to tmux's
/// `allow-passthrough` — so a real GNU screen instance, which passes an
/// unrecognized raw OSC through harmlessly, and a tmux-behind-ssh instance,
/// which forwards a raw OSC to the outer terminal, both need the raw
/// sequence; only real screen also needs the DCS wrapper.
fn deliver_dual_for_screen(seq: &str) -> String {
    match detect_multiplexer() {
        Multiplexer::Screen => format!("{seq}{}", deliver_in_mux(seq, Multiplexer::Screen)),
        mux => deliver_in_mux(seq, mux),
    }
}

/// Control characters (C0, DEL, C1) are stripped at the OSC boundary (#649):
/// a BEL or ESC inside a thread title or URL could otherwise split or inject
/// sequences in the terminal itself. Printable text — including multi-byte
/// scripts — passes untouched.
fn strip_controls(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).collect()
}

/// Wrap `label` in an OSC 8 hyperlink pointing at `url`.
pub fn hyperlink(url: &str, label: &str) -> String {
    let seq = format!(
        "\x1b]8;;{}\x1b\\{}\x1b]8;;\x1b\\",
        strip_controls(url),
        strip_controls(label)
    );
    deliver_dual_for_screen(&seq)
}

/// Set terminal window and tab title via OSC 0.
pub fn set_title(title: &str) -> String {
    let seq = format!("\x1b]0;{}\x07", strip_controls(title));
    deliver_dual_for_screen(&seq)
}

/// Largest OSC 52 payload source we will base64 (#648): a megabyte-scale
/// selection becomes a megabyte-and-a-third escape sequence, and many
/// terminals cap OSC 52 far below that (or stall on it). Past the cap the
/// sequence is simply not emitted; the system-clipboard path has no such
/// limit.
pub const MAX_CLIPBOARD_BYTES: usize = 64 * 1024;

/// OSC 52 clipboard write (base64 payload, BEL terminator). Empty output
/// means "over the cap — not sent".
pub fn set_clipboard(value: &str) -> String {
    if value.len() > MAX_CLIPBOARD_BYTES {
        return String::new();
    }
    let b64 = Base64::encode_string(value.as_bytes());
    let raw = format!("\x1b]52;c;{b64}\x07");
    match detect_multiplexer() {
        Multiplexer::Tmux => {
            // Dual-delivery: send raw OSC 52 (for tmux native set-clipboard)
            // PLUS DCS-wrapped OSC 52 (for tmux allow-passthrough outer terminal).
            let dcs = deliver_in_mux(&raw, Multiplexer::Tmux);
            format!("{raw}{dcs}")
        }
        Multiplexer::Screen => deliver_dual_for_screen(&raw),
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
                // Chunk on char boundaries (never split a multi-byte char in
                // two) instead of raw bytes + `from_utf8_lossy`, which turned
                // a straddling char into replacement characters.
                let mut chunks: Vec<&str> = Vec::new();
                let mut start = 0;
                while start < escaped.len() {
                    let mut end = (start + CHUNK_SIZE).min(escaped.len());
                    while !escaped.is_char_boundary(end) {
                        end -= 1;
                    }
                    chunks.push(&escaped[start..end]);
                    start = end;
                }
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
                    // `Child::wait` returns Ok even for a non-zero exit, so
                    // `is_ok()` here let a present-but-failing xclip (no X,
                    // wayland-only session) stop the loop before xsel was
                    // tried (#648) — only actual success may.
                    if c.wait().map(|s| s.success()).unwrap_or(false) {
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

    /// Control characters in user-sourced titles/URLs must be stripped at
    /// the OSC boundary (#649): a BEL/ESC in a thread title could otherwise
    /// terminate or inject sequences inside the terminal itself.
    #[test]
    fn titles_and_hrefs_carry_no_control_characters() {
        let t = set_title("Windows \u{1b}]0;pwned\u{7} Forum tabs");
        // Exactly the framing escapes remain: OSC open, BEL terminator; the
        // injected sequences' control bytes are gone, leaving plain text.
        assert!(t.starts_with("\x1b]0;"), "{t:?}");
        assert!(t.ends_with("\x07"), "{t:?}");
        let inner = &t["\x1b]0;".len()..t.len() - 1];
        assert!(!inner.chars().any(char::is_control), "injected: {t:?}");
        // The injected sequence's bytes are gone; what remains of the title
        // text ("Windows ]0;pwned Forum tabs") is inert, printable text.
        assert!(inner.starts_with("Windows ") && inner.ends_with("Forum tabs"), "{t:?}");

        let h = hyperlink("https://x.test/a\u{7}b", "click\u{1b}me");
        assert!(!h.contains('\u{7}'), "{h:?}");
        let payload = h.trim_start_matches("\x1b]8;;").trim_end_matches("\x1b]8;;\x1b\\");
        assert!(payload.starts_with("https://x.test/ab\x1b\\clickme"), "{h:?}");
    }

    /// #648: an OSC 52 payload far past what terminals accept is not
    /// emitted at all (empty output = "not sent"); small values still are.
    #[test]
    fn clipboard_over_the_cap_is_not_emitted() {
        assert!(set_clipboard("small").starts_with("\x1b]52;c;"));
        let huge = "x".repeat(MAX_CLIPBOARD_BYTES + 1);
        assert!(set_clipboard(&huge).is_empty(), "over-cap payload must not be emitted");
        let at_cap = "x".repeat(MAX_CLIPBOARD_BYTES);
        assert!(!set_clipboard(&at_cap).is_empty(), "at-cap is fine");
    }

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

    /// Issue #599 (chunking half): a 3-byte char straddling the 76-byte chunk
    /// boundary used to be split by `bytes.chunks()` + `from_utf8_lossy`,
    /// turning it into replacement characters on both sides of the cut.
    #[test]
    fn screen_chunking_never_splits_a_multibyte_char() {
        let payload = format!("{}{}{}", "A".repeat(74), '\u{2500}', "B".repeat(30));
        let wrapped = deliver_in_mux(&payload, Multiplexer::Screen);
        assert!(!wrapped.contains('\u{FFFD}'), "{wrapped:?}");
        assert!(wrapped.contains('\u{2500}'), "{wrapped:?}");
    }

    /// Issue #599 (misdetection half): `TERM=screen*` with no `$TMUX` is also
    /// exactly the shape of a local tmux reached over ssh (ssh forwards TERM,
    /// never TMUX). Dual-deliver the raw sequence (which a tmux-behind-ssh
    /// outer terminal forwards, and a real screen passes through harmlessly)
    /// alongside the screen-wrapped DCS (which real screen needs).
    #[test]
    fn screen_like_term_over_ssh_dual_delivers_raw_plus_dcs() {
        let _g = crate::config::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev_tmux = std::env::var_os("TMUX");
        let prev_sty = std::env::var_os("STY");
        let prev_term = std::env::var_os("TERM");
        unsafe {
            std::env::remove_var("TMUX");
            std::env::remove_var("STY");
            std::env::set_var("TERM", "screen-256color");
        }

        let out = set_clipboard("https://windowsforum.com/x");
        assert!(out.starts_with("\x1b]52;c;"), "missing raw OSC 52: {out:?}");
        assert!(out.contains("\x1bP"), "missing screen-wrapped DCS: {out:?}");

        unsafe {
            match prev_tmux {
                Some(v) => std::env::set_var("TMUX", v),
                None => std::env::remove_var("TMUX"),
            }
            match prev_sty {
                Some(v) => std::env::set_var("STY", v),
                None => std::env::remove_var("STY"),
            }
            match prev_term {
                Some(v) => std::env::set_var("TERM", v),
                None => std::env::remove_var("TERM"),
            }
        }
    }

    #[test]
    fn terminal_title_generation() {
        // `set_title` consults TMUX/STY/TERM (detect_multiplexer). The env is
        // process-global and sibling tests mutate it under ENV_LOCK, so this
        // read takes the same lock — a bare read alongside a set_var is a
        // race, not just a flake (#660).
        let _lock = crate::config::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let title_seq = set_title("wftui - Windows Forums");
        assert!(title_seq.contains("wftui - Windows Forums"));
    }
}
