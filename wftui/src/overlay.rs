//! The three overlays that float above every screen: the go-to palette
//! (`Ctrl+K` / `:`), the `g` which-key, and the `?` keys card.
//!
//! Everything that can be tested without a terminal lives here as a plain
//! function or a small state machine (the fuzzy matcher, the prefix machine);
//! the renderers take a `Frame` and draw into the *body* rect only, so the
//! header band, key bar and status row are never covered.
//!
//! Draw order matters (see `app::draw`): the screen renders first, then
//! `dim_body` re-styles the body cells to `theme.faint`, then the overlay is
//! `Clear`ed and drawn on top. Nothing here emits an escape sequence into span
//! content (CLAUDE.md hard rule 1).

use std::time::{Duration, Instant};

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};

use common::models::User;

use crate::chrome::{self, Hints};
use crate::hit::{Hit, HitMap};
use crate::glyph::Glyphs;
use crate::theme::Theme;

/// Palette panel width (DESIGN.md / `Overlays.dc.html`: 60 cells).
pub const PALETTE_WIDTH: u16 = 60;
/// Result rows the palette shows at once.
pub const PALETTE_ROWS: usize = 8;
/// Which-key panel size.
const WHICH_KEY_WIDTH: u16 = 52;
/// The built-in site's panel height (13 cells → 5 rows + border); other
/// sites reflow to their own cell count (`render_which_key`).
#[cfg(test)]
const WHICH_KEY_HEIGHT: u16 = 7;
/// Keys card size (`Help.dc.html`: 96 × 16).
const KEYS_CARD_WIDTH: u16 = 96;
/// DESIGN.md specifies 96 x 16; the card grew by the three rows the mouse
/// and touch layer added to its MOUSE & CLIPBOARD group (click, right-click,
/// and the `WFTUI_MOUSE=0` escape hatch beside Shift+drag). `render_keys_card`
/// clamps this to the body's height, so a small terminal is unaffected.
const KEYS_CARD_HEIGHT: u16 = 19;
/// Width of the dim kind column (`forum` / `action` / `member`).
const KIND_COL: usize = 6;
/// Cells between a hint's cap and the next hint's cap inside a keys-card column.
const HINT_STRIDE: usize = 17;

const ELLIPSIS: &str = "\u{2026}";

// ============================================================ fuzzy matching

/// A subsequence hit: its score and the matched character indices of the
/// haystack (used to bold exactly the characters the member typed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Match {
    pub score: i32,
    pub positions: Vec<usize>,
}

/// Case-insensitive subsequence match, scored so that a prefix beats a
/// word-start beats a mid-word hit.
///
/// Whitespace in the query is ignored, so `win help` finds
/// `Windows Help and Support` the same way `winhelp` does. An empty query
/// matches everything with score 0, which keeps the unfiltered palette in its
/// natural order.
pub fn fuzzy_match(query: &str, text: &str) -> Option<Match> {
    let q: Vec<char> = query
        .chars()
        .filter(|c| !c.is_whitespace())
        .map(lower)
        .collect();
    let t: Vec<char> = text.chars().collect();
    if q.is_empty() {
        return Some(Match {
            score: 0,
            positions: Vec::new(),
        });
    }
    let mut positions = Vec::with_capacity(q.len());
    let mut score = 0i32;
    let mut ti = 0usize;
    let mut prev: Option<usize> = None;
    for &qc in &q {
        let found = (ti..t.len()).find(|&i| lower(t[i]) == qc)?;
        score += 1;
        if found == 0 {
            // Prefix: the strongest signal a member can give.
            score += 8;
        } else if !t[found - 1].is_alphanumeric() {
            // Word start: `hs` should find "Windows Help and Support".
            score += 6;
        }
        if prev == Some(found.saturating_sub(1)) && found > 0 {
            score += 4;
        }
        positions.push(found);
        prev = Some(found);
        ti = found + 1;
    }
    // Earlier and shorter beats later and longer when the bonuses tie.
    let first = positions.first().copied().unwrap_or(0) as i32;
    score -= first.min(16);
    score -= (t.len() / 8) as i32;
    Some(Match { score, positions })
}

fn lower(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

// ================================================================== palette

/// The three row kinds, in the order the dim label column shows them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemKind {
    Forum,
    Action,
    Member,
}

impl ItemKind {
    fn label(self) -> &'static str {
        match self {
            ItemKind::Forum => "forum",
            ItemKind::Action => "action",
            ItemKind::Member => "member",
        }
    }
}

/// What running a palette row does. The app maps these onto the openers it
/// already has (`open_list`, `open_inbox`, `execute_action`, …) — the palette
/// never performs navigation itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// A forum from the cached node tree.
    Forum(u32, String),
    /// One of the quick nodes (news / security / tutorials), resolved through
    /// the tree so categories and link-forums still behave.
    QuickNode(u32, String),
    Latest,
    NewThread(u32),
    MarkForumRead(u32),
    Inbox,
    Alerts,
    /// The XFMG media catalog and the XFRM resource catalog (#680) — the
    /// palette twins of the `g m` / `g r` chords.
    MediaGallery,
    Resources,
    /// Unsent composer drafts (#716) — the palette twin of `g d`.
    Drafts,
    /// Ask the AI — the palette twin of `g k`.
    AskAi,
    Search,
    /// Check for a newer release, or act on one already found (#722) — the
    /// palette twin of `g u`.
    Update,
    SignOut,
    Quit,
    Member(u32, String),
}

#[derive(Debug, Clone)]
pub struct Item {
    pub kind: ItemKind,
    pub title: String,
    /// The key that does the same thing outside the palette, right-aligned on
    /// the row so the palette teaches the keyboard instead of replacing it.
    pub key: Option<&'static str>,
    pub target: Target,
}

impl Item {
    pub fn forum(node_id: u32, title: String) -> Self {
        Item {
            kind: ItemKind::Forum,
            title,
            key: None,
            target: Target::Forum(node_id, String::new()),
        }
        .with_forum_title(node_id)
    }

    fn with_forum_title(mut self, node_id: u32) -> Self {
        self.target = Target::Forum(node_id, self.title.clone());
        self
    }

    pub fn action(title: String, key: &'static str, target: Target) -> Self {
        Item {
            kind: ItemKind::Action,
            title,
            key: Some(key),
            target,
        }
    }

    pub fn member(user: &User) -> Self {
        Item {
            kind: ItemKind::Member,
            title: format!("@{}", user.username),
            key: None,
            target: Target::Member(user.user_id, user.username.clone()),
        }
    }
}

/// What a keystroke did to the palette.
pub enum PaletteEvent {
    None,
    Close,
    Run(Target),
}

