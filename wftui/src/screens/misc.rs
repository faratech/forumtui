//! Login, compose, search, profile screens.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;

use common::models::SearchHit;

use super::{
    browse::{push_bbcode, truncate},
    link_style, solo_panel, Action, ComposeTarget, LoginStage,
};
use crate::chrome::{self, Hints};
use crate::glyph::Glyphs;
use crate::images;
use crate::theme::{fmt_time, Theme};

// ================= login =================

pub fn login_key(s: &mut super::LoginState, key: KeyEvent) -> Action {
    if s.busy {
        return Action::None;
    }
    match (&s.stage, key.code) {
        (LoginStage::Idle, KeyCode::Enter) => Action::LoginBegin,
        (LoginStage::Waiting, KeyCode::Char('c')) => Action::OscCopy(s.url.clone()),
        (LoginStage::Waiting, KeyCode::Char('o')) => Action::OpenUrl(s.url.clone()),
        _ => Action::None,
    }
}

pub fn login_hints() -> Hints {
    Hints::new(
        &[
            ("c", "copy link"),
            ("o", "open here"),
            ("Enter", "restart login"),
            ("q", "quit"),
        ],
        0,
    )
}

/// Columns inside the login panel's inner area (DESIGN.md / `LoginText.dc.html`
/// pixel reference): the mark box sits `MARK_PAD` in, its accompanying brand
/// copy starts at `TEXT_COL`, and the numbered steps indent their body text
/// (and the link box) to `STEP_BODY_COL`, with the digit itself one tab in at
/// `STEP_NUM_COL`.
const LOGIN_PANEL_WIDTH: u16 = 72;
const MARK_PAD: usize = 4;
const TEXT_COL: usize = 21;
const STEP_NUM_COL: usize = 3;
const STEP_BODY_COL: usize = 6;

/// The mark as a white speech-bubble chip, echoing `wf-logo.png`: rounded
/// top-left/top-right/bottom-left corners (quarter-block glyphs `▗ ▖ ▝`) and a
/// deliberately square bottom-right corner (a full block `█`, matching the
/// logo exactly), with the interior painted in chrome_fg (white) and `WF` set
/// bold in chrome_bg (brand blue). The four-color quadrant glyph the logo
/// carries inside that same bubble is dropped here on purpose: shown bare (no
/// bubble, no wordmark for context) the way the old block mark rendered it,
/// red/green/blue/yellow quadrants read as the Microsoft Windows logo — a
/// trademark problem this white-on-blue "WF" chip does not have.
///
/// The bubble itself is 4 rows (top / WF row / blank card row / bottom); rows
/// 0 and 5 are blank padding so this still returns exactly 6 rows, aligned
/// with `login_text_rows`'s 6 slots. ASCII has no half-block glyphs to fake
/// rounding, so it falls back to a single plain `[ WF ]` chip on row 2 instead
/// of a multi-row shape.
fn login_mark_rows(theme: &Theme, g: &Glyphs) -> Vec<Vec<Span<'static>>> {
    let pad = || Span::raw(" ".repeat(MARK_PAD));
    let blank = || vec![pad()];
    let wf = Style::new()
        .fg(theme.chrome_bg)
        .bg(theme.chrome_fg)
        .add_modifier(Modifier::BOLD);
    if g.ascii {
        let chip = vec![
            pad(),
            Span::styled("[", theme.faint()),
            Span::styled(" WF ", wf),
            Span::styled("]", theme.faint()),
        ];
        return vec![blank(), blank(), chip, blank(), blank(), blank()];
    }
    let white = Style::new().fg(theme.chrome_fg);
    let fill = Style::new().bg(theme.chrome_fg);
    let top = vec![
        pad(),
        Span::styled("\u{2597}\u{2584}\u{2584}\u{2584}\u{2584}\u{2584}\u{2584}\u{2584}\u{2596}", white),
    ];
    let label = vec![
        pad(),
        Span::styled("\u{2588}", white),
        Span::styled("  ", fill),
        Span::styled("WF", wf),
        Span::styled("   ", fill),
        Span::styled("\u{2588}", white),
    ];
    let card = vec![
        pad(),
        Span::styled("\u{2588}", white),
        Span::styled(" ".repeat(7), fill),
        Span::styled("\u{2588}", white),
    ];
    let bottom = vec![
        pad(),
        // The last glyph is a full block, not a rounded quarter-block: the
        // one corner that stays square, matching wf-logo.png exactly.
        Span::styled("\u{259D}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2588}", white),
    ];
    vec![blank(), top, label, card, bottom, blank()]
}

/// The brand copy beside the mark box, one entry per `login_mark_rows` slot;
/// `None` rows (the box's own top/bottom border) carry no text.
fn login_text_rows(theme: &Theme) -> Vec<Option<Vec<Span<'static>>>> {
    vec![
        None,
        Some(vec![
            Span::styled("Windows", theme.base().add_modifier(Modifier::BOLD)),
            Span::styled("Forum", theme.base()),
        ]),
        Some(vec![Span::styled("for your terminal", theme.dim())]),
        None,
        Some(vec![Span::styled(
            "Sign in with your browser.",
            theme.base(),
        )]),
        Some(vec![Span::styled("Nothing to type here.", theme.dim())]),
    ]
}

/// The brand block when the terminal paints real pixels: 7 blank rows in the
/// logo's columns (the app paints `assets/wf-logo.png` over them) with the
/// same copy beside them, at the same `TEXT_COL` as the block-mark version so
/// nothing else on the screen moves.
fn login_logo_lines(theme: &Theme) -> Vec<Line<'static>> {
    let mut rows: Vec<Option<Vec<Span<'static>>>> = vec![None; images::LOGO_ROWS as usize];
    rows[1] = Some(vec![
        Span::styled("Windows", theme.base().add_modifier(Modifier::BOLD)),
        Span::styled("Forum", theme.base()),
    ]);
    rows[2] = Some(vec![Span::styled("for your terminal", theme.dim())]);
    rows[4] = Some(vec![Span::styled("Sign in with your browser.", theme.base())]);
    rows[5] = Some(vec![Span::styled("Nothing to type here.", theme.dim())]);
    rows.into_iter()
        .map(|text| {
            let mut spans = vec![Span::raw(" ".repeat(TEXT_COL))];
            if let Some(t) = text {
                spans.extend(t);
            }
            Line::from(spans)
        })
        .collect()
}

fn login_brand_lines(theme: &Theme, g: &Glyphs) -> Vec<Line<'static>> {
    // "[ WF ]" (6) in ASCII; the 9-cell bubble chip in Unicode.
    let box_w = if g.ascii { 6 } else { 9 };
    let gap = TEXT_COL.saturating_sub(MARK_PAD + box_w);
    login_mark_rows(theme, g)
        .into_iter()
        .zip(login_text_rows(theme))
        .map(|(mut spans, text)| {
            if let Some(t) = text {
                spans.push(Span::raw(" ".repeat(gap)));
                spans.extend(t);
            }
            Line::from(spans)
        })
        .collect()
}

fn login_step_line(theme: &Theme, n: &str, body: &str) -> Line<'static> {
    Line::from(vec![
        Span::raw(" ".repeat(STEP_NUM_COL)),
        Span::styled(n.to_string(), Style::new().fg(theme.accent).add_modifier(Modifier::BOLD)),
        Span::raw("  "),
        Span::styled(body.to_string(), theme.base()),
    ])
}

