//! Shared chrome: the header band, the key bar, the status line, and the one
//! panel style every screen uses.
//!
//! Everything here returns plain ratatui values (`Line`, `Span`, `Block`) and
//! takes no `Frame`, so it is unit-testable without a terminal — which is how
//! the overflow rules below are pinned. Nothing here ever emits an escape
//! sequence into span content (CLAUDE.md hard rule 1); OSC goes through
//! `app::emit_raw`.

use std::time::Duration;

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders};

use crate::glyph::Glyphs;
use crate::theme::{Theme, Tier};

/// One key hint: the cap text and what it does. `("Enter", "open")`.
pub type Hint = (&'static str, &'static str);

/// A screen's key bar. Exactly one cap — `keys[primary]` — is drawn in the
/// accent background; everything else is a neutral cap.
///
/// `short` is the narrow-terminal set (DESIGN.md: "below 90 columns screens
/// supply a short hint set"). Dropping whole descriptions is the intended
/// narrow behaviour; the `…` clip is only the last resort, and on its own it
/// cuts *inside* a word. Empty means "no short set — clip instead".
/// `primary` indexes whichever set is drawn, so keep the primary cap first in
/// both.
pub struct Hints {
    pub keys: Vec<Hint>,
    pub short: Vec<Hint>,
    pub primary: usize,
}

impl Hints {
    /// `Hints::new(&[("Enter","open"), ("j/k","move")], 0)`
    pub fn new(keys: &[Hint], primary: usize) -> Self {
        Hints {
            keys: keys.to_vec(),
            short: Vec::new(),
            primary,
        }
    }

    /// Same, plus the set drawn below `NARROW_COLS` columns.
    pub fn with_short(keys: &[Hint], short: &[Hint], primary: usize) -> Self {
        Hints {
            keys: keys.to_vec(),
            short: short.to_vec(),
            primary,
        }
    }
}

/// Below this width a screen's `short` hint set replaces its full one.
pub const NARROW_COLS: u16 = 90;

/// Write-gate state for the status line.
pub enum GateState {
    Ready,
    Waiting { left: Duration, total: Duration },
}

const ELLIPSIS: &str = "\u{2026}"; // …
/// The header mark: a white bubble chip, echoing the rounded speech-bubble
/// logo (`wf-logo.png`) without the four-color glyph inside it — bare
/// red/green/blue/yellow quadrants read as the Microsoft Windows logo with no
/// other context around them, which is a trademark problem out of context.
/// Pure letters, so ASCII and Unicode share the same chip.
const MARK_CHIP: &str = " WF ";
/// Cells the countdown bar occupies.
const GATE_BAR_CELLS: usize = 10;

// ---------------------------------------------------------------- primitives

/// One key cap: ` Enter `. Words, not symbols (DESIGN.md).
pub fn keycap(theme: &Theme, key: &str, primary: bool) -> Span<'static> {
    Span::styled(format!(" {key} "), theme.keycap(primary))
}

/// A neutral inline chip: ` Win11 ` on the key-cap background.
pub fn chip(theme: &Theme, text: &str) -> Span<'static> {
    Span::styled(format!(" {text} "), theme.keycap(false))
}

/// The one active chip per row (DESIGN.md: "the active chip in accent_bg"),
/// e.g. the selected filter in Search's `All | Threads | Posts` row.
pub fn chip_active(theme: &Theme, text: &str) -> Span<'static> {
    Span::styled(format!(" {text} "), theme.keycap(true))
}

/// Two-letter avatar stand-in with a background hashed from the name, so the
/// same member always gets the same color in the same session and across runs.
pub fn initials_chip(theme: &Theme, username: &str) -> Span<'static> {
    let initials = initials(username);
    let style = if theme.tier == Tier::Mono {
        theme.keycap(false)
    } else {
        Style::new().fg(theme.badge_fg).bg(hash_color(theme, username))
    };
    Span::styled(format!(" {initials} "), style)
}

/// Loading frame for `tick`. Frames are single-cell in both glyph sets, so the
/// text after a spinner never shifts.
pub fn spinner(g: &Glyphs, tick: usize) -> &'static str {
    g.spinner[tick % g.spinner.len()]
}