pub struct Palette {
    pub query: String,
    /// Cursor position, in characters into `query`.
    pub cursor: usize,
    pub sel: usize,
    scroll: usize,
    items: Vec<Item>,
    /// `(index into items, matched char positions)`, best first.
    filtered: Vec<(usize, Vec<usize>)>,
    /// Member lookups already issued while this palette has been open, so a
    /// held key never fans out into a burst of requests.
    asked: Vec<String>,
    edited_at: Instant,
}

impl Palette {
    pub fn new(items: Vec<Item>) -> Self {
        let mut p = Palette {
            query: String::new(),
            cursor: 0,
            sel: 0,
            scroll: 0,
            items,
            filtered: Vec::new(),
            asked: Vec::new(),
            edited_at: Instant::now(),
        };
        p.refilter();
        p
    }

    fn refilter(&mut self) {
        let mut scored: Vec<(i32, usize, Vec<usize>)> = self
            .items
            .iter()
            .enumerate()
            .filter_map(|(i, item)| {
                fuzzy_match(&self.query, &item.title).map(|m| (m.score, i, m.positions))
            })
            .collect();
        // Stable: equal scores keep the insertion order (actions, then forums,
        // then any resolved member), which is what an empty query shows.
        scored.sort_by_key(|a| std::cmp::Reverse(a.0));
        self.filtered = scored.into_iter().map(|(_, i, p)| (i, p)).collect();
        if self.sel >= self.filtered.len() {
            self.sel = self.filtered.len().saturating_sub(1);
        }
        self.clamp_scroll();
    }

    fn clamp_scroll(&mut self) {
        if self.sel < self.scroll {
            self.scroll = self.sel;
        } else if self.sel >= self.scroll + PALETTE_ROWS {
            self.scroll = self.sel + 1 - PALETTE_ROWS;
        }
        if self.filtered.len() <= PALETTE_ROWS {
            self.scroll = 0;
        }
    }

    /// Put the selection on filtered row `i` — what a click on a palette row
    /// does before running it. Out of range is ignored: the row a click names
    /// is always one the last frame drew.
    pub fn select(&mut self, i: usize) {
        if i < self.filtered.len() {
            self.sel = i;
            self.clamp_scroll();
        }
    }

    /// Every row's title, unfiltered — what a test asserts the palette offers.
    #[cfg(test)]
    pub fn titles(&self) -> Vec<String> {
        self.items.iter().map(|i| i.title.clone()).collect()
    }

    pub fn selected(&self) -> Option<&Item> {
        self.filtered
            .get(self.sel)
            .and_then(|(i, _)| self.items.get(*i))
    }

    /// A member lookup that is due: at least three characters, typing has
    /// paused, and this exact query has not been asked before. Marks it asked.
    pub fn member_query_due(&mut self) -> Option<String> {
        let q = self.query.trim().to_string();
        if q.chars().count() < 3 || self.edited_at.elapsed() < Duration::from_millis(350) {
            return None;
        }
        if self.asked.iter().any(|a| a == &q) {
            return None;
        }
        self.asked.push(q.clone());
        Some(q)
    }

    /// Add a resolved member row (at most one per user id).
    pub fn push_member(&mut self, user: &User) {
        if self
            .items
            .iter()
            .any(|i| i.target == Target::Member(user.user_id, user.username.clone()))
        {
            return;
        }
        self.items.push(Item::member(user));
        self.refilter();
    }

    pub fn key(&mut self, k: KeyEvent) -> PaletteEvent {
        match k.code {
            KeyCode::Esc => return PaletteEvent::Close,
            KeyCode::Enter => {
                return match self.selected() {
                    Some(item) => PaletteEvent::Run(item.target.clone()),
                    None => PaletteEvent::Close,
                };
            }
            KeyCode::Up => self.move_sel(-1),
            KeyCode::Down => self.move_sel(1),
            KeyCode::PageUp => self.move_sel(-(PALETTE_ROWS as i32)),
            KeyCode::PageDown => self.move_sel(PALETTE_ROWS as i32),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.query.chars().count(),
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(self.query.chars().count()),
            KeyCode::Backspace => {
                if self.cursor > 0 {
                    crate::editor::delete_back(&mut self.query, &mut self.cursor);
                    self.edited();
                }
            }
            KeyCode::Delete => {
                crate::editor::delete_forward(&mut self.query, &mut self.cursor);
                self.edited();
            }
            KeyCode::Char('n') if k.modifiers.contains(KeyModifiers::CONTROL) => self.move_sel(1),
            KeyCode::Char('p') if k.modifiers.contains(KeyModifiers::CONTROL) => self.move_sel(-1),
            KeyCode::Char(c) if !k.modifiers.contains(KeyModifiers::CONTROL) => {
                crate::editor::insert_char(&mut self.query, &mut self.cursor, c);
                self.edited();
            }
            _ => {}
        }
        PaletteEvent::None
    }

    /// Re-rank after the app has pasted straight into `query`.
    pub fn after_paste(&mut self) {
        self.edited();
    }

    fn edited(&mut self) {
        self.edited_at = Instant::now();
        self.sel = 0;
        self.scroll = 0;
        self.refilter();
    }

    fn move_sel(&mut self, delta: i32) {
        if self.filtered.is_empty() {
            return;
        }
        let last = self.filtered.len() as i32 - 1;
        let next = (self.sel as i32 + delta).clamp(0, last);
        self.sel = next as usize;
        self.clamp_scroll();
    }

    /// The key bar shown while the palette owns the keyboard.
    pub fn hints(g: &Glyphs) -> Hints {
        Hints::new(
            &[
                ("Enter", "go"),
                (if g.ascii { "Up/Dn" } else { "\u{2191}\u{2193}" }, "choose"),
                ("Esc", "close"),
            ],
            0,
        )
    }

