//! Terminal escape-sequence helpers: OSC 8 hyperlinks and OSC 52 clipboard.
//!
//! Both travel as plain bytes over SSH, so they act on the user's LOCAL
//! terminal/clipboard — exactly what a remote TUI needs. Emit these bytes
//! through `App::emit_raw` between frames, never inside ratatui spans.

use base64ct::{Base64, Encoding};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Multiplexer {
    None,
    Tmux,
    Screen,
}

/// Detect whether wftui is running inside tmux or GNU screen.
pub fn detect_multiplexer() -> Multiplexer {
    multiplexer_for(
        std::env::var_os("TMUX").is_some(),
        std::env::var_os("STY").is_some(),
        std::env::var("TERM").ok().as_deref(),
    )
}

fn multiplexer_for(tmux: bool, screen: bool, term: Option<&str>) -> Multiplexer {
    if tmux {
        Multiplexer::Tmux
    } else if screen || term.is_some_and(|t| t.starts_with("screen")) {
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
fn deliver_dual_for_screen(seq: &str, mux: Multiplexer) -> String {
    match mux {
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
    hyperlink_in_mux(url, label, detect_multiplexer())
}

fn hyperlink_in_mux(url: &str, label: &str, mux: Multiplexer) -> String {
    let seq = format!(
        "\x1b]8;;{}\x1b\\{}\x1b]8;;\x1b\\",
        strip_controls(url),
        strip_controls(label)
    );
    deliver_dual_for_screen(&seq, mux)
}

/// Set terminal window and tab title via OSC 0.
pub fn set_title(title: &str) -> String {
    title_in_mux(title, detect_multiplexer())
}

fn title_in_mux(title: &str, mux: Multiplexer) -> String {
    let seq = format!("\x1b]0;{}\x07", strip_controls(title));
    deliver_dual_for_screen(&seq, mux)
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
    clipboard_in_mux(value, detect_multiplexer())
}

fn clipboard_in_mux(value: &str, mux: Multiplexer) -> String {
    if value.len() > MAX_CLIPBOARD_BYTES {
        return String::new();
    }
    let b64 = Base64::encode_string(value.as_bytes());
    let raw = format!("\x1b]52;c;{b64}\x07");
    match mux {
        Multiplexer::Tmux => {
            // Dual-delivery: send raw OSC 52 (for tmux native set-clipboard)
            // PLUS DCS-wrapped OSC 52 (for tmux allow-passthrough outer terminal).
            let dcs = deliver_in_mux(&raw, Multiplexer::Tmux);
            format!("{raw}{dcs}")
        }
        Multiplexer::Screen => deliver_dual_for_screen(&raw, mux),
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
                // a straddling char into replacement characters. Also never
                // cut between the two ESCs of a doubled pair: the chunk
                // separator is itself `\x1b\\` + `\x1bP`, so a chunk ending
                // on a lone ESC reads to GNU screen as a literal ESC, a
                // stray backslash and a nested DCS start (#39). Extend over
                // the pair rather than shrinking, so a pathological run of
                // bare doubled ESCs cannot walk the boundary into an empty
                // chunk.
                let mut chunks: Vec<&str> = Vec::new();
                let mut start = 0;
                while start < escaped.len() {
                    let mut end = (start + CHUNK_SIZE).min(escaped.len());
                    while !escaped.is_char_boundary(end) {
                        end -= 1;
                    }
                    if end < escaped.len()
                        && escaped.as_bytes()[end - 1] == b'\x1b'
                        && escaped.as_bytes()[end] == b'\x1b'
                    {
                        end += 1;
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

    #[cfg(target_os = "windows")]
    {
        // The read half of the paste key on Windows (#38): without this
        // branch the function was structurally dead and Ctrl+Y/v could
        // never see text copied in another application. PowerShell prints
        // its console output in the OEM code page unless told otherwise, so
        // force UTF-8; stdin is nulled (hard rule 2) so the child can never
        // eat the reader's keystrokes. Get-Clipboard appends one newline,
        // which is stripped for parity with wl-paste's --no-newline.
        let output = std::process::Command::new("powershell")
            .args([
                "-NoProfile",
                "-Command",
                "[Console]::OutputEncoding=[Text.Encoding]::UTF8; Get-Clipboard",
            ])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .output();
        if let Ok(output) = output
            && output.status.success()
            && let Ok(text) = String::from_utf8(output.stdout)
        {
            let text = text.strip_suffix('\n').unwrap_or(&text);
            let text = text.strip_suffix('\r').unwrap_or(text);
            let text = text.replace("\r\n", "\n");
            if !text.is_empty() {
                return Some(text);
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

    /// #39: GNU screen chunking must never cut between the two ESCs of a
    /// doubled pair — the separator is itself ESC-backed, so a chunk ending
    /// on a lone ESC reads as a literal ESC, a stray backslash and a nested
    /// DCS start. URL lengths 67-69 put a doubled pair exactly across the
    /// 76-byte boundary; the sweep makes sure every multiple of it holds.
    #[test]
    fn screen_chunking_never_splits_a_doubled_esc_pair() {
        for n in 1..=170usize {
            let seq = format!("\x1b]8;;{}\x1b\\TEXT\x1b]8;;\x1b\\", "u".repeat(n));
            let out = deliver_in_mux(&seq, Multiplexer::Screen);
            let body = out
                .strip_prefix("\x1bP")
                .and_then(|r| r.strip_suffix("\x1b\\"))
                .unwrap_or(&out);
            if !body.contains("\x1b\\\x1bP") {
                continue; // single chunk, nothing to split
            }
            for chunk in body.split("\x1b\\\x1bP") {
                let trail = chunk.bytes().rev().take_while(|&b| b == 0x1b).count();
                assert!(
                    trail % 2 == 0,
                    "url len {n}: a chunk ends inside a doubled ESC: {chunk:?}"
                );
            }
            // And the chunking is transparent: dropping the separators and
            // unescaping the pairs rebuilds the original sequence.
            let rejoined = body.replace("\x1b\\\x1bP", "");
            assert_eq!(rejoined.replace("\x1b\x1b", "\x1b"), seq, "url len {n}");
        }
    }

    /// Control characters in user-sourced titles/URLs must be stripped at
    /// the OSC boundary (#649): a BEL/ESC in a thread title could otherwise
    /// terminate or inject sequences inside the terminal itself.
    #[test]
    fn titles_and_hrefs_carry_no_control_characters() {
        let t = title_in_mux("Windows \u{1b}]0;pwned\u{7} Forum tabs", Multiplexer::None);
        // Exactly the framing escapes remain: OSC open, BEL terminator; the
        // injected sequences' control bytes are gone, leaving plain text.
        assert!(t.starts_with("\x1b]0;"), "{t:?}");
        assert!(t.ends_with("\x07"), "{t:?}");
        let inner = &t["\x1b]0;".len()..t.len() - 1];
        assert!(!inner.chars().any(char::is_control), "injected: {t:?}");
        // The injected sequence's bytes are gone; what remains of the title
        // text ("Windows ]0;pwned Forum tabs") is inert, printable text.
        assert!(inner.starts_with("Windows ") && inner.ends_with("Forum tabs"), "{t:?}");

        let h = hyperlink_in_mux("https://x.test/a\u{7}b", "click\u{1b}me", Multiplexer::None);
        assert!(!h.contains('\u{7}'), "{h:?}");
        let payload = h.trim_start_matches("\x1b]8;;").trim_end_matches("\x1b]8;;\x1b\\");
        assert!(payload.starts_with("https://x.test/ab\x1b\\clickme"), "{h:?}");
    }

    /// #648: an OSC 52 payload far past what terminals accept is not
    /// emitted at all (empty output = "not sent"); small values still are.
    #[test]
    fn clipboard_over_the_cap_is_not_emitted() {
        assert!(clipboard_in_mux("small", Multiplexer::None).starts_with("\x1b]52;c;"));
        let huge = "x".repeat(MAX_CLIPBOARD_BYTES + 1);
        for mux in [Multiplexer::None, Multiplexer::Tmux, Multiplexer::Screen] {
            assert!(clipboard_in_mux(&huge, mux).is_empty(), "over-cap payload must not be emitted");
        }
        let at_cap = "x".repeat(MAX_CLIPBOARD_BYTES);
        assert!(!clipboard_in_mux(&at_cap, Multiplexer::None).is_empty(), "at-cap is fine");
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
        let mux = multiplexer_for(false, false, Some("screen-256color"));
        assert_eq!(mux, Multiplexer::Screen);
        let out = clipboard_in_mux("https://windowsforum.com/x", mux);
        assert!(out.starts_with("\x1b]52;c;"), "missing raw OSC 52: {out:?}");
        assert!(out.contains("\x1bP"), "missing screen-wrapped DCS: {out:?}");
        assert_eq!(multiplexer_for(true, true, Some("screen")), Multiplexer::Tmux);
        assert_eq!(multiplexer_for(false, true, Some("xterm")), Multiplexer::Screen);
        assert_eq!(multiplexer_for(false, false, Some("xterm")), Multiplexer::None);
        assert_eq!(multiplexer_for(false, false, None), Multiplexer::None);
    }

    #[test]
    fn terminal_title_generation() {
        let title_seq = title_in_mux("wftui - Windows Forums", Multiplexer::None);
        assert!(title_seq.contains("wftui - Windows Forums"));
    }
}