/// A tick derived from the wall clock (8 fps), so a screen can animate a
/// spinner without threading a frame counter through its state. The event loop
/// redraws about every 50 ms, which is comfortably faster than this.
pub fn spinner_tick() -> usize {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| (d.as_millis() / 125) as usize)
        .unwrap_or(0)
}

/// Shorten a XenForo thread prefix for the inline chip.
///
/// `"Windows 11"` → `"Win11"` (numbered releases stay recognisable when
/// squeezed), `"Windows Server"` → `"Server"` (the word "Windows" carries no
/// information on a Windows forum). Anything else is passed through, capped at
/// 8 cells so the title column keeps its width.
pub fn short_prefix(prefix: &str) -> String {
    let p = prefix.trim();
    let out = match p.strip_prefix("Windows ").map(str::trim) {
        Some("") => "Windows".to_string(),
        // "Windows 11 24H2" -> "Win11": the release number is the chip; the
        // build/update qualifier does not fit and is in the title anyway.
        Some(rest) if rest.starts_with(|c: char| c.is_ascii_digit()) => {
            format!("Win{}", rest.split_whitespace().next().unwrap_or(rest))
        }
        Some(rest) => rest.to_string(),
        None => p.to_string(),
    };
    truncate(&out, 8)
}

// ------------------------------------------------------------- header band

/// Row 0. `▀▀ WindowsForum › crumb › crumb` on the left, member + unread
/// badges + online count on the right, the whole row painted in chrome.
///
/// Overflow ladder, in order (DESIGN.md): drop the online count → replace the
/// middle crumbs with `…` → clip the last crumb with `…`. A width that cannot
/// even hold the brand is hard-clipped, never wrapped.
///
/// `online` is not in the DESIGN.md signature because no API surface supplies
/// it yet; it is here so the first rung of the ladder is real code rather than
/// a comment, and `app.rs` passes `None` today.
#[allow(clippy::too_many_arguments)]
pub fn header_line(
    theme: &Theme,
    g: &Glyphs,
    crumbs: &[String],
    user: Option<&str>,
    inbox: u32,
    alerts: u32,
    online: Option<u32>,
    width: u16,
) -> Line<'static> {
    let w = width as usize;
    let brand = brand_spans(theme);
    let brand_w = spans_width(&brand);
    // One leading space before the mark, one minimum gap before the right side.
    const LEAD: usize = 1;
    const GAP: usize = 1;
    let needed = |crumbs: &[String], right_w: usize| {
        LEAD + brand_w + crumbs_width(g, crumbs) + right_w + GAP
    };

    let mut right = right_spans(theme, user, inbox, alerts, online);
    let mut crumbs: Vec<String> = crumbs.to_vec();

    // Rung 1: the online count is the least load-bearing thing on the row.
    if needed(&crumbs, spans_width(&right)) > w && online.is_some() {
        right = right_spans(theme, user, inbox, alerts, None);
    }
    // Rung 2: the path's middle is inferable; its ends are not.
    if needed(&crumbs, spans_width(&right)) > w && crumbs.len() > 2 {
        let last = crumbs.pop().unwrap_or_default();
        let first = crumbs.remove(0);
        crumbs = vec![first, ELLIPSIS.to_string(), last];
    }
    // Rung 3: clip the last crumb into whatever is left.
    let right_w = spans_width(&right);
    if needed(&crumbs, right_w) > w && !crumbs.is_empty() {
        let last_idx = crumbs.len() - 1;
        let others: Vec<String> = crumbs[..last_idx].to_vec();
        // The last crumb costs its text plus its own 3-cell " > " separator.
        let fixed = needed(&others, right_w) + 3;
        let budget = w.saturating_sub(fixed);
        if budget >= 2 {
            crumbs[last_idx] = truncate(&crumbs[last_idx], budget);
        } else {
            crumbs.truncate(last_idx);
        }
    }

    let mut spans = vec![Span::styled(" ", theme.chrome())];
    spans.extend(brand);
    let last_idx = crumbs.len().saturating_sub(1);
    for (i, c) in crumbs.iter().enumerate() {
        spans.push(Span::styled(format!(" {} ", g.crumb), theme.chrome_dim()));
        let style = if i == last_idx {
            theme.chrome_bold()
        } else {
            theme.chrome()
        };
        spans.push(Span::styled(c.clone(), style));
    }

    let used = spans_width(&spans);
    let right_w = spans_width(&right);
    // `<=`, not `<`: rung 3 sizes the last crumb so the row lands exactly on
    // the width, and a strict `<` would then throw the whole right side away.
    if used + right_w <= w {
        spans.push(Span::styled(" ".repeat(w - used - right_w), theme.chrome()));
        spans.extend(right);
    } else {
        // No room for the right side at all: the path wins.
        spans = clip_spans(spans, w);
        let used = spans_width(&spans);
        if used < w {
            spans.push(Span::styled(" ".repeat(w - used), theme.chrome()));
        }
    }
    Line::from(clip_spans(spans, w))
}

