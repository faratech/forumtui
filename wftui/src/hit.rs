//! The per-frame hit map: what the pointer is over.
//!
//! Terminals deliver touch as mouse events — a tap is a left click, a
//! two-finger scroll is a wheel, and a long press is a right click on most
//! mobile terminals — so there is exactly one hit-testing layer here and both
//! pointers and fingers go through it.
//!
//! Every renderer registers `Rect -> Hit` entries while it draws, into the
//! `HitMap` `App::draw` clears at the top of each frame. Resolution is
//! **last registered wins**, which is what makes overlays work: the palette,
//! the keys card and the link popup draw after the body, so their entries sit
//! on top of whatever they cover.
//!
//! Nothing here draws or reads the terminal: a `HitMap` is plain geometry, so
//! a screen's hit registration is testable on a `TestBackend` frame with no
//! pointer anywhere near it.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Position, Rect};

use crate::screens::InboxTab;

/// Which pane of a two-pane screen a point is in. Home's halves are
/// tree/list, the Inbox's are list/view; the click handler re-uses
/// `Screen::focus_pane_at` (issue #549's rects) to actually move the
/// keyboard, so this payload is what a test asserts on rather than a second
/// source of truth.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HitPane {
    /// Home's Forums tree.
    Tree,
    /// Home's thread list, or the Inbox's tabbed list.
    List,
    /// The Inbox's open-conversation pane.
    View,
}