pub fn render_login(
    s: &mut super::LoginState,
    f: &mut Frame,
    area: Rect,
    theme: &Theme,
    g: &Glyphs,
) {
    // Tiers 1-3 replace the block mark with the real logo; tiers 4 (half-
    // blocks) and 5 (text) keep the mark, which is already drawn out of block
    // glyphs and reads better than a 16x7 half-block rendering would.
    let logo = s.images.pixels();
    let mut lines = if logo {
        login_logo_lines(theme)
    } else {
        login_brand_lines(theme, g)
    };

    match &s.stage {
        LoginStage::Idle => {
            lines.push(Line::from(Span::raw("")));
            lines.push(Line::from(vec![
                Span::raw(" ".repeat(STEP_BODY_COL)),
                chrome::keycap(theme, "Enter", true),
                Span::raw(" begin sign-in"),
            ]));
        }
        LoginStage::Waiting => {
            lines.push(login_step_line(
                theme,
                "1",
                "Open this link on any device \u{2014} phone is fine",
            ));

            let vbar = if g.ascii { "|" } else { "\u{2502}" };
            let (tl, tr, bl, br) = if g.ascii {
                ("+", "+", "+", "+")
            } else {
                ("\u{256D}", "\u{256E}", "\u{2570}", "\u{256F}")
            };
            let rule = if g.ascii { "-" } else { "\u{2500}" };
            let url_len = s.url.chars().count();
            let link_inner = (url_len + 4).max(48);

            lines.push(Line::from(vec![
                Span::raw(" ".repeat(STEP_BODY_COL)),
                Span::styled(format!("{tl}{}{tr}", rule.repeat(link_inner)), theme.faint()),
            ]));
            let pad_after = link_inner.saturating_sub(2 + url_len);
            lines.push(Line::from(vec![
                Span::raw(" ".repeat(STEP_BODY_COL)),
                Span::styled(vbar, theme.faint()),
                Span::raw("  "),
                Span::styled(
                    // Plain text, never OSC-embedded: escape sequences inside
                    // ratatui spans get re-emitted per-cell on diff and
                    // corrupt the screen. Drag-select copies it; c copies it
                    // without selecting.
                    s.url.clone(),
                    Style::new()
                        .fg(theme.accent)
                        .add_modifier(Modifier::BOLD)
                        .add_modifier(Modifier::UNDERLINED),
                ),
                Span::raw(" ".repeat(pad_after)),
                Span::styled(vbar, theme.faint()),
            ]));
            lines.push(Line::from(vec![
                Span::raw(" ".repeat(STEP_BODY_COL)),
                Span::styled(format!("{bl}{}{br}", rule.repeat(link_inner)), theme.faint()),
            ]));
            let copied_text = format!(
                "copied to your clipboard \u{b7} also in {}/login-url.txt",
                std::path::Path::new(&common::config::token_path())
                    .parent()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default()
            );
            // The panel is a fixed 72 wide, so the room for this line is
            // fixed too — truncate rather than let `Wrap` push the rows
            // below (including the spinner) past the panel's bottom border.
            let copied_max = (LOGIN_PANEL_WIDTH as usize).saturating_sub(2 + STEP_BODY_COL);
            lines.push(Line::from(vec![
                Span::raw(" ".repeat(STEP_BODY_COL)),
                Span::styled(truncate(&copied_text, copied_max), theme.dim()),
            ]));
            lines.push(login_step_line(
                theme,
                "2",
                "Approve access on the site (your usual 2FA applies)",
            ));
            lines.push(login_step_line(theme, "3", "This window finishes on its own"));
            lines.push(Line::from(vec![
                Span::raw(" ".repeat(STEP_BODY_COL)),
                Span::styled(
                    format!("{} ", chrome::spinner(g, chrome::spinner_tick())),
                    Style::new().fg(theme.accent),
                ),
                Span::styled("waiting for approval\u{2026}", theme.dim()),
            ]));
        }
    }
    if s.busy {
        lines.push(Line::from(Span::styled("Working\u{2026}", theme.dim())));
    }
    if let Some(err) = &s.error {
        lines.push(Line::from(Span::styled(
            format!("Error: {err}"),
            Style::new().fg(theme.error),
        )));
    }

    let height = (lines.len() as u16 + 2).min(area.height);
    let panel_area = super::centered_box(area, LOGIN_PANEL_WIDTH, height);
    let block = solo_panel(theme, g, "Sign in to WindowsForum", None, None);
    let inner = block.inner(panel_area);
    f.render_widget(block, panel_area);
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);

    // Cleared every frame: the app paints only what this frame reserved.
    s.image_requests.clear();
    if logo
        && inner.width >= MARK_PAD as u16 + images::LOGO_COLS
        && inner.height >= images::LOGO_ROWS
    {
        s.image_requests.push(images::Request {
            key: images::LOGO_KEY.to_string(),
            rect: Rect::new(
                inner.x + MARK_PAD as u16,
                inner.y,
                images::LOGO_COLS,
                images::LOGO_ROWS,
            ),
        });
    }
}

// ================= compose =================

/// Insert `[TAG]` before the cursor and `[/TAG]` after it — the no-selection
/// wrap case (DESIGN.md): the cursor lands between the two tags so the next
/// character typed continues inside them. `^B`/`^I`/`^K`/`^Q`/`^U` all funnel
/// through this with `tag` = `B`/`I`/`ICODE`/`QUOTE`/`URL`.
pub(crate) fn insert_bbcode_wrap(body: &mut String, cursor: &mut usize, tag: &str) {
    let open = format!("[{tag}]");
    let close = format!("[/{tag}]");
    crate::editor::insert_str(body, cursor, &open);
    let inner = *cursor;
    crate::editor::insert_str(body, cursor, &close);
    *cursor = inner;
}

pub fn compose_key(s: &mut super::ComposeState, key: KeyEvent) -> Action {
    if s.busy {
        return Action::None;
    }
    let target = match &s.target {
        Some(t) => t.clone(),
        None => return Action::None,
    };
    let is_new_thread = matches!(target, ComposeTarget::NewThread { .. });
    if !is_new_thread {
        s.title_field = false;
    }

    if key.code == KeyCode::Esc {
        return Action::PopScreen;
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        match key.code {
            KeyCode::Char('s') => {
                match target {
                    ComposeTarget::ThreadReply { thread_id, .. } => {
                        if s.body.trim().is_empty() {
                            s.error = Some("Reply is empty.".into());
                            return Action::None;
                        }
                        s.busy = true;
                        return Action::SubmitReply {
                            thread_id,
                            message: s.body.clone(),
                        };
                    }
                    ComposeTarget::NewThread { node_id } => {
                        if s.title.trim().is_empty() || s.body.trim().is_empty() {
                            s.error = Some("Title and message are required.".into());
                            return Action::None;
                        }
                        s.busy = true;
                        return Action::SubmitThread {
                            node_id,
                            title: s.title.clone(),
                            message: s.body.clone(),
                        };
                    }
                    ComposeTarget::ConversationReply {
                        conversation_id, ..
                    } => {
                        if s.body.trim().is_empty() {
                            s.error = Some("Message is empty.".into());
                            return Action::None;
                        }
                        s.busy = true;
                        return Action::SubmitConvoReply {
                            id: conversation_id,
                            message: s.body.clone(),
                        };
                    }
                }
            }
            KeyCode::Char('y') | KeyCode::Char('v') => {
                return Action::PasteClipboard;
            }
            KeyCode::Char('o') => {
                s.preview = !s.preview;
                return Action::None;
            }
            // BBCode tag caps: wrap-with-no-selection inserts `[TAG][/TAG]`
            // around the cursor so typing continues inside the pair. Only
            // meaningful in the message body, never the new-thread title.
            KeyCode::Char('b') if !(s.title_field && is_new_thread) => {
                insert_bbcode_wrap(&mut s.body, &mut s.body_cursor, "B");
                return Action::None;
            }
            KeyCode::Char('i') if !(s.title_field && is_new_thread) => {
                insert_bbcode_wrap(&mut s.body, &mut s.body_cursor, "I");
                return Action::None;
            }
            KeyCode::Char('k') if !(s.title_field && is_new_thread) => {
                insert_bbcode_wrap(&mut s.body, &mut s.body_cursor, "ICODE");
                return Action::None;
            }
            KeyCode::Char('q') if !(s.title_field && is_new_thread) => {
                insert_bbcode_wrap(&mut s.body, &mut s.body_cursor, "QUOTE");
                return Action::None;
            }
            KeyCode::Char('u') if !(s.title_field && is_new_thread) => {
                insert_bbcode_wrap(&mut s.body, &mut s.body_cursor, "URL");
                return Action::None;
            }
            _ => {}
        }
    }

    if s.title_field && is_new_thread {
        match key.code {
            KeyCode::Tab | KeyCode::Enter => {
                s.title_field = false;
                s.body_cursor = s.body.chars().count();
                Action::None
            }
            KeyCode::Backspace => {
                crate::editor::delete_back(&mut s.title, &mut s.title_cursor);
                Action::None
            }
            KeyCode::Delete => {
                crate::editor::delete_forward(&mut s.title, &mut s.title_cursor);
                Action::None
            }
            KeyCode::Left => {
                crate::editor::move_left(&mut s.title_cursor);
                Action::None
            }
            KeyCode::Right => {
                crate::editor::move_right(&s.title, &mut s.title_cursor);
                Action::None
            }
            KeyCode::Home => {
                crate::editor::move_home(&s.title, &mut s.title_cursor);
                Action::None
            }
            KeyCode::End => {
                crate::editor::move_end(&s.title, &mut s.title_cursor);
                Action::None
            }
            KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                crate::editor::move_home(&s.title, &mut s.title_cursor);
                Action::None
            }
            KeyCode::Char('e') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                crate::editor::move_end(&s.title, &mut s.title_cursor);
                Action::None
            }
            KeyCode::Char('w') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                crate::editor::delete_word_back(&mut s.title, &mut s.title_cursor);
                Action::None
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                crate::editor::kill_to_start(&mut s.title, &mut s.title_cursor);
                Action::None
            }
            KeyCode::Char('k') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                crate::editor::kill_to_end(&mut s.title, &mut s.title_cursor);
                Action::None
            }
            KeyCode::Char(c)
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                crate::editor::insert_char(&mut s.title, &mut s.title_cursor, c);
                Action::None
            }
            _ => Action::None,
        }
    } else {
        match key.code {
            KeyCode::Tab => {
                if is_new_thread {
                    s.title_field = true;
                    s.title_cursor = s.title.chars().count();
                } else {
                    crate::editor::insert_str(&mut s.body, &mut s.body_cursor, "    ");
                }
                Action::None
            }
            KeyCode::Backspace => {
                crate::editor::delete_back(&mut s.body, &mut s.body_cursor);
                Action::None
            }
            KeyCode::Delete => {
                crate::editor::delete_forward(&mut s.body, &mut s.body_cursor);
                Action::None
            }
            KeyCode::Left => {
                crate::editor::move_left(&mut s.body_cursor);
                Action::None
            }
            KeyCode::Right => {
                crate::editor::move_right(&s.body, &mut s.body_cursor);
                Action::None
            }
            KeyCode::Home => {
                crate::editor::move_home(&s.body, &mut s.body_cursor);
                Action::None
            }
            KeyCode::End => {
                crate::editor::move_end(&s.body, &mut s.body_cursor);
                Action::None
            }
            KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                crate::editor::move_home(&s.body, &mut s.body_cursor);
                Action::None
            }
            KeyCode::Char('e') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                crate::editor::move_end(&s.body, &mut s.body_cursor);
                Action::None
            }
            KeyCode::Char('w') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                crate::editor::delete_word_back(&mut s.body, &mut s.body_cursor);
                Action::None
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                crate::editor::kill_to_start(&mut s.body, &mut s.body_cursor);
                Action::None
            }
            KeyCode::Char('k') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                crate::editor::kill_to_end(&mut s.body, &mut s.body_cursor);
                Action::None
            }
            KeyCode::Enter => {
                crate::editor::insert_char(&mut s.body, &mut s.body_cursor, '\n');
                Action::None
            }
            KeyCode::Char(c)
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                crate::editor::insert_char(&mut s.body, &mut s.body_cursor, c);
                Action::None
            }
            _ => Action::None,
        }
    }
}