    pub fn status() -> &'static str {
        "Go to: forums, actions and members in one list \u{b7} Ctrl+K or : opens it anywhere"
    }

    /// Draw the palette over `body`: 60 wide, centered, two rows down — the
    /// geometry of `Overlays.dc.html`.
    pub fn render(&self, f: &mut Frame, body: Rect, theme: &Theme, g: &Glyphs, hits: &mut HitMap) {
        let width = PALETTE_WIDTH.min(body.width);
        let height = (PALETTE_ROWS as u16 + 4).min(body.height);
        let x = body.x + body.width.saturating_sub(width) / 2;
        let y = body.y + 2.min(body.height.saturating_sub(height));
        let area = Rect::new(x, y, width, height);
        // The palette owns the keyboard while it is up, so it owns the
        // pointer too: everything outside it closes it, and nothing under it
        // can be reached (`push_around` + last-registered-wins).
        hits.push_around(body, area, Hit::CloseOverlay);
        f.render_widget(Clear, area);
        let block = chrome::panel(
            theme,
            g,
            "Go to",
            true,
            None,
            Some(&format!(
                "{} choose \u{b7} Enter go \u{b7} Esc close",
                if g.ascii { "Up/Dn" } else { "\u{2191}\u{2193}" }
            )),
        );
        let inner = block.inner(area);
        f.render_widget(block, area);
        if inner.width == 0 || inner.height == 0 {
            return;
        }
        let w = inner.width as usize;

        // Row 0: the query line, with a block cursor (the terminal cursor is
        // never moved here — a second caret on the screen reads as a bug).
        let before: String = self.query.chars().take(self.cursor).collect();
        let at: String = self.query.chars().skip(self.cursor).take(1).collect();
        let after: String = self.query.chars().skip(self.cursor + 1).collect();
        let hit = hit_style(theme);
        let mut q = vec![
            Span::raw(" "),
            Span::styled(
                "\u{203A} ",
                Style::new().fg(theme.accent).add_modifier(Modifier::BOLD),
            ),
            Span::styled(before, hit),
        ];
        q.push(Span::styled(
            if at.is_empty() { " ".to_string() } else { at },
            Style::new().add_modifier(Modifier::REVERSED),
        ));
        q.push(Span::styled(after, hit));
        f.render_widget(
            Paragraph::new(Line::from(clip(q, w))),
            Rect::new(inner.x, inner.y, inner.width, 1),
        );

        // Row 1: the rule under the query.
        if inner.height > 1 {
            f.render_widget(
                Paragraph::new(Line::from(Span::styled("\u{2500}".repeat(w), theme.faint()))),
                Rect::new(inner.x, inner.y + 1, inner.width, 1),
            );
        }

        // Rows 2..: the matches.
        let rows = (inner.height as usize).saturating_sub(2);
        let mut lines: Vec<Line<'static>> = Vec::with_capacity(rows);
        for slot in 0..rows {
            let Some((idx, positions)) = self.filtered.get(self.scroll + slot) else {
                break;
            };
            let Some(item) = self.items.get(*idx) else { break };
            let selected = self.scroll + slot == self.sel;
            // The index is into `filtered`, which is what `sel` indexes too,
            // so a click can just move the selection and run it.
            hits.push(
                Rect::new(inner.x, inner.y + 2 + slot as u16, inner.width, 1),
                Hit::PaletteRow(self.scroll + slot),
            );
            lines.push(self.row(theme, item, positions, selected, w));
        }
        if lines.is_empty() {
            lines.push(Line::from(Span::styled(
                "  no match".to_string(),
                theme.dim(),
            )));
        }
        f.render_widget(
            Paragraph::new(lines),
            Rect::new(inner.x, inner.y + 2, inner.width, rows as u16),
        );
    }

    fn row(
        &self,
        theme: &Theme,
        item: &Item,
        positions: &[usize],
        selected: bool,
        w: usize,
    ) -> Line<'static> {
        let key_w = item.key.map(|k| chrome::cell_width(k) + 4).unwrap_or(0);
        let title_w = w.saturating_sub(1 + KIND_COL + 3 + key_w);
        let mut spans = vec![
            Span::raw(" "),
            Span::styled(format!("{:<KIND_COL$}", item.kind.label()), theme.dim()),
            Span::raw("   "),
        ];
        spans.extend(highlight(theme, &item.title, positions, title_w));
        let used: usize = spans.iter().map(Span::width).sum();
        if let Some(key) = item.key {
            let pad = w.saturating_sub(used + chrome::cell_width(key) + 4);
            spans.push(Span::raw(" ".repeat(pad)));
            spans.push(Span::styled(key.to_string(), theme.dim()));
            spans.push(Span::raw("    "));
        }
        let used: usize = spans.iter().map(Span::width).sum();
        if used < w {
            spans.push(Span::raw(" ".repeat(w - used)));
        }
        let mut spans = clip(spans, w);
        if selected {
            // The band covers the whole row, so it reads as one object.
            let band = theme.selected();
            for s in &mut spans {
                s.style = s.style.patch(band);
            }
        }
        Line::from(spans)
    }
}

/// The style the matched characters take: bold, and as close to white as the
/// tier allows (Reset in mono, where bold is the whole signal).
fn hit_style(theme: &Theme) -> Style {
    Style::new()
        .fg(theme.keycap_fg)
        .add_modifier(Modifier::BOLD)
}

/// Title spans with the matched characters bold, truncated to `max` cells —
/// cells, not chars, so a CJK/emoji title is not billed at half its real
/// width (issue #516). `positions` are char indices into `title`, matching
/// `chars` below.
fn highlight(theme: &Theme, title: &str, positions: &[usize], max: usize) -> Vec<Span<'static>> {
    let hit = hit_style(theme);
    let base = theme.base();
    let chars: Vec<char> = title.chars().collect();
    let truncated = chrome::cell_width(title) > max && max > 0;
    let budget = if truncated { max - 1 } else { max };
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut buf = String::new();
    let mut buf_hit = false;
    let mut used = 0usize;
    for (i, c) in chars.iter().enumerate() {
        let mut char_buf = [0u8; 4];
        let cw = chrome::cell_width(c.encode_utf8(&mut char_buf));
        if used + cw > budget {
            break;
        }
        used += cw;
        let is_hit = positions.contains(&i);
        if is_hit != buf_hit && !buf.is_empty() {
            spans.push(Span::styled(
                std::mem::take(&mut buf),
                if buf_hit { hit } else { base },
            ));
        }
        buf_hit = is_hit;
        buf.push(*c);
    }
    if !buf.is_empty() {
        spans.push(Span::styled(buf, if buf_hit { hit } else { base }));
    }
    if truncated {
        spans.push(Span::styled(ELLIPSIS.to_string(), base));
    }
    spans
}

// ============================================================ `g` which-key

/// Where a `g <key>` chord goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GoTarget {
    /// The site's n-th quick destination (`site.quick[n]`): the `g <letter>`
    /// chords a forum declares for itself.
    Quick(usize),
    Latest,
    Inbox,
    Alerts,
    Media,
    Resources,
    /// Unsent composer drafts (#716).
    Drafts,
    /// Ask the AI ("as*k*"), where the site has it.
    AskAi,
    Home,
    Profile,
    Top,
    /// Check for a newer release now (#722).
    Update,
}

/// The `g` prefix state machine. `arm()` shows the which-key panel; the next
/// key either resolves to a `GoTarget` or cancels — silently, so a mistyped
/// chord costs nothing.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Prefix {
    armed: bool,
}

/// What the next key after `g` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrefixEvent {
    /// The prefix was not armed; the key belongs to whoever is next in line.
    NotArmed,
    /// Armed and cancelled (Esc, or a key that means nothing here).
    Cancelled,
    Go(GoTarget),
}

