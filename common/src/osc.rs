//! Terminal escape-sequence helpers: OSC 8 hyperlinks and OSC 52 clipboard.
//!
//! Both travel as plain bytes over SSH, so they act on the user's LOCAL
//! terminal/clipboard — exactly what a remote TUI needs. Control characters
//! are zero-width in unicode-width, so embedding these in ratatui spans does
//! not skew layout (keep each sequence inside one span and off wrapped lines).

use base64ct::{Base64, Encoding};

/// Wrap `label` in an OSC 8 hyperlink pointing at `url`.
pub fn hyperlink(url: &str, label: &str) -> String {
    let seq = format!("\x1b]8;;{url}\x1b\\{label}\x1b]8;;\x1b\\");
    tmux_deliver(seq)
}

/// OSC 52 clipboard write (base64 payload, BEL terminator).
pub fn set_clipboard(value: &str) -> String {
    let seq = format!("\x1b]52;c;{}\x07", Base64::encode_string(value.as_bytes()));
    tmux_deliver(seq)
}

/// tmux swallows unknown OSC sequences coming from applications. Inside
/// tmux, sequences must be wrapped in the DCS passthrough
/// (`ESC Ptmux; ESC <seq with ESC doubled> ESC \`) to reach the outer
/// terminal; the user needs `tmux set -g allow-passthrough on` (tmux ≥ 3.3)
/// or `set -g set-clipboard external` for the plain OSC 52 path.
fn tmux_deliver(seq: String) -> String {
    deliver_in(seq, std::env::var_os("TMUX").is_some())
}

fn deliver_in(seq: String, in_tmux: bool) -> String {
    if in_tmux {
        let escaped = seq.replace('\x1b', "\x1b\x1b");
        format!("\x1bPtmux;\x1b{escaped}\x1b\\")
    } else {
        seq
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hyperlink_wraps_label_with_reset() {
        // Bare form (tests may run inside tmux, which adds the DCS wrapper).
        let s = deliver_in(
            "\x1b]8;;https://x.example/a\x1b\\click\x1b]8;;\x1b\\".to_string(),
            false,
        );
        assert!(s.starts_with("\x1b]8;;https://x.example/a\x1b\\"));
        assert!(s.contains("click"));
        assert!(s.ends_with("\x1b]8;;\x1b\\"));
    }

    #[test]
    fn clipboard_payload_is_base64() {
        use base64ct::{Base64, Encoding};
        let s = deliver_in(
            format!(
                "\x1b]52;c;{}\x07",
                Base64::encode_string("https://windowsforum.com/x".as_bytes())
            ),
            false,
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
        let wrapped = deliver_in(raw.to_string(), true);
        assert!(wrapped.starts_with("\x1bPtmux;\x1b"));
        assert!(wrapped.ends_with("\x1b\\"));
        // The inner ESC must be doubled so tmux's own DCS terminator is the
        // only unpaired one.
        assert!(wrapped.contains("\x1b\x1b]52;c;QUJD\x07"));
        // Outside tmux the sequence is delivered bare.
        assert_eq!(deliver_in(raw.to_string(), false), raw);
    }
}