pub fn compose_hints(s: &super::ComposeState) -> Hints {
    let is_new_thread = matches!(s.target, Some(ComposeTarget::NewThread { .. }));
    let primary = if is_new_thread { "post thread" } else { "send" };
    let primary_short = if is_new_thread { "post" } else { "send" };
    Hints::with_short(
        &[
            ("^S", primary),
            ("^O", "preview on/off"),
            ("^Y", "paste"),
            ("^A", "attach"),
            ("Tab", "field"),
            ("Esc", "discard"),
        ],
        &[
            ("^S", primary_short),
            ("^O", "preview"),
            ("^Y", ""),
            ("^A", ""),
            ("Esc", "discard"),
        ],
        0,
    )
}

pub fn compose_crumb(s: &super::ComposeState) -> String {
    match &s.target {
        Some(ComposeTarget::NewThread { .. }) => "New thread".into(),
        Some(ComposeTarget::ConversationReply { .. }) => "Reply".into(),
        _ => "Reply".into(),
    }
}

fn compose_is_new_thread(target: &Option<ComposeTarget>) -> bool {
    matches!(target, Some(ComposeTarget::NewThread { .. }))
}

/// Rows the editor's header occupies: the new-thread flow needs a Title line
/// plus the "switch field" hint; a conversation reply with named participants
/// gets a second line for them; everything else is the single `To … · as …
/// · post #n` line.
fn compose_header_rows(s: &super::ComposeState) -> u16 {
    if compose_is_new_thread(&s.target) {
        return 2;
    }
    let has_participants = matches!(
        &s.target,
        Some(ComposeTarget::ConversationReply { participants, .. }) if !participants.is_empty()
    );
    if has_participants { 2 } else { 1 }
}

fn compose_header_lines(s: &super::ComposeState, theme: &Theme) -> Vec<Line<'static>> {
    if compose_is_new_thread(&s.target) {
        return vec![
            Line::from(vec![
                Span::styled("Title: ", theme.dim()),
                Span::styled(s.title.clone(), theme.base()),
            ]),
            Line::from(Span::styled("Message (Tab = switch field):", theme.dim())),
        ];
    }
    let label = match &s.target {
        Some(ComposeTarget::ThreadReply { thread_title, .. }) => thread_title.clone(),
        Some(ComposeTarget::ConversationReply {
            conversation_title, ..
        }) => conversation_title.clone(),
        _ => String::new(),
    };
    let mut meta = vec![Span::styled("To ", theme.dim()), Span::styled(label, theme.base())];
    if !s.author.is_empty() {
        meta.push(Span::styled(format!(" \u{b7} as {}", s.author), theme.dim()));
    }
    if let Some(n) = s.reply_number {
        meta.push(Span::styled(format!(" \u{b7} post #{n}"), theme.dim()));
    }
    let mut lines = vec![Line::from(meta)];
    if let Some(ComposeTarget::ConversationReply { participants, .. }) = &s.target
        && !participants.is_empty()
    {
        lines.push(Line::from(vec![
            Span::styled("Participants: ", theme.dim()),
            Span::styled(
                participants.clone(),
                theme.base().add_modifier(Modifier::BOLD),
            ),
        ]));
    }
    lines
}

/// The editor's bottom row: BBCode caps `^B ^I ^K ^Q ^U` on the left, the
/// character count right-aligned against the panel width.
fn caps_line(theme: &Theme, chars: usize, width: u16) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = vec![Span::raw(" ")];
    for (key, desc) in [
        ("^B", "bold"),
        ("^I", "italic"),
        ("^K", "code"),
        ("^Q", "quote"),
        ("^U", "link"),
    ] {
        spans.push(chrome::keycap(theme, key, false));
        spans.push(Span::styled(format!(" {desc}  "), theme.dim()));
    }
    let label = format!("{chars} chars");
    let left_w: usize = spans.iter().map(Span::width).sum();
    let w = width as usize;
    let label_w = label.chars().count();
    if left_w + label_w < w {
        spans.push(Span::raw(" ".repeat(w - left_w - label_w)));
    }
    spans.push(Span::styled(label, theme.dim()));
    Line::from(spans)
}