fn brand_spans(theme: &Theme) -> Vec<Span<'static>> {
    let mut spans = mark_spans(theme);
    spans.push(Span::styled(" ", theme.chrome()));
    spans.push(Span::styled("Windows", theme.chrome_bold()));
    spans.push(Span::styled("Forum", theme.chrome()));
    spans
}

/// The mark: one `MARK_CHIP` cell, background chrome_fg (white) foreground
/// chrome_bg (brand blue), bold — the same chip in both glyph sets, since it
/// is letters rather than block-drawing characters.
fn mark_spans(theme: &Theme) -> Vec<Span<'static>> {
    vec![Span::styled(
        MARK_CHIP,
        Style::new()
            .fg(theme.chrome_bg)
            .bg(theme.chrome_fg)
            .add_modifier(Modifier::BOLD),
    )]
}

fn right_spans(
    theme: &Theme,
    user: Option<&str>,
    inbox: u32,
    alerts: u32,
    online: Option<u32>,
) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    match user {
        // Signed in: name plus the Inbox/Alerts badges.
        Some(u) => {
            spans.push(Span::styled(u.to_string(), theme.chrome_bold()));
            spans.extend(counter(theme, "Inbox", inbox));
            spans.extend(counter(theme, "Alerts", alerts));
        }
        // Logged out (no stored session, or one that just ended): say so
        // explicitly rather than leaving a blank where the member's name
        // goes, and never show unread badges — they would be either 0 (from
        // a `Default` state that never had a session) or a stale carry-over
        // from the session that just ended, either way meaningless with
        // nobody signed in (issue #561).
        None => {
            spans.push(Span::styled("not signed in", theme.chrome_dim()));
        }
    }
    if let Some(n) = online {
        spans.push(Span::styled(
            format!("   {} online", thousands(n)),
            theme.chrome_dim(),
        ));
    }
    spans.push(Span::styled(" ", theme.chrome()));
    spans
}

/// `Inbox [2]` with a badge only when the count is > 0; `Inbox 0` in chrome_dim
/// otherwise, so the row does not shout at a member with nothing waiting.
fn counter(theme: &Theme, label: &str, n: u32) -> Vec<Span<'static>> {
    if n > 0 {
        vec![
            Span::styled(format!("   {label} "), theme.chrome_dim()),
            Span::styled(format!(" {n} "), theme.badge()),
        ]
    } else {
        vec![Span::styled(format!("   {label} 0"), theme.chrome_dim())]
    }
}

// ---------------------------------------------------------------- key bar

/// Row n-2. Below `NARROW_COLS` a screen's `short` set is drawn instead of its
/// full one — whole descriptions go, never half a word. Whatever is drawn is
/// still clipped with `…` from the right as a last resort; the leftmost hints
/// are the most used, so losing the tail is the cheap loss.
pub fn key_bar(theme: &Theme, hints: &Hints, width: u16) -> Line<'static> {
    let keys = if width < NARROW_COLS && !hints.short.is_empty() {
        &hints.short
    } else {
        &hints.keys
    };
    let mut spans = vec![Span::raw(" ")];
    for (i, (key, desc)) in keys.iter().enumerate() {
        spans.push(keycap(theme, key, i == hints.primary));
        spans.push(Span::styled(format!(" {desc}  "), theme.dim()));
    }
    let w = width as usize;
    if spans_width(&spans) > w {
        let mut clipped = clip_spans(spans, w.saturating_sub(1));
        clipped.push(Span::styled(ELLIPSIS.to_string(), theme.dim()));
        return Line::from(clipped);
    }
    Line::from(spans)
}

