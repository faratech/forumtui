//! Theme: one role table, four fidelity tiers.
//!
//! Principle (DESIGN.md): **brand in the chrome, content in the reader's own
//! colors**. `text` is `Color::Reset` in every tier so body text, list rows and
//! post bodies inherit the terminal's own foreground — the client then looks
//! right in a light terminal and a dark one without a "light theme". Only the
//! chrome (header band, key caps, selection band, unread marks, badges) carries
//! WindowsForum blue.
//!
//! Tier detection: `NO_COLOR` → Mono; `COLORTERM` = truecolor/24bit →
//! TrueColor; `TERM` containing `256color` → Ansi256; otherwise Ansi16.
//! Ansi16 and Mono have no usable selection background, so `selected()` swaps
//! to the REVERSED modifier there instead of a background color.

use ratatui::style::{Color, Modifier, Style};

/// A BBCode colour, brought down to what the terminal can actually show
/// (#702).
///
/// TrueColor gets the exact value; 256-colour terminals get the nearest cube
/// entry; 16-colour terminals get the nearest basic colour, which is a
/// coarse but honest approximation. On the mono tier a post's colours are
/// dropped entirely rather than approximated into noise — the reader asked
/// for no colour.
pub fn quantize(tier: Tier, c: common::bbcode::Rgb) -> Option<Color> {
    let (r, g, b) = (c.r, c.g, c.b);
    match tier {
        Tier::TrueColor => Some(Color::Rgb(r, g, b)),
        Tier::Ansi256 => Some(Color::Indexed(xterm256_index(r, g, b))),
        Tier::Ansi16 => Some(basic16(r, g, b)),
        Tier::Mono => None,
    }
}

/// xterm's 6x6x6 cube (16-231) plus its 24-step greyscale ramp (232-255),
/// whichever is closer.
fn xterm256_index(r: u8, g: u8, b: u8) -> u8 {
    let level = |v: u8| -> u8 {
        // The cube's levels are 0, 95, 135, 175, 215, 255.
        const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
        let mut best = 0usize;
        let mut best_d = u16::MAX;
        for (i, l) in LEVELS.iter().enumerate() {
            let d = (*l as i16 - v as i16).unsigned_abs();
            if d < best_d {
                best_d = d;
                best = i;
            }
        }
        best as u8
    };
    let (ri, gi, bi) = (level(r), level(g), level(b));
    const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
    let cube = (
        LEVELS[ri as usize],
        LEVELS[gi as usize],
        LEVELS[bi as usize],
    );
    let cube_idx = 16 + 36 * ri + 6 * gi + bi;

    // The grey ramp can be a much better match for near-greys.
    let avg = ((r as u16 + g as u16 + b as u16) / 3) as u8;
    let grey_step = ((avg as i16 - 8) / 10).clamp(0, 23) as u8;
    let grey_val = 8 + 10 * grey_step;
    let dist = |a: (u8, u8, u8)| -> u32 {
        let d = |x: u8, y: u8| ((x as i32 - y as i32) * (x as i32 - y as i32)) as u32;
        d(a.0, r) + d(a.1, g) + d(a.2, b)
    };
    if dist((grey_val, grey_val, grey_val)) < dist(cube) {
        232 + grey_step
    } else {
        cube_idx
    }
}

/// Nearest of the 16 terminal colours, by squared distance against their
/// conventional values.
fn basic16(r: u8, g: u8, b: u8) -> Color {
    const TABLE: [(u8, u8, u8, Color); 16] = [
        (0, 0, 0, Color::Black),
        (128, 0, 0, Color::Red),
        (0, 128, 0, Color::Green),
        (128, 128, 0, Color::Yellow),
        (0, 0, 128, Color::Blue),
        (128, 0, 128, Color::Magenta),
        (0, 128, 128, Color::Cyan),
        (192, 192, 192, Color::Gray),
        (128, 128, 128, Color::DarkGray),
        (255, 0, 0, Color::LightRed),
        (0, 255, 0, Color::LightGreen),
        (255, 255, 0, Color::LightYellow),
        (0, 0, 255, Color::LightBlue),
        (255, 0, 255, Color::LightMagenta),
        (0, 255, 255, Color::LightCyan),
        (255, 255, 255, Color::White),
    ];
    let mut best = Color::White;
    let mut best_d = u32::MAX;
    for (tr, tg, tb, c) in TABLE {
        let d = |x: u8, y: u8| ((x as i32 - y as i32) * (x as i32 - y as i32)) as u32;
        let dist = d(tr, r) + d(tg, g) + d(tb, b);
        if dist < best_d {
            best_d = dist;
            best = c;
        }
    }
    best
}