/// The `Reply` editor panel: header, rule, body, rule, BBCode caps.
fn draw_editor_panel(
    s: &mut super::ComposeState,
    f: &mut Frame,
    area: Rect,
    theme: &Theme,
    g: &Glyphs,
    focused: bool,
) {
    let block = chrome::panel(theme, g, "Reply", focused, Some("edit"), None);
    let inner = block.inner(area);
    f.render_widget(block, area);

    let header_rows = compose_header_rows(s);
    let chunks = Layout::vertical([
        Constraint::Length(header_rows),
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .split(inner);

    f.render_widget(Paragraph::new(compose_header_lines(s, theme)), chunks[0]);

    let rule = if g.ascii { "-" } else { "\u{2500}" };
    let rule_line = |w: u16| Line::from(Span::styled(rule.repeat(w as usize), theme.faint()));
    f.render_widget(Paragraph::new(rule_line(chunks[1].width)), chunks[1]);

    let is_new_thread = compose_is_new_thread(&s.target);
    let mut body_lines: Vec<Line<'static>> = Vec::new();
    for line in s.body.split('\n') {
        body_lines.push(Line::from(Span::styled(line.to_string(), theme.base())));
    }
    if let Some(err) = &s.error {
        body_lines.push(Line::from(Span::styled(
            format!("Error: {err}"),
            Style::new().fg(theme.error),
        )));
    }
    if s.busy {
        body_lines.push(Line::from(Span::styled("Sending\u{2026}", theme.dim())));
    }
    f.render_widget(
        Paragraph::new(body_lines).wrap(Wrap { trim: false }),
        chunks[2],
    );

    f.render_widget(rule_line(chunks[3].width), chunks[3]);
    f.render_widget(
        caps_line(theme, s.body.chars().count(), chunks[4].width),
        chunks[4],
    );

    if is_new_thread && s.title_field {
        let cur_col = s.title.chars().take(s.title_cursor).count() as u16;
        let cur_x = (chunks[0].x + 7 + cur_col).min(chunks[0].x + chunks[0].width.saturating_sub(1));
        f.set_cursor_position((cur_x, chunks[0].y));
    } else {
        let (b_col, b_row) = crate::editor::cursor_coords(&s.body, s.body_cursor);
        let cur_x = (chunks[2].x + b_col).min(chunks[2].x + chunks[2].width.saturating_sub(1));
        let cur_y = (chunks[2].y + b_row).min(chunks[2].y + chunks[2].height.saturating_sub(1));
        f.set_cursor_position((cur_x, cur_y));
    }
}

/// The `Preview` panel: renders the draft through the same
/// `common::bbcode::render` chunk-to-line logic the thread view uses
/// (`push_bbcode`), so what a member sees here is what the post will render
/// as, not the raw BBCode source.
fn draw_preview_panel(s: &super::ComposeState, f: &mut Frame, area: Rect, theme: &Theme, g: &Glyphs) {
    let block = chrome::panel(theme, g, "Preview", false, None, None);
    let inner = block.inner(area);
    f.render_widget(block, area);

    let chunks =
        Layout::vertical([Constraint::Length(1), Constraint::Length(1), Constraint::Min(1)])
            .split(inner);
    f.render_widget(
        Paragraph::new(Line::from(Span::styled("as it will appear", theme.dim()))),
        chunks[0],
    );

    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut links: Vec<String> = Vec::new();
    push_bbcode(&mut lines, &mut links, &s.body, theme);
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), chunks[2]);
}

pub fn render_compose(
    s: &mut super::ComposeState,
    f: &mut Frame,
    area: Rect,
    theme: &Theme,
    g: &Glyphs,
) {
    if area.width >= 110 {
        let cols = Layout::horizontal([
            Constraint::Length(72),
            Constraint::Length(48),
            Constraint::Min(0),
        ])
        .split(area);
        draw_editor_panel(s, f, cols[0], theme, g, true);
        draw_preview_panel(s, f, cols[1], theme, g);
    } else if s.preview {
        draw_preview_panel(s, f, area, theme, g);
    } else {
        draw_editor_panel(s, f, area, theme, g, true);
    }
}

// ================= search =================

pub fn search_key(s: &mut super::SearchState, key: KeyEvent) -> Action {
    if s.input_mode {
        if key.code == KeyCode::Esc {
            s.input_mode = false;
            return Action::None;
        }
        if key.code == KeyCode::Tab {
            s.active_field = if s.active_field == 0 { 1 } else { 0 };
            return Action::None;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('v') => return Action::PasteClipboard,
                KeyCode::Char('a') => {
                    let (txt, cur) = if s.active_field == 0 {
                        (&mut s.query, &mut s.query_cursor)
                    } else {
                        (&mut s.author, &mut s.author_cursor)
                    };
                    crate::editor::move_home(txt, cur);
                    return Action::None;
                }
                KeyCode::Char('e') => {
                    let (txt, cur) = if s.active_field == 0 {
                        (&mut s.query, &mut s.query_cursor)
                    } else {
                        (&mut s.author, &mut s.author_cursor)
                    };
                    crate::editor::move_end(txt, cur);
                    return Action::None;
                }
                KeyCode::Char('w') => {
                    let (txt, cur) = if s.active_field == 0 {
                        (&mut s.query, &mut s.query_cursor)
                    } else {
                        (&mut s.author, &mut s.author_cursor)
                    };
                    crate::editor::delete_word_back(txt, cur);
                    return Action::None;
                }
                KeyCode::Char('u') => {
                    let (txt, cur) = if s.active_field == 0 {
                        (&mut s.query, &mut s.query_cursor)
                    } else {
                        (&mut s.author, &mut s.author_cursor)
                    };
                    crate::editor::kill_to_start(txt, cur);
                    return Action::None;
                }
                KeyCode::Char('k') => {
                    let (txt, cur) = if s.active_field == 0 {
                        (&mut s.query, &mut s.query_cursor)
                    } else {
                        (&mut s.author, &mut s.author_cursor)
                    };
                    crate::editor::kill_to_end(txt, cur);
                    return Action::None;
                }
                _ => {}
            }
        }
        match key.code {
            KeyCode::Enter => {
                let q = s.query.trim().to_string();
                let a = s.author.trim().to_string();
                if q.is_empty() && a.is_empty() {
                    return Action::None;
                }
                s.input_mode = false;
                s.loading = true;
                let ct = match s.content_type {
                    1 => Some("thread".into()),
                    2 => Some("post".into()),
                    _ => None,
                };
                let ord = match s.order {
                    1 => Some("relevance".into()),
                    _ => Some("date".into()),
                };
                let user = if a.is_empty() { None } else { Some(a) };
                Action::RunSearchQuery(common::models::SearchQuery {
                    keywords: q,
                    user,
                    content_type: ct,
                    order: ord,
                    page: 1,
                })
            }
            KeyCode::Backspace => {
                let (txt, cur) = if s.active_field == 0 {
                    (&mut s.query, &mut s.query_cursor)
                } else {
                    (&mut s.author, &mut s.author_cursor)
                };
                crate::editor::delete_back(txt, cur);
                Action::None
            }
            KeyCode::Delete => {
                let (txt, cur) = if s.active_field == 0 {
                    (&mut s.query, &mut s.query_cursor)
                } else {
                    (&mut s.author, &mut s.author_cursor)
                };
                crate::editor::delete_forward(txt, cur);
                Action::None
            }
            KeyCode::Left => {
                let (_, cur) = if s.active_field == 0 {
                    (&mut s.query, &mut s.query_cursor)
                } else {
                    (&mut s.author, &mut s.author_cursor)
                };
                crate::editor::move_left(cur);
                Action::None
            }
            KeyCode::Right => {
                let (txt, cur) = if s.active_field == 0 {
                    (&mut s.query, &mut s.query_cursor)
                } else {
                    (&mut s.author, &mut s.author_cursor)
                };
                crate::editor::move_right(txt, cur);
                Action::None
            }
            KeyCode::Home => {
                let (txt, cur) = if s.active_field == 0 {
                    (&mut s.query, &mut s.query_cursor)
                } else {
                    (&mut s.author, &mut s.author_cursor)
                };
                crate::editor::move_home(txt, cur);
                Action::None
            }
            KeyCode::End => {
                let (txt, cur) = if s.active_field == 0 {
                    (&mut s.query, &mut s.query_cursor)
                } else {
                    (&mut s.author, &mut s.author_cursor)
                };
                crate::editor::move_end(txt, cur);
                Action::None
            }
            KeyCode::Char(c)
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                let (txt, cur) = if s.active_field == 0 {
                    (&mut s.query, &mut s.query_cursor)
                } else {
                    (&mut s.author, &mut s.author_cursor)
                };
                crate::editor::insert_char(txt, cur, c);
                Action::None
            }
            _ => Action::None,
        }
    } else {
        match key.code {
            KeyCode::Esc => Action::PopScreen,
            KeyCode::Char('i') => {
                s.input_mode = true;
                s.active_field = 0;
                s.query_cursor = s.query.chars().count();
                Action::None
            }
            KeyCode::Char('a') => {
                s.input_mode = true;
                s.active_field = 1;
                s.author_cursor = s.author.chars().count();
                Action::None
            }
            KeyCode::Char('t') => {
                s.content_type = (s.content_type + 1) % 3;
                let q = s.query.trim().to_string();
                let a = s.author.trim().to_string();
                if !q.is_empty() || !a.is_empty() {
                    s.loading = true;
                    let ct = match s.content_type {
                        1 => Some("thread".into()),
                        2 => Some("post".into()),
                        _ => None,
                    };
                    let ord = match s.order {
                        1 => Some("relevance".into()),
                        _ => Some("date".into()),
                    };
                    let user = if a.is_empty() { None } else { Some(a) };
                    Action::RunSearchQuery(common::models::SearchQuery {
                        keywords: q,
                        user,
                        content_type: ct,
                        order: ord,
                        page: 1,
                    })
                } else {
                    Action::None
                }
            }
            KeyCode::Char('o') => {
                s.order = (s.order + 1) % 2;
                let q = s.query.trim().to_string();
                let a = s.author.trim().to_string();
                if !q.is_empty() || !a.is_empty() {
                    s.loading = true;
                    let ct = match s.content_type {
                        1 => Some("thread".into()),
                        2 => Some("post".into()),
                        _ => None,
                    };
                    let ord = match s.order {
                        1 => Some("relevance".into()),
                        _ => Some("date".into()),
                    };
                    let user = if a.is_empty() { None } else { Some(a) };
                    Action::RunSearchQuery(common::models::SearchQuery {
                        keywords: q,
                        user,
                        content_type: ct,
                        order: ord,
                        page: 1,
                    })
                } else {
                    Action::None
                }
            }
            KeyCode::Char('p') => {
                if let Some(hit) = s.results.get(s.sel) {
                    if !hit.username.is_empty() {
                        Action::OpenProfile(0, hit.username.clone())
                    } else {
                        Action::None
                    }
                } else {
                    Action::None
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if s.sel > 0 {
                    s.sel -= 1;
                }
                Action::None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if s.sel + 1 < s.results.len() {
                    s.sel += 1;
                }
                Action::None
            }
            KeyCode::Char('[') | KeyCode::PageUp => {
                if s.page > 1 {
                    s.loading = true;
                    let ct = match s.content_type {
                        1 => Some("thread".into()),
                        2 => Some("post".into()),
                        _ => None,
                    };
                    let ord = match s.order {
                        1 => Some("relevance".into()),
                        _ => Some("date".into()),
                    };
                    let a = s.author.trim().to_string();
                    let user = if a.is_empty() { None } else { Some(a) };
                    Action::RunSearchQuery(common::models::SearchQuery {
                        keywords: s.query.trim().to_string(),
                        user,
                        content_type: ct,
                        order: ord,
                        page: s.page - 1,
                    })
                } else {
                    Action::None
                }
            }
            KeyCode::Char(']') | KeyCode::PageDown => {
                if s.page < s.last_page {
                    s.loading = true;
                    let ct = match s.content_type {
                        1 => Some("thread".into()),
                        2 => Some("post".into()),
                        _ => None,
                    };
                    let ord = match s.order {
                        1 => Some("relevance".into()),
                        _ => Some("date".into()),
                    };
                    let a = s.author.trim().to_string();
                    let user = if a.is_empty() { None } else { Some(a) };
                    Action::RunSearchQuery(common::models::SearchQuery {
                        keywords: s.query.trim().to_string(),
                        user,
                        content_type: ct,
                        order: ord,
                        page: s.page + 1,
                    })
                } else {
                    Action::None
                }
            }
            KeyCode::Enter | KeyCode::Char('l') => match s.results.get(s.sel) {
                Some(hit) if hit.content_type == "thread" && hit.content_id > 0 => {
                    Action::OpenThread(common::models::Thread {
                        thread_id: hit.content_id as u32,
                        title: hit.title.clone(),
                        view_url: hit.view_url.clone(),
                        ..Default::default()
                    })
                }
                Some(hit) if hit.thread_id.is_some() => {
                    Action::OpenThread(common::models::Thread {
                        thread_id: hit.thread_id.unwrap(),
                        title: hit.title.clone(),
                        view_url: hit.view_url.clone(),
                        ..Default::default()
                    })
                }
                Some(hit) => match &hit.view_url {
                    Some(url) => Action::OpenUrl(url.clone()),
                    None => Action::None,
                },
                None => Action::None,
            },
            _ => Action::None,
        }
    }
}

