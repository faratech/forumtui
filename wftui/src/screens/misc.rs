//! Login, compose, search, profile screens.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;

use std::collections::HashSet;

use common::bbcode;
use common::models::{Attachment, SearchHit};

use super::{
    browse::{chunk_lines, truncate, wrap_spans},
    link_style, solo_panel, Action, ComposeTarget, LoginStage,
};
use crate::chrome::{self, Hints};
use crate::glyph::Glyphs;
use crate::hit::{Hit, HitMap};
use crate::images;
use crate::theme::{fmt_time, Theme};

// ================= login =================

pub fn login_key(s: &mut super::LoginState, key: KeyEvent) -> Action {
    // While a register/poll round-trip is busy every other key is inert —
    // the bar hides them (#658) — but quit must always work: process exit
    // simply aborts the in-flight flow.
    if s.busy {
        return if key.code == KeyCode::Char('q') {
            Action::Quit
        } else {
            Action::None
        };
    }
    match (&s.stage, key.code) {
        // The key bar advertises "restart login" on the whole screen, so Enter
        // has to restart from `Waiting` too: a denied approval, a failed
        // Turnstile/2FA or a closed tab leaves the poll running for the link's
        // full 10-minute TTL with no other way out (issue #547). `begin_login`
        // aborts the superseded flow.
        (_, KeyCode::Enter) => Action::LoginBegin,
        (LoginStage::Waiting, KeyCode::Char('c')) => Action::OscCopy(s.url.clone()),
        (LoginStage::Waiting, KeyCode::Char('o')) => Action::OpenUrl(s.url.clone()),
        // Idle is a real resting state (`end_session` lands here, so does
        // every poll/exchange failure) with no link yet to copy or open —
        // say so instead of silently ignoring the keys the bar just
        // advertised for `Waiting` (issue #605).
        (LoginStage::Idle, KeyCode::Char('c') | KeyCode::Char('o')) => {
            Action::Notice("No link yet \u{2014} press Enter to begin sign-in".into())
        }
        // Advertised on the key bar (issue #561); a login task in flight, if
        // any, is simply aborted by process exit — there is nothing to clean
        // up first.
        (_, KeyCode::Char('q')) => Action::Quit,
        _ => Action::None,
    }
}

