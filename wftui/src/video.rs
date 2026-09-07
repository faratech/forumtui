//! Playing a post's video in the terminal (#710).
//!
//! The client does not decode anything: it hands the URL to `mpv`, which
//! already knows how to resolve a YouTube page through `yt-dlp` and how to
//! paint frames with kitty's graphics protocol, sixel, or true-colour text
//! blocks. What this module owns is the argument list and the reasons a
//! playback cannot happen — both testable without a terminal, unlike the
//! playback itself.
//!
//! The suspend/restore dance around it lives in `app::run`, because that is
//! what owns the terminal and the reader thread.

use crate::images::Tier as GraphicsTier;
use crate::theme::Tier as ColorTier;

/// The player this delegates to, overridable with `WFTUI_PLAYER`.
///
/// Named in one place so the "not installed" message and the spawn can never
/// disagree. The override is not test scaffolding: a reader may want a
/// wrapper script, a pinned build, or `mpv` under another name — and it is
/// what lets the suspend/forward/restore machinery be exercised end to end
/// without a real player.
pub fn player() -> String {
    std::env::var("WFTUI_PLAYER")
        .ok()
        .filter(|p| !p.trim().is_empty())
        .unwrap_or_else(|| "mpv".to_string())
}

/// Why a video cannot be played, in words a reader can act on.
pub fn unavailable_reason(colors: ColorTier) -> Option<String> {
    if which_player().is_none() {
        let p = player();
        return Some(format!(
            "{p} is not installed — `sudo apt install {p}` (yt-dlp is already here)."
        ));
    }
    if matches!(colors, ColorTier::Mono) {
        // tct paints with colour; there is nothing honest to draw without it.
        return Some(
            "This terminal reports no colour, so there is nothing to draw video with — press o to open it in a browser."
                .to_string(),
        );
    }
    None
}

/// `mpv` on PATH, if it is there.
pub fn which_player() -> Option<std::path::PathBuf> {
    let player = player();
    // An override may name a path outright rather than something on PATH.
    let direct = std::path::Path::new(&player);
    if direct.is_absolute() || player.contains('/') {
        return direct.is_file().then(|| direct.to_path_buf());
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(&player))
        .find(|p| p.is_file())
}

/// mpv's video output for the terminal we are on.
///
/// The graphics tier already answers "what can this terminal paint", so it
/// answers this too: kitty and sixel are mpv's own outputs, iTerm2 reads
/// sixel, and everything else falls back to `tct` — true-colour text blocks,
/// which work on any colour terminal and are what makes this usable over a
/// plain SSH session.
pub fn video_output(graphics: GraphicsTier) -> &'static str {
    match graphics {
        GraphicsTier::Kitty => "kitty",
        GraphicsTier::Sixel | GraphicsTier::Iterm2 => "sixel",
        GraphicsTier::Halfblocks | GraphicsTier::Text => "tct",
    }
}

/// The argv for playing `url`.
///
/// `--input-terminal=yes` with a piped stdin is deliberate: the reader thread
/// owns the real stdin (hard rule 3) and forwards keystrokes down that pipe,
/// so mpv is never a second reader on the terminal. `--no-config` keeps a
/// stray user config from turning the video output back into a GUI window
/// that cannot exist over SSH.
pub fn player_argv(graphics: GraphicsTier, url: &str) -> Vec<String> {
    vec![
        format!("--vo={}", video_output(graphics)),
        "--no-config".to_string(),
        "--really-quiet".to_string(),
        "--input-terminal=yes".to_string(),
        // Keep going without audio rather than refusing to start: a server
        // over SSH has no sound card, and a silent video is still the thing
        // the reader asked to see.
        "--audio-fallback-to-null=yes".to_string(),
        "--".to_string(),
        url.to_string(),
    ]
}

/// The bytes a key means to mpv.
///
/// mpv reads terminal input, so its own bindings are plain bytes and escape
/// sequences. Only the keys mpv actually binds are forwarded — anything else
/// would be noise on its input, and `q` never gets here because the caller
/// stops the player itself rather than trusting the child to exit.
pub fn key_bytes(code: ratatui::crossterm::event::KeyCode) -> Option<Vec<u8>> {
    use ratatui::crossterm::event::KeyCode as K;
    let bytes: Vec<u8> = match code {
        K::Char(c) => c.to_string().into_bytes(),
        K::Left => b"\x1b[D".to_vec(),
        K::Right => b"\x1b[C".to_vec(),
        K::Up => b"\x1b[A".to_vec(),
        K::Down => b"\x1b[B".to_vec(),
        K::Enter => b"\r".to_vec(),
        _ => return None,
    };
    Some(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The output follows what the terminal can paint, and never leaves mpv
    /// to pick a GUI window that cannot exist over SSH.
    #[test]
    fn the_video_output_follows_the_graphics_tier() {
        assert_eq!(video_output(GraphicsTier::Kitty), "kitty");
        assert_eq!(video_output(GraphicsTier::Sixel), "sixel");
        assert_eq!(video_output(GraphicsTier::Iterm2), "sixel");
        // The fallback is the point: text blocks work on any colour
        // terminal, which is most of them.
        assert_eq!(video_output(GraphicsTier::Halfblocks), "tct");
        assert_eq!(video_output(GraphicsTier::Text), "tct");

        for tier in [
            GraphicsTier::Kitty,
            GraphicsTier::Sixel,
            GraphicsTier::Iterm2,
            GraphicsTier::Halfblocks,
            GraphicsTier::Text,
        ] {
            let argv = player_argv(tier, "https://www.youtube.com/watch?v=abc");
            assert!(argv.iter().any(|a| a.starts_with("--vo=")), "{argv:?}");
            assert!(argv.contains(&"--no-config".to_string()));
            // `--` before the URL: a URL starting with a dash must never be
            // read as an option.
            let dashdash = argv.iter().position(|a| a == "--").expect("--");
            let url = argv.iter().position(|a| a.contains("youtube")).expect("url");
            assert!(dashdash < url, "{argv:?}");
        }
    }

    /// Mono means the reader asked for no colour, and there is nothing
    /// honest to paint video with — say so and point at the browser.
    #[test]
    fn mono_is_refused_with_a_reason() {
        let reason = unavailable_reason(ColorTier::Mono);
        // Only meaningful when a player exists; otherwise the missing-player
        // message wins, which is also correct.
        if which_player().is_some() {
            assert!(reason.is_some_and(|r| r.contains("browser")), "mono must refuse");
        }
    }

    /// Only keys mpv binds are forwarded, as the bytes a terminal would have
    /// sent it.
    #[test]
    fn forwarded_keys_are_the_bytes_mpv_expects() {
        use ratatui::crossterm::event::KeyCode as K;
        assert_eq!(key_bytes(K::Char(' ')), Some(b" ".to_vec()));
        assert_eq!(key_bytes(K::Char('f')), Some(b"f".to_vec()));
        assert_eq!(key_bytes(K::Right), Some(b"\x1b[C".to_vec()));
        assert_eq!(key_bytes(K::Up), Some(b"\x1b[A".to_vec()));
        // A multi-byte character survives as its UTF-8, not as a cast.
        assert_eq!(key_bytes(K::Char('\u{e9}')), Some("\u{e9}".as_bytes().to_vec()));
        assert_eq!(key_bytes(K::F(5)), None, "unbound keys are not forwarded");
    }
}