/// Color fidelity of the attached terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    TrueColor,
    Ansi256,
    Ansi16,
    Mono,
}

#[derive(Debug, Clone, Copy)]
pub struct Theme {
    pub tier: Tier,

    // ---- roles the screens already used (kept, remapped) ----
    /// All content. Always `Reset` — the reader's own foreground.
    pub text: Color,
    /// Meta, hints, column headers.
    pub dim: Color,
    /// Focused border, unread dot, links, headings, spinner.
    pub accent: Color,
    /// Alias of `accent` (DESIGN.md: "`header` and `link` both map to accent").
    pub link: Color,
    /// Alias of `accent`.
    pub header: Color,
    pub warn: Color,
    pub error: Color,
    /// Selected row band. `Reset` on Ansi16/Mono — see `selected()`.
    pub selected_bg: Color,

    // ---- new roles ----
    /// Unfocused borders, rules, quote gutters.
    pub faint: Color,
    /// The ONE primary key cap per bar, and the active chip.
    pub accent_bg: Color,
    pub accent_fg: Color,
    /// Every other key cap.
    pub keycap_bg: Color,
    pub keycap_fg: Color,
    /// The header band.
    pub chrome_bg: Color,
    pub chrome_fg: Color,
    pub chrome_dim: Color,
    /// Unread counts in the header, only when > 0.
    pub badge_bg: Color,
    pub badge_fg: Color,
    /// Solved, gate ready, success toast.
    pub ok: Color,
    /// `[ICODE]` / `[CODE]`.
    pub code_fg: Color,
    pub code_bg: Color,
}

impl Theme {
    pub const fn truecolor() -> Self {
        Theme {
            tier: Tier::TrueColor,
            text: Color::Reset,
            dim: Color::Rgb(0x6F, 0x7C, 0x8B),
            accent: Color::Rgb(0x4D, 0xA3, 0xF5),
            link: Color::Rgb(0x4D, 0xA3, 0xF5),
            header: Color::Rgb(0x4D, 0xA3, 0xF5),
            warn: Color::Rgb(0xFF, 0xB9, 0x02),
            error: Color::Rgb(0xF5, 0x4E, 0x25),
            selected_bg: Color::Rgb(0x17, 0x30, 0x4A),
            faint: Color::Rgb(0x31, 0x3B, 0x47),
            accent_bg: Color::Rgb(0x00, 0x78, 0xD4),
            accent_fg: Color::Rgb(0xFF, 0xFF, 0xFF),
            keycap_bg: Color::Rgb(0x23, 0x2C, 0x38),
            keycap_fg: Color::Rgb(0xE6, 0xED, 0xF3),
            chrome_bg: Color::Rgb(0x0F, 0x6C, 0xBD),
            chrome_fg: Color::Rgb(0xFF, 0xFF, 0xFF),
            chrome_dim: Color::Rgb(0xBC, 0xD6, 0xEE),
            badge_bg: Color::Rgb(0xFF, 0xB9, 0x02),
            badge_fg: Color::Rgb(0x1A, 0x13, 0x00),
            ok: Color::Rgb(0x81, 0xB8, 0x00),
            code_fg: Color::Rgb(0xF0, 0xC6, 0x74),
            code_bg: Color::Rgb(0x1A, 0x20, 0x28),
        }
    }