pub fn search_hints() -> Hints {
    Hints::new(
        &[
            ("Enter", "open"),
            ("i", "edit query"),
            ("t", "type"),
            ("o", "order"),
            ("[ ]", "page"),
            ("Esc", "back"),
        ],
        0,
    )
}

pub fn search_crumb(s: &super::SearchState) -> String {
    if s.query.trim().is_empty() {
        "Search".into()
    } else {
        format!("Search: {}", truncate(s.query.trim(), 30))
    }
}

/// `1–8 of 312`, derived the same way `ThreadList`'s panel footer is: backed
/// out from the total on the last (possibly partial) page rather than
/// guessed. `None` while the total is unknown (0) or there are no results.
fn search_range(s: &super::SearchState) -> Option<String> {
    if s.total == 0 || s.results.is_empty() {
        return None;
    }
    let len = s.results.len() as u64;
    let (start, end) = if s.page >= s.last_page.max(1) && s.total >= len {
        (s.total - len + 1, s.total)
    } else {
        let start = (s.page.max(1) as u64 - 1) * len + 1;
        (start, start + len - 1)
    };
    Some(format!("{start}\u{2013}{end} of {}", s.total))
}

/// Strip BBCode, collapse whitespace, and cap to `max_chars` — the dim
/// snippet under each result's title line.
pub(crate) fn search_snippet(message: &str, max_chars: usize) -> String {
    // `to_plain` already collapses internal whitespace to single spaces; this
    // just trims the ends and caps the length.
    let plain = common::bbcode::to_plain(message);
    let trimmed = plain.trim();
    if trimmed.chars().count() <= max_chars {
        trimmed.to_string()
    } else {
        let cut: String = trimmed.chars().take(max_chars).collect();
        format!("{cut}\u{2026}")
    }
}

/// Style `text`, highlighting every case-insensitive occurrence of a query
/// word in `theme.warn` + bold; the rest is base + bold (`Search.dc.html`).
fn highlight_query_words(theme: &Theme, text: &str, words: &[String]) -> Vec<Span<'static>> {
    let base_style = theme.base().add_modifier(Modifier::BOLD);
    if words.is_empty() {
        return vec![Span::styled(text.to_string(), base_style)];
    }
    let warn_style = Style::new().fg(theme.warn).add_modifier(Modifier::BOLD);
    // ASCII-only case-folding: guarantees the same byte length as `text`, so
    // byte offsets found in `lower` stay valid slice points into `text` even
    // for non-ASCII titles (full Unicode lowercasing can change length).
    let lower = text.to_ascii_lowercase();
    let mut spans = Vec::new();
    let mut i = 0usize;
    while i < text.len() {
        let mut best: Option<(usize, usize)> = None;
        for w in words {
            if w.is_empty() {
                continue;
            }
            if let Some(pos) = lower[i..].find(w.as_str()) {
                let start = i + pos;
                let better = match best {
                    None => true,
                    Some((bs, _)) => start < bs,
                };
                if better {
                    best = Some((start, w.len()));
                }
            }
        }
        match best {
            Some((start, len)) => {
                if start > i {
                    spans.push(Span::styled(text[i..start].to_string(), base_style));
                }
                spans.push(Span::styled(
                    text[start..start + len].to_string(),
                    warn_style,
                ));
                i = start + len;
            }
            None => {
                spans.push(Span::styled(text[i..].to_string(), base_style));
                break;
            }
        }
    }
    spans
}

/// Hard-clip a span run to `max` cells, keeping each span's style (mirrors
/// `chrome::clip_spans`, which is private to that module).
fn clip_span_run(spans: Vec<Span<'static>>, max: usize) -> Vec<Span<'static>> {
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
            let text: String = s.content.chars().take(room).collect();
            out.push(Span::styled(text, s.style));
        }
        break;
    }
    out
}