impl Prefix {
    pub fn armed(&self) -> bool {
        self.armed
    }

    pub fn arm(&mut self) {
        self.armed = true;
    }

    /// Feed the key that follows `g`. Always disarms. `quick` is the site's
    /// own chord list; its letters are validated at startup never to collide
    /// with the fixed ones below.
    pub fn resolve(&mut self, k: KeyEvent, quick: &[common::site::QuickNode]) -> PrefixEvent {
        if !self.armed {
            return PrefixEvent::NotArmed;
        }
        self.armed = false;
        if !k.modifiers.difference(KeyModifiers::SHIFT).is_empty() {
            return PrefixEvent::Cancelled;
        }
        if let KeyCode::Char(c) = k.code
            && let Some(i) = quick.iter().position(|q| q.key == c)
        {
            return PrefixEvent::Go(GoTarget::Quick(i));
        }
        match k.code {
            KeyCode::Char('l') => PrefixEvent::Go(GoTarget::Latest),
            KeyCode::Char('i') => PrefixEvent::Go(GoTarget::Inbox),
            KeyCode::Char('a') => PrefixEvent::Go(GoTarget::Alerts),
            KeyCode::Char('m') => PrefixEvent::Go(GoTarget::Media),
            KeyCode::Char('r') => PrefixEvent::Go(GoTarget::Resources),
            KeyCode::Char('d') => PrefixEvent::Go(GoTarget::Drafts),
            KeyCode::Char('k') => PrefixEvent::Go(GoTarget::AskAi),
            KeyCode::Char('h') => PrefixEvent::Go(GoTarget::Home),
            KeyCode::Char('p') => PrefixEvent::Go(GoTarget::Profile),
            KeyCode::Char('g') => PrefixEvent::Go(GoTarget::Top),
            KeyCode::Char('u') => PrefixEvent::Go(GoTarget::Update),
            _ => PrefixEvent::Cancelled,
        }
    }
}

/// One lowercase letter as a `&'static str`, for the key caps and `Hit::Key`
/// (both are `'static`): a site's chord letters are validated to `a-z`, so
/// every one of them is in this table and nothing is ever leaked.
pub fn static_letter(c: char) -> &'static str {
    const LETTERS: [&str; 26] = [
        "a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k", "l", "m", "n", "o", "p", "q", "r",
        "s", "t", "u", "v", "w", "x", "y", "z",
    ];
    match c {
        'a'..='z' => LETTERS[(c as u8 - b'a') as usize],
        _ => "?",
    }
}