// -------------------------------------------------------------- status line

/// Row n-1. Free text (or a toast) on the left, the write gate on the right.
pub fn status_line(
    theme: &Theme,
    g: &Glyphs,
    left: &str,
    left_style: Style,
    gate: GateState,
    width: u16,
) -> Line<'static> {
    let w = width as usize;
    let mut right: Vec<Span<'static>> = vec![Span::styled("write gate ", theme.dim())];
    match gate {
        GateState::Ready => {
            right.push(Span::styled(g.unread.to_string(), Style::new().fg(theme.ok)));
            right.push(Span::styled(" ready ", theme.dim()));
        }
        GateState::Waiting { left: rem, total } => {
            let secs = rem.as_secs_f64().ceil() as u64;
            right.push(Span::styled(
                g.unread.to_string(),
                Style::new().fg(theme.warn),
            ));
            right.push(Span::styled(
                format!(" {secs} s "),
                Style::new().fg(theme.warn),
            ));
            right.extend(gate_bar(theme, g, rem, total));
            right.push(Span::raw(" "));
        }
    }
    let right_w = spans_width(&right);

    let mut spans = vec![Span::styled(format!(" {left}"), left_style)];
    let used = spans_width(&spans);
    if used + right_w < w {
        spans.push(Span::raw(" ".repeat(w - used - right_w)));
        spans.extend(right);
        Line::from(spans)
    } else if right_w < w {
        // The gate is the one thing on this row that must never disappear.
        let keep = w - right_w;
        let mut spans = clip_spans(spans, keep);
        let used = spans_width(&spans);
        if used < keep {
            spans.push(Span::raw(" ".repeat(keep - used)));
        }
        spans.extend(right);
        // Safety net matching header_line's last statement: whatever `left`
        // contained (wide characters included), the combined line can never
        // exceed `w` cells, so the write gate can never be pushed off the
        // Rect at render time.
        Line::from(clip_spans(spans, w))
    } else {
        Line::from(clip_spans(spans, w))
    }
}

/// Ten cells filling up as the cooldown elapses, so the bar grows toward
/// "you may post" rather than draining toward it.
fn gate_bar(theme: &Theme, g: &Glyphs, left: Duration, total: Duration) -> Vec<Span<'static>> {
    let total_ms = total.as_millis().max(1);
    let left_ms = left.as_millis().min(total_ms);
    let elapsed = total_ms - left_ms;
    let filled = ((elapsed * GATE_BAR_CELLS as u128) / total_ms) as usize;
    let filled = filled.min(GATE_BAR_CELLS);
    let mut spans = Vec::new();
    if filled > 0 {
        spans.push(Span::styled(
            g.gate_on.repeat(filled),
            Style::new().fg(theme.ok),
        ));
    }
    if filled < GATE_BAR_CELLS {
        spans.push(Span::styled(
            g.gate_off.repeat(GATE_BAR_CELLS - filled),
            theme.faint(),
        ));
    }
    spans
}

// ------------------------------------------------------------------- panel

/// The one panel style: rounded (plain in ASCII), accent border when focused,
/// faint when not; optional right-aligned top segment and bottom-left segment.
pub fn panel(
    theme: &Theme,
    g: &Glyphs,
    title: &str,
    focused: bool,
    right: Option<&str>,
    bottom: Option<&str>,
) -> Block<'static> {
    let mut b = Block::default()
        .borders(Borders::ALL)
        .border_type(g.border_type())
        .border_style(theme.border(focused))
        .title_top(Line::from(Span::styled(
            format!(" {title} "),
            theme.panel_title(focused),
        )));
    if let Some(r) = right {
        let text = if g.ascii {
            format!("[ {r} ]")
        } else {
            format!("\u{2524} {r} \u{251C}")
        };
        b = b.title_top(Line::from(Span::styled(text, theme.dim())).right_aligned());
    }
    if let Some(bt) = bottom {
        b = b.title_bottom(Line::from(Span::styled(
            format!(" {bt} "),
            theme.dim(),
        )));
    }
    b
}

// ------------------------------------------------------------------ helpers

