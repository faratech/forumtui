//! Glyph table: single-cell marks, with an ASCII fallback set.
//!
//! Emoji left the client deliberately. 📌 ❓ 📰 💡 💬 📁 🔗 📄 ✉ 🔔 are
//! *ambiguous width*: East-Asian-Wide in some terminals, narrow in others, and
//! a blank tofu box in a plain `xterm`. A list column built from them
//! misaligns on exactly the terminals people SSH from. Everything here is one
//! `char` wide, which is what makes the row grammar in DESIGN.md line up.
//!
//! The one deliberate exception is the ASCII set: `<3`, `[img]` and `->` have
//! no single-character ASCII spelling that reads as anything. They are only
//! used inline (reaction count, attachment placeholder, alert kind), never in
//! a fixed-width column, so their extra width costs nothing. The unit test
//! below asserts single-cell for everything else in *both* sets.

/// The glyph table. Copy — it is passed by reference everywhere but cheap to
/// clone into tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Glyphs {
    /// Unread thread / conversation (accent).
    pub unread: &'static str,
    /// Open question (warn).
    pub question: &'static str,
    /// Solved question (ok).
    pub solved: &'static str,
    /// Sticky thread (accent).
    pub sticky: &'static str,
    /// Article thread (mark blue).
    pub article: &'static str,
    /// Closed thread (dim).
    pub locked: &'static str,
    /// Post body gutter (accent when selected, faint otherwise).
    pub gutter: &'static str,
    /// `[HEADING]` prefix (accent bold).
    pub heading: &'static str,
    /// Watching (warn).
    pub watching: &'static str,
    /// Reaction count. ASCII spelling `<3` is two cells — see module docs.
    pub like: &'static str,
    /// Vote score.
    pub vote: &'static str,
    /// Loading, four frames.
    pub spinner: [&'static str; 4],
    /// Write-gate countdown bar, filled cell.
    pub gate_on: &'static str,
    /// Write-gate countdown bar, empty cell.
    pub gate_off: &'static str,
    /// Breadcrumb separator.
    pub crumb: &'static str,
    /// "N more" scroll indicator.
    pub more: &'static str,
    /// Link forum / page.
    pub external: &'static str,
    /// Attachment placeholder. ASCII spelling `[img]` is five cells.
    pub image: &'static str,
    /// A playable video (#710). Single cell, like every other glyph here —
    /// no emoji, which are double-width or blank in too many terminals.
    pub play: &'static str,
    /// Alert kind: reply. ASCII spelling `->` is two cells.
    pub reply_alert: &'static str,
    /// Alert kind: mention.
    pub mention: &'static str,
    /// Alert kind: quote.
    pub quote_alert: &'static str,
    /// True when this is the ASCII set (drives `BorderType` too).
    pub ascii: bool,
}

pub const UNICODE: Glyphs = Glyphs {
    unread: "\u{25CF}",       // ●
    question: "?",
    solved: "\u{2713}",       // ✓
    sticky: "\u{00BB}",       // »
    article: "\u{25AA}",      // ▪
    locked: "\u{2298}",       // ⊘
    gutter: "\u{2503}",       // ┃
    heading: "\u{258C}",      // ▌
    watching: "\u{2605}",     // ★
    like: "\u{2661}",         // ♡
    vote: "\u{25B2}",         // ▲
    spinner: ["\u{25D0}", "\u{25D3}", "\u{25D1}", "\u{25D2}"], // ◐ ◓ ◑ ◒
    gate_on: "\u{25AE}",      // ▮
    gate_off: "\u{25AF}",     // ▯
    crumb: "\u{203A}",        // ›
    more: "\u{25BE}",         // ▾
    external: "\u{2197}",     // ↗
    image: "\u{25A3}",        // ▣
    play: "\u{25B6}",         // ▶
    reply_alert: "\u{21A9}",  // ↩
    mention: "@",
    quote_alert: "\u{275D}",  // ❝
    ascii: false,
};