/// `/ query` on the left; `author x` / `in all forums` right-aligned. Returns
/// the absolute column where the author's editable text begins, so the
/// caller can place the terminal cursor there.
fn render_query_row(s: &super::SearchState, f: &mut Frame, area: Rect, theme: &Theme) -> u16 {
    let mut spans = vec![Span::styled(
        "/ ",
        Style::new().fg(theme.accent).add_modifier(Modifier::BOLD),
    )];
    if s.query.is_empty() && !s.input_mode {
        spans.push(Span::styled("type to search", theme.dim()));
    } else {
        spans.push(Span::styled(
            s.query.clone(),
            theme.base().add_modifier(Modifier::BOLD),
        ));
    }

    let author_display = if s.author.is_empty() { "any" } else { &s.author };
    let author_label = format!("author {author_display}");
    let forum_label = "in all forums";
    let right_w = author_label.chars().count() + 4 + forum_label.chars().count();

    let w = area.width as usize;
    let left_w: usize = spans.iter().map(Span::width).sum();
    let mut author_x = area.x + area.width;
    if left_w + right_w < w {
        spans.push(Span::raw(" ".repeat(w - left_w - right_w)));
        author_x = area.x + (w - right_w) as u16 + "author ".len() as u16;
        spans.push(Span::styled(author_label, theme.dim()));
        spans.push(Span::raw("    "));
        spans.push(Span::styled(forum_label, theme.dim()));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
    author_x
}

/// Chip row: `All | Threads | Posts` then `Latest | Relevance`, the active
/// chip in accent_bg (`chrome::chip_active`), the mode hint dim on the right.
fn render_chip_row(s: &super::SearchState, f: &mut Frame, area: Rect, theme: &Theme) {
    let mut spans = Vec::new();
    for (i, label) in ["All", "Threads", "Posts"].into_iter().enumerate() {
        spans.push(if s.content_type == i as u8 {
            chrome::chip_active(theme, label)
        } else {
            chrome::chip(theme, label)
        });
        spans.push(Span::raw(" "));
    }
    spans.push(Span::raw("  "));
    for (i, label) in ["Latest", "Relevance"].into_iter().enumerate() {
        spans.push(if s.order == i as u8 {
            chrome::chip_active(theme, label)
        } else {
            chrome::chip(theme, label)
        });
        spans.push(Span::raw(" "));
    }

    let hint = Span::styled("t type \u{b7} o order \u{b7} Tab author", theme.dim());
    let w = area.width as usize;
    let left_w: usize = spans.iter().map(Span::width).sum();
    let right_w = hint.width();
    if left_w + right_w < w {
        spans.push(Span::raw(" ".repeat(w - left_w - right_w)));
        spans.push(hint);
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// One result: a dim kind label + title with query-word hits highlighted,
/// `{author} · {age}` right-aligned against the panel edge (`SearchHit`
/// carries no forum name — see `common/src/models.rs` — so the author
/// substitutes for it), then a dim snippet line.
fn search_hit_lines(
    theme: &Theme,
    hit: &SearchHit,
    words: &[String],
    width: u16,
) -> Vec<Line<'static>> {
    let mut left = vec![Span::styled(format!("{:>8}  ", hit.content_type), theme.dim())];
    left.extend(highlight_query_words(theme, &hit.title, words));

    let mut right_text = String::new();
    if !hit.username.is_empty() {
        right_text.push_str(&hit.username);
    }
    if hit.date > 0 {
        if !right_text.is_empty() {
            right_text.push_str(" \u{b7} ");
        }
        right_text.push_str(&fmt_time(hit.date));
    }
    let right = Span::styled(right_text, theme.dim());

    let w = width as usize;
    let left_w: usize = left.iter().map(Span::width).sum();
    let right_w = right.width();
    let mut spans = if left_w + right_w < w {
        left.push(Span::raw(" ".repeat(w - left_w - right_w)));
        left
    } else {
        clip_span_run(left, w.saturating_sub(right_w))
    };
    spans.push(right);

    let mut lines = vec![Line::from(spans)];
    let snippet = search_snippet(&hit.message, 100);
    if !snippet.is_empty() {
        lines.push(Line::from(Span::styled(
            format!("   {snippet}"),
            theme.dim(),
        )));
    }
    lines
}

pub fn render_search(
    s: &mut super::SearchState,
    f: &mut Frame,
    area: Rect,
    theme: &Theme,
    g: &Glyphs,
) {
    let right_title = format!("page {} of {}", s.page.max(1), s.last_page.max(1));
    let bottom = search_range(s);
    let block = solo_panel(theme, g, "Search", Some(&right_title), bottom.as_deref());
    let inner = block.inner(area);
    f.render_widget(block, area);

    let sections = Layout::vertical([
        Constraint::Length(1), // `/ query` + author/forum
        Constraint::Length(1), // chip row
        Constraint::Length(1), // rule
        Constraint::Length(1), // `N results`
        Constraint::Length(1), // blank
        Constraint::Min(1),    // results
    ])
    .split(inner);

    let author_x = render_query_row(s, f, sections[0], theme);
    render_chip_row(s, f, sections[1], theme);

    let rule = if g.ascii { "-" } else { "\u{2500}" };
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            rule.repeat(sections[2].width as usize),
            theme.faint(),
        ))),
        sections[2],
    );

    let total_display = if s.total > 0 {
        s.total
    } else {
        s.results.len() as u64
    };
    if !s.loading && total_display > 0 {
        f.render_widget(
            Paragraph::new(Span::styled(
                format!("{total_display} results"),
                theme.dim(),
            )),
            sections[3],
        );
    }

    if s.input_mode {
        if s.active_field == 0 {
            let cur_col = 2 + s.query.chars().take(s.query_cursor).count() as u16;
            let cur_x =
                (sections[0].x + cur_col).min(sections[0].x + sections[0].width.saturating_sub(1));
            f.set_cursor_position((cur_x, sections[0].y));
        } else {
            let cur_col = s.author.chars().take(s.author_cursor).count() as u16;
            let cur_x =
                (author_x + cur_col).min(sections[0].x + sections[0].width.saturating_sub(1));
            f.set_cursor_position((cur_x, sections[0].y));
        }
    }

    if s.loading {
        f.render_widget(Paragraph::new(Span::styled("Searching\u{2026}", theme.dim())), sections[5]);
    } else if s.results.is_empty() {
        f.render_widget(
            Paragraph::new(Span::styled(
                "No results (press i to edit query, a for author, Enter to search).".to_string(),
                theme.dim(),
            )),
            sections[5],
        );
    } else {
        let words: Vec<String> = s
            .query
            .split_whitespace()
            .map(|w| w.to_ascii_lowercase())
            .collect();
        let items: Vec<ListItem> = s
            .results
            .iter()
            .map(|hit: &SearchHit| ListItem::new(search_hit_lines(theme, hit, &words, sections[5].width)))
            .collect();
        let mut state =
            ListState::default().with_selected(Some(s.sel.min(s.results.len() - 1)));
        f.render_stateful_widget(
            List::new(items).highlight_style(theme.selected()),
            sections[5],
            &mut state,
        );
    }
}

// ================= profile =================

pub fn profile_key(s: &mut super::ProfileState, key: KeyEvent) -> Action {
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('h') | KeyCode::Left => Action::PopScreen,
        KeyCode::Char('t') => {
            if let Some(u) = &s.user {
                Action::OpenMemberContent {
                    user_id: u.user_id,
                    username: u.username.clone(),
                    content: "thread".into(),
                }
            } else {
                Action::None
            }
        }
        KeyCode::Char('p') => {
            if let Some(u) = &s.user {
                Action::OpenMemberContent {
                    user_id: u.user_id,
                    username: u.username.clone(),
                    content: "post".into(),
                }
            } else {
                Action::None
            }
        }
        KeyCode::Char('c') => {
            if let Some(u) = &s.user {
                Action::StartNewConversation(Some(u.username.clone()))
            } else {
                Action::None
            }
        }
        KeyCode::Char('o') => match s.user.as_ref().and_then(|u| u.view_url.clone()) {
            Some(url) => Action::OpenUrl(url),
            None => Action::None,
        },
        KeyCode::Char('y') => match s.user.as_ref().and_then(|u| u.view_url.clone()) {
            Some(url) => Action::OscCopy(url),
            None => Action::None,
        },
        _ => Action::None,
    }
}

pub fn profile_hints() -> Hints {
    Hints::with_short(
        &[
            ("t", "member threads"),
            ("p", "member posts"),
            ("c", "send DM"),
            ("o", "open web"),
            ("y", "copy link"),
            ("Esc", "back"),
        ],
        &[
            ("t", "threads"),
            ("p", "posts"),
            ("c", "DM"),
            ("o", "web"),
            ("Esc", "back"),
        ],
        0,
    )
}