fn spans_width(spans: &[Span<'static>]) -> usize {
    spans.iter().map(Span::width).sum()
}

/// Display width in terminal cells — ratatui's own unicode-width measure,
/// the one the buffer bills a run of cells at. Shared by every "fits exactly
/// W cells" computation in the chrome and overlay modules so a CJK/emoji
/// character (2 cells) is never billed as 1 (see issues #511/#512/#516).
pub(crate) fn cell_width(s: &str) -> usize {
    Span::raw(s).width()
}

/// Take a prefix of `s` whose total display width does not exceed `max`
/// cells — never characters — so a double-width character never straddles
/// the boundary and gets counted as narrower than it renders.
pub(crate) fn take_cells(s: &str, max: usize) -> String {
    let mut out = String::new();
    let mut used = 0usize;
    for ch in s.chars() {
        let mut buf = [0u8; 4];
        let w = cell_width(ch.encode_utf8(&mut buf));
        if used + w > max {
            break;
        }
        out.push(ch);
        used += w;
    }
    out
}

fn crumbs_width(g: &Glyphs, crumbs: &[String]) -> usize {
    // Each crumb is " > " plus its text.
    crumbs
        .iter()
        .map(|c| cell_width(c) + 2 + cell_width(g.crumb))
        .sum()
}

/// Truncate to `max` cells, spending the last cell on `…` when it cuts.
fn truncate(s: &str, max: usize) -> String {
    if cell_width(s) <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let cut = take_cells(s, max - 1);
    format!("{cut}{ELLIPSIS}")
}

/// Hard-clip a span run to `max` cells, keeping each span's style.
fn clip_spans(spans: Vec<Span<'static>>, max: usize) -> Vec<Span<'static>> {
    let mut out = Vec::with_capacity(spans.len());
    let mut used = 0usize;
    for s in spans {
        let w = s.width();
        if used + w <= max {
            used += w;
            out.push(s);
            continue;
        }
        let room = max - used;
        if room > 0 {
            let text = take_cells(&s.content, room);
            out.push(Span::styled(text, s.style));
        }
        break;
    }
    out
}

fn initials(username: &str) -> String {
    let mut words = username.split_whitespace();
    let first = words.next().unwrap_or("");
    let mut chars = first.chars().filter(|c| c.is_alphanumeric());
    let a = chars.next().unwrap_or('?');
    let b = match words.next().and_then(|w| w.chars().find(|c| c.is_alphanumeric())) {
        Some(c) => c,
        None => chars.next().unwrap_or(' '),
    };
    let mut out: String = a.to_uppercase().collect();
    if b != ' ' {
        out.extend(b.to_uppercase());
    }
    out
}

/// FNV-1a over the name, so the color is stable across runs and machines
/// (`DefaultHasher` is explicitly not).
fn hash_color(theme: &Theme, name: &str) -> ratatui::style::Color {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in name.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x1000_0000_01b3);
    }
    let palette = [
        theme.accent,
        theme.accent_bg,
        theme.ok,
        theme.warn,
        theme.error,
        theme.chrome_bg,
    ];
    palette[(h % palette.len() as u64) as usize]
}