/// The cells of the which-key panel, in the order the design shows them:
/// the site's own quick chords first, then the fixed destinations, with
/// `media` and `resources` only when the site has those add-ons. Laid out
/// three per row by [`render_which_key`].
pub fn which_key_cells(site: &common::site::SiteConfig) -> Vec<(&'static str, String)> {
    let mut cells: Vec<(&'static str, String)> = site
        .quick
        .iter()
        .map(|q| (static_letter(q.key), which_key_label(&q.label, &site.prefix_strip)))
        .collect();
    cells.push(("l", "latest".into()));
    cells.push(("i", "inbox".into()));
    cells.push(("a", "alerts".into()));
    if site.features.xfmg {
        cells.push(("m", "media".into()));
    }
    if site.features.xfrm {
        cells.push(("r", "resources".into()));
    }
    if site.features.ask_ai {
        cells.push(("k", "ask ai".into()));
    }
    for (k, label) in [("d", "drafts"), ("h", "home"), ("p", "profile"), ("g", "top"), ("u", "update")] {
        cells.push((k, label.into()));
    }
    cells
}

/// A quick destination's label as the panel shows it: lowercase, without
/// the site's stripped words ("Windows News" → "news"), and cut to its first
/// word when it still would not fit beside its cap in a 15-cell column
/// ("Security Alerts" → "security"). The palette row keeps the full label.
fn which_key_label(label: &str, strip: &[String]) -> String {
    let mut l = label.trim().to_string();
    for word in strip {
        if let Some(rest) = l.strip_prefix(word.as_str())
            && !rest.trim().is_empty()
        {
            l = rest.trim().to_string();
        }
    }
    let l = l.to_lowercase();
    // 15 cells: a 3-cell cap, a space, and the label.
    if chrome::cell_width(&l) > 11 {
        l.split_whitespace().next().unwrap_or(&l).to_string()
    } else {
        l
    }
}

/// Rows of the panel for `cells`: three per row, the last padded.
fn which_key_rows(cells: &[(&'static str, String)]) -> Vec<Vec<(&'static str, String)>> {
    cells.chunks(3).map(|c| c.to_vec()).collect()
}

/// Draw the `g …` panel bottom-right of the body, one row above the key bar.
pub fn render_which_key(
    f: &mut Frame,
    body: Rect,
    theme: &Theme,
    g: &Glyphs,
    cells: &[(&'static str, String)],
    hits: &mut HitMap,
) {
    let rows = which_key_rows(cells);
    let width = WHICH_KEY_WIDTH.min(body.width);
    // Two border rows plus one per row of cells; for the built-in site that
    // is `WHICH_KEY_HEIGHT`, the DESIGN.md panel.
    let height = (rows.len() as u16 + 2).min(body.height);
    let x = body
        .x
        .max(body.x + body.width.saturating_sub(width + 2));
    let y = body.y + body.height.saturating_sub(height + 1);
    let area = Rect::new(x, y, width, height);
    hits.push_around(body, area, Hit::CloseOverlay);
    f.render_widget(Clear, area);
    let block = chrome::panel(
        theme,
        g,
        "g \u{2026}",
        true,
        None,
        Some("press a key \u{b7} Esc cancel"),
    );
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let w = inner.width as usize;
    let stride = 15usize;
    let lines: Vec<Line<'static>> = rows
        .iter()
        .enumerate()
        .map(|(r, row)| {
            let mut spans = vec![Span::raw(" ")];
            for (key, label) in row {
                let start: usize = spans.iter().map(Span::width).sum();
                // The whole `cap label` cell is the target, not the two-cell
                // cap: the chord's key is pressed through `handle_key`, so an
                // armed `g` resolves exactly as it does from the keyboard.
                hits.push(
                    Rect::new(inner.x + start as u16, inner.y + r as u16, stride as u16, 1),
                    Hit::Key(key),
                );
                let key: &str = key;
                spans.push(chrome::keycap(theme, key, false));
                spans.push(Span::styled(format!(" {label}"), theme.dim()));
                let used: usize = spans.iter().map(Span::width).sum();
                let want = start + stride;
                if used < want {
                    spans.push(Span::raw(" ".repeat(want - used)));
                }
            }
            Line::from(clip(spans, w))
        })
        .collect();
    f.render_widget(Paragraph::new(lines), inner);
}

// ============================================================== keys card

/// The key bar shown while the keys card is up.
pub fn keys_card_hints() -> Hints {
    Hints::new(&[("?", "close")], 0)
}

pub fn keys_card_status() -> &'static str {
    "Tip: you never need this card \u{2014} the bar above always shows what works right now"
}

/// Hints that the MOVE and EVERYWHERE columns already teach; a screen's own
/// group never repeats them.
const CARD_GLOBAL_KEYS: [&str; 10] = [
    "j/k", "n/N", "[ ]", "[/]", "Tab", "Esc", "q", "?", "/", "g",
];

/// Draw the `?` card: MOVE / <this screen's group> / EVERYWHERE / MOUSE &
/// CLIPBOARD, two columns, 96 × 16 (`Help.dc.html`).
pub fn render_keys_card(
    f: &mut Frame,
    body: Rect,
    theme: &Theme,
    g: &Glyphs,
    group: &str,
    hints: &Hints,
    hits: &mut HitMap,
) {
    let width = KEYS_CARD_WIDTH.min(body.width);
    let height = KEYS_CARD_HEIGHT.min(body.height);
    let x = body.x + body.width.saturating_sub(width) / 2;
    // A third of the way down, not centered: the card then sits over the top
    // of the body where the reader's eye already is.
    let y = body.y + body.height.saturating_sub(height) / 3;
    let area = Rect::new(x, y, width, height);
    // Any key closes the card; so does a click anywhere off it. A click
    // *on* it is inert — the card is a reference, not a menu.
    hits.push_around(body, area, Hit::CloseOverlay);
    f.render_widget(Clear, area);
    let block = chrome::panel(
        theme,
        g,
        "Keys",
        true,
        None,
        Some("any key closes \u{b7} keys stay visible in the bar below"),
    );
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    // Below ~90 columns each card column is under 40 cells wide, and the long
    // mouse descriptions would be cut mid-word. Shorter copy beats a clip.
    let compact = inner.width < 90;
    let pick = |long: &'static str, short: &'static str| if compact { short } else { long };
    let arrows = if g.ascii { "Up/Dn" } else { "\u{2191}\u{2193}" };
    let head = |t: &str| Span::styled(t.to_string(), theme.dim().add_modifier(Modifier::BOLD));
    let cap = |k: &str| chrome::keycap(theme, k, false);
    let txt = |t: String| Span::styled(t, theme.dim());

    let mut left: Vec<Vec<Span<'static>>> = vec![
        vec![Span::raw(" "), head("MOVE")],
        vec![
            Span::raw(" "),
            cap("j/k"),
            txt(" or ".into()),
            cap(arrows),
            txt(" line".into()),
        ],
        vec![
            Span::raw(" "),
            cap("n/N"),
            txt(" next / previous post".into()),
        ],
        vec![Span::raw(" "), cap("[ ]"), txt(" page".into())],
        vec![
            Span::raw(" "),
            cap("g g"),
            txt(" top".into()),
            Span::raw("      "),
            cap("G"),
            txt(" bottom".into()),
        ],
        vec![Span::raw(" "), cap("Tab"), txt(" switch pane".into())],
        Vec::new(),
        vec![Span::raw(" "), head(group)],
    ];
    left.extend(group_rows(theme, hints));

    let right: Vec<Vec<Span<'static>>> = vec![
        vec![Span::raw(" "), head("EVERYWHERE")],
        vec![
            Span::raw(" "),
            cap("g"),
            txt(pick(" then a letter: go to\u{2026}", " go to\u{2026}").into()),
        ],
        vec![
            Span::raw(" "),
            cap("^K"),
            txt(" or ".into()),
            cap(":"),
            txt(" go-to palette".into()),
        ],
        pair(theme, ("/", "search"), Some(("i", "inbox"))),
        pair(theme, ("a", "alerts"), Some(("N", "new thread"))),
        pair(theme, ("Esc", "back"), Some(("q", "quit"))),
        pair(theme, ("^L", "sign out"), Some(("?", "this card"))),
        Vec::new(),
        vec![Span::raw(" "), head("MOUSE & CLIPBOARD")],
        mouse_row(
            theme,
            "click",
            pick("select \u{b7} again or double opens", "select \u{b7} again opens"),
        ),
        mouse_row(theme, "right-click", pick("open in your browser", "open in web")),
        mouse_row(theme, "drag", pick("select + copy on release", "select + copy")),
        mouse_row(theme, "wheel", "scroll"),
        vec![
            Span::raw(" "),
            cap("^C"),
            txt(" copy selection".into()),
        ],
        vec![
            Span::raw(" "),
            cap("^Y"),
            txt(pick(" paste (OSC 52, works in tmux)", " paste (OSC 52)").into()),
        ],
        mouse_row(
            theme,
            "Shift+drag",
            pick("native terminal selection", "native select"),
        ),
        // The permanent form of Shift+drag: no capture at all, so every
        // gesture (and the scrollback) belongs to the terminal.
        mouse_row(
            theme,
            "WFTUI_MOUSE=0",
            pick("start with no mouse capture", "no mouse capture"),
        ),
    ];

    let half = (inner.width / 2) as usize;
    let full = inner.width as usize;
    let rows = inner.height as usize;
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(rows);
    for i in 0..rows {
        let mut spans = clip(left.get(i).cloned().unwrap_or_default(), half);
        let used: usize = spans.iter().map(Span::width).sum();
        if used < half {
            spans.push(Span::raw(" ".repeat(half - used)));
        }
        spans.extend(clip(
            right.get(i).cloned().unwrap_or_default(),
            full - half,
        ));
        lines.push(Line::from(spans));
    }
    f.render_widget(Paragraph::new(lines), inner);
}

/// `drag         select + copy on release` — a mouse gesture has no cap.
fn mouse_row(theme: &Theme, gesture: &str, what: &str) -> Vec<Span<'static>> {
    let label = format!(" {gesture}");
    let pad = HINT_STRIDE.saturating_sub(label.chars().count() + 1);
    vec![
        Span::raw(" "),
        Span::styled(label, theme.base()),
        Span::raw(" ".repeat(pad.max(1))),
        Span::styled(what.to_string(), theme.dim()),
    ]
}

/// One or two `cap desc` hints on a row, on the card's 17-cell stride.
fn pair(theme: &Theme, a: (&str, &str), b: Option<(&str, &str)>) -> Vec<Span<'static>> {
    let mut spans = vec![
        Span::raw(" "),
        chrome::keycap(theme, a.0, false),
        Span::styled(format!(" {}", a.1), theme.dim()),
    ];
    if let Some((k, d)) = b {
        let used: usize = spans.iter().map(Span::width).sum();
        let want = 1 + HINT_STRIDE;
        if used < want {
            spans.push(Span::raw(" ".repeat(want - used)));
        } else {
            spans.push(Span::raw(" "));
        }
        spans.push(chrome::keycap(theme, k, false));
        spans.push(Span::styled(format!(" {d}"), theme.dim()));
    }
    spans
}