pub fn render_profile(
    s: &mut super::ProfileState,
    f: &mut ratatui::Frame,
    area: ratatui::layout::Rect,
    theme: &Theme,
    g: &Glyphs,
) {
    let block = solo_panel(theme, g, &format!("Member: {}", s.title), None, None);
    let inner = block.inner(area);
    f.render_widget(block, area);

    if s.loading {
        f.render_widget(Paragraph::new("Loading profile…"), inner);
        return;
    }
    if let Some(err) = &s.error {
        f.render_widget(
            Paragraph::new(format!("Error: {err}")).style(Style::new().fg(theme.error)),
            inner,
        );
        return;
    }
    let Some(user) = &s.user else {
        return;
    };

    let mut user_spans = vec![
        chrome::initials_chip(theme, &user.username),
        Span::raw(" "),
        Span::styled(
            user.username.clone(),
            theme.title().add_modifier(Modifier::BOLD),
        ),
    ];
    if user.is_admin {
        user_spans.push(Span::raw(" "));
        user_spans.push(chrome::chip(theme, "ADMIN"));
    } else if user.is_moderator {
        user_spans.push(Span::raw(" "));
        user_spans.push(chrome::chip(theme, "MOD"));
    } else if user.is_staff {
        user_spans.push(Span::raw(" "));
        user_spans.push(chrome::chip(theme, "STAFF"));
    }
    if !user.custom_title.is_empty() {
        user_spans.push(Span::styled(format!(" — {}", user.custom_title), theme.dim()));
    }

    let rule = if g.ascii { "-" } else { "\u{2500}" };
    let mut lines = vec![
        Line::from(user_spans),
        Line::from(Span::styled(rule.repeat(60), theme.faint())),
        Line::from(vec![
            Span::styled("Messages: ", theme.dim()),
            Span::styled(
                format!("{:<8} ", user.message_count),
                theme.base().add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("{} Reactions: ", g.like), theme.dim()),
            Span::styled(
                format!("{:<8} ", user.reaction_score),
                theme.base().add_modifier(Modifier::BOLD),
            ),
            Span::styled("Trophies: ", theme.dim()),
            Span::styled(
                format!("{:<8}", user.trophy_points),
                theme.base().add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::styled("Registered: ", theme.dim()),
            Span::styled(fmt_time(user.register_date), theme.base()),
            Span::raw("    "),
            Span::styled("Last active: ", theme.dim()),
            Span::styled(
                if user.last_activity > 0 {
                    fmt_time(user.last_activity)
                } else {
                    "Hidden".to_string()
                },
                theme.base(),
            ),
        ]),
    ];

    if !user.location.is_empty() {
        lines.push(Line::from(vec![
            Span::styled("Location: ", theme.dim()),
            Span::styled(user.location.clone(), theme.base()),
        ]));
    }
    if !user.website.is_empty() {
        lines.push(Line::from(vec![
            Span::styled(format!("{} Website: ", g.external), theme.dim()),
            Span::styled(user.website.clone(), link_style(theme)),
        ]));
    }

    if !user.about.is_empty() {
        lines.push(Line::from(Span::raw("")));
        lines.push(Line::from(Span::styled(
            format!("About {}:", user.username),
            Style::new().fg(theme.accent).add_modifier(Modifier::BOLD),
        )));
        for (i, para) in user.about.split('\n').enumerate() {
            if i > 10 {
                lines.push(Line::from(Span::styled("…", theme.dim())));
                break;
            }
            lines.push(Line::from(Span::styled(para.to_string(), theme.base())));
        }
    }

    f.render_widget(Paragraph::new(lines), inner);

}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::glyph::UNICODE;
    use crate::screens::ComposeState;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    /// Render a screen state headless and return its rows as plain strings
    /// (styling dropped), for substring/column assertions.
    fn render_rows(w: u16, h: u16, draw: impl FnOnce(&mut ratatui::Frame, ratatui::layout::Rect)) -> Vec<String> {
        let mut term = Terminal::new(TestBackend::new(w, h)).expect("test terminal");
        term.draw(|f| {
            let area = f.area();
            draw(f, area);
        })
        .expect("draw");
        let buf = term.backend().buffer().clone();
        (0..h)
            .map(|y| (0..w).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>())
            .collect()
    }

    #[test]
    fn new_thread_enter_in_title_advances_to_body() {
        let mut s = ComposeState {
            target: Some(ComposeTarget::NewThread { node_id: 4 }),
            title_field: true,
            title: "Test Title".into(),
            body: String::new(),
            ..Default::default()
        };
        compose_key(&mut s, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(!s.title_field, "Enter in title field should advance to body");
        assert!(s.body.is_empty(), "Enter in title should not push newline to body");
    }

    #[test]
    fn reply_tab_does_not_divert_to_title() {
        let mut s = ComposeState {
            target: Some(ComposeTarget::ThreadReply {
                thread_id: 1,
                thread_title: "Thread".into(),
            }),
            title_field: false,
            title: String::new(),
            body: String::new(),
            ..Default::default()
        };
        compose_key(&mut s, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert!(!s.title_field, "Reply mode should never focus title");
        compose_key(&mut s, KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE));
        assert_eq!(s.body, "    a");
        assert!(s.title.is_empty());
    }

    #[test]
    fn compose_cursor_navigation_and_word_deletion() {
        let mut s = ComposeState {
            target: Some(ComposeTarget::ThreadReply {
                thread_id: 1,
                thread_title: "Thread".into(),
            }),
            title_field: false,
            title: String::new(),
            body: "hello world".into(),
            body_cursor: 11,
            ..Default::default()
        };

        // Delete word back: removes "world"
        compose_key(
            &mut s,
            KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL),
        );
        assert_eq!(s.body, "hello ");
        assert_eq!(s.body_cursor, 6);

        // Move left 2 chars
        compose_key(&mut s, KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        compose_key(&mut s, KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        assert_eq!(s.body_cursor, 4);

        // Insert character inside
        compose_key(&mut s, KeyEvent::new(KeyCode::Char('X'), KeyModifiers::NONE));
        assert_eq!(s.body, "hellXo ");
        assert_eq!(s.body_cursor, 5);

        // Home key
        compose_key(&mut s, KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
        assert_eq!(s.body_cursor, 0);

        // End key
        compose_key(&mut s, KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
        assert_eq!(s.body_cursor, 7);
    }

    #[test]
    fn search_input_editing_and_kill_lines() {
        let mut s = crate::screens::SearchState {
            query: "rust ratatui crossterm".into(),
            query_cursor: 22,
            input_mode: true,
            ..Default::default()
        };

        // Ctrl+W: delete word back
        search_key(
            &mut s,
            KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL),
        );
        assert_eq!(s.query, "rust ratatui ");
        assert_eq!(s.query_cursor, 13);

        // Move home
        search_key(
            &mut s,
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL),
        );
        assert_eq!(s.query_cursor, 0);

        // Move right 4
        for _ in 0..4 {
            search_key(&mut s, KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        }
        assert_eq!(s.query_cursor, 4);

        // Ctrl+K: kill to end
        search_key(
            &mut s,
            KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL),
        );
        assert_eq!(s.query, "rust");
        assert_eq!(s.query_cursor, 4);

        // Tab toggles to author field
        search_key(&mut s, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(s.active_field, 1);

        // Typing in author field
        search_key(&mut s, KeyEvent::new(KeyCode::Char('M'), KeyModifiers::NONE));
        search_key(&mut s, KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE));
        search_key(&mut s, KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE));
        search_key(&mut s, KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE));
        assert_eq!(s.author, "Mike");
        assert_eq!(s.author_cursor, 4);

        // Tab cycles back to query field
        search_key(&mut s, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(s.active_field, 0);

        // Esc exits input mode
        search_key(&mut s, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(!s.input_mode);

        // 't' toggles content type
        let initial_type = s.content_type;
        search_key(&mut s, KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE));
        assert_eq!(s.content_type, (initial_type + 1) % 3);

        // 'o' toggles order
        let initial_order = s.order;
        search_key(&mut s, KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE));
        assert_eq!(s.order, 1 - initial_order);
    }

    #[test]
    fn profile_key_shortcuts() {
        use common::models::User;
        use super::profile_key;
        use crate::screens::ProfileState;

        let user = User {
            user_id: 42,
            username: "SysAdmin".into(),
            view_url: Some("https://windowsforum.com/members/sysadmin.42/".into()),
            ..Default::default()
        };
        let mut s = ProfileState {
            title: "SysAdmin".into(),
            user: Some(user),
            ..Default::default()
        };

        // 't': member threads
        let act = profile_key(&mut s, KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE));
        assert!(matches!(act, Action::OpenMemberContent { user_id, username, content } if user_id == 42 && username == "SysAdmin" && content == "thread"));

        // 'p': member posts
        let act = profile_key(&mut s, KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE));
        assert!(matches!(act, Action::OpenMemberContent { user_id, username, content } if user_id == 42 && username == "SysAdmin" && content == "post"));

        // 'c': direct message
        let act = profile_key(&mut s, KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE));
        assert!(matches!(act, Action::StartNewConversation(Some(name)) if name == "SysAdmin"));

        // 'o': open on web
        let act = profile_key(&mut s, KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE));
        assert!(matches!(act, Action::OpenUrl(url) if url.contains("sysadmin.42")));

        // 'y': copy link
        let act = profile_key(&mut s, KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
        assert!(matches!(act, Action::OscCopy(url) if url.contains("sysadmin.42")));
    }

    // ---------- BBCode tag wrap ----------

    #[test]
    fn insert_bbcode_wrap_places_cursor_between_the_tags() {
        let mut body = String::new();
        let mut cursor = 0usize;
        insert_bbcode_wrap(&mut body, &mut cursor, "B");
        assert_eq!(body, "[B][/B]");
        assert_eq!(cursor, 3, "cursor should land right after the opening tag");

        // Wraps at the cursor position, not the end of the string.
        let mut body = "hello world".to_string();
        let mut cursor = 5;
        insert_bbcode_wrap(&mut body, &mut cursor, "I");
        assert_eq!(body, "hello[I][/I] world");
        assert_eq!(cursor, 5 + "[I]".chars().count());

        // Typing right after continues inside the pair.
        crate::editor::insert_char(&mut body, &mut cursor, 'x');
        assert_eq!(body, "hello[I]x[/I] world");
    }

    // ---------- search snippet ----------

    #[test]
    fn search_snippet_strips_bbcode_and_caps_length() {
        assert_eq!(
            search_snippet("[B]Edge[/B] decides   on\n\nits own schedule.", 100),
            "Edge decides on its own schedule."
        );
        assert_eq!(search_snippet("", 10), "");
        let long = "word ".repeat(40);
        let snip = search_snippet(&long, 20);
        assert_eq!(snip.chars().count(), 21, "{snip}");
        assert!(snip.ends_with('\u{2026}'), "{snip}");
    }

    // ---------- Reply / compose ----------

    fn sample_reply() -> ComposeState {
        ComposeState {
            target: Some(ComposeTarget::ThreadReply {
                thread_id: 1,
                thread_title: "How do I control MS edge update schedule?".into(),
            }),
            body: "Edge can't be told to wait.".into(),
            author: "Mike".into(),
            reply_number: Some(29),
            ..Default::default()
        }
    }

    #[test]
    fn compose_shows_two_panels_wide_and_one_narrow() {
        let theme = Theme::truecolor();
        let mut s = sample_reply();
        let rows = render_rows(120, 36, |f, area| render_compose(&mut s, f, area, &theme, &UNICODE));
        assert!(rows[0].contains(" Reply "), "{}", rows[0]);
        assert!(rows[0].contains(" Preview "), "{}", rows[0]);
        let all = rows.join("\n");
        assert!(all.contains("as Mike"), "{all}");
        assert!(all.contains("post #29"), "{all}");
        assert!(all.contains("bold"), "{all}");
        assert!(all.contains("chars"), "{all}");

        // Narrow: the editor alone (preview defaults to off).
        let mut s = sample_reply();
        let rows = render_rows(80, 24, |f, area| render_compose(&mut s, f, area, &theme, &UNICODE));
        assert!(rows[0].contains(" Reply "), "{}", rows[0]);
        assert!(!rows[0].contains(" Preview "), "{}", rows[0]);

        // Narrow, toggled to preview.
        let mut s = ComposeState {
            preview: true,
            ..sample_reply()
        };
        let rows = render_rows(80, 24, |f, area| render_compose(&mut s, f, area, &theme, &UNICODE));
        assert!(rows[0].contains(" Preview "), "{}", rows[0]);
        assert!(!rows[0].contains(" Reply "), "{}", rows[0]);
        assert!(rows.join("\n").contains("as it will appear"));
    }

    // ---------- Sign in ----------

    #[test]
    fn login_shows_the_mark_box_and_the_full_link_at_both_sizes() {
        let theme = Theme::truecolor();
        for (w, h) in [(120u16, 36u16), (80, 24)] {
            let mut s = crate::screens::LoginState {
                stage: LoginStage::Waiting,
                url: "https://windowsforum.com/tui-start/k7Qx2p".into(),
                ..Default::default()
            };
            let rows = render_rows(w, h, |f, area| render_login(&mut s, f, area, &theme, &UNICODE));
            let all = rows.join("\n");
            assert!(all.contains('\u{2588}'), "no mark glyph at {w}x{h}: {all}");
            assert!(
                all.contains("https://windowsforum.com/tui-start/k7Qx2p"),
                "link not intact at {w}x{h}: {all}"
            );
            assert!(all.contains("waiting for approval"), "{all}");
            assert!(all.contains("copied to your clipboard"), "{all}");
        }
    }

    #[test]
    fn a_pixel_tier_swaps_the_block_mark_for_the_logo_and_lower_tiers_keep_it() {
        let theme = Theme::truecolor();

        // Tiers 1-3: the mark's block glyphs give way to a 16x7 logo rect.
        for tier in [
            crate::images::Tier::Kitty,
            crate::images::Tier::Sixel,
            crate::images::Tier::Iterm2,
        ] {
            let mut s = crate::screens::LoginState {
                stage: LoginStage::Waiting,
                url: "https://windowsforum.com/tui-start/k7Qx2p".into(),
                images: crate::images::Policy { tier, font: (10, 20) },
                ..Default::default()
            };
            let rows =
                render_rows(120, 36, |f, area| render_login(&mut s, f, area, &theme, &UNICODE));
            let all = rows.join("\n");
            assert!(!all.contains('\u{2588}'), "{tier:?} still draws the block mark: {all}");
            // The copy beside it is untouched.
            assert!(all.contains("WindowsForum"), "{all}");
            assert!(all.contains("for your terminal"), "{all}");
            assert!(all.contains("https://windowsforum.com/tui-start/k7Qx2p"), "{all}");

            assert_eq!(s.image_requests.len(), 1, "{tier:?}: {:?}", s.image_requests);
            let req = &s.image_requests[0];
            assert_eq!(req.key, crate::images::LOGO_KEY, "the logo is embedded, not fetched");
            assert_eq!(req.rect.width, crate::images::LOGO_COLS);
            assert_eq!(req.rect.height, crate::images::LOGO_ROWS);
        }

        // Tier 4 (half-blocks) and tier 5 (text) keep the crafted mark.
        for tier in [crate::images::Tier::Halfblocks, crate::images::Tier::Text] {
            let mut s = crate::screens::LoginState {
                stage: LoginStage::Waiting,
                url: "https://windowsforum.com/tui-start/k7Qx2p".into(),
                images: crate::images::Policy { tier, font: (10, 20) },
                ..Default::default()
            };
            let rows =
                render_rows(120, 36, |f, area| render_login(&mut s, f, area, &theme, &UNICODE));
            let all = rows.join("\n");
            assert!(all.contains('\u{2588}'), "{tier:?} lost the block mark: {all}");
            assert!(s.image_requests.is_empty(), "{tier:?} asked for an image");
        }
    }

    // ---------- Search ----------

    #[test]
    fn search_renders_two_hits_with_meta_flush_to_the_panel_edge() {
        let theme = Theme::truecolor();
        let mut s = crate::screens::SearchState {
            query: "edge update".into(),
            results: vec![
                SearchHit {
                    content_type: "thread".into(),
                    title: "How do I control MS edge update schedule?".into(),
                    username: "HItest".into(),
                    date: 1_700_000_000,
                    message: "Edge decides to do them on its own schedule.".into(),
                    ..Default::default()
                },
                SearchHit {
                    content_type: "post".into(),
                    title: "Re: Edge keeps restarting after an update".into(),
                    username: "kemical".into(),
                    date: 1_700_000_000,
                    message: "The update service runs on its own schedule.".into(),
                    ..Default::default()
                },
            ],
            page: 1,
            last_page: 1,
            total: 2,
            ..Default::default()
        };

        let width = 120u16;
        let rows = render_rows(width, 36, |f, area| render_search(&mut s, f, area, &theme, &UNICODE));
        let all = rows.join("\n");
        assert!(all.contains("HItest"), "{all}");
        assert!(all.contains("kemical"), "{all}");
        assert!(all.contains("2 results"), "{all}");

        // First hit's title/meta row: the right-aligned meta column ends
        // flush against the panel's inner edge, no trailing gap before the
        // border (query/chip/rule/count/blank = 5 header rows, so results
        // start at row 6).
        let hit_row: Vec<char> = rows[6].chars().collect();
        let last_content_col = (width - 2) as usize;
        let border_col = (width - 1) as usize;
        assert_ne!(hit_row[last_content_col], ' ', "{}", rows[6]);
        assert_eq!(hit_row[border_col], '\u{2502}', "{}", rows[6]);
    }
}