    pub const fn ansi256() -> Self {
        Theme {
            tier: Tier::Ansi256,
            text: Color::Reset,
            dim: Color::Indexed(243),
            accent: Color::Indexed(75),
            link: Color::Indexed(75),
            header: Color::Indexed(75),
            warn: Color::Indexed(214),
            error: Color::Indexed(202),
            selected_bg: Color::Indexed(236),
            faint: Color::Indexed(238),
            accent_bg: Color::Indexed(31),
            accent_fg: Color::Indexed(231),
            keycap_bg: Color::Indexed(236),
            keycap_fg: Color::Indexed(254),
            chrome_bg: Color::Indexed(24),
            chrome_fg: Color::Indexed(231),
            chrome_dim: Color::Indexed(153),
            badge_bg: Color::Indexed(214),
            badge_fg: Color::Indexed(16),
            ok: Color::Indexed(106),
            code_fg: Color::Indexed(221),
            code_bg: Color::Indexed(234),
        }
    }

    pub const fn ansi16() -> Self {
        Theme {
            tier: Tier::Ansi16,
            text: Color::Reset,
            dim: Color::DarkGray,
            accent: Color::LightBlue,
            link: Color::LightBlue,
            header: Color::LightBlue,
            warn: Color::Yellow,
            error: Color::Red,
            // No usable 16-color selection band: `selected()` reverses instead.
            selected_bg: Color::Reset,
            faint: Color::DarkGray,
            accent_bg: Color::Blue,
            accent_fg: Color::White,
            keycap_bg: Color::DarkGray,
            keycap_fg: Color::White,
            chrome_bg: Color::Blue,
            chrome_fg: Color::White,
            chrome_dim: Color::Gray,
            badge_bg: Color::Yellow,
            badge_fg: Color::Black,
            ok: Color::Green,
            code_fg: Color::Yellow,
            code_bg: Color::Reset,
        }
    }

    /// `NO_COLOR`: every role is `Reset`. Emphasis survives as BOLD/REVERSED,
    /// which is not color.
    pub const fn mono() -> Self {
        Theme {
            tier: Tier::Mono,
            text: Color::Reset,
            dim: Color::Reset,
            accent: Color::Reset,
            link: Color::Reset,
            header: Color::Reset,
            warn: Color::Reset,
            error: Color::Reset,
            selected_bg: Color::Reset,
            faint: Color::Reset,
            accent_bg: Color::Reset,
            accent_fg: Color::Reset,
            keycap_bg: Color::Reset,
            keycap_fg: Color::Reset,
            chrome_bg: Color::Reset,
            chrome_fg: Color::Reset,
            chrome_dim: Color::Reset,
            badge_bg: Color::Reset,
            badge_fg: Color::Reset,
            ok: Color::Reset,
            code_fg: Color::Reset,
            code_bg: Color::Reset,
        }
    }

    /// Back-compat alias for the pre-redesign constructor used by tests.
    pub const fn dark() -> Self {
        Theme::truecolor()
    }

    /// Back-compat alias for the pre-redesign `NO_COLOR` constructor.
    pub const fn no_color() -> Self {
        Theme::mono()
    }

    pub fn detect() -> Self {
        if std::env::var_os("NO_COLOR").is_some() {
            return Theme::mono();
        }
        let colorterm = std::env::var("COLORTERM").unwrap_or_default();
        let colorterm = colorterm.to_ascii_lowercase();
        if colorterm.contains("truecolor") || colorterm.contains("24bit") {
            return Theme::truecolor();
        }
        let term = std::env::var("TERM").unwrap_or_default();
        if term.contains("256color") {
            return Theme::ansi256();
        }
        if std::env::var_os("WT_SESSION").is_some() || std::env::var_os("WT_PROFILE_ID").is_some() {
            return Theme::truecolor();
        }
        #[cfg(windows)]
        {
            Theme::truecolor()
        }
        #[cfg(not(windows))]
        Theme::ansi16()
    }

    /// True where no selection background exists, so bands must be reversed.
    fn reverses(&self) -> bool {
        matches!(self.tier, Tier::Ansi16 | Tier::Mono)
    }

    // ---- style helpers ----

    pub fn base(&self) -> Style {
        Style::new().fg(self.text)
    }