/// The screen's own group, built from its `hints()`: everything the MOVE and
/// EVERYWHERE columns do not already teach, two hints to a row, with the
/// screen's primary action keeping its accent cap.
fn group_rows(theme: &Theme, hints: &Hints) -> Vec<Vec<Span<'static>>> {
    let own: Vec<(usize, chrome::Hint)> = hints
        .keys
        .iter()
        .enumerate()
        .filter(|(_, (k, _))| !CARD_GLOBAL_KEYS.contains(k))
        .map(|(i, h)| (i, *h))
        .collect();
    let mut rows = Vec::new();
    for chunk in own.chunks(2) {
        let mut spans = vec![Span::raw(" ")];
        for (n, (idx, (key, desc))) in chunk.iter().enumerate() {
            if n > 0 {
                let used: usize = spans.iter().map(Span::width).sum();
                let want = 1 + HINT_STRIDE;
                spans.push(Span::raw(" ".repeat(want.saturating_sub(used).max(1))));
            }
            spans.push(chrome::keycap(theme, key, *idx == hints.primary));
            spans.push(Span::styled(format!(" {desc}"), theme.dim()));
        }
        rows.push(spans);
    }
    rows
}

// ================================================================== helpers

/// Re-style every cell of `area` to `theme.faint`, clearing bold/reverse and
/// any background: the body reads as "behind glass" while an overlay is up.
///
/// It re-styles EVERY cell, which is why `App::draw` suppresses inline
/// images entirely while an overlay is up: kitty encodes an image's id in the
/// cell foreground colour, so re-styling an anchor cell here would repaint a
/// different image (or none), and the overlay's own `Clear` erases the rect
/// for that frame anyway. Closing the overlay makes those cells differ again,
/// so the next frame re-emits the image with no extra bookkeeping.
pub fn dim_body(f: &mut Frame, area: Rect, theme: &Theme) {
    let buf = f.buffer_mut();
    let style = Style::reset().fg(theme.faint).bg(Color::Reset);
    for y in area.y..area.bottom() {
        for x in area.x..area.right() {
            buf[(x, y)].set_style(style);
        }
    }
}