/// What the cell under the pointer stands for.
///
/// Indices are always **screen-local**: `Row(3)` means "the fourth row of the
/// list the pointer's pane owns", not a global id. That keeps the map free of
/// model types (and of lifetimes) and leaves the meaning where it belongs —
/// with the screen that registered it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Hit {
    /// A list row: index into the list the pointer's pane owns.
    Row(usize),
    /// A key cap: press this key, through `App::handle_key`, so every gating
    /// rule a real keypress obeys still applies.
    Key(&'static str),
    /// A header unread badge — opens the Inbox on that tab.
    Badge(InboxTab),
    /// An Inbox tab chip — switches tab in place.
    Tab(InboxTab),
    /// A focusable pane. Registered under everything else in the pane, so a
    /// click anywhere inside gives it the keyboard.
    Pane(HitPane),
    /// A hyperlink.
    Link(String),
    /// The nth image (1-based, the digit that opens it) of the post it is in.
    Image(usize),
    /// A text field: screen-local index (Compose 0 = title / 1 = body;
    /// New conversation 0 = to / 1 = title / 2 = message; Search 0 = query /
    /// 1 = author).
    Field(usize),
    /// A BBCode cap on the composer's caps row (`^B`, `^I`, …).
    Cap(&'static str),
    /// A breadcrumb segment: the index of the screen it names, or
    /// `usize::MAX` for the brand at the far left, which is Home (#700).
    Crumb(usize),
    /// A go-to palette row: index into the palette's filtered list.
    PaletteRow(usize),
    /// Anywhere outside an open overlay: closes it.
    CloseOverlay,
    /// A post card (thread view) or message card (DM view).
    Post(usize),
}

impl Hit {
    /// True for the hits that are really "this is text, under a pane":
    /// clicking selects, but double/triple click still means word/line
    /// select, and a drag through them is a selection, not a gesture.
    pub fn is_text_like(&self) -> bool {
        matches!(self, Hit::Post(_) | Hit::Pane(_))
    }
}

/// The frame's registered hits, in draw order.
///
/// `enabled` is `WFTUI_MOUSE`: with the mouse off nothing is capturing
/// pointer events in the first place, so the map stays empty rather than
/// paying for entries no one can ever resolve.
pub struct HitMap {
    entries: Vec<(Rect, Hit)>,
    enabled: bool,
}

impl Default for HitMap {
    fn default() -> Self {
        HitMap::new(true)
    }
}

impl HitMap {
    pub fn new(enabled: bool) -> Self {
        HitMap {
            entries: Vec::new(),
            enabled,
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Start a new frame.
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// Register one rect. Zero-area rects are dropped — a pane that was not
    /// on screen must never answer for a point (issue #549's rule).
    pub fn push(&mut self, rect: Rect, hit: Hit) {
        if !self.enabled || rect.width == 0 || rect.height == 0 {
            return;
        }
        self.entries.push((rect, hit));
    }

    /// One full-width hit per visible row of a uniform-height list, given the
    /// `offset` the `List` widget resolved (read `ListState::offset()` back
    /// **after** rendering — that is when the widget has scrolled it).
    pub fn rows(
        &mut self,
        area: Rect,
        offset: usize,
        count: usize,
        mut hit: impl FnMut(usize) -> Hit,
    ) {
        if !self.enabled {
            return;
        }
        for (y, i) in (area.y..area.bottom()).zip(offset..count) {
            self.push(Rect::new(area.x, y, area.width, 1), hit(i));
        }
    }

    /// The variable-height sibling of [`HitMap::rows`]: `heights[i]` is item
    /// `i`'s row count (a two-line conversation row, a search hit with its
    /// snippet). Items are laid out from `offset` down, clipped at the
    /// area's bottom, exactly as the widget draws them.
    pub fn list(
        &mut self,
        area: Rect,
        offset: usize,
        heights: &[u16],
        mut hit: impl FnMut(usize) -> Hit,
    ) {
        if !self.enabled {
            return;
        }
        let mut y = area.y;
        for (i, h) in heights.iter().enumerate().skip(offset) {
            if y >= area.bottom() {
                break;
            }
            let rows = (*h).min(area.bottom() - y);
            self.push(Rect::new(area.x, y, area.width, rows), hit(i));
            y += rows;
        }
    }

    /// `hit` everywhere in `outer` that `inner` does not cover — the shape a
    /// "click outside this overlay closes it" region has.
    pub fn push_around(&mut self, outer: Rect, inner: Rect, hit: Hit) {
        if !self.enabled {
            return;
        }
        // Above.
        if inner.y > outer.y {
            self.push(
                Rect::new(outer.x, outer.y, outer.width, inner.y - outer.y),
                hit.clone(),
            );
        }
        // Below.
        if inner.bottom() < outer.bottom() {
            self.push(
                Rect::new(
                    outer.x,
                    inner.bottom(),
                    outer.width,
                    outer.bottom() - inner.bottom(),
                ),
                hit.clone(),
            );
        }
        let top = inner.y.max(outer.y);
        let bottom = inner.bottom().min(outer.bottom());
        let height = bottom.saturating_sub(top);
        // Left.
        if inner.x > outer.x {
            self.push(
                Rect::new(outer.x, top, inner.x - outer.x, height),
                hit.clone(),
            );
        }
        // Right.
        if inner.right() < outer.right() {
            self.push(
                Rect::new(inner.right(), top, outer.right() - inner.right(), height),
                hit,
            );
        }
    }

    /// The topmost hit at `(x, y)` — the last one registered, so an overlay
    /// covers the body it was drawn over.
    pub fn at(&self, x: u16, y: u16) -> Option<&Hit> {
        self.find(x, y, |_| true)
    }

    /// The topmost hit at `(x, y)` matching `pred`. Used to read the pane or
    /// the post *under* a link/image/row that was registered over it.
    pub fn find(&self, x: u16, y: u16, pred: impl Fn(&Hit) -> bool) -> Option<&Hit> {
        let at = Position::new(x, y);
        self.entries
            .iter()
            .rev()
            .find(|(rect, hit)| rect.contains(at) && pred(hit))
            .map(|(_, hit)| hit)
    }

    /// The pane the point is in, if any.
    pub fn pane_at(&self, x: u16, y: u16) -> Option<HitPane> {
        match self.find(x, y, |h| matches!(h, Hit::Pane(_))) {
            Some(Hit::Pane(p)) => Some(*p),
            _ => None,
        }
    }

    /// The post/message card the point is in, if any — including when a link
    /// or image row inside it is what `at` answers with.
    pub fn post_at(&self, x: u16, y: u16) -> Option<usize> {
        match self.find(x, y, |h| matches!(h, Hit::Post(_))) {
            Some(Hit::Post(i)) => Some(*i),
            _ => None,
        }
    }

    /// How many entries this frame registered — test-only introspection.
    #[cfg(test)]
    pub fn count(&self) -> usize {
        self.entries.len()
    }
}

/// The `KeyEvent` a key-bar cap (or a which-key cell) stands for, so a click
/// on it can be pushed through `App::handle_key` rather than re-implementing
/// what the key does.
///
/// Compound caps name alternatives (`j/k`, `n/N`, `[/]`, `Tab/Esc`) or a
/// range (`1-9`, `[ ]`); a click can only mean one key, so it means the
/// first — `j` scrolls down, `[` pages back. `None` is a cap with no key at
/// all to synthesize.
pub fn key_event_for_label(label: &str) -> Option<KeyEvent> {
    let label = label.trim();
    if label.is_empty() {
        return None;
    }
    // "j/k", "Tab/Esc", "Up/Dn", "1/2/3" -> the first alternative.
    if let Some((first, _)) = label.split_once('/')
        && !first.is_empty()
    {
        return key_event_for_label(first);
    }
    // "[ ]" (page keys), "1-9" (images) -> the first of the pair/range.
    if let Some((first, _)) = label.split_once([' ', '-'])
        && !first.is_empty()
    {
        return key_event_for_label(first);
    }
    let plain = |code: KeyCode| Some(KeyEvent::new(code, KeyModifiers::NONE));
    match label {
        "Enter" => plain(KeyCode::Enter),
        "Esc" => plain(KeyCode::Esc),
        "Tab" => plain(KeyCode::Tab),
        "Up" | "\u{2191}" | "\u{2191}\u{2193}" => plain(KeyCode::Up),
        "Dn" | "Down" | "\u{2193}" => plain(KeyCode::Down),
        "Space" => plain(KeyCode::Char(' ')),
        _ => {
            let mut chars = label.chars();
            match (chars.next(), chars.next()) {
                // `^S`, `^K`: a control chord.
                (Some('^'), Some(c)) if chars.next().is_none() => Some(KeyEvent::new(
                    KeyCode::Char(c.to_ascii_lowercase()),
                    KeyModifiers::CONTROL,
                )),
                (Some(c), None) => plain(KeyCode::Char(c)),
                _ => None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn last_registered_wins_and_zero_area_never_answers() {
        let mut hits = HitMap::default();
        hits.push(Rect::new(0, 0, 10, 5), Hit::Pane(HitPane::List));
        hits.push(Rect::new(0, 2, 10, 1), Hit::Row(7));
        // An off-screen pane (issue #549's zero-sized rect) answers nothing.
        hits.push(Rect::new(0, 0, 0, 0), Hit::Pane(HitPane::Tree));

        assert_eq!(hits.at(3, 2), Some(&Hit::Row(7)), "the row is on top");
        assert_eq!(hits.at(3, 1), Some(&Hit::Pane(HitPane::List)));
        assert_eq!(hits.pane_at(3, 2), Some(HitPane::List), "the pane is under it");
        assert_eq!(hits.at(30, 2), None, "outside every rect");
    }

    #[test]
    fn a_disabled_map_registers_nothing() {
        let mut hits = HitMap::new(false);
        hits.push(Rect::new(0, 0, 10, 5), Hit::Pane(HitPane::List));
        hits.rows(Rect::new(0, 0, 10, 5), 0, 5, Hit::Row);
        hits.push_around(Rect::new(0, 0, 10, 5), Rect::new(2, 2, 2, 2), Hit::CloseOverlay);
        assert_eq!(hits.count(), 0);
        assert_eq!(hits.at(1, 1), None);
        assert!(!hits.enabled());
    }

    #[test]
    fn rows_start_at_the_widgets_own_offset_and_stop_at_the_bottom() {
        let mut hits = HitMap::default();
        let area = Rect::new(4, 10, 20, 3);
        hits.rows(area, 5, 20, Hit::Row);
        assert_eq!(hits.at(4, 10), Some(&Hit::Row(5)), "offset row is the first");
        assert_eq!(hits.at(23, 12), Some(&Hit::Row(7)));
        assert_eq!(hits.at(4, 13), None, "past the area's bottom");
        assert_eq!(hits.at(3, 11), None, "left of the area");
    }

    #[test]
    fn variable_height_items_map_every_row_they_cover() {
        let mut hits = HitMap::default();
        let area = Rect::new(0, 0, 10, 6);
        // Two-line rows, the Inbox's conversation shape.
        hits.list(area, 0, &[2, 2, 2, 2], Hit::Row);
        assert_eq!(hits.at(0, 0), Some(&Hit::Row(0)));
        assert_eq!(hits.at(0, 1), Some(&Hit::Row(0)), "the meta line is the row");
        assert_eq!(hits.at(0, 2), Some(&Hit::Row(1)));
        assert_eq!(hits.at(0, 5), Some(&Hit::Row(2)));
        assert_eq!(hits.at(0, 6), None);
    }

    #[test]
    fn push_around_covers_the_body_but_not_the_overlay() {
        let mut hits = HitMap::default();
        let body = Rect::new(0, 1, 40, 20);
        let panel = Rect::new(10, 5, 20, 8);
        hits.push_around(body, panel, Hit::CloseOverlay);
        for (x, y) in [(0u16, 1u16), (39, 20), (0, 5), (39, 12), (20, 4), (20, 13)] {
            assert_eq!(
                hits.at(x, y),
                Some(&Hit::CloseOverlay),
                "({x},{y}) is outside the overlay"
            );
        }
        for (x, y) in [(10u16, 5u16), (29, 12), (20, 8)] {
            assert_eq!(hits.at(x, y), None, "({x},{y}) is inside the overlay");
        }
    }

    #[test]
    fn every_cap_shape_the_key_bar_draws_maps_to_one_key() {
        let ev = |label: &str| key_event_for_label(label).expect(label);
        assert_eq!(ev("Enter").code, KeyCode::Enter);
        assert_eq!(ev("Esc").code, KeyCode::Esc);
        assert_eq!(ev("Tab").code, KeyCode::Tab);
        assert_eq!(ev("r").code, KeyCode::Char('r'));
        assert_eq!(ev("N").code, KeyCode::Char('N'));
        assert_eq!(ev("/").code, KeyCode::Char('/'));
        // Compound caps mean their first alternative.
        assert_eq!(ev("j/k").code, KeyCode::Char('j'));
        assert_eq!(ev("n/N").code, KeyCode::Char('n'));
        assert_eq!(ev("[/]").code, KeyCode::Char('['));
        assert_eq!(ev("1/2/3").code, KeyCode::Char('1'));
        assert_eq!(ev("Tab/Esc").code, KeyCode::Tab);
        assert_eq!(ev("[ ]").code, KeyCode::Char('['));
        assert_eq!(ev("1-9").code, KeyCode::Char('1'));
        assert_eq!(ev("\u{2191}\u{2193}").code, KeyCode::Up);
        assert_eq!(ev("Up/Dn").code, KeyCode::Up);
        // Control chords keep their modifier.
        let ctrl_s = ev("^S");
        assert_eq!(ctrl_s.code, KeyCode::Char('s'));
        assert!(ctrl_s.modifiers.contains(KeyModifiers::CONTROL));
        assert_eq!(key_event_for_label(""), None);
    }
}