fn thousands(n: u32) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::glyph::{ASCII, UNICODE};

    fn crumbs(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn header_fills_exactly_the_width_at_every_size() {
        let t = Theme::truecolor();
        let c = crumbs(&["Forums", "Windows Help and Support", "How do I control MS edge?"]);
        for w in [40u16, 60, 80, 100, 120, 200] {
            let line = header_line(&t, &UNICODE, &c, Some("Mike"), 2, 5, Some(1384), w);
            assert_eq!(
                line.width(),
                w as usize,
                "header at width {w} is {} cells",
                line.width()
            );
        }
    }

    /// A wide-character (CJK) crumb must still be billed by cell width, not
    /// char count, in every rung of the overflow ladder (issue #516).
    #[test]
    fn header_fills_exactly_the_width_with_cjk_crumbs() {
        let t = Theme::truecolor();
        let c = crumbs(&["视频编辑软件推荐帮助教程升级指南", "第二个论坛版块名称"]);
        for w in [40u16, 60, 80, 100, 120, 200] {
            let line = header_line(&t, &UNICODE, &c, Some("Mike"), 2, 5, Some(1384), w);
            assert_eq!(
                line.width(),
                w as usize,
                "CJK header at width {w} is {} cells",
                line.width()
            );
        }
    }

    #[test]
    fn header_overflow_ladder_drops_online_then_middle_then_clips() {
        let t = Theme::truecolor();
        let c = crumbs(&["Forums", "Windows Help and Support", "How do I control MS edge update schedule?"]);

        // Wide: everything fits, so the online count survives.
        let wide = header_line(&t, &UNICODE, &c, Some("Mike"), 2, 5, Some(1384), 200);
        let wide_text: String = wide.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(wide_text.contains("1,384 online"), "{wide_text}");
        assert!(wide_text.contains("Windows Help and Support"));
        assert!(!wide_text.contains(ELLIPSIS));

        // Rung 1: online goes first, the path is untouched.
        let mid = header_line(&t, &UNICODE, &c, Some("Mike"), 2, 5, Some(1384), 130);
        let mid_text: String = mid.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(!mid_text.contains("online"), "{mid_text}");
        assert!(mid_text.contains("Windows Help and Support"), "{mid_text}");

        // Rung 2: the middle crumb becomes an ellipsis, the ends survive.
        let narrow = header_line(&t, &UNICODE, &c, Some("Mike"), 2, 5, Some(1384), 110);
        let narrow_text: String = narrow.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(!narrow_text.contains("Windows Help and Support"), "{narrow_text}");
        assert!(narrow_text.contains("Forums"), "{narrow_text}");
        assert!(narrow_text.contains(ELLIPSIS), "{narrow_text}");

        // Rung 3: the last crumb is clipped, and the row still fits exactly.
        let tiny = header_line(&t, &UNICODE, &c, Some("Mike"), 2, 5, Some(1384), 80);
        assert_eq!(tiny.width(), 80);
        let tiny_text: String = tiny.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            !tiny_text.contains("update schedule?"),
            "last crumb should be clipped: {tiny_text}"
        );
        // Clipping the crumb must not cost the member their unread counts: the
        // rung-3 budget reserves the right side and a gap before it.
        assert!(tiny_text.contains("Mike"), "{tiny_text}");
        assert!(tiny_text.contains("Alerts"), "{tiny_text}");
        assert!(
            tiny_text.contains(&format!("{ELLIPSIS} Mike")),
            "a gap must separate the crumb from the member: {tiny_text}"
        );
    }

    #[test]
    fn header_survives_absurdly_small_widths() {
        let t = Theme::truecolor();
        let c = crumbs(&["Forums", "Windows Help and Support"]);
        for w in [1u16, 2, 3, 8, 16, 24] {
            let line = header_line(&t, &UNICODE, &c, Some("Mike"), 2, 5, Some(1384), w);
            assert_eq!(line.width(), w as usize, "width {w}");
        }
    }

    #[test]
    fn header_mark_is_a_four_cell_chip_unicode_and_ascii() {
        let t = Theme::truecolor();
        let uni = header_line(&t, &UNICODE, &[], None, 0, 0, None, 80);
        // lead space + the chip, one span: " WF " white-on-blue, bold.
        assert_eq!(uni.spans[1].content.as_ref(), MARK_CHIP);
        assert_eq!(uni.spans[1].width(), 4);
        assert_eq!(uni.spans[1].style.fg, Some(t.chrome_bg));
        assert_eq!(uni.spans[1].style.bg, Some(t.chrome_fg));
        assert!(uni.spans[1].style.add_modifier.contains(Modifier::BOLD));

        // Pure letters: ASCII draws the identical chip, not a shrunken one.
        let ascii = header_line(&t, &ASCII, &[], None, 0, 0, None, 80);
        assert_eq!(ascii.spans[1].content.as_ref(), MARK_CHIP);
        assert_eq!(ascii.spans[1].width(), 4);
    }

    #[test]
    fn header_badges_only_when_unread() {
        let t = Theme::truecolor();
        let is_badge = |s: &Span<'static>| {
            s.style.bg == Some(t.badge_bg) && s.style.fg == Some(t.badge_fg)
        };
        let with = header_line(&t, &UNICODE, &[], Some("Mike"), 2, 0, None, 80);
        assert!(with.spans.iter().any(is_badge));
        let text: String = with.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("Alerts 0"), "{text}");

        let without = header_line(&t, &UNICODE, &[], Some("Mike"), 0, 0, None, 80);
        assert!(!without.spans.iter().any(is_badge));
    }

    /// Issue #561: a logged-out header (no stored session, or one that just
    /// ended) used to render a blank where the username goes and still show
    /// `Inbox 0`/`Alerts 0` — indistinguishable from a signed-in member with
    /// an empty inbox. `user: None` must say "not signed in" and must never
    /// show the unread badges at all (not even the zero form).
    #[test]
    fn header_says_not_signed_in_and_hides_badges_when_logged_out() {
        let t = Theme::truecolor();
        let line = header_line(&t, &UNICODE, &[], None, 3, 5, None, 80);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("not signed in"), "{text}");
        assert!(!text.contains("Inbox"), "{text}");
        assert!(!text.contains("Alerts"), "{text}");
    }

    #[test]
    fn key_bar_clips_from_the_right_and_never_overflows() {
        let t = Theme::truecolor();
        let h = Hints::new(
            &[
                ("Enter", "open"),
                ("j/k", "move"),
                ("Tab", "pane"),
                ("N", "new thread"),
                ("m", "mark read"),
                ("/", "search"),
                ("?", "help"),
                ("q", "quit"),
            ],
            0,
        );
        let full = key_bar(&t, &h, 200);
        let full_text: String = full.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(full_text.contains("quit"));
        assert!(!full_text.contains(ELLIPSIS));

        for w in [10u16, 20, 40, 60, 80] {
            let line = key_bar(&t, &h, w);
            assert!(line.width() <= w as usize, "key bar overflowed at {w}");
        }
        let narrow = key_bar(&t, &h, 40);
        let narrow_text: String = narrow.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(narrow_text.ends_with(ELLIPSIS), "{narrow_text}");
        // The leading hints — the ones people actually press — survive.
        assert!(narrow_text.contains("Enter"), "{narrow_text}");
        assert!(!narrow_text.contains("quit"), "{narrow_text}");
    }

    #[test]
    fn key_bar_swaps_in_the_short_set_below_ninety_columns() {
        let t = Theme::truecolor();
        let h = Hints::with_short(
            &[("Enter", "open"), ("j/k", "move"), ("m", "mark read")],
            &[("Enter", "open"), ("j/k", ""), ("m", "read")],
            0,
        );
        let text = |w: u16| -> String {
            key_bar(&t, &h, w)
                .spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect()
        };
        // At and above the threshold the full words are drawn.
        assert!(text(NARROW_COLS).contains("mark read"), "{}", text(NARROW_COLS));
        // Below it, whole descriptions are dropped — not cut mid-word.
        let narrow = text(NARROW_COLS - 1);
        assert!(!narrow.contains("mark read"), "{narrow}");
        assert!(narrow.contains("read"), "{narrow}");
        assert!(!narrow.contains(ELLIPSIS), "{narrow}");
        assert!(narrow.contains("j/k"), "{narrow}");

        // No short set -> the clip is still the fallback, unchanged.
        let plain = Hints::new(&[("Enter", "open"), ("m", "mark read")], 0);
        let clipped: String = key_bar(&t, &plain, 12)
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(clipped.ends_with(ELLIPSIS), "{clipped}");
    }

    #[test]
    fn key_bar_marks_exactly_one_primary_cap() {
        let t = Theme::truecolor();
        let h = Hints::new(&[("Enter", "open"), ("j/k", "move"), ("q", "quit")], 1);
        let line = key_bar(&t, &h, 120);
        let primaries: Vec<&str> = line
            .spans
            .iter()
            .filter(|s| s.style.bg == Some(t.accent_bg))
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(primaries, vec![" j/k "]);
    }

    #[test]
    fn status_line_keeps_the_gate_visible_and_fits() {
        let t = Theme::truecolor();
        let ready = status_line(&t, &UNICODE, "20 threads", t.dim(), GateState::Ready, 100);
        assert_eq!(ready.width(), 100);
        let text: String = ready.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("write gate"), "{text}");
        assert!(text.contains("ready"), "{text}");

        let waiting = status_line(
            &t,
            &UNICODE,
            "Reply posted",
            t.dim(),
            GateState::Waiting {
                left: Duration::from_secs(24),
                total: Duration::from_secs(30),
            },
            100,
        );
        let text: String = waiting.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("24 s"), "{text}");
        // 6 s of 30 elapsed -> 2 filled cells of 10.
        assert!(text.contains("\u{25AE}\u{25AE}\u{25AF}"), "{text}");

        // A long left string is clipped, the gate is not.
        let long = "x".repeat(300);
        let squeezed = status_line(&t, &UNICODE, &long, t.dim(), GateState::Ready, 40);
        assert!(squeezed.width() <= 40);
        let text: String = squeezed.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("ready"), "{text}");
    }

    /// CJK `left` text must be clipped by cell width, not char count (issue
    /// #512): the gate is exactly 29 cells wide with `Waiting{24s}`, so at
    /// width 40 the middle branch clips the wide-character left text — a
    /// char-count clip would keep too many cells and push the gate past the
    /// Rect at render time.
    #[test]
    fn status_line_clips_wide_left_text_by_cell_not_char_and_keeps_the_gate() {
        let t = Theme::truecolor();
        let cjk = "视频视频视频视频视频"; // 10 chars, 20 cells
        let line = status_line(
            &t,
            &UNICODE,
            cjk,
            t.dim(),
            GateState::Waiting {
                left: Duration::from_secs(24),
                total: Duration::from_secs(30),
            },
            40,
        );
        assert_eq!(line.width(), 40, "must land exactly on the requested width");
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("write gate"), "{text}");
        assert!(text.contains("24 s"), "{text}");
    }

    #[test]
    fn short_prefix_shortens_the_way_the_design_says() {
        assert_eq!(short_prefix("Windows 11"), "Win11");
        assert_eq!(short_prefix("Windows 10"), "Win10");
        assert_eq!(short_prefix("Windows Server"), "Server");
        assert_eq!(short_prefix("Windows 11 24H2"), "Win11");
        assert_eq!(short_prefix("Solved"), "Solved");
        assert_eq!(short_prefix("  Windows 11  "), "Win11");
        assert_eq!(short_prefix("Windows"), "Windows");
        // Capped so the title column keeps its width.
        assert_eq!(short_prefix("Announcement").chars().count(), 8);
    }

    #[test]
    fn chip_active_is_the_only_accent_background_chip() {
        let t = Theme::truecolor();
        assert_eq!(chip(&t, "Threads").style.bg, Some(t.keycap_bg));
        assert_eq!(chip_active(&t, "Latest").style.bg, Some(t.accent_bg));
        assert_eq!(chip_active(&t, "Latest").content.as_ref(), " Latest ");
    }

    #[test]
    fn initials_chip_is_deterministic_and_two_cells_of_text() {
        let t = Theme::truecolor();
        let a = initials_chip(&t, "HItest");
        let b = initials_chip(&t, "HItest");
        assert_eq!(a.content, b.content);
        assert_eq!(a.style.bg, b.style.bg);
        assert_eq!(a.content.as_ref(), " HI ");

        assert_eq!(initials_chip(&t, "WindowsForum AI").content.as_ref(), " WA ");
        assert_eq!(initials_chip(&t, "kemical").content.as_ref(), " KE ");
        // Degenerate names must not panic and must still be a chip.
        assert_eq!(initials_chip(&t, "").content.as_ref(), " ? ");
        assert_eq!(initials_chip(&t, "x").content.as_ref(), " X ");

        // Different names generally land on different colors; the same name
        // never changes.
        let c1 = initials_chip(&t, "alice").style.bg;
        let c2 = initials_chip(&t, "alice").style.bg;
        assert_eq!(c1, c2);
    }

    #[test]
    fn spinner_cycles_and_never_panics() {
        for i in 0..40 {
            assert_eq!(spinner(&UNICODE, i).chars().count(), 1);
            assert_eq!(spinner(&ASCII, i).chars().count(), 1);
        }
        assert_eq!(spinner(&UNICODE, 0), spinner(&UNICODE, 4));
    }

    #[test]
    fn thousands_groups_digits() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1384), "1,384");
        assert_eq!(thousands(1_234_567), "1,234,567");
    }
}