/// Hard-clip a span run to `max` cells, keeping each span's style — cells,
/// not chars, so a wide (CJK/emoji) character never straddles the boundary
/// and gets counted narrower than it renders (issue #516).
fn clip(spans: Vec<Span<'static>>, max: usize) -> Vec<Span<'static>> {
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
            let text = chrome::take_cells(&s.content, room);
            out.push(Span::styled(text, s.style));
        }
        break;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::glyph::{ASCII, UNICODE};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn score(q: &str, t: &str) -> i32 {
        fuzzy_match(q, t).expect("expected a match").score
    }

    #[test]
    fn fuzzy_matches_subsequences_case_insensitively() {
        let m = fuzzy_match("win", "Windows News").expect("match");
        assert_eq!(m.positions, vec![0, 1, 2]);
        assert_eq!(
            fuzzy_match("WIN", "Windows News"),
            fuzzy_match("win", "windows news")
        );
        assert!(fuzzy_match("zq", "Windows News").is_none());
        // Order matters: a subsequence, not a bag of characters.
        assert!(fuzzy_match("niw", "Windows").is_none());
    }

    #[test]
    fn fuzzy_ignores_whitespace_in_the_query() {
        let m = fuzzy_match("win help", "Windows Help and Support").expect("match");
        assert_eq!(m.positions, vec![0, 1, 2, 8, 9, 10, 11]);
    }

    #[test]
    fn fuzzy_prefers_prefix_then_word_start_then_mid_word() {
        // Prefix beats a hit in the middle of a word.
        assert!(score("win", "Windows News") > score("win", "Show window"));
        // A word start beats a mid-word hit.
        assert!(score("hs", "Windows Help and Support") > score("hs", "Chess"));
        // Consecutive characters beat scattered ones at the same offset.
        assert!(score("new", "New thread") > score("new", "Nice edge widget"));
    }

    #[test]
    fn fuzzy_empty_query_matches_everything_without_reordering() {
        let a = fuzzy_match("", "Windows News").expect("match");
        let b = fuzzy_match("   ", "Security Alerts").expect("match");
        assert_eq!(a.score, 0);
        assert_eq!(b.score, 0);
        assert!(a.positions.is_empty());
    }

    fn palette() -> Palette {
        Palette::new(vec![
            Item::action("Latest posts".into(), "L", Target::Latest),
            Item::action("Mark forum read".into(), "m", Target::MarkForumRead(4)),
            Item::action("Quit".into(), "q", Target::Quit),
            Item::forum(4, "Windows News".to_string()),
            Item::forum(24, "Windows Help and Support".to_string()),
            Item::forum(84, "Security Alerts".to_string()),
        ])
    }

    fn press(p: &mut Palette, c: char) -> PaletteEvent {
        p.key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))
    }

    /// A CJK forum title (plausible: a foreign-language sub-forum) must
    /// still be billed by cell width, not char count, so the row lands
    /// exactly on `w` and the right-aligned key hint is never pushed off
    /// (issue #516).
    #[test]
    fn palette_row_fits_exactly_w_with_a_cjk_title() {
        let theme = Theme::truecolor();
        let p = palette();
        let item = Item::forum(4, "视频编辑软件推荐帮助教程升级指南论坛".to_string());
        for w in [20usize, 30, 40, 60, 80] {
            let line = p.row(&theme, &item, &[], false, w);
            assert_eq!(line.width(), w, "w={w}: {:?}", line.spans);
        }
    }

    #[test]
    fn palette_filters_ranks_and_runs() {
        let mut p = palette();
        // Nothing typed: every item, in insertion order.
        assert_eq!(p.filtered.len(), 6);
        assert_eq!(p.selected().expect("sel").title, "Latest posts");

        for c in "win".chars() {
            press(&mut p, c);
        }
        let titles: Vec<String> = p
            .filtered
            .iter()
            .map(|(i, _)| p.items[*i].title.clone())
            .collect();
        assert!(
            titles[0].starts_with("Windows"),
            "a prefix hit must rank first: {titles:?}"
        );
        assert!(!titles.iter().any(|t| t == "Quit"), "{titles:?}");

        match p.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)) {
            PaletteEvent::Run(Target::Forum(id, _)) => assert!(id == 4 || id == 24),
            _ => panic!("Enter must run the selected row"),
        }
        assert!(matches!(
            p.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            PaletteEvent::Close
        ));
    }

    #[test]
    fn palette_selection_moves_and_clamps() {
        let mut p = palette();
        p.key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(p.sel, 0, "up at the top stays at the top");
        for _ in 0..20 {
            p.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        }
        assert_eq!(p.sel, 5, "down past the end stays on the last row");
        // Editing the query resets the cursor to the best match.
        press(&mut p, 'q');
        assert_eq!(p.sel, 0);
    }

    #[test]
    fn palette_member_lookup_is_debounced_and_asked_once() {
        let mut p = palette();
        for c in "kem".chars() {
            press(&mut p, c);
        }
        // Typing just happened: nothing is due yet.
        assert_eq!(p.member_query_due(), None);
        p.edited_at = Instant::now() - Duration::from_millis(600);
        assert_eq!(p.member_query_due().as_deref(), Some("kem"));
        // Asked once, never again for the same string.
        assert_eq!(p.member_query_due(), None);
        // Too short to be worth a request.
        let mut q = palette();
        press(&mut q, 'k');
        q.edited_at = Instant::now() - Duration::from_millis(600);
        assert_eq!(q.member_query_due(), None);
    }

    #[test]
    fn palette_member_rows_are_added_once() {
        let mut p = palette();
        let user = User {
            user_id: 7,
            username: "kemical".into(),
            ..Default::default()
        };
        p.push_member(&user);
        p.push_member(&user);
        assert_eq!(
            p.items
                .iter()
                .filter(|i| i.kind == ItemKind::Member)
                .count(),
            1
        );
        assert_eq!(
            p.items.last().expect("member").title.as_str(),
            "@kemical"
        );
    }

    #[test]
    fn prefix_machine_arms_resolves_and_cancels() {
        let quick = common::site::SiteConfig::windowsforum().quick;
        let mut p = Prefix::default();
        assert!(!p.armed());
        assert_eq!(
            p.resolve(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE), &quick),
            PrefixEvent::NotArmed
        );

        p.arm();
        assert!(p.armed());
        assert_eq!(
            p.resolve(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE), &quick),
            PrefixEvent::Go(GoTarget::Quick(0))
        );
        assert!(!p.armed(), "resolving always disarms");

        p.arm();
        assert_eq!(
            p.resolve(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE), &quick),
            PrefixEvent::Go(GoTarget::Top)
        );

        p.arm();
        assert_eq!(
            p.resolve(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::NONE), &quick),
            PrefixEvent::Go(GoTarget::Update)
        );

        // Esc, an unknown key and a modified key all cancel silently.
        for key in [
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL),
        ] {
            p.arm();
            assert_eq!(p.resolve(key, &quick), PrefixEvent::Cancelled);
            assert!(!p.armed());
        }
    }

    /// The rendered screen, one `String` per row.
    fn rows(term: &Terminal<TestBackend>) -> Vec<String> {
        let buf = term.backend().buffer();
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect()
    }

    /// Draw one overlay into a `w × h` terminal whose body is rows 1..h-2 (the
    /// header band, key bar and status row that `app::draw` reserves).
    fn shot(w: u16, h: u16, draw: impl Fn(&mut Frame, Rect)) -> Vec<String> {
        let mut term = Terminal::new(TestBackend::new(w, h)).expect("terminal");
        term.draw(|f| {
            let body = Rect::new(0, 1, w, h - 3);
            draw(f, body);
        })
        .expect("render");
        rows(&term)
    }

    /// Assert the drawn panel is a closed box inside the body: its corners are
    /// present, its widest row is no wider than the screen, and the header /
    /// key bar / status rows it must never cover are still blank.
    fn assert_boxed_in_body(rows: &[String], w: u16, h: u16, title: &str, bottom: &str) {
        let painted: Vec<usize> = rows
            .iter()
            .enumerate()
            .filter(|(_, r)| !r.trim().is_empty())
            .map(|(i, _)| i)
            .collect();
        assert!(!painted.is_empty(), "overlay drew nothing at {w}x{h}");
        let (first, last) = (painted[0], painted[painted.len() - 1]);
        assert!(first >= 1, "overlay covered the header band at {w}x{h}");
        assert!(
            last <= (h - 3) as usize,
            "overlay covered the key bar / status rows at {w}x{h}: last painted row {last}"
        );
        for (i, row) in rows.iter().enumerate() {
            assert_eq!(
                row.chars().count(),
                w as usize,
                "row {i} is not {w} cells at {w}x{h}"
            );
        }
        let text = rows.join("\n");
        assert!(text.contains(title), "missing title {title} at {w}x{h}");
        assert!(
            text.contains(bottom),
            "missing bottom title {bottom:?} at {w}x{h}"
        );
        // A closed box: every painted row starts and ends with a border cell.
        let corners = ['\u{256D}', '\u{256E}', '\u{2570}', '\u{256F}', '\u{2502}'];
        for (i, row) in rows.iter().enumerate().take(last + 1).skip(first) {
            let trimmed = row.trim();
            assert!(
                trimmed.starts_with(corners) && trimmed.ends_with(corners),
                "row {i} is not inside the panel at {w}x{h}: {trimmed:?}"
            );
        }
    }

    #[test]
    fn palette_which_key_and_keys_card_fit_at_120x36_and_80x24() {
        let theme = Theme::truecolor();
        let hints = Hints::new(
            &[
                ("r", "reply"),
                ("j/k", "scroll"),
                ("l", "like"),
                ("o", "links"),
                ("u", "open in web"),
            ],
            0,
        );
        for (w, h) in [(120u16, 36u16), (80, 24)] {
            let mut p = palette();
            p.push_member(&User {
                user_id: 125694,
                username: "WindowsForum AI".into(),
                ..Default::default()
            });
            for c in "win".chars() {
                press(&mut p, c);
            }

            let shot_palette = shot(w, h, |f, body| p.render(f, body, &theme, &UNICODE, &mut crate::hit::HitMap::default()));
            assert_boxed_in_body(&shot_palette, w, h, "Go to", "Enter go");
            let text = shot_palette.join("\n");
            assert!(text.contains("\u{203A} win"), "query line missing:\n{text}");
            assert!(text.contains("forum "), "kind column missing:\n{text}");
            assert!(
                text.contains("Windows Help and Support"),
                "fuzzy hit missing:\n{text}"
            );
            assert!(text.contains("@WindowsForum AI"), "member row missing");

            let shot_which = shot(w, h, |f, body| {
                render_which_key(f, body, &theme, &UNICODE, &which_key_cells(&common::site::SiteConfig::windowsforum()), &mut crate::hit::HitMap::default())
            });
            assert_boxed_in_body(&shot_which, w, h, "g \u{2026}", "Esc cancel");
            let text = shot_which.join("\n");
            for word in ["news", "security", "tutorials", "inbox", "alerts", "top", "update"] {
                assert!(text.contains(word), "which-key is missing {word}:\n{text}");
            }

            let shot_card = shot(w, h, |f, body| {
                render_keys_card(f, body, &theme, &UNICODE, "THIS THREAD", &hints, &mut crate::hit::HitMap::default())
            });
            assert_boxed_in_body(&shot_card, w, h, "Keys", "any key closes");
            let text = shot_card.join("\n");
            for group in ["MOVE", "THIS THREAD", "EVERYWHERE", "MOUSE & CLIPBOARD"] {
                assert!(text.contains(group), "card is missing {group}:\n{text}");
            }
            // The MOUSE group teaches the pointer/touch gestures and the one
            // way to turn the whole layer off (beside Shift+drag).
            for gesture in ["click", "right-click", "drag", "wheel", "Shift+drag", "WFTUI_MOUSE=0"] {
                assert!(
                    text.contains(gesture),
                    "the MOUSE group is missing {gesture}:\n{text}"
                );
            }
            // The screen's own group comes from its hints, minus what the MOVE
            // and EVERYWHERE columns already teach (`j/k` here).
            assert!(text.contains("reply"), "{text}");
            assert!(text.contains("open in web"), "{text}");
            assert!(
                !text.contains("j/k  scroll"),
                "the MOVE column already teaches j/k:\n{text}"
            );
        }
    }

    #[test]
    fn palette_selection_band_spans_the_panel_and_dimming_flattens_the_body() {
        let theme = Theme::truecolor();
        let mut p = palette();
        press(&mut p, 'w');
        let mut term = Terminal::new(TestBackend::new(120, 36)).expect("terminal");
        let body = Rect::new(0, 1, 120, 33);
        term.draw(|f| {
            // Something under the overlay, in a style dimming must flatten.
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    "a thread title".to_string(),
                    Style::new().fg(theme.accent).add_modifier(Modifier::BOLD),
                ))),
                Rect::new(0, 30, 120, 1),
            );
            dim_body(f, body, &theme);
            p.render(f, body, &theme, &UNICODE, &mut crate::hit::HitMap::default());
        })
        .expect("render");
        let buf = term.backend().buffer();

        // The body behind the palette is faint, flat and unbolded.
        let cell = &buf[(0, 30)];
        assert_eq!(cell.symbol(), "a");
        assert_eq!(cell.fg, theme.faint);
        assert!(!cell.modifier.contains(Modifier::BOLD));

        // Exactly one row carries the selection band, across the panel's
        // whole inner width (60 wide, centered: inner columns 31..=88).
        let banded: Vec<u16> = (0..buf.area.height)
            .filter(|y| (31..=88).all(|x| buf[(x, *y)].bg == theme.selected_bg))
            .collect();
        assert_eq!(banded.len(), 1, "one selection band: rows {banded:?}");
    }

    #[test]
    fn keys_card_marks_the_screen_primary_and_nothing_else() {
        let theme = Theme::truecolor();
        let hints = Hints::new(&[("r", "reply"), ("l", "like")], 0);
        let mut term = Terminal::new(TestBackend::new(120, 36)).expect("terminal");
        term.draw(|f| {
            render_keys_card(
                f,
                Rect::new(0, 1, 120, 33),
                &theme,
                &UNICODE,
                "THIS THREAD",
                &hints, &mut crate::hit::HitMap::default(),)
        })
        .expect("render");
        let buf = term.backend().buffer();
        let accents: String = (0..buf.area.height)
            .flat_map(|y| {
                (0..buf.area.width).filter_map(move |x| {
                    let cell = &buf[(x, y)];
                    (cell.bg == theme.accent_bg).then(|| cell.symbol().to_string())
                })
            })
            .collect();
        assert_eq!(accents, " r ", "only the primary cap is accented: {accents:?}");
    }

    /// Every cap the which-key panel shows must resolve: a chord the panel
    /// teaches and the prefix machine cancels is the silent no-op the key
    /// contract forbids.
    #[test]
    fn every_which_key_cell_resolves_to_a_go_target() {
        let site = common::site::SiteConfig::windowsforum();
        let cells = which_key_cells(&site);
        assert_eq!(which_key_rows(&cells).len() as u16 + 2, WHICH_KEY_HEIGHT, "the built-in panel is the DESIGN.md one");
        for (key, label) in &cells {
            let mut p = Prefix::default();
            p.arm();
            let c = key.chars().next().unwrap();
            assert!(
                matches!(p.resolve(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE), &site.quick), PrefixEvent::Go(_)),
                "g {key} ({label}) is advertised but does not resolve"
            );
        }
        // The built-in chords resolve to the site's quick list, in order.
        let mut p = Prefix::default();
        p.arm();
        assert_eq!(p.resolve(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE), &site.quick), PrefixEvent::Go(GoTarget::Quick(1)));
        // A site with no add-ons and no quick nodes: the fixed rows only.
        assert_eq!(
            cells.iter().take(3).map(|(k, l)| format!("{k} {l}")).collect::<Vec<_>>(),
            ["n news", "s security", "t tutorials"],
            "the built-in panel reads as it always did"
        );
        let plain = common::site::SiteConfig::blank("plain");
        let cells = which_key_cells(&plain);
        assert_eq!(cells.iter().map(|(k, _)| *k).collect::<Vec<_>>(), ["l", "i", "a", "d", "h", "p", "g", "u"]);
        let mut p = Prefix::default();
        p.arm();
        assert_eq!(p.resolve(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE), &plain.quick), PrefixEvent::Cancelled);
        assert_eq!(static_letter('q'), "q");
        assert_eq!(static_letter('Q'), "?");
    }

    #[test]
    fn overlays_render_inside_the_body_at_every_size() {
        for (w, h) in [(120u16, 36u16), (80, 24), (60, 18), (40, 10), (20, 5)] {
            for theme in [Theme::truecolor(), Theme::ansi16(), Theme::mono()] {
                for g in [&UNICODE, &ASCII] {
                    let mut term = Terminal::new(TestBackend::new(w, h)).expect("terminal");
                    let mut p = palette();
                    press(&mut p, 'w');
                    let hints = Hints::new(&[("r", "reply"), ("l", "like")], 0);
                    term.draw(|f| {
                        let body = Rect::new(0, 1, w, h.saturating_sub(3).max(1));
                        dim_body(f, body, &theme);
                        p.render(f, body, &theme, g, &mut crate::hit::HitMap::default());
                        render_which_key(f, body, &theme, g, &which_key_cells(&common::site::SiteConfig::windowsforum()), &mut crate::hit::HitMap::default());
                        render_keys_card(f, body, &theme, g, "THIS THREAD", &hints, &mut crate::hit::HitMap::default());
                    })
                    .expect("render");
                }
            }
        }
    }
}