pub fn login_hints(s: &super::LoginState) -> Hints {
    // Busy: the flow owns the screen and every key but q is inert —
    // advertise only what works (#658; the never-a-silent-no-op rule).
    if s.busy {
        return Hints::new(&[("q", "quit")], 0);
    }
    match s.stage {
        LoginStage::Idle => Hints::new(&[("Enter", "begin sign-in"), ("q", "quit")], 0),
        LoginStage::Waiting => Hints::new(
            &[
                ("c", "copy link"),
                ("o", "open here"),
                ("Enter", "restart login"),
                ("q", "quit"),
            ],
            0,
        ),
    }
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
                full: false,
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
    // The sticky column belongs to a *run* of vertical moves; any other key
    // (including a Ctrl chord) ends the run.
    if !matches!(
        key.code,
        KeyCode::Up | KeyCode::Down | KeyCode::PageUp | KeyCode::PageDown
    ) {
        s.body_desired_col = None;
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
                crate::editor::move_left(&s.title, &mut s.title_cursor);
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
            // The always-drawn caps row (`caps_line`) advertises these as
            // BBCode wrap actions (`^B` bold, `^I` italic, `^K` code, `^Q`
            // quote, `^U` link) — that is a body-editor affordance the
            // title has no use for, and `^K`/`^U` used to silently alias to
            // kill-to-end/kill-to-start on the TITLE instead, destroying
            // whatever was typed there (issue #605). No-op with a notice
            // rather than either meaning.
            KeyCode::Char('b' | 'i' | 'k' | 'q' | 'u')
                if key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                Action::Notice("BBCode formatting isn't available in the title".into())
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
                crate::editor::move_left(&s.body, &mut s.body_cursor);
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
            // Vertical motion is over *visual* rows — the ones the editor
            // panel actually drew (issue #523). `body_width`/`body_height`
            // are stamped by that draw, which always precedes this key.
            KeyCode::Up => {
                compose_move_vertical(s, -1);
                Action::None
            }
            KeyCode::Down => {
                compose_move_vertical(s, 1);
                Action::None
            }
            KeyCode::PageUp => {
                compose_move_vertical(s, -compose_page(s));
                Action::None
            }
            KeyCode::PageDown => {
                compose_move_vertical(s, compose_page(s));
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

/// One PageUp/PageDown step: a pane's worth of visual rows, less one for
/// context. 1 until the editor has been drawn once.
fn compose_page(s: &super::ComposeState) -> isize {
    (s.body_height.saturating_sub(1)).max(1) as isize
}

fn compose_move_vertical(s: &mut super::ComposeState, delta: isize) {
    let width = if s.body_width == 0 { 1 } else { s.body_width as usize };
    // Through the cache (issue #678): on a pasted log this used to re-wrap
    // every row of the draft per arrow key.
    s.wrap.sync(&s.body, width);
    s.wrap
        .move_vertical(&mut s.body_cursor, &mut s.body_desired_col, delta);
}

pub fn compose_hints(s: &super::ComposeState) -> Hints {
    let is_new_thread = matches!(s.target, Some(ComposeTarget::NewThread { .. }));
    let primary = if is_new_thread { "post thread" } else { "send" };
    let primary_short = if is_new_thread { "post" } else { "send" };
    // Issue #577: `^A attach` was advertised here while Ctrl+A actually
    // moves the caret to the start of the line in both fields (there is no
    // `Action` for attachments — upload is implemented in `common` but not
    // wired into this screen, per CLAUDE.md's "Known gaps"). Never advertise
    // a key and then do something else / refuse silently (the same rule
    // that dropped `N`/`m` from the Latest list). Drop the cap until upload
    // is wired; Ctrl+A stays bound to move-home, just not in this namespace.
    // Issue #605: Tab only switches fields in the new-thread flow (Title <->
    // body) — everywhere else (ThreadReply/ConversationReply) it inserts
    // four spaces, so the cap must say "indent", not "field".
    let tab_desc = if is_new_thread { "field" } else { "indent" };
    Hints::with_short(
        &[
            ("^S", primary),
            ("^O", "preview on/off"),
            ("^Y", "paste"),
            ("Tab", tab_desc),
            ("Esc", "discard"),
        ],
        &[
            ("^S", primary_short),
            ("^O", "preview"),
            ("^Y", ""),
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

/// The new-thread Title field's label — its width is also the caret's offset,
/// measured rather than hand-counted (issue #535).
const COMPOSE_TITLE_LABEL: &str = "Title: ";

/// The visible slice of the Title field and the caret column inside it, for
/// a header area `width` cells wide (issue #606). Both the line and the caret
/// come from here so they can never disagree.
fn compose_title_window(s: &super::ComposeState, width: u16) -> (String, u16) {
    let room = (width as usize).saturating_sub(chrome::cell_width(COMPOSE_TITLE_LABEL));
    chrome::field_window(&s.title, s.title_cursor, room)
}

fn compose_header_lines(s: &super::ComposeState, theme: &Theme, width: u16) -> Vec<Line<'static>> {
    if compose_is_new_thread(&s.target) {
        let (visible, _) = compose_title_window(s, width);
        return vec![
            Line::from(vec![
                Span::styled(COMPOSE_TITLE_LABEL, theme.dim()),
                Span::styled(visible, theme.base()),
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
///
/// `title_focused` (issue #605): these are body-editor chords — with the
/// new-thread Title focused they used to stay drawn and live (`^K`/`^U`
/// silently killed the title's text instead of wrapping BBCode), so the bar
/// must say they don't apply here rather than advertise a body affordance
/// over a field that no longer has one.
/// The editor's BBCode chords, in the order the caps row draws them. One
/// table so the row and its hit boxes cannot drift apart, and `&'static str`
/// so a click can press exactly the cap it names (`Hit::Cap`).
const BBCODE_CAPS: [(&str, &str); 5] = [
    ("^B", "bold"),
    ("^I", "italic"),
    ("^K", "code"),
    ("^Q", "quote"),
    ("^U", "link"),
];

fn caps_line(theme: &Theme, chars: usize, width: u16, title_focused: bool) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = vec![Span::raw(" ")];
    if title_focused {
        spans.push(Span::styled(
            "BBCode formatting is not available in the title",
            theme.dim(),
        ));
    } else {
        for (key, desc) in BBCODE_CAPS {
            spans.push(chrome::keycap(theme, key, false));
            spans.push(Span::styled(format!(" {desc}  "), theme.dim()));
        }
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
#[allow(clippy::too_many_arguments)]
fn draw_editor_panel(
    s: &mut super::ComposeState,
    f: &mut Frame,
    area: Rect,
    theme: &Theme,
    g: &Glyphs,
    focused: bool,
    hits: &mut HitMap,
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

    f.render_widget(
        Paragraph::new(compose_header_lines(s, theme, chunks[0].width)),
        chunks[0],
    );

    let rule = if g.ascii { "-" } else { "\u{2500}" };
    let rule_line = |w: u16| Line::from(Span::styled(rule.repeat(w as usize), theme.faint()));
    f.render_widget(Paragraph::new(rule_line(chunks[1].width)), chunks[1]);

    let is_new_thread = compose_is_new_thread(&s.target);
    // The body is pre-wrapped into visual rows here and drawn with
    // `Paragraph::scroll` and no `Wrap`: the caret is tracked in the same
    // rows, so it never drifts below a wrapped paragraph, and the offset
    // follows it so a draft taller than the pane keeps typing on screen
    // (issues #519/#523).
    let body_area = chunks[2];
    s.body_width = body_area.width;
    s.body_height = body_area.height;
    s.body_rect = body_area;
    // Field 1 is the body; field 0 is the new-thread Title, which only
    // exists on that target (`compose_header_lines`), so it only registers
    // where it is actually drawn.
    hits.push(body_area, Hit::Field(1));
    if is_new_thread {
        s.title_rect = Rect::new(chunks[0].x, chunks[0].y, chunks[0].width, 1);
        hits.push(s.title_rect, Hit::Field(0));
    } else {
        s.title_rect = Rect::default();
    }
    // The wrap is incremental (issue #678) and only the rows the pane shows
    // are turned into `Line`s — building one per row allocated a String for
    // every row of a 70 000-row paste, on every frame.
    s.wrap.sync(&s.body, body_area.width as usize);
    let (caret_row, caret_col) = s.wrap.caret(s.body_cursor);
    let mut tail: Vec<Line<'static>> = Vec::new();
    if let Some(err) = &s.error {
        tail.push(Line::from(Span::styled(
            format!("Error: {err}"),
            Style::new().fg(theme.error),
        )));
    }
    if s.busy {
        tail.push(Line::from(Span::styled("Sending\u{2026}", theme.dim())));
    }
    let body_rows = s.wrap.row_count();
    let total_rows = body_rows + tail.len();
    // Sliced rather than `Paragraph::scroll` (a `u16` offset) — this site
    // takes unlimited-length posts, so a draft really can pass 65,536
    // visual rows (issue #558).
    s.body_scroll = crate::editor::follow_caret(
        s.body_scroll,
        caret_row,
        total_rows,
        body_area.height as usize,
    );
    let window = crate::editor::window_lines(
        &s.wrap,
        &tail,
        s.body_scroll,
        body_area.height,
        theme.base(),
    );
    f.render_widget(Paragraph::new(window), body_area);

    f.render_widget(rule_line(chunks[3].width), chunks[3]);
    let title_focused = is_new_thread && s.title_field;
    f.render_widget(
        caps_line(theme, s.body.chars().count(), chunks[4].width, title_focused),
        chunks[4],
    );
    // The BBCode caps are click targets only when they are live — with the
    // Title focused the row says they do not apply here (issue #605), and a
    // cap you cannot press must not be one you can click either.
    if !title_focused {
        let mut x = chunks[4].x + 1;
        for (key, desc) in BBCODE_CAPS {
            let cap_w = chrome::cell_width(key) as u16 + 2;
            hits.push(Rect::new(x, chunks[4].y, cap_w, 1), Hit::Cap(key));
            x = x
                .saturating_add(cap_w)
                .saturating_add(chrome::cell_width(desc) as u16 + 3);
        }
    }

    if is_new_thread && s.title_field {
        let (_, cur_col) = compose_title_window(s, chunks[0].width);
        let label_w = chrome::cell_width(COMPOSE_TITLE_LABEL) as u16;
        let cur_x = (chunks[0].x + label_w + cur_col)
            .min(chunks[0].x + chunks[0].width.saturating_sub(1));
        f.set_cursor_position((cur_x, chunks[0].y));
    } else {
        let cur_x =
            (body_area.x + caret_col as u16).min(body_area.x + body_area.width.saturating_sub(1));
        // Subtract in `usize` and narrow after: the result is at most one
        // pane height even for a 70,000-row draft (issue #558).
        let screen_row = caret_row.saturating_sub(s.body_scroll).min(u16::MAX as usize) as u16;
        let cur_y = (body_area.y + screen_row).min(body_area.y + body_area.height.saturating_sub(1));
        f.set_cursor_position((cur_x, cur_y));
    }
}

// ---- compose preview: inline images ----

/// One image the draft points at, resolved as far as the compose screen can
/// resolve it: `[IMG]url[/IMG]` is already a URL, `[ATTACH]id[/ATTACH]` needs
/// the draft's attachment list (`ComposeState::attachments`) to become one.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PreviewImage {
    /// What the image store is keyed by: the absolute URL to fetch.
    key: String,
    /// Caption label — the file name when the URL has one, else the host.
    label: String,
    /// Pixel size when it is already known without fetching (an attachment
    /// record carries `width`/`height`; a bare URL carries nothing).
    px: Option<(u32, u32)>,
}

/// The compose preview's derived state.
///
/// The layout is a pure function of the draft, the pane width, the graphics
/// tier and what the image store has learned, so it is rebuilt only when one
/// of those changes — the event loop redraws every 50 ms, and re-parsing the
/// draft on each of those frames would re-derive the same picture twenty
/// times a second. `requested` is the other half: a URL is handed to the
/// store once per session, so editing the text around an image (or deleting
/// the reference and typing it back) never starts a second fetch.
#[derive(Default)]
pub struct PreviewCache {
    built: bool,
    src: String,
    width: u16,
    policy: images::Policy,
    /// The store's `sizes` length the build saw. It only grows, so a change
    /// means a caption can now show its `W×H`.
    sizes_len: usize,
    lines: Vec<Line<'static>>,
    slots: Vec<images::Slot>,
    /// Every image key this compose session has already handed to the store.
    /// The store dedups the *fetch* (in-flight and failure memos, per box);
    /// this is the compose half of the contract — one hand-off per URL per
    /// session, however much the draft is edited around it.
    requested: HashSet<String>,
    /// When `src` (the draft text a build last saw) last actually changed —
    /// `None` until the first edit. Width/tier/learned-size changes don't
    /// touch this, only the text does; see `settled`.
    last_edit: Option<std::time::Instant>,
}

impl PreviewCache {
    /// Editing a URL fires a rebuild (and a fresh, never-seen-before slot
    /// key) on every keystroke; fetching each intermediate string leaks
    /// partial URLs to whatever host they happen to spell and floods the
    /// image store with junk (issue #530). Image fetches wait for the draft
    /// to sit still for this long after the text itself last changed.
    const EDIT_SETTLE: std::time::Duration = std::time::Duration::from_millis(500);

    /// True when nothing the layout depends on has changed since the build.
    fn is_fresh(&self, src: &str, width: u16, policy: images::Policy, sizes_len: usize) -> bool {
        self.built
            && self.width == width
            && self.policy == policy
            && self.sizes_len == sizes_len
            && self.src == src
    }

    /// Restarts the settle timer when `body` differs from the text the last
    /// build saw (call before folding `body` into `src`) — but only from the
    /// *second* build on. The very first build a freshly opened composer (or
    /// a draft resumed with a reference already in it) does is not a live
    /// edit in progress; only a build that *replaces* prior content is.
    /// Without this exemption every composer would wait out the settle
    /// window before showing an image already sitting in the draft when the
    /// screen opened.
    fn note_body(&mut self, body: &str) {
        if self.built && self.src != body {
            self.last_edit = Some(std::time::Instant::now());
        }
    }

    /// False while within `EDIT_SETTLE` of the last text change — callers
    /// must not start a *new* (never-`requested`) image fetch until this is
    /// true. A key already in `requested` is exempt: draw_preview_panel
    /// keeps painting it every frame regardless, at no extra cost.
    fn settled(&self) -> bool {
        self.last_edit.is_none_or(|t| t.elapsed() >= Self::EDIT_SETTLE)
    }
}

/// The caption line for one preview image (DESIGN.md's `▣ name · W×H`).
///
/// `▣ loading…` while an inline tier is still fetching: the size of a bare
/// `[IMG]` URL is not knowable until the bytes arrive, and a caption that
/// claimed a name and no size would look like a finished render of nothing.
/// On the text tier (and `--no-default-features`) nothing will ever be
/// fetched, so the label is shown straight away — the caption is all there is.
fn preview_caption(g: &Glyphs, img: &PreviewImage, px: Option<(u32, u32)>, inline: bool) -> String {
    match px {
        Some((w, h)) => format!("{} {} \u{00b7} {w}\u{00d7}{h}", g.image, img.label),
        None if inline => format!("{} loading\u{2026}", g.image),
        None => format!("{} {}", g.image, img.label),
    }
}

/// File name from a URL, or its host when the path has none ("…/photo.png" →
/// `photo.png`, "https://imgur.com/a/xyz/" → `imgur.com`).
fn url_label(url: &str) -> String {
    let after_scheme = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let path = after_scheme.split(['?', '#']).next().unwrap_or("");
    let host = path.split('/').next().unwrap_or("");
    let last = path.rsplit('/').next().unwrap_or("");
    if last != host && last.contains('.') {
        last.to_string()
    } else if host.is_empty() {
        url.to_string()
    } else {
        host.to_string()
    }
}

/// Resolve one parsed image reference against the draft's attachment list.
/// `None` means "not an image this client can show" — an `[ATTACH]` id the
/// draft does not carry, or an attachment that is not an image — and the
/// reference then renders as the plain text placeholder it always did.
fn resolve_image(r: &bbcode::ImageRef, attachments: &[Attachment]) -> Option<PreviewImage> {
    match r {
        bbcode::ImageRef::Url(url) => Some(PreviewImage {
            key: url.clone(),
            label: url_label(url),
            px: None,
        }),
        bbcode::ImageRef::Attachment(id) => {
            let att = attachments.iter().find(|a| a.attachment_id == *id)?;
            let url = images::attachment_url(att)?;
            let px = match (att.width, att.height) {
                (Some(w), Some(h)) if w > 0 && h > 0 => Some((w, h)),
                _ => None,
            };
            Some(PreviewImage {
                key: url.to_string(),
                label: att.filename.clone(),
                px,
            })
        }
    }
}

/// Lay the draft out for the preview pane: the same BBCode rendering the
/// thread view uses, with every resolvable image reference lifted out into a
/// caption line plus the blank rows the app paints the picture over.
///
/// Reserving the rows here (rather than drawing over whatever follows) is
/// what keeps the text after an image from ending up underneath it — the same
/// contract `ThreadViewState::rebuild_lines` keeps for post attachments.
fn build_preview(
    body: &str,
    attachments: &[Attachment],
    theme: &Theme,
    g: &Glyphs,
    width: u16,
    policy: images::Policy,
    sizes: &images::Sizes,
) -> (Vec<Line<'static>>, Vec<images::Slot>) {
    let w = width.max(1);
    let chunks = bbcode::render(body);
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut slots: Vec<images::Slot> = Vec::new();
    let mut links: Vec<String> = Vec::new();
    let mut run_start = 0usize;

    for (i, chunk) in chunks.iter().enumerate() {
        let Some(img) = chunk.image_ref().and_then(|r| resolve_image(&r, attachments)) else {
            continue;
        };
        push_wrapped(&mut lines, &chunks[run_start..i], &mut links, theme, w);
        run_start = i + 1;

        let px = img.px.or_else(|| sizes.get(&img.key).copied());
        lines.push(Line::from(Span::styled(
            truncate(&preview_caption(g, &img, px, policy.inline()), w as usize),
            theme.dim(),
        )));
        if !policy.inline() {
            continue;
        }
        // Until the bytes arrive the aspect is a guess, exactly as it is for
        // an attachment the API sent no dimensions for; the box is re-fitted
        // (and the payload re-encoded for it) on the frame after the load.
        let (cols, rows) = images::fit(w, px.unwrap_or((16, 9)), policy.font);
        slots.push(images::Slot {
            line: lines.len(),
            x: 0,
            cols: cols.min(w).max(1),
            rows,
            key: img.key.clone(),
        });
        for _ in 0..rows {
            lines.push(Line::from(Span::raw("")));
        }
    }
    push_wrapped(&mut lines, &chunks[run_start..], &mut links, theme, w);
    (lines, slots)
}

/// Render one run of chunks into the preview's line list, pre-wrapped.
///
/// Pre-wrapped rather than left to `Paragraph`'s own `Wrap`: a widget-level
/// wrap would fold a long line into rows the slot arithmetic above knows
/// nothing about, and the images would then land on top of the text.
fn push_wrapped(
    out: &mut Vec<Line<'static>>,
    chunks: &[bbcode::Chunk],
    links: &mut Vec<String>,
    theme: &Theme,
    width: u16,
) {
    // The preview shows the post's DEFAULT render: spoilers hidden, as
    // every reader will first see them (#621).
    for logical in chunk_lines(chunks, links, theme, false) {
        for wrapped in wrap_spans(&logical, width as usize) {
            out.push(Line::from(wrapped));
        }
    }
}

/// The `Preview` panel: renders the draft through the same
/// `common::bbcode::render` chunk-to-line logic the thread view uses, so what
/// a member sees here is what the post will render as, not the raw BBCode
/// source — and, on a graphics tier, with the images the draft references
/// drawn under their captions once they resolve.
fn draw_preview_panel(
    s: &mut super::ComposeState,
    f: &mut Frame,
    area: Rect,
    theme: &Theme,
    g: &Glyphs,
) {
    let block = chrome::panel(theme, g, "Preview", false, None, None);
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let chunks =
        Layout::vertical([Constraint::Length(1), Constraint::Length(1), Constraint::Min(1)])
            .split(inner);
    f.render_widget(
        Paragraph::new(Line::from(Span::styled("as it will appear", theme.dim()))),
        chunks[0],
    );
    let body = chunks[2];

    if !s
        .preview_cache
        .is_fresh(&s.body, body.width, s.images, s.image_sizes.len())
    {
        // Restart the settle timer before folding the new text into `src`
        // below — this is the only place that changes `src`, so it is the
        // only place that can tell an actual edit from a resize/tier/size
        // change.
        s.preview_cache.note_body(&s.body);
        let (lines, slots) = build_preview(
            &s.body,
            &s.attachments,
            theme,
            g,
            body.width,
            s.images,
            &s.image_sizes,
        );
        s.preview_cache.built = true;
        s.preview_cache.src = s.body.clone();
        s.preview_cache.width = body.width;
        s.preview_cache.policy = s.images;
        s.preview_cache.sizes_len = s.image_sizes.len();
        s.preview_cache.lines = lines;
        s.preview_cache.slots = slots;
    }

    // Already wrapped to `body.width` by `build_preview` — no widget-level
    // `Wrap`, or the reserved image rows would stop lining up.
    f.render_widget(Paragraph::new(s.preview_cache.lines.clone()), body);

    // Translate the reserved slots into absolute screen rects for the app to
    // paint. A slot that does not fit whole is dropped and its caption left
    // in place: kitty and sixel paint pixels, not cells, so half an image
    // would spill over the panel border.
    if !s.images.inline() {
        return;
    }
    // Debounced, but only for a key the store has never seen: a URL that is
    // already `requested` keeps painting every frame with no extra delay
    // (it is a free cache hit, and the whole point of "one hand-off per
    // session" is that retyping it must not make the picture disappear and
    // reload). A brand-new key waits for the draft to sit still first — the
    // fix for issue #530: without this, every keystroke while typing a URL
    // is itself a new, never-seen key and would fetch immediately.
    let mut newly_requested = 0usize;
    for slot in &s.preview_cache.slots {
        let y = body.y as usize + slot.line;
        if y + slot.rows as usize > body.bottom() as usize {
            continue;
        }
        let x = body.x as usize + slot.x as usize;
        if x + slot.cols as usize > body.right() as usize {
            continue;
        }
        let known = s.preview_cache.requested.contains(&slot.key);
        if !known {
            if !s.preview_cache.settled() {
                continue;
            }
            s.preview_cache.requested.insert(slot.key.clone());
            newly_requested += 1;
        }
        s.image_requests.push(images::Request {
            key: slot.key.clone(),
            rect: Rect::new(x as u16, y as u16, slot.cols, slot.rows),
                full: false,
        });
    }
    if newly_requested > 0 {
        tracing::debug!("compose preview: {newly_requested} image(s) to load");
    }
}

pub fn render_compose(
    s: &mut super::ComposeState,
    f: &mut Frame,
    area: Rect,
    theme: &Theme,
    g: &Glyphs,
    hits: &mut HitMap,
) {
    // Cleared before every frame: a stale rect would have the app paint an
    // image over the editor (narrow layout) or over a shrunk panel.
    s.image_requests.clear();
    // Same for the field rects a click resolves against: with the preview
    // showing, the editor is not on screen and must answer for no point.
    s.body_rect = Rect::default();
    s.title_rect = Rect::default();
    if area.width >= 110 {
        let cols = Layout::horizontal([
            Constraint::Length(72),
            Constraint::Length(48),
            Constraint::Min(0),
        ])
        .split(area);
        draw_editor_panel(s, f, cols[0], theme, g, true, hits);
        draw_preview_panel(s, f, cols[1], theme, g);
    } else if s.preview {
        draw_preview_panel(s, f, area, theme, g);
    } else {
        draw_editor_panel(s, f, area, theme, g, true, hits);
    }
}

/// Focus field `field` (0 = the new-thread Title, 1 = the body) and put the
/// caret under the pointer, through the same visual-row model the renderer
/// drew it with (`Hit::Field`).
pub(crate) fn compose_click_field(
    s: &mut super::ComposeState,
    field: usize,
    col: u16,
    row: u16,
) {
    if s.busy {
        return;
    }
    if field == 0 && compose_is_new_thread(&s.target) {
        s.title_field = true;
        let label = chrome::cell_width(COMPOSE_TITLE_LABEL);
        let room = (s.title_rect.width as usize).saturating_sub(label);
        let x = col
            .saturating_sub(s.title_rect.x)
            .saturating_sub(label as u16);
        s.title_cursor =
            crate::editor::field_caret_at(&s.title, s.title_cursor, room, x as usize);
        return;
    }
    s.title_field = false;
    let line = s.body_scroll + row.saturating_sub(s.body_rect.y) as usize;
    let x = col.saturating_sub(s.body_rect.x) as usize;
    s.wrap.sync(&s.body, s.body_width as usize);
    s.body_cursor = s.wrap.caret_at_cell(line, x);
    // Any non-vertical move clears the sticky Up/Down column (issue #523).
    s.body_desired_col = None;
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
                    // "Enter search" is advertised while typing; an empty
                    // submit refuses out loud instead of doing nothing (#658).
                    return Action::Notice("Type something to search.".into());
                }
                if s.loading {
                    return Action::Notice("Already searching — one moment.".into());
                }
                s.input_mode = false;
                s.loading = true;
                // A typed query is a real keyword search: this screen stops
                // being a member's content list (issue #548).
                s.member = None;
                let ct = content_type_param(s.content_type).map(str::to_string);
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
                let (txt, cur) = if s.active_field == 0 {
                    (&mut s.query, &mut s.query_cursor)
                } else {
                    (&mut s.author, &mut s.author_cursor)
                };
                crate::editor::move_left(txt, cur);
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
                // `query` is a display label in member mode, not a search
                // term — start from an empty field so Enter cannot submit it
                // as one (issue #548).
                if s.member.is_some() {
                    s.query.clear();
                }
                s.input_mode = true;
                s.active_field = 0;
                s.query_cursor = s.query.chars().count();
                Action::None
            }
            KeyCode::Char('a') => {
                // The author field is a keyword-search filter; a member's own
                // threads/posts are already one member's (issue #548).
                if s.member.is_some() {
                    return Action::Notice(
                        "This list is already one member's — press i to search instead.".into(),
                    );
                }
                s.input_mode = true;
                s.active_field = 1;
                s.author_cursor = s.author.chars().count();
                Action::None
            }
            KeyCode::Char('t') => {
                if s.loading {
                    return Action::Notice("Already searching — one moment.".into());
                }
                // In member mode `t` flips between the member's threads and
                // their posts, through `search_member` (issue #548).
                if let Some((user_id, content)) = &s.member {
                    let user_id = *user_id;
                    let content = if content == "thread" { "post" } else { "thread" };
                    s.member = Some((user_id, content.to_string()));
                    s.content_type = if content == "thread" { 1 } else { 2 };
                    s.query = member_label(&s.query, content);
                    s.loading = true;
                    return Action::LoadMemberContent {
                        user_id,
                        content: content.to_string(),
                        page: 1,
                    };
                }
                s.content_type = (s.content_type + 1) % 5;
                let q = s.query.trim().to_string();
                let a = s.author.trim().to_string();
                if !q.is_empty() || !a.is_empty() {
                    s.loading = true;
                    let ct = content_type_param(s.content_type).map(str::to_string);
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
                // `/search/member` has no order parameter — re-running the
                // label as a keyword search is what #548 was.
                if s.member.is_some() {
                    return Action::Notice(
                        "Newest first is the only order for a member's content.".into(),
                    );
                }
                s.order = (s.order + 1) % 2;
                let q = s.query.trim().to_string();
                let a = s.author.trim().to_string();
                if !q.is_empty() || !a.is_empty() {
                    s.loading = true;
                    let ct = content_type_param(s.content_type).map(str::to_string);
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
                if s.loading {
                    return Action::Notice("Already searching — one moment.".into());
                }
                if s.page > 1 {
                    s.loading = true;
                    if let Some((user_id, content)) = &s.member {
                        return Action::LoadMemberContent {
                            user_id: *user_id,
                            content: content.clone(),
                            page: s.page - 1,
                        };
                    }
                    let ct = content_type_param(s.content_type).map(str::to_string);
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
                if s.loading {
                    return Action::Notice("Already searching — one moment.".into());
                }
                if s.page < s.last_page {
                    s.loading = true;
                    if let Some((user_id, content)) = &s.member {
                        return Action::LoadMemberContent {
                            user_id: *user_id,
                            content: content.clone(),
                            page: s.page + 1,
                        };
                    }
                    let ct = content_type_param(s.content_type).map(str::to_string);
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

/// The member-content screen's display label with its content word swapped:
/// `by: kemical (thread)` -> `by: kemical (post)`. Falls back to rebuilding
/// nothing when the label is not in that shape (issue #548).
fn member_label(current: &str, content: &str) -> String {
    match current.rfind(" (") {
        Some(at) if current.ends_with(')') => format!("{} ({content})", &current[..at]),
        _ => current.to_string(),
    }
}

pub fn search_hints(s: &super::SearchState) -> Hints {
    // While a field owns the keyboard the browse-mode caps type text —
    // advertising them there was a lie (#658). Only the two keys that work
    // in the editor are shown.
    if s.input_mode {
        return Hints::new(&[("Enter", "search"), ("Esc", "done")], 0);
    }
    // Member content has no order and no author filter (issue #548), so the
    // bar must not advertise them there — `t` still flips threads/posts.
    // "Enter open" and "[ ] page" are only advertised when they can act:
    // with no results, or nothing to page through, they were inert (#658,
    // the #604 rule).
    let openable = !s.results.is_empty();
    let paged = s.last_page > 1;
    if s.member.is_some() {
        let mut keys: Vec<(&str, &str)> = Vec::new();
        if openable {
            keys.push(("Enter", "open"));
        }
        keys.push(("t", "threads/posts"));
        if paged {
            keys.push(("[ ]", "page"));
        }
        keys.push(("i", "new search"));
        keys.push(("Esc", "back"));
        Hints::new(&keys, 0)
    } else {
        let mut keys: Vec<(&str, &str)> = Vec::new();
        if openable {
            keys.push(("Enter", "open"));
        }
        keys.push(("i", "edit query"));
        keys.push(("t", "type"));
        keys.push(("o", "order"));
        if paged {
            keys.push(("[ ]", "page"));
        }
        keys.push(("Esc", "back"));
        Hints::new(&keys, 0)
    }
}

/// The `search_type` the wire API expects for each chip position
/// (#673): 0 = all types, 1 = threads, 2 = posts, 3 = Media Gallery
/// items (`xfmg_media`), 4 = Resource Manager entries (`resource`).
pub(crate) fn content_type_param(ct: u8) -> Option<&'static str> {
    match ct {
        1 => Some("thread"),
        2 => Some("post"),
        3 => Some("xfmg_media"),
        4 => Some("resource"),
        _ => None,
    }
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
            let text = chrome::take_cells(&s.content, room);
            out.push(Span::styled(text, s.style));
        }
        break;
    }
    out
}

/// `/ query` on the left; `author x` / `in all forums` right-aligned. Returns
/// the absolute column the caret belongs on for the field being edited, so
/// the caller can place it without re-deriving the layout.
///
/// Both fields have a horizontal viewport (issue #606): the query used to be
/// pushed onto the row whole, which clipped it at the pane edge with the caret
/// stuck on the border, and — worse — the right-hand labels were only drawn
/// when they still fitted *after* the whole query, so typing a long query made
/// the author field the user was about to Tab into disappear. The right side
/// is now reserved first, and the query scrolls inside what is left.
/// Draws the `/ query … author … in all forums` row and returns the caret's
/// column. It also stamps the query and author fields' rects on the state:
/// which of them a click lands in, and where in the text, is decided by the
/// same overflow ladder that decided what to draw, so the two can never
/// disagree (the `author` segment simply is not there on the narrow rungs).
fn render_query_row(s: &mut super::SearchState, f: &mut Frame, area: Rect, theme: &Theme) -> u16 {
    const FORUM_LABEL: &str = "in all forums";
    /// Gap between the author and forum labels.
    const GAP: usize = 4;
    /// Cells the query keeps even when the right-hand labels want the room.
    const MIN_QUERY: usize = 10;

    let w = area.width as usize;
    let prompt_w = chrome::cell_width(PROMPT);
    let author_focused = s.input_mode && s.active_field == 1;

    let (author_text, author_caret) = if s.author.is_empty() {
        ("any".to_string(), 0)
    } else {
        chrome::field_window(&s.author, s.author_cursor, AUTHOR_VIEW)
    };
    let author_label = format!("{AUTHOR_LABEL}{author_text}");
    let author_w = chrome::cell_width(&author_label);
    let with_forum = author_w + GAP + chrome::cell_width(FORUM_LABEL);

    // Ladder: everything → drop the forum label → drop the right side, unless
    // the author field is the one being edited, in which case it stays.
    // Each rung leaves the query at least `MIN_QUERY` cells plus the one-cell
    // gap the layout below inserts before the right side.
    let (right_w, show_forum) = if prompt_w + MIN_QUERY + with_forum < w {
        (with_forum, true)
    } else if prompt_w + MIN_QUERY + author_w < w || (author_focused && prompt_w + author_w < w) {
        (author_w, false)
    } else {
        (0, false)
    };
    let query_room = w.saturating_sub(prompt_w + right_w + usize::from(right_w > 0));

    let mut spans = vec![Span::styled(
        PROMPT,
        Style::new().fg(theme.accent).add_modifier(Modifier::BOLD),
    )];
    let mut used = prompt_w;
    let mut query_caret = 0u16;
    if s.query.is_empty() && !s.input_mode {
        let hint = chrome::take_cells("type to search", query_room);
        used += chrome::cell_width(&hint);
        spans.push(Span::styled(hint, theme.dim()));
    } else {
        let (visible, caret) = chrome::field_window(&s.query, s.query_cursor, query_room);
        used += chrome::cell_width(&visible);
        query_caret = caret;
        spans.push(Span::styled(visible, theme.base().add_modifier(Modifier::BOLD)));
    }

    s.query_rect = Rect::new(area.x, area.y, (prompt_w + query_room).min(w) as u16, 1);
    s.author_rect = Rect::default();
    let mut author_x = area.x + area.width;
    if right_w > 0 {
        let pad = w.saturating_sub(used + right_w);
        spans.push(Span::raw(" ".repeat(pad)));
        author_x = area.x + (used + pad + chrome::cell_width(AUTHOR_LABEL)) as u16;
        s.author_rect = Rect::new(
            area.x + (used + pad) as u16,
            area.y,
            author_w as u16,
            1,
        );
        spans.push(Span::styled(author_label, theme.dim()));
        if show_forum {
            spans.push(Span::raw(" ".repeat(GAP)));
            spans.push(Span::styled(FORUM_LABEL, theme.dim()));
        }
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);

    let caret = if author_focused {
        author_x + author_caret
    } else {
        area.x + prompt_w as u16 + query_caret
    };
    caret.min(area.x + area.width.saturating_sub(1))
}

/// The `/ ` prompt, the `author ` label, and the widest the author's own
/// text is ever drawn (it scrolls horizontally inside that). Module-level
/// because a click has to measure its caret against exactly the window the
/// row was drawn with (`search_click_field`).
const PROMPT: &str = "/ ";
const AUTHOR_LABEL: &str = "author ";
const AUTHOR_VIEW: usize = 24;

/// The chip row's dim right-hand hint, derived from state (issue #605): the
/// constant `"t type · o order · Tab author"` used to show regardless of
/// mode — `Tab` only switches fields once already typing (there is no Tab
/// arm in the browse-mode key match at all; `a` is the real way in from
/// there), and member-content mode (issue #548) has neither an order toggle
/// nor an author filter to reach.
fn search_chip_hint(s: &super::SearchState) -> &'static str {
    if s.input_mode {
        "Tab switch field \u{b7} Esc done"
    } else if s.member.is_some() {
        "t threads/posts"
    } else {
        "t type \u{b7} o order \u{b7} a author"
    }
}

/// Chip row: `All | Threads | Posts` then `Latest | Relevance`, the active
/// chip in accent_bg (`chrome::chip_active`), the mode hint dim on the right.
fn render_chip_row(s: &super::SearchState, f: &mut Frame, area: Rect, theme: &Theme) {
    let mut spans = Vec::new();
    for (i, label) in ["All", "Threads", "Posts", "Media", "Resources"].into_iter().enumerate() {
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

    let hint = Span::styled(search_chip_hint(s), theme.dim());
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
    snippet: &str,
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
    if !snippet.is_empty() {
        lines.push(Line::from(Span::styled(
            format!("   {snippet}"),
            theme.dim(),
        )));
    }
    lines
}

/// Focus the query (0) or author (1) field and put the caret under the
/// pointer. Clicking a field is also the way *into* edit mode — the same
/// thing `i`/`a` do from the browse mode (`Hit::Field`).
pub(crate) fn search_click_field(s: &mut super::SearchState, field: usize, col: u16) {
    s.input_mode = true;
    s.active_field = if field == 1 { 1 } else { 0 };
    if field == 1 {
        let label = chrome::cell_width(AUTHOR_LABEL);
        let x = col
            .saturating_sub(s.author_rect.x)
            .saturating_sub(label as u16) as usize;
        s.author_cursor =
            crate::editor::field_caret_at(&s.author, s.author_cursor, AUTHOR_VIEW, x);
    } else {
        let prompt = chrome::cell_width(PROMPT);
        let room = (s.query_rect.width as usize).saturating_sub(prompt);
        let x = col.saturating_sub(s.query_rect.x).saturating_sub(prompt as u16) as usize;
        s.query_cursor = crate::editor::field_caret_at(&s.query, s.query_cursor, room, x);
    }
}

pub fn render_search(
    s: &mut super::SearchState,
    f: &mut Frame,
    area: Rect,
    theme: &Theme,
    g: &Glyphs,
    hits: &mut HitMap,
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

    let caret_x = render_query_row(s, f, sections[0], theme);
    hits.push(s.query_rect, Hit::Field(0));
    hits.push(s.author_rect, Hit::Field(1));
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
        // `render_query_row` already derived the caret from the same window it
        // drew, so the two can never disagree (issue #606).
        f.set_cursor_position((caret_x, sections[0].y));
    }

    if s.loading {
        f.render_widget(Paragraph::new(Span::styled("Searching\u{2026}", theme.dim())), sections[5]);
    } else if let Some(err) = &s.error {
        f.render_widget(
            Paragraph::new(Span::styled(format!("Error: {err}"), Style::new().fg(theme.error))),
            sections[5],
        );
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
        // Snippets come from the cache `set_results` filled; a mismatched
        // length can only mean someone assigned `results` directly, so
        // re-derive once rather than per frame (issue #522).
        if s.snippets.len() != s.results.len() {
            let hits = std::mem::take(&mut s.results);
            s.set_results(hits);
        }
        let items: Vec<ListItem> = s
            .results
            .iter()
            .enumerate()
            .map(|(i, hit): (usize, &SearchHit)| {
                let snippet = s.snippets.get(i).map(String::as_str).unwrap_or("");
                ListItem::new(search_hit_lines(theme, hit, snippet, &words, sections[5].width))
            })
            .collect();
        let mut state =
            ListState::default().with_selected(Some(s.sel.min(s.results.len() - 1)));
        // A hit is one line, or two once it has a snippet — the same shape
        // `search_hit_lines` just built, so a click lands on the row the eye
        // is on.
        let heights: Vec<u16> = s
            .snippets
            .iter()
            .map(|sn| if sn.is_empty() { 1 } else { 2 })
            .collect();
        f.render_stateful_widget(
            List::new(items).highlight_style(theme.selected()),
            sections[5],
            &mut state,
        );
        hits.list(sections[5], state.offset(), &heights, Hit::Row);
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
        KeyCode::Char('d') => {
            // `d`, not `c`: the global `c` (open Inbox) ran first, so the
            // advertised "send DM" key could never fire (#613).
            if let Some(u) = &s.user {
                Action::StartNewConversation(Some(u.username.clone()))
            } else {
                Action::Notice("No member loaded yet.".into())
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
            ("d", "send DM"),
            ("o", "open web"),
            ("y", "copy link"),
            ("Esc", "back"),
        ],
        &[
            ("t", "threads"),
            ("p", "posts"),
            ("d", "DM"),
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

    /// Render a compose screen headless, returning its rows *and* where the
    /// terminal cursor was left — the caret is the whole point of the editor
    /// fixes, and it is invisible in the cell dump.
    fn render_compose_probe(
        s: &mut ComposeState,
        w: u16,
        h: u16,
    ) -> (Vec<String>, (u16, u16)) {
        let theme = Theme::truecolor();
        let mut term = Terminal::new(TestBackend::new(w, h)).expect("test terminal");
        term.draw(|f| {
            let area = f.area();
            render_compose(s, f, area, &theme, &UNICODE, &mut crate::hit::HitMap::default());
        })
        .expect("draw");
        let pos = term.get_cursor_position().expect("cursor");
        let buf = term.backend().buffer().clone();
        let rows = (0..h)
            .map(|y| (0..w).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>())
            .collect();
        (rows, (pos.x, pos.y))
    }

    fn reply_state(body: &str) -> ComposeState {
        let mut s = ComposeState {
            target: Some(ComposeTarget::ThreadReply {
                thread_id: 1,
                thread_title: "Thread".into(),
            }),
            body: body.to_string(),
            ..Default::default()
        };
        s.body_cursor = s.body.chars().count();
        s
    }

    /// Issue #519: a draft taller than the pane used to be typed blind (no
    /// scroll offset) and the caret was placed from unwrapped char counts,
    /// so it drifted a row for every wrapped paragraph above it.
    #[test]
    fn compose_editor_scrolls_to_the_caret_and_wraps_before_placing_it() {
        // 40 short lines in an 18-row body: the tail must be on screen and
        // the caret must sit at the end of the last one.
        let body: String = (0..40).map(|i| format!("line {i:02}\n")).collect();
        let mut s = reply_state(body.trim_end_matches('\n'));
        let (rows, (cx, cy)) = render_compose_probe(&mut s, 80, 24);
        let screen = rows.join("\n");
        assert!(screen.contains("line 39"), "the caret's own line is off screen:\n{screen}");
        assert!(!screen.contains("line 00"), "the pane did not scroll:\n{screen}");
        let caret_row = rows
            .iter()
            .position(|r| r.contains("line 39"))
            .expect("last line on screen") as u16;
        // x = panel border (1) + the 7 cells of "line 39".
        assert_eq!((cx, cy), (1 + 7, caret_row), "caret is not at the end of the last line");

        // Same probe, past the u16 row ceiling — see the #558 test below.

        // One long paragraph: the caret belongs on the *wrapped* row, not on
        // row 0 with a column of 200.
        let para = "w".repeat(200);
        let mut s = reply_state(&para);
        let (rows, (cx, cy)) = render_compose_probe(&mut s, 80, 24);
        let body_top = rows
            .iter()
            .position(|r| r.trim_start_matches(['\u{2502}', ' ']).starts_with('w'))
            .expect("body on screen") as u16;
        // 200 cells over a 78-cell body = rows 0,1,2 with 44 cells on the last.
        assert_eq!(cy, body_top + 2, "caret is not on the third wrapped row");
        assert_eq!(cx, 1 + 44, "caret column ignores the wrap");
    }

    /// Issue #558: `messageMaxLength` is 0 on this site, so pasting a
    /// 70,000-line CBS.log into a reply is something a member really does.
    /// With the old `u16` row model the editor scrolled to row 70_000 mod
    /// 65_536 and drew the caret in that wrong window; typing was blind.
    /// The window on screen must be the caret's own.
    #[test]
    fn compose_editor_follows_the_caret_past_the_u16_row_ceiling() {
        let body: String = (0..70_001).map(|i| format!("line {i:05}\n")).collect();
        let mut s = reply_state(body.trim_end_matches('\n'));
        let (rows, (cx, cy)) = render_compose_probe(&mut s, 80, 24);
        let screen = rows.join("\n");
        assert!(
            screen.contains("line 70000"),
            "the caret's own line is off screen — the offset wrapped:\n{screen}"
        );
        assert!(
            !screen.contains("line 04464") && !screen.contains("line 00000"),
            "the pane is showing the wrapped-offset window:\n{screen}"
        );
        let caret_row = rows
            .iter()
            .position(|r| r.contains("line 70000"))
            .expect("last line on screen") as u16;
        // x = panel border (1) + the 10 cells of "line 70000".
        assert_eq!((cx, cy), (1 + 10, caret_row), "caret is not at the end of the last line");
    }

    /// Issue #523: Up/Down did not exist in the body branch at all, and a run
    /// of them must keep aiming at the column it started from.
    #[test]
    fn compose_up_down_move_by_visual_row_with_a_sticky_column() {
        let mut s = reply_state("aaaaaaaa\nbb\ncccccccc");
        render_compose_probe(&mut s, 80, 24); // stamps body_width/height
        assert_eq!(s.body_cursor, 20, "cursor starts at the end");

        compose_key(&mut s, KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(s.body_cursor, 11, "Up lands on the short line, clamped to its end");
        compose_key(&mut s, KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(s.body_cursor, 8, "the sticky column survives the short line");
        compose_key(&mut s, KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        compose_key(&mut s, KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(s.body_cursor, 20, "Down returns to column 8 of the last line");

        // Any other key ends the run: the column is re-read from the caret.
        compose_key(&mut s, KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
        assert_eq!(s.body_desired_col, None);
        compose_key(&mut s, KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(s.body_cursor, 9, "Up from column 0 stays at column 0");

        // PageUp/PageDown step by a pane of visual rows.
        let body: String = (0..60).map(|i| format!("line {i:02}\n")).collect();
        let mut s = reply_state(body.trim_end_matches('\n'));
        render_compose_probe(&mut s, 80, 24);
        let page = (s.body_height - 1) as usize;
        compose_key(&mut s, KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE));
        let (row, _) = crate::editor::caret_position(&s.body, s.body_width as usize, s.body_cursor);
        assert_eq!(row, 59 - page, "PageUp moved {} rows", 59 - row);
    }

    /// A wrapped body is scrolled by whole visual rows, and Up/Down inside a
    /// single long paragraph moves between those rows rather than doing
    /// nothing (both editors share `editor::visual_rows_of`).
    #[test]
    fn compose_vertical_motion_works_inside_one_wrapped_paragraph() {
        let mut s = reply_state(&"z".repeat(200));
        render_compose_probe(&mut s, 80, 24);
        let w = s.body_width as usize;
        assert_eq!(w, 78);
        compose_key(&mut s, KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(s.body_cursor, 78 + 44, "Up moves one wrapped row, same column");
        compose_key(&mut s, KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(s.body_cursor, 44);
    }

    /// Issue #522: the search list used to call `to_plain` on every hit's
    /// full BBCode message on every frame. The snippet is derived once, when
    /// the results arrive, and the renderer only reads it.
    #[test]
    fn search_snippets_are_derived_once_and_read_from_the_cache() {
        let hit = |title: &str, message: &str| SearchHit {
            content_type: "post".into(),
            title: title.into(),
            message: message.into(),
            username: "kemical".into(),
            date: 1_700_000_000,
            ..Default::default()
        };
        let mut s = super::super::SearchState {
            query: "edge".into(),
            ..Default::default()
        };
        s.set_results(vec![
            hit("First", "[B]Edge[/B] updated again, see https://example.com/a"),
            hit("Second", "plain body"),
        ]);
        assert_eq!(s.snippets.len(), 2);
        assert_eq!(s.snippets[0], "Edge updated again, see https://example.com/a");

        // Poison the cache: whatever the renderer draws must come from it,
        // not from a fresh parse of the message.
        s.snippets[0] = "FROM-THE-CACHE".into();
        let theme = Theme::truecolor();
        let rows = render_rows(100, 20, |f, area| {
            render_search(&mut s, f, area, &theme, &UNICODE, &mut crate::hit::HitMap::default())
        });
        let screen = rows.join("\n");
        assert!(
            screen.contains("FROM-THE-CACHE"),
            "the renderer re-parsed the message instead of reading the cache:\n{screen}"
        );
    }

    /// Issue #598: a failed search must not look like "no results" (or a
    /// silently stale page) — `render_search` has to surface `s.error`.
    #[test]
    fn render_search_shows_the_error_instead_of_no_results() {
        let mut s = super::super::SearchState {
            query: "edge".into(),
            error: Some("boom".into()),
            ..Default::default()
        };
        let theme = Theme::truecolor();
        let rows = render_rows(100, 20, |f, area| {
            render_search(&mut s, f, area, &theme, &UNICODE, &mut crate::hit::HitMap::default())
        });
        let screen = rows.join("\n");
        assert!(screen.contains("Error: boom"), "screen:\n{screen}");
        assert!(!screen.contains("No results"), "screen:\n{screen}");
    }

    /// Issue #605: the chip row's right-hand hint was a constant
    /// (`"t type · o order · Tab author"`) regardless of mode — `Tab` has no
    /// arm at all in the browse-mode key match (`a` is the real way to reach
    /// the author field there), and member-content mode (issue #548) has
    /// neither an order toggle nor an author filter.
    #[test]
    fn search_chip_hint_matches_the_actual_mode() {
        let browse = super::super::SearchState::default();
        assert_eq!(search_chip_hint(&browse), "t type \u{b7} o order \u{b7} a author");

        let input = super::super::SearchState { input_mode: true, ..Default::default() };
        assert_eq!(search_chip_hint(&input), "Tab switch field \u{b7} Esc done");

        let member = super::super::SearchState {
            member: Some((42, "thread".into())),
            ..Default::default()
        };
        assert_eq!(search_chip_hint(&member), "t threads/posts");
    }

    /// `search_hit_lines` clips its left span run with `clip_span_run`, which
    /// used to clip by chars instead of cells (issue #545); once the right
    /// side (a CJK author name) fits, the clipped left+right row must never
    /// come out wider than the panel.
    #[test]
    fn search_hit_lines_keeps_exact_width_with_a_cjk_username() {
        let theme = Theme::truecolor();
        let hit = SearchHit {
            content_type: "post".into(),
            title: "How do I control MS edge update schedule?".into(),
            username: "\u{6f22}\u{6f22}\u{6f22}".into(), // 3 ideographs = 6 cells
            ..Default::default()
        };
        // Widths from "right side alone barely fits" up to "no clipping
        // needed at all" — every width where clip_span_run's cell math runs.
        for width in [10u16, 14, 20, 30, 60, 80] {
            let lines = search_hit_lines(&theme, &hit, "", &[], width);
            for line in &lines {
                assert!(
                    line.width() <= width as usize,
                    "width {width}: {:?}",
                    line.spans.iter().map(|s| s.content.as_ref()).collect::<String>()
                );
            }
        }
    }

    /// Issue #569: the new-thread Title field placed its caret with
    /// `chars().take(cursor).count()` — one column per *character* rather
    /// than per cell — so a CJK title's caret drifted left by however many
    /// double-width characters had already been typed.
    #[test]
    fn new_thread_title_caret_measures_cjk_in_cells_not_chars() {
        let theme = Theme::truecolor();
        let mut s = ComposeState {
            target: Some(ComposeTarget::NewThread { node_id: 4 }),
            title_field: true,
            title: "\u{6f22}\u{5b57}".into(), // 漢字, two double-width chars
            title_cursor: 2,
            body: String::new(),
            ..Default::default()
        };
        let mut term = Terminal::new(TestBackend::new(80, 24)).expect("terminal");
        term.draw(|f| {
            let area = f.area();
            render_compose(&mut s, f, area, &theme, &UNICODE, &mut crate::hit::HitMap::default());
        })
        .expect("draw");
        let pos = term.get_cursor_position().expect("cursor");
        let buf = term.backend().buffer().clone();
        let cells: Vec<String> = (0..80).map(|x| buf[(x, pos.y)].symbol().to_string()).collect();
        let text_start = cells
            .iter()
            .position(|c| c == "\u{6f22}")
            .expect("title text on screen") as u16;
        assert_eq!(
            pos.x,
            text_start + 4,
            "caret must move 4 cells for two double-width characters, not 2"
        );
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

    /// Issue #577: `compose_hints()` used to advertise `^A attach` in both
    /// the full and short sets while `compose_key` bound Ctrl+A to
    /// `move_home` in both fields — an advertised key that silently does
    /// something else, with no attach flow behind it (upload is implemented
    /// in `common` but not wired into this screen). The cap must not be
    /// advertised until that is fixed; Ctrl+A must keep working as
    /// move-to-start-of-line either way.
    #[test]
    fn compose_hints_never_advertise_attach_and_ctrl_a_still_moves_home() {
        for target in [
            ComposeTarget::NewThread { node_id: 4 },
            ComposeTarget::ThreadReply {
                thread_id: 1,
                thread_title: "Thread".into(),
            },
        ] {
            let s = ComposeState {
                target: Some(target),
                ..Default::default()
            };
            let hints = compose_hints(&s);
            assert!(
                hints.keys.iter().all(|(cap, _)| *cap != "^A"),
                "^A must not be advertised in the full hint set until attach is wired"
            );
            assert!(
                hints.short.iter().all(|(cap, _)| *cap != "^A"),
                "^A must not be advertised in the short hint set until attach is wired"
            );
        }

        // Ctrl+A itself is unchanged: still home, in both fields.
        let mut s = ComposeState {
            target: Some(ComposeTarget::NewThread { node_id: 4 }),
            title_field: true,
            title: "hello".into(),
            title_cursor: 5,
            body: "world".into(),
            body_cursor: 5,
            ..Default::default()
        };
        compose_key(&mut s, KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));
        assert_eq!(s.title_cursor, 0, "Ctrl+A in the title field must still move home");

        s.title_field = false;
        compose_key(&mut s, KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));
        assert_eq!(s.body_cursor, 0, "Ctrl+A in the body field must still move home");
    }

    /// Issue #605: `compose_hints` used to advertise `Tab field` for every
    /// target, but Tab only switches fields in the new-thread flow — a
    /// ThreadReply/ConversationReply Tab inserts four spaces. And with the
    /// new-thread Title focused, the always-drawn BBCode caps row (`^K
    /// code`/`^U link` etc.) used to alias to kill-to-end/kill-to-start on
    /// the title instead — a destructive surprise behind an unrelated cap.
    #[test]
    fn compose_tab_label_matches_target_and_title_focus_disarms_the_bbcode_caps() {
        let new_thread = ComposeState {
            target: Some(ComposeTarget::NewThread { node_id: 4 }),
            ..Default::default()
        };
        let hints = compose_hints(&new_thread);
        assert_eq!(
            hints.keys.iter().find(|(cap, _)| *cap == "Tab").map(|(_, d)| *d),
            Some("field"),
            "new-thread Tab really does switch field"
        );

        let reply = ComposeState {
            target: Some(ComposeTarget::ThreadReply {
                thread_id: 1,
                thread_title: "Thread".into(),
            }),
            ..Default::default()
        };
        let hints = compose_hints(&reply);
        assert_eq!(
            hints.keys.iter().find(|(cap, _)| *cap == "Tab").map(|(_, d)| *d),
            Some("indent"),
            "a reply's Tab really does insert spaces, not switch a field"
        );

        // With the Title focused, ^B/^I/^K/^Q/^U must no-op with a Notice —
        // never wrap BBCode into the title, and never (issue #605's specific
        // regression) silently kill the title's text via ^K/^U.
        let mut s = ComposeState {
            target: Some(ComposeTarget::NewThread { node_id: 4 }),
            title_field: true,
            title: "hello world".into(),
            title_cursor: 11,
            ..Default::default()
        };
        for c in ['b', 'i', 'k', 'q', 'u'] {
            let act = compose_key(&mut s, KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL));
            assert!(matches!(act, Action::Notice(_)), "^{c} in the title must notice, not act");
            assert_eq!(s.title, "hello world", "^{c} must never alter the title");
        }
        // Ctrl+A/E/W keep working in the title.
        compose_key(&mut s, KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));
        assert_eq!(s.title_cursor, 0, "Ctrl+A in the title must still move home");
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

    /// Issue #569: the search query and author fields placed their carets
    /// with `chars().take(cursor).count()` — cell-blind, same class of bug
    /// as the new-thread title.
    #[test]
    fn search_query_and_author_carets_measure_cjk_in_cells_not_chars() {
        let theme = Theme::truecolor();

        let mut s = crate::screens::SearchState {
            query: "\u{6f22}\u{5b57}".into(),
            query_cursor: 2,
            input_mode: true,
            active_field: 0,
            ..Default::default()
        };
        let mut term = Terminal::new(TestBackend::new(80, 24)).expect("terminal");
        term.draw(|f| {
            let area = f.area();
            render_search(&mut s, f, area, &theme, &UNICODE, &mut crate::hit::HitMap::default());
        })
        .expect("draw");
        let pos = term.get_cursor_position().expect("cursor");
        let buf = term.backend().buffer().clone();
        let cells: Vec<String> = (0..80).map(|x| buf[(x, pos.y)].symbol().to_string()).collect();
        let text_start = cells
            .iter()
            .position(|c| c == "\u{6f22}")
            .expect("query text on screen") as u16;
        assert_eq!(pos.x, text_start + 4, "query caret must move 4 cells, not 2");

        let mut s = crate::screens::SearchState {
            query: "x".into(),
            author: "\u{6f22}\u{5b57}".into(),
            author_cursor: 2,
            input_mode: true,
            active_field: 1,
            ..Default::default()
        };
        let mut term = Terminal::new(TestBackend::new(120, 24)).expect("terminal");
        term.draw(|f| {
            let area = f.area();
            render_search(&mut s, f, area, &theme, &UNICODE, &mut crate::hit::HitMap::default());
        })
        .expect("draw");
        let pos = term.get_cursor_position().expect("cursor");
        let buf = term.backend().buffer().clone();
        let cells: Vec<String> = (0..120).map(|x| buf[(x, pos.y)].symbol().to_string()).collect();
        let text_start = cells
            .iter()
            .position(|c| c == "\u{6f22}")
            .expect("author text on screen") as u16;
        assert_eq!(pos.x, text_start + 4, "author caret must move 4 cells, not 2");
    }

    /// Issue #606: the new-thread Title had no horizontal viewport. Past the
    /// 63 cells the 72-column editor leaves it, the typing went on invisibly
    /// and the caret sat pinned to the pane border.
    #[test]
    fn new_thread_title_scrolls_horizontally_and_keeps_the_caret_visible() {
        let title: String = std::iter::repeat_n('x', 79).chain(['Z']).collect();
        let mut s = ComposeState {
            target: Some(ComposeTarget::NewThread { node_id: 4 }),
            title,
            title_field: true,
            ..Default::default()
        };
        s.title_cursor = s.title.chars().count();

        let (rows, (cx, cy)) = render_compose_probe(&mut s, 120, 24);
        let row = &rows[cy as usize];
        assert!(row.contains("Title: "), "{row}");
        assert!(
            row.contains('Z'),
            "the character just typed must be on screen: {row}"
        );
        // The caret sits just past the last visible character, inside the pane.
        // Char position, not byte: the panel border is a 3-byte box glyph.
        let z = row.chars().position(|c| c == 'Z').expect("Z on screen") as u16;
        assert_eq!(cx, z + 1, "caret must follow the text, not the border: {row}");
        assert!(cx < 119, "caret must stay inside the panel: {cx}");
    }

    /// Issue #606, second half: the right-hand labels used to be drawn only if
    /// they still fitted after the *whole* query, so a long query made the
    /// author field vanish — including while it was the focused one.
    #[test]
    fn a_long_query_still_leaves_the_author_field_on_screen() {
        let theme = Theme::truecolor();
        let mut s = crate::screens::SearchState {
            query: "windows update failure 0x800f0922 ".repeat(2),
            author: "kemical".into(),
            author_cursor: 7,
            input_mode: true,
            active_field: 1,
            ..Default::default()
        };
        s.query_cursor = s.query.chars().count();

        let mut term = Terminal::new(TestBackend::new(80, 24)).expect("terminal");
        term.draw(|f| {
            let area = f.area();
            render_search(&mut s, f, area, &theme, &UNICODE, &mut crate::hit::HitMap::default());
        })
        .expect("draw");
        let pos = term.get_cursor_position().expect("cursor");
        let buf = term.backend().buffer().clone();
        let row: String = (0..80).map(|x| buf[(x, pos.y)].symbol().to_string()).collect();
        assert!(row.contains("author kemical"), "author field must be drawn: {row}");
        let start = row
            .char_indices()
            .position(|(i, _)| row[i..].starts_with("kemical"))
            .expect("author text") as u16;
        assert_eq!(pos.x, start + 7, "caret must sit at the end of the author: {row}");
        assert!(pos.x < 79, "caret must stay inside the panel: {}", pos.x);
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

    /// #673: the type cycler targets the Media Gallery (`xfmg_media`) and
    /// the Resource Manager (`resource`) in addition to threads and posts,
    /// and every firing key maps the chip position to the wire
    /// `search_type` the server's searchers answer to.
    #[test]
    fn the_type_cycler_reaches_media_and_resources() {
        let mut s = crate::screens::SearchState {
            query: "edge".into(),
            ..Default::default()
        };
        let t = KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE);

        let act = search_key(&mut s, t);
        assert!(
            matches!(&act, Action::RunSearchQuery(q) if q.content_type.as_deref() == Some("thread")),
            "chip 1 is threads"
        );
        s.loading = false;
        let act = search_key(&mut s, t);
        assert!(
            matches!(&act, Action::RunSearchQuery(q) if q.content_type.as_deref() == Some("post")),
            "chip 2 is posts"
        );
        s.loading = false;
        let act = search_key(&mut s, t);
        assert!(
            matches!(&act, Action::RunSearchQuery(q) if q.content_type.as_deref() == Some("xfmg_media")),
            "chip 3 is Media Gallery items"
        );
        s.loading = false;
        let act = search_key(&mut s, t);
        assert!(
            matches!(&act, Action::RunSearchQuery(q) if q.content_type.as_deref() == Some("resource")),
            "chip 4 is Resource Manager entries"
        );
        s.loading = false;
        let act = search_key(&mut s, t);
        assert!(
            matches!(&act, Action::RunSearchQuery(q) if q.content_type.is_none()),
            "chip 5 wraps back to all types"
        );
    }

    /// Issue #548: a Search screen opened from a profile (`t`/`p`) is showing
    /// one member's content, and `query` is only the label "by: name (kind)".
    /// Every key that re-fetches has to go back to `search_member` — the old
    /// code posted that label as `keywords` to the keyword search, so page 2
    /// of a member's threads was a search for the literal string.
    #[test]
    fn member_content_paging_and_toggles_route_to_search_member_not_keywords() {
        let mut s = crate::screens::SearchState {
            query: "by: kemical (thread)".into(),
            member: Some((42, "thread".into())),
            content_type: 1,
            page: 2,
            last_page: 5,
            input_mode: false,
            ..Default::default()
        };

        // ']' / '[' page the member's content, not a keyword search.
        // (`s.loading` is reset between presses: the fetch these actions
        // start sets it until the reply lands — see the double-fire test.)
        let act = search_key(&mut s, KeyEvent::new(KeyCode::Char(']'), KeyModifiers::NONE));
        assert!(
            matches!(&act, Action::LoadMemberContent { user_id, content, page }
                if *user_id == 42 && content == "thread" && *page == 3),
            "] must ask for the member's next page"
        );
        s.loading = false;
        let act = search_key(&mut s, KeyEvent::new(KeyCode::Char('['), KeyModifiers::NONE));
        assert!(
            matches!(&act, Action::LoadMemberContent { user_id, content, page }
                if *user_id == 42 && content == "thread" && *page == 1),
            "[ must ask for the member's previous page"
        );
        s.loading = false;

        // 't' flips threads <-> posts through the same endpoint, from page 1,
        // and relabels the screen.
        let act = search_key(&mut s, KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE));
        assert!(
            matches!(&act, Action::LoadMemberContent { user_id, content, page }
                if *user_id == 42 && content == "post" && *page == 1),
            "t must switch the member's content type, not cycle the search filter"
        );
        assert_eq!(s.member.as_ref().map(|m| m.1.as_str()), Some("post"));
        assert_eq!(s.query, "by: kemical (post)");
        assert_eq!(s.content_type, 2, "the chip row must follow the real mode");
        s.loading = false;
        let act = search_key(&mut s, KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE));
        assert!(
            matches!(&act, Action::LoadMemberContent { content, .. } if content == "thread")
        );
        s.loading = false;

        // 'o' (order) and 'a' (author) do not exist for member content: they
        // must refuse out loud, never re-run the label as a keyword search.
        let order = s.order;
        let act = search_key(&mut s, KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE));
        assert!(matches!(act, Action::Notice(_)), "o must refuse in member mode");
        assert_eq!(s.order, order, "the order flag must not move either");
        let act = search_key(&mut s, KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE));
        assert!(matches!(act, Action::Notice(_)), "a must refuse in member mode");
        assert!(!s.input_mode, "author editing stays closed in member mode");
        assert!(!search_hints(&s).keys.iter().any(|(k, _)| *k == "o"));

        // 'i' starts a genuine keyword search: the label is cleared and, once
        // submitted, the screen leaves member mode for good.
        search_key(&mut s, KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE));
        assert!(s.input_mode);
        assert!(s.query.is_empty(), "the label must never become the query");
        for c in "wsl".chars() {
            search_key(&mut s, KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        let act = search_key(&mut s, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(
            matches!(&act, Action::RunSearchQuery(q) if q.keywords == "wsl"),
            "a typed query is a keyword search again"
        );
        assert!(s.member.is_none(), "submitting a query leaves member mode");
        let act = search_key(&mut s, KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE));
        assert!(
            matches!(act, Action::RunSearchQuery(_)),
            "and the ordinary search keys work again"
        );
    }

    /// A fetch sets `loading` until its reply lands, and the page/toggle
    /// keys must not fire a second request into that window — behind the
    /// 3 s `search_gate` each duplicate delays the user's next real search.
    /// The reply clears `loading`; here the actions that set it stand in for
    /// the fetch.
    #[test]
    fn search_keys_do_not_double_fire_while_a_load_is_in_flight() {
        let mut s = crate::screens::SearchState {
            query: "by: kemical (thread)".into(),
            member: Some((42, "thread".into())),
            content_type: 1,
            page: 2,
            last_page: 5,
            input_mode: false,
            ..Default::default()
        };

        let act = search_key(&mut s, KeyEvent::new(KeyCode::Char(']'), KeyModifiers::NONE));
        assert!(matches!(&act, Action::LoadMemberContent { page: 3, .. }));
        let act = search_key(&mut s, KeyEvent::new(KeyCode::Char(']'), KeyModifiers::NONE));
        assert!(matches!(act, Action::Notice(_)), "] must refuse while loading");
        let act = search_key(&mut s, KeyEvent::new(KeyCode::Char('['), KeyModifiers::NONE));
        assert!(matches!(act, Action::Notice(_)), "[ must refuse while loading");
        let act = search_key(&mut s, KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE));
        assert!(matches!(act, Action::Notice(_)), "t must refuse while loading");

        // The reply lands (loading clears) and paging works again.
        s.loading = false;
        let act = search_key(&mut s, KeyEvent::new(KeyCode::Char(']'), KeyModifiers::NONE));
        assert!(matches!(&act, Action::LoadMemberContent { page: 3, .. }));

        // The submit path refuses too: type into the input row while a load
        // is in flight, then press Enter.
        s.loading = true;
        search_key(&mut s, KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE));
        search_key(&mut s, KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE));
        search_key(&mut s, KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE));
        search_key(&mut s, KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE));
        let act = search_key(&mut s, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(act, Action::Notice(_)), "submit must refuse while loading");
        assert!(s.input_mode, "the refused submit must not close the input row");
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

        // 'd': direct message (rebound from `c`, which the global nav
        // intercepted — issue #613)
        let act = profile_key(&mut s, KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE));
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
        let rows = render_rows(120, 36, |f, area| render_compose(&mut s, f, area, &theme, &UNICODE, &mut crate::hit::HitMap::default()));
        assert!(rows[0].contains(" Reply "), "{}", rows[0]);
        assert!(rows[0].contains(" Preview "), "{}", rows[0]);
        let all = rows.join("\n");
        assert!(all.contains("as Mike"), "{all}");
        assert!(all.contains("post #29"), "{all}");
        assert!(all.contains("bold"), "{all}");
        assert!(all.contains("chars"), "{all}");

        // Narrow: the editor alone (preview defaults to off).
        let mut s = sample_reply();
        let rows = render_rows(80, 24, |f, area| render_compose(&mut s, f, area, &theme, &UNICODE, &mut crate::hit::HitMap::default()));
        assert!(rows[0].contains(" Reply "), "{}", rows[0]);
        assert!(!rows[0].contains(" Preview "), "{}", rows[0]);

        // Narrow, toggled to preview.
        let mut s = ComposeState {
            preview: true,
            ..sample_reply()
        };
        let rows = render_rows(80, 24, |f, area| render_compose(&mut s, f, area, &theme, &UNICODE, &mut crate::hit::HitMap::default()));
        assert!(rows[0].contains(" Preview "), "{}", rows[0]);
        assert!(!rows[0].contains(" Reply "), "{}", rows[0]);
        assert!(rows.join("\n").contains("as it will appear"));
    }

    /// Issue #566: the Preview panel renders the draft through
    /// `common::bbcode` + `wrap_spans` (`push_wrapped`), while the editor
    /// panel shows the raw typed body — so a `wrap_spans` bug that ate a
    /// line's own leading whitespace desynced the two: the editor kept a
    /// `[CODE]` block's indentation, the preview flattened it. Both panels
    /// must show the same indentation once wrapped.
    #[test]
    fn compose_preview_keeps_the_same_indentation_the_editor_shows() {
        let theme = Theme::truecolor();
        let mut s = reply_state("[CODE]def f():\n    return 1[/CODE]");
        let rows = render_rows(120, 24, |f, area| render_compose(&mut s, f, area, &theme, &UNICODE, &mut crate::hit::HitMap::default()));
        assert!(rows[0].contains(" Reply "), "{}", rows[0]);
        assert!(rows[0].contains(" Preview "), "{}", rows[0]);
        assert!(
            rows.iter().any(|r| r.contains("    return 1")),
            "the editor's own draft must still show the indentation:\n{}",
            rows.join("\n")
        );
        // Everything left of the editor/preview divider (column 72) is the
        // Reply panel; everything from the divider on is the Preview panel.
        assert!(
            rows.iter().any(|r| r.get(72..).is_some_and(|right| right.contains("return 1"))),
            "the preview must show the same body, indentation included:\n{}",
            rows.join("\n")
        );
    }

    // ---------- Reply / compose: inline images ----------

    fn img_draft(url: &str) -> String {
        format!("before\n[IMG]{url}[/IMG]\nafter")
    }

    fn halfblocks() -> crate::images::Policy {
        crate::images::Policy {
            tier: crate::images::Tier::Halfblocks,
            font: (10, 20),
        }
    }

    #[test]
    fn url_labels_prefer_the_file_name_and_fall_back_to_the_host() {
        assert_eq!(url_label("https://cdn.example/a/b/shot.png"), "shot.png");
        assert_eq!(url_label("https://cdn.example/a/b/shot.png?w=800#x"), "shot.png");
        assert_eq!(url_label("https://imgur.com/a/xyz/"), "imgur.com");
        assert_eq!(url_label("https://imgur.com/a/xyz"), "imgur.com");
        assert_eq!(url_label("https://example.com"), "example.com");
    }

    #[test]
    fn attach_references_resolve_only_against_the_drafts_own_attachments() {
        let att = Attachment {
            attachment_id: 77,
            filename: "screenshot.png".into(),
            width: Some(1200),
            height: Some(800),
            thumbnail_url: Some("https://wf/attachments/77/thumb".into()),
            ..Default::default()
        };
        let refs = bbcode::image_refs(r#"[ATTACH type="full"]77[/ATTACH] [ATTACH]78[/ATTACH]"#);
        assert_eq!(refs.len(), 2);

        let resolved = resolve_image(&refs[0], std::slice::from_ref(&att)).expect("77 resolves");
        assert_eq!(resolved.key, "https://wf/attachments/77/thumb");
        assert_eq!(resolved.label, "screenshot.png");
        assert_eq!(resolved.px, Some((1200, 800)));

        // An id the draft does not carry stays a text placeholder.
        assert!(resolve_image(&refs[1], std::slice::from_ref(&att)).is_none());
        // …and with no attachment list at all (today's compose screen),
        // nothing but `[IMG]` URLs can resolve.
        assert!(resolve_image(&refs[0], &[]).is_none());
    }

    #[test]
    fn the_text_tier_captions_an_image_reference_and_asks_for_no_pixels() {
        let theme = Theme::truecolor();
        let mut s = ComposeState {
            preview: true,
            body: img_draft("https://cdn.example/shot.png"),
            ..sample_reply()
        };
        let rows = render_rows(80, 24, |f, area| render_compose(&mut s, f, area, &theme, &UNICODE, &mut crate::hit::HitMap::default()));
        let all = rows.join("\n");
        assert!(all.contains("\u{25a3} shot.png"), "no caption line: {all}");
        assert!(!all.contains("[image]"), "the placeholder survived: {all}");
        assert!(all.contains("before") && all.contains("after"), "{all}");
        assert!(
            s.image_requests.is_empty(),
            "the text tier must not ask for pixels: {:?}",
            s.image_requests
        );
    }

    #[test]
    fn an_inline_tier_reserves_the_rows_under_the_caption_within_the_design_caps() {
        let theme = Theme::truecolor();
        let url = "https://cdn.example/shot.png";
        let mut s = ComposeState {
            preview: true,
            images: halfblocks(),
            body: img_draft(url),
            ..sample_reply()
        };
        let rows = render_rows(80, 30, |f, area| render_compose(&mut s, f, area, &theme, &UNICODE, &mut crate::hit::HitMap::default()));
        let all = rows.join("\n");
        // Nothing is known about a bare URL until its bytes arrive.
        assert!(all.contains("\u{25a3} loading\u{2026}"), "{all}");

        assert_eq!(s.image_requests.len(), 1, "{:?}", s.image_requests);
        let req = &s.image_requests[0];
        assert_eq!(req.key, url);
        let pane = s.preview_cache.width;
        assert!(
            req.rect.width <= pane * crate::images::WIDTH_PERCENT / 100,
            "{} exceeds 40 % of {pane}",
            req.rect.width
        );
        assert!(req.rect.height <= crate::images::MAX_ROWS, "{}", req.rect.height);

        // The text after the reference is below the reserved rows, not under
        // them: the line list accounts for the image.
        let after = rows
            .iter()
            .position(|r| r.contains("after"))
            .expect("the trailing text is still drawn") as u16;
        assert!(
            after >= req.rect.y + req.rect.height,
            "text at row {after} runs under an image at {}..{}",
            req.rect.y,
            req.rect.y + req.rect.height
        );
        // And the rows the image covers are blank.
        for y in req.rect.y..req.rect.y + req.rect.height {
            let band: String = rows[y as usize]
                .chars()
                .skip(req.rect.x as usize)
                .take(req.rect.width as usize)
                .collect();
            assert!(band.trim().is_empty(), "row {y} is not reserved: {band:?}");
        }
    }

    #[test]
    fn a_learned_size_turns_the_loading_caption_into_the_design_caption() {
        let theme = Theme::truecolor();
        let url = "https://cdn.example/shot.png";
        let mut s = ComposeState {
            preview: true,
            images: halfblocks(),
            body: img_draft(url),
            ..sample_reply()
        };
        let _ = render_rows(80, 30, |f, area| render_compose(&mut s, f, area, &theme, &UNICODE, &mut crate::hit::HitMap::default()));
        // What `Msg::ImageLoaded` teaches the store, the app stamps here.
        s.image_sizes.insert(url.to_string(), (1200, 800));
        let rows = render_rows(80, 30, |f, area| render_compose(&mut s, f, area, &theme, &UNICODE, &mut crate::hit::HitMap::default()));
        let all = rows.join("\n");
        assert!(all.contains("\u{25a3} shot.png \u{b7} 1200\u{d7}800"), "{all}");
        assert!(!all.contains("loading"), "{all}");
    }

    #[test]
    fn an_image_that_would_run_past_the_pane_edge_is_dropped_and_keeps_its_caption() {
        let theme = Theme::truecolor();
        let mut s = ComposeState {
            preview: true,
            images: halfblocks(),
            body: img_draft("https://cdn.example/shot.png"),
            ..sample_reply()
        };
        // Tall enough for the caption, too short for the twelve reserved rows.
        let rows = render_rows(80, 10, |f, area| render_compose(&mut s, f, area, &theme, &UNICODE, &mut crate::hit::HitMap::default()));
        assert!(rows.join("\n").contains("\u{25a3} loading\u{2026}"), "{rows:?}");
        assert!(
            s.image_requests.is_empty(),
            "a half-visible image was still requested: {:?}",
            s.image_requests
        );
    }

    #[test]
    fn typing_around_an_image_neither_re_scans_it_nor_asks_for_it_again() {
        let theme = Theme::truecolor();
        let url = "https://cdn.example/shot.png";
        let mut s = ComposeState {
            preview: true,
            images: halfblocks(),
            body: img_draft(url),
            ..sample_reply()
        };
        let draw = |s: &mut ComposeState, theme: &Theme| {
            render_rows(80, 30, |f, area| render_compose(s, f, area, theme, &UNICODE, &mut crate::hit::HitMap::default()));
        };
        // The composer opened with this reference already in the draft —
        // not a live edit in progress — so the very first build is exempt
        // from the settle wait and fetches right away.
        draw(&mut s, &theme);
        assert_eq!(s.image_requests.len(), 1);

        // Same draft, same pane, same tier: the next frame reuses the build.
        assert!(s.preview_cache.is_fresh(
            &s.body,
            s.preview_cache.width,
            s.images,
            s.image_sizes.len()
        ));

        // One more character: the layout is rebuilt, the URL is not re-asked
        // (and, being already `requested`, keeps painting with no wait).
        s.body.push('!');
        draw(&mut s, &theme);
        assert!(!s.preview_cache.is_fresh("", s.preview_cache.width, s.images, 0));
        assert!(
            s.preview_cache.requested.contains(url),
            "a keystroke re-requested an image already in flight"
        );

        // Even deleting the reference and typing it back does not re-request:
        // the memo is per session, not per build — and, already known, it
        // paints again immediately with no new settle wait.
        s.body = String::new();
        draw(&mut s, &theme);
        s.body = img_draft(url);
        draw(&mut s, &theme);
        assert!(s.preview_cache.requested.contains(url));
        assert_eq!(s.image_requests.len(), 1, "it still paints, it just does not re-ask");
    }

    /// Issue #530: typing a URL character by character between an existing
    /// `[IMG][/IMG]` pair used to fetch every intermediate string — leaking
    /// partial hostnames/paths to whatever they happened to spell and
    /// filling the store with junk. None of the never-settled intermediate
    /// keys may reach `requested` (the only gate a network fetch is behind);
    /// only the final, settled string may.
    #[test]
    fn typing_a_url_one_character_at_a_time_never_requests_an_intermediate_string() {
        let theme = Theme::truecolor();
        let mut s = ComposeState {
            preview: true,
            images: halfblocks(),
            body: img_draft(""),
            ..sample_reply()
        };
        let draw = |s: &mut ComposeState, theme: &Theme| {
            render_rows(80, 30, |f, area| render_compose(s, f, area, theme, &UNICODE, &mut crate::hit::HitMap::default()));
        };

        let target = "https://example.com/a/b/c.png";
        let mut typed = String::new();
        for ch in target.chars() {
            typed.push(ch);
            s.body = img_draft(&typed);
            draw(&mut s, &theme);
            assert!(
                s.image_requests.is_empty(),
                "fetched mid-typing at {typed:?}, before the draft ever settled"
            );
            assert!(
                !s.preview_cache.requested.contains(typed.as_str()),
                "{typed:?} (an intermediate keystroke) was handed to the store"
            );
        }

        // Only once the draft sits still does the finished URL get requested.
        std::thread::sleep(PreviewCache::EDIT_SETTLE + std::time::Duration::from_millis(50));
        draw(&mut s, &theme);
        assert_eq!(s.image_requests.len(), 1);
        assert!(s.preview_cache.requested.contains(target));
    }

    #[test]
    fn a_non_http_image_reference_is_never_requested() {
        let theme = Theme::truecolor();
        let mut s = ComposeState {
            preview: true,
            images: halfblocks(),
            body: "[IMG]data:image/png;base64,AAAA[/IMG]".into(),
            ..sample_reply()
        };
        let rows = render_rows(80, 30, |f, area| render_compose(&mut s, f, area, &theme, &UNICODE, &mut crate::hit::HitMap::default()));
        assert!(s.image_requests.is_empty(), "{:?}", s.image_requests);
        // Nothing is dropped: it still renders as the placeholder it was.
        assert!(rows.join("\n").contains("[image]"), "{rows:?}");
    }

    // ---------- Sign in ----------

    /// Issue #605: `login_hints()` used to be stateless and always advertise
    /// `c`/`o`, but `login_key` binds them only in `LoginStage::Waiting` —
    /// Idle is a real resting state (`end_session` lands there, so does
    /// every poll/exchange failure) with no link yet to copy or open.
    #[test]
    fn login_hints_hide_copy_and_open_while_idle_and_notice_instead_of_silently_ignoring() {
        let idle = crate::screens::LoginState { stage: LoginStage::Idle, ..Default::default() };
        let hints = login_hints(&idle);
        for (cap, _) in hints.keys.iter().chain(hints.short.iter()) {
            assert!(*cap != "c" && *cap != "o", "Idle must not advertise {cap}");
        }

        let mut s = idle;
        let act = login_key(&mut s, KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE));
        assert!(matches!(act, Action::Notice(_)), "c in Idle must notice, not silently no-op");
        let act = login_key(&mut s, KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE));
        assert!(matches!(act, Action::Notice(_)), "o in Idle must notice, not silently no-op");

        let waiting = crate::screens::LoginState { stage: LoginStage::Waiting, ..Default::default() };
        let hints = login_hints(&waiting);
        assert!(hints.keys.iter().any(|(cap, _)| *cap == "c"), "Waiting must still advertise c");
        assert!(hints.keys.iter().any(|(cap, _)| *cap == "o"), "Waiting must still advertise o");
    }

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
        let rows = render_rows(width, 36, |f, area| render_search(&mut s, f, area, &theme, &UNICODE, &mut crate::hit::HitMap::default()));
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