    pub fn dim(&self) -> Style {
        Style::new().fg(self.dim)
    }

    pub fn faint(&self) -> Style {
        Style::new().fg(self.faint)
    }

    /// The existing no-arg panel title: accent + bold. Kept for callers that
    /// do not know about focus; `panel_title(focused)` is the new form.
    pub fn title(&self) -> Style {
        Style::new().fg(self.header).add_modifier(Modifier::BOLD)
    }

    /// DESIGN.md calls this `title(focused)`; Rust has no overloading and the
    /// no-arg `title()` above must keep working, so the focus-aware variant
    /// carries a distinct name.
    pub fn panel_title(&self, focused: bool) -> Style {
        if focused {
            Style::new().fg(self.text).add_modifier(Modifier::BOLD)
        } else {
            Style::new().fg(self.dim).add_modifier(Modifier::BOLD)
        }
    }

    pub fn border(&self, focused: bool) -> Style {
        if focused {
            Style::new().fg(self.accent)
        } else {
            Style::new().fg(self.faint)
        }
    }

    pub fn selected(&self) -> Style {
        let st = Style::new().add_modifier(Modifier::BOLD);
        if self.reverses() {
            st.add_modifier(Modifier::REVERSED)
        } else {
            st.bg(self.selected_bg)
        }
    }

    /// One key cap. Exactly one cap per key bar is `primary`.
    pub fn keycap(&self, primary: bool) -> Style {
        if primary {
            let st = Style::new()
                .fg(self.accent_fg)
                .bg(self.accent_bg)
                .add_modifier(Modifier::BOLD);
            if self.reverses() && self.tier == Tier::Mono {
                return Style::new()
                    .add_modifier(Modifier::BOLD)
                    .add_modifier(Modifier::REVERSED);
            }
            st
        } else {
            let st = Style::new().fg(self.keycap_fg).bg(self.keycap_bg);
            if self.tier == Tier::Mono {
                return Style::new().add_modifier(Modifier::BOLD);
            }
            st
        }
    }

    /// The header band.
    pub fn chrome(&self) -> Style {
        let st = Style::new().fg(self.chrome_fg).bg(self.chrome_bg);
        if self.tier == Tier::Mono {
            return Style::new().add_modifier(Modifier::REVERSED);
        }
        st
    }

    pub fn chrome_bold(&self) -> Style {
        self.chrome().add_modifier(Modifier::BOLD)
    }

    pub fn chrome_dim(&self) -> Style {
        let st = Style::new().fg(self.chrome_dim).bg(self.chrome_bg);
        if self.tier == Tier::Mono {
            return Style::new().add_modifier(Modifier::REVERSED);
        }
        st
    }

    /// Unread counts in the header — render only when the count is > 0.
    pub fn badge(&self) -> Style {
        let st = Style::new()
            .fg(self.badge_fg)
            .bg(self.badge_bg)
            .add_modifier(Modifier::BOLD);
        if self.tier == Tier::Mono {
            return Style::new()
                .add_modifier(Modifier::BOLD)
                .add_modifier(Modifier::REVERSED);
        }
        st
    }

    pub fn link(&self) -> Style {
        Style::new()
            .fg(self.link)
            .add_modifier(Modifier::UNDERLINED)
    }

    pub fn code(&self) -> Style {
        Style::new().fg(self.code_fg).bg(self.code_bg)
    }
}

/// Time formatting: relative for <24h, absolute after.
pub fn fmt_time(ts: i64) -> String {
    let Ok(t) = time::OffsetDateTime::from_unix_timestamp(ts) else {
        return String::new();
    };
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    let age = now - ts;
    if age < 0 {
        return t.date().to_string();
    }
    if age < 60 {
        "just now".into()
    } else if age < 3600 {
        format!("{}m ago", age / 60)
    } else if age < 86_400 {
        format!("{}h ago", age / 3600)
    } else {
        format!("{}", t.date())
    }
}