pub const ASCII: Glyphs = Glyphs {
    unread: "*",
    question: "?",
    solved: "v",
    sticky: ">",
    article: "-",
    locked: "x",
    gutter: "|",
    heading: "#",
    watching: "*",
    like: "<3",
    vote: "^",
    spinner: ["-", "\\", "|", "/"],
    gate_on: "=",
    gate_off: ".",
    crumb: ">",
    more: "v",
    external: "^",
    image: "[img]",
    play: ">",
    reply_alert: "->",
    mention: "@",
    quote_alert: "\"",
    ascii: true,
};

impl Glyphs {
    /// Panel border style that matches this set: rounded in unicode, plain in
    /// ASCII (`╭╮╰╯` are not representable).
    pub fn border_type(&self) -> ratatui::widgets::BorderType {
        if self.ascii {
            ratatui::widgets::BorderType::Plain
        } else {
            ratatui::widgets::BorderType::Rounded
        }
    }
}

/// ASCII when `WFTUI_ASCII=1`, or when the locale is not UTF-8.
///
/// The locale is read from `LC_ALL`, then `LC_CTYPE`, then `LANG` (POSIX
/// precedence). An unset locale is treated as UTF-8 — modern terminals default
/// to it and the common CI/container case has no locale variables at all.
pub fn detect() -> Glyphs {
    if std::env::var("WFTUI_ASCII").is_ok_and(|v| v == "1") {
        return ASCII;
    }
    for key in ["LC_ALL", "LC_CTYPE", "LANG"] {
        let Ok(v) = std::env::var(key) else { continue };
        if v.is_empty() {
            continue;
        }
        let v = v.to_ascii_lowercase();
        return if v.contains("utf-8") || v.contains("utf8") {
            UNICODE
        } else {
            ASCII
        };
    }
    UNICODE
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Glyphs that are deliberately wider than one cell in the ASCII set: they
    /// are inline-only, never in a fixed-width column (see module docs).
    const ASCII_WIDE_ALLOWED: [&str; 3] = ["<3", "[img]", "->"];

    fn all(g: &Glyphs) -> Vec<(&'static str, &'static str)> {
        let mut v = vec![
            ("unread", g.unread),
            ("question", g.question),
            ("solved", g.solved),
            ("sticky", g.sticky),
            ("article", g.article),
            ("locked", g.locked),
            ("gutter", g.gutter),
            ("heading", g.heading),
            ("watching", g.watching),
            ("like", g.like),
            ("vote", g.vote),
            ("gate_on", g.gate_on),
            ("gate_off", g.gate_off),
            ("crumb", g.crumb),
            ("more", g.more),
            ("external", g.external),
            ("image", g.image),
            ("reply_alert", g.reply_alert),
            ("mention", g.mention),
            ("quote_alert", g.quote_alert),
        ];
        for s in g.spinner {
            v.push(("spinner", s));
        }
        v
    }

    #[test]
    fn unicode_glyphs_are_all_exactly_one_char() {
        for (name, s) in all(&UNICODE) {
            assert_eq!(
                s.chars().count(),
                1,
                "unicode glyph {name} = {s:?} is not a single char"
            );
        }
    }

    #[test]
    fn ascii_glyphs_are_one_char_except_the_three_documented_inline_ones() {
        for (name, s) in all(&ASCII) {
            if ASCII_WIDE_ALLOWED.contains(&s) {
                continue;
            }
            assert_eq!(
                s.chars().count(),
                1,
                "ascii glyph {name} = {s:?} is not a single char"
            );
        }
        // The exceptions must actually still be present, or the allowlist is
        // silently covering a typo.
        assert_eq!(ASCII.like, "<3");
        assert_eq!(ASCII.image, "[img]");
        assert_eq!(ASCII.reply_alert, "->");
    }

    #[test]
    fn ascii_set_is_pure_ascii_and_unicode_set_is_not() {
        for (name, s) in all(&ASCII) {
            assert!(s.is_ascii(), "ascii glyph {name} = {s:?} is not ASCII");
        }
        assert!(!UNICODE.unread.is_ascii());
    }

    #[test]
    fn border_type_follows_the_set() {
        assert_eq!(
            UNICODE.border_type(),
            ratatui::widgets::BorderType::Rounded
        );
        assert_eq!(ASCII.border_type(), ratatui::widgets::BorderType::Plain);
    }
}