/// The list-column age (DESIGN.md row grammar): `14h` under a day, `6d` under
/// a month, `Jul 26` within the current year, then the bare year.
///
/// Never wider than 6 cells, which is what lets the `Active` column be a fixed
/// right-aligned 6 — a longer spelling would push the whole row.
pub fn fmt_age(ts: i64) -> String {
    fmt_age_at(ts, time::OffsetDateTime::now_utc())
}

/// `fmt_age` with an injected "now", so the ladder is testable without
/// freezing the clock.
pub fn fmt_age_at(ts: i64, now: time::OffsetDateTime) -> String {
    fmt_age_parts_at(ts, now).0
}

/// `fmt_age`, plus whether the rung it landed on is *relative* ("now",
/// "45m", "14h", "6d") rather than a calendar date or bare year.
///
/// A caller that wraps the text in its own phrase (`"started {} ago"`) needs
/// this to know whether "ago" is even grammatical: it is for the relative
/// rungs, but not once the ladder falls back to `"Jul 26"` or `"2025"` —
/// "started Jul 26 ago" / "started 2025 ago" (issue #578).
pub fn fmt_age_parts(ts: i64) -> (String, bool) {
    fmt_age_parts_at(ts, time::OffsetDateTime::now_utc())
}

/// `fmt_age_parts` with an injected "now", so the ladder is testable without
/// freezing the clock.
pub fn fmt_age_parts_at(ts: i64, now: time::OffsetDateTime) -> (String, bool) {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let Ok(t) = time::OffsetDateTime::from_unix_timestamp(ts) else {
        return (String::new(), true);
    };
    let age = now.unix_timestamp() - ts;
    if age < 0 {
        // A clock skew or a scheduled post: fall back to the date rather than
        // printing a negative age.
        return (format!("{} {}", MONTHS[t.month() as usize - 1], t.day()), false);
    }
    if age < 60 {
        return ("now".into(), true);
    }
    if age < 3_600 {
        return (format!("{}m", age / 60), true);
    }
    if age < 86_400 {
        return (format!("{}h", age / 3_600), true);
    }
    if age < 30 * 86_400 {
        return (format!("{}d", age / 86_400), true);
    }
    if t.year() == now.year() {
        return (format!("{} {}", MONTHS[t.month() as usize - 1], t.day()), false);
    }
    (t.year().to_string(), false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(y: i32, m: time::Month, d: u8) -> time::OffsetDateTime {
        time::OffsetDateTime::new_utc(
            time::Date::from_calendar_date(y, m, d).expect("date"),
            time::Time::from_hms(12, 0, 0).expect("time"),
        )
    }

    #[test]
    fn fmt_age_walks_the_design_ladder() {
        let now = at(2026, time::Month::August, 30);
        let n = now.unix_timestamp();
        assert_eq!(fmt_age_at(n - 5, now), "now");
        assert_eq!(fmt_age_at(n - 60, now), "1m");
        assert_eq!(fmt_age_at(n - 45 * 60, now), "45m");
        assert_eq!(fmt_age_at(n - 14 * 3600, now), "14h");
        assert_eq!(fmt_age_at(n - 23 * 3600, now), "23h");
        assert_eq!(fmt_age_at(n - 24 * 3600, now), "1d");
        assert_eq!(fmt_age_at(n - 6 * 86_400, now), "6d");
        assert_eq!(fmt_age_at(n - 29 * 86_400, now), "29d");
        // Past a month, the same calendar year reads as a date…
        assert_eq!(fmt_age_at(at(2026, time::Month::July, 26).unix_timestamp(), now), "Jul 26");
        assert_eq!(fmt_age_at(at(2026, time::Month::February, 27).unix_timestamp(), now), "Feb 27");
        // …and an older year as the year alone.
        assert_eq!(fmt_age_at(at(2025, time::Month::November, 2).unix_timestamp(), now), "2025");
        assert_eq!(fmt_age_at(at(2013, time::Month::May, 1).unix_timestamp(), now), "2013");
    }

    #[test]
    fn fmt_age_never_exceeds_the_six_cell_active_column() {
        let now = at(2026, time::Month::August, 30);
        let n = now.unix_timestamp();
        let samples = [
            n, n - 30, n - 3600, n - 59 * 60, n - 23 * 3600, n - 29 * 86_400,
            at(2026, time::Month::December, 31).unix_timestamp(),
            at(2026, time::Month::January, 1).unix_timestamp(),
            at(1999, time::Month::January, 1).unix_timestamp(),
            n + 5_000, // clock skew
        ];
        for s in samples {
            let out = fmt_age_at(s, now);
            assert!(
                out.chars().count() <= 6,
                "{out:?} is wider than the Active column"
            );
        }
    }

    #[test]
    fn every_tier_leaves_body_text_to_the_terminal() {
        for t in [
            Theme::truecolor(),
            Theme::ansi256(),
            Theme::ansi16(),
            Theme::mono(),
        ] {
            assert_eq!(t.text, Color::Reset, "{:?} must not paint body text", t.tier);
        }
    }

    #[test]
    fn mono_paints_nothing() {
        let t = Theme::mono();
        for c in [
            t.dim, t.accent, t.link, t.header, t.warn, t.error, t.selected_bg, t.faint,
            t.accent_bg, t.accent_fg, t.keycap_bg, t.keycap_fg, t.chrome_bg, t.chrome_fg,
            t.chrome_dim, t.badge_bg, t.badge_fg, t.ok, t.code_fg, t.code_bg,
        ] {
            assert_eq!(c, Color::Reset);
        }
        // Emphasis survives without color.
        assert!(t.selected().add_modifier.contains(Modifier::REVERSED));
    }

    #[test]
    fn low_color_tiers_reverse_the_selection_band_instead_of_tinting_it() {
        assert!(
            Theme::ansi16()
                .selected()
                .add_modifier
                .contains(Modifier::REVERSED)
        );
        assert!(
            !Theme::truecolor()
                .selected()
                .add_modifier
                .contains(Modifier::REVERSED)
        );
        assert_eq!(Theme::truecolor().selected().bg, Some(Theme::truecolor().selected_bg));
    }

    #[test]
    fn focus_changes_border_and_title() {
        let t = Theme::truecolor();
        assert_eq!(t.border(true).fg, Some(t.accent));
        assert_eq!(t.border(false).fg, Some(t.faint));
        assert_eq!(t.panel_title(true).fg, Some(t.text));
        assert_eq!(t.panel_title(false).fg, Some(t.dim));
    }

    #[test]
    fn primary_keycap_is_the_only_accent_background() {
        let t = Theme::truecolor();
        assert_eq!(t.keycap(true).bg, Some(t.accent_bg));
        assert_eq!(t.keycap(false).bg, Some(t.keycap_bg));
    }
    /// #702: a post's colour, brought down to what the terminal can show.
    #[test]
    fn quantize_matches_the_terminals_fidelity() {
        use common::bbcode::Rgb;
        let red = Rgb { r: 255, g: 0, b: 0 };
        assert_eq!(
            quantize(Tier::TrueColor, red),
            Some(Color::Rgb(255, 0, 0)),
            "truecolor is exact"
        );
        // 16 + 36*5 + 6*0 + 0 = 196, xterm's pure red.
        assert_eq!(quantize(Tier::Ansi256, red), Some(Color::Indexed(196)));
        assert_eq!(quantize(Tier::Ansi16, red), Some(Color::LightRed));
        assert_eq!(quantize(Tier::Mono, red), None, "mono means no colour");

        // A near-grey should take the grey ramp, not the cube.
        let grey = Rgb { r: 120, g: 122, b: 121 };
        match quantize(Tier::Ansi256, grey) {
            Some(Color::Indexed(i)) => {
                assert!((232..=255).contains(&i), "expected the grey ramp, got {i}")
            }
            other => panic!("{other:?}"),
        }
        // Every tier answers for every colour, and never panics.
        for tier in [Tier::TrueColor, Tier::Ansi256, Tier::Ansi16, Tier::Mono] {
            for (r, g, b) in [(0, 0, 0), (255, 255, 255), (13, 200, 77), (1, 1, 1)] {
                let _ = quantize(tier, Rgb { r, g, b });
            }
        }
    }

}
