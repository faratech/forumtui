//! Social screens: the Inbox (conversations + alerts, tabbed and — at >= 110
//! columns — split with the open conversation), the standalone conversation
//! view + new-conversation composer it can still push, and the pieces shared
//! between the inline and standalone view.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState, Paragraph};

use super::{
    browse::{push_bbcode, truncate},
    Action, AlertsState, ConversationViewState, ConversationsState, InboxPane, InboxState,
    InboxTab, NewConversationState,
};
use crate::chrome::{self, Hints};
use crate::glyph::Glyphs;
use crate::hit::{Hit, HitMap, HitPane};
use crate::theme::{fmt_age, fmt_age_parts, Theme};

/// Both panes fit side by side from here up (DESIGN.md) — the same threshold
/// Home uses for its own tree+list split.
const INBOX_DUAL_MIN_COLS: u16 = 110;
/// Width of the tabbed list pane in the two-pane layout (DESIGN.md: "Inbox
/// list 50 + view").
const INBOX_PANE_COLS: u16 = 50;

// ================= Inbox =================

pub fn inbox_hints(s: &InboxState) -> Hints {
    // Issue #605: with the view pane focused, `inbox_key` routes everything
    // but q/h/Left to `conversation_view_key_inner` — `n`/`m`/`Enter`/`R`
    // (the list pane's own keys) do something else there or nothing at all,
    // while the keys that DO work (`n/N` next/prev message, `p/P` profile,
    // `[/]` page) went unadvertised.
    if s.focus == InboxPane::View && s.view.is_some() {
        return Hints::with_short(
            &[
                ("r", "reply"),
                ("j/k", "scroll"),
                ("n/N", "msg"),
                ("p/P", "profile"),
                ("[/]", "page"),
                ("Tab/Esc", "list"),
            ],
            &[
                ("r", "reply"),
                ("j/k", ""),
                ("n/N", "msg"),
                ("p/P", "profile"),
                ("[/]", "page"),
                ("Tab/Esc", "list"),
            ],
            0,
        );
    }
    // `r` replies to whatever the view pane has open, so it can only be
    // advertised while there is one: with `view: None` (a narrow terminal,
    // or the Alerts tab, where nothing auto-primes the pane) the key was a
    // silent no-op (issue class #561/#605 — never advertise a dead key).
    let mut keys = vec![
        ("Enter", "open"),
        ("Tab", "alerts/conversations"),
        ("n", "new message"),
    ];
    let mut short = vec![("Enter", "open"), ("Tab", "tabs"), ("n", "new")];
    if s.view.is_some() {
        keys.push(("r", "reply"));
        short.push(("r", "reply"));
    }
    keys.extend_from_slice(&[("m", "mark read"), ("R", "refresh"), ("Esc", "back")]);
    short.extend_from_slice(&[("m", "read"), ("R", ""), ("Esc", "back")]);
    Hints::with_short(&keys, &short, 0)
}

/// `Tab` means two different things depending on which half has the
/// keyboard: from the tabbed list it switches Conversations/Alerts, from the
/// view pane it returns focus to the list (mirrored by `q`, `h`/`Left`, and by
/// the global Esc branch in `app::handle_key` since Esc never reaches here for
/// an unfocused-capture screen). `q` goes back one level here, not out of the
/// client — the same key on the pushed `ConversationView` pops that screen, and
/// the inline pane is the same step at a wider terminal.
/// `n` (new message) and `r` (reply to whatever the view pane already has open)
/// work from the list regardless of which tab is active; from the view pane `r`
/// is handled identically inside `conversation_view_key_inner`.
pub fn inbox_key(s: &mut InboxState, key: KeyEvent) -> Action {
    if matches!(key.code, KeyCode::Tab | KeyCode::BackTab) {
        match s.focus {
            InboxPane::List => s.tab = s.tab.other(),
            InboxPane::View => s.focus = InboxPane::List,
        }
        return Action::None;
    }
    match s.focus {
        InboxPane::View => match key.code {
            KeyCode::Char('q') | KeyCode::Char('h') | KeyCode::Left => {
                s.focus = InboxPane::List;
                Action::None
            }
            _ => match &mut s.view {
                Some(view) => conversation_view_key_inner(view, key),
                None => {
                    s.focus = InboxPane::List;
                    Action::None
                }
            },
        },
        InboxPane::List => {
            match key.code {
                KeyCode::Char('n') => return Action::StartNewConversation(None),
                KeyCode::Char('r') => {
                    return match &s.view {
                        Some(view) => Action::StartReplyConversation(view.conversation.clone()),
                        None => Action::None,
                    };
                }
                _ => {}
            }
            match s.tab {
                InboxTab::Conversations => conversations_list_key(&mut s.convos, key),
                InboxTab::Alerts => alerts_list_key(&mut s.alerts, key),
            }
        }
    }
}

fn conversations_list_key(c: &mut ConversationsState, key: KeyEvent) -> Action {
    match key.code {
        KeyCode::Char('q') | KeyCode::Char('h') | KeyCode::Left => Action::PopScreen,
        // `r` is taken by "reply to the open conversation" one level up, so
        // manual refresh is `R`/F5. Without it the only way to re-fetch is to
        // leave the screen and come back — `open_inbox` short-circuits when
        // the Inbox is already on top.
        KeyCode::Char('R') | KeyCode::F(5) => {
            if c.loading {
                return Action::Notice("Already loading — one moment.".into());
            }
            c.loading = true;
            Action::LoadConversations(c.page)
        }
        KeyCode::Up | KeyCode::Char('k') => {
            if c.sel > 0 {
                c.sel -= 1;
            }
            Action::None
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if c.sel + 1 < c.conversations.len() {
                c.sel += 1;
            }
            Action::None
        }
        KeyCode::Char('[') | KeyCode::PageUp => {
            if c.loading {
                return Action::Notice("Already loading — one moment.".into());
            }
            if c.page > 1 {
                c.loading = true;
                Action::LoadConversations(c.page - 1)
            } else {
                Action::None
            }
        }
        KeyCode::Char(']') | KeyCode::PageDown => {
            if c.loading {
                return Action::Notice("Already loading — one moment.".into());
            }
            if c.page < c.last_page {
                c.loading = true;
                Action::LoadConversations(c.page + 1)
            } else {
                Action::None
            }
        }
        KeyCode::Enter | KeyCode::Char('l') => match c.conversations.get(c.sel) {
            Some(conv) => Action::OpenConversation(conv.clone()),
            None => Action::None,
        },
        KeyCode::Char('m') => match c.conversations.get(c.sel) {
            Some(conv) => Action::MarkConversationRead(conv.conversation_id),
            None => Action::None,
        },
        _ => Action::None,
    }
}

fn alerts_list_key(a: &mut AlertsState, key: KeyEvent) -> Action {
    match key.code {
        KeyCode::Char('q') | KeyCode::Char('h') | KeyCode::Left => Action::PopScreen,
        // See `conversations_list_key`: `R`/F5, because `r` replies.
        KeyCode::Char('R') | KeyCode::F(5) => {
            if a.loading {
                return Action::Notice("Already loading — one moment.".into());
            }
            a.loading = true;
            Action::LoadAlerts
        }
        KeyCode::Up | KeyCode::Char('k') => {
            if a.sel > 0 {
                a.sel -= 1;
            }
            Action::None
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if a.sel + 1 < a.alerts.len() {
                a.sel += 1;
            }
            Action::None
        }
        // "Enter opens as today": today's Enter on an alert marks it read
        // (there is no separate "open" step short of `o`), so that is exactly
        // what the Inbox's primary `Enter` hint does here too.
        KeyCode::Enter | KeyCode::Char(' ') | KeyCode::Char('m') => match a.alerts.get(a.sel) {
            Some(al) => Action::MarkAlertRead(al.alert_id),
            None => Action::None,
        },
        KeyCode::Char('o') => match a.alerts.get(a.sel).and_then(|al| al.alert_url.clone()) {
            Some(url) => Action::OpenUrl(url),
            None => Action::None,
        },
        _ => Action::None,
    }
}

pub fn render_inbox(
    s: &mut InboxState,
    f: &mut ratatui::Frame,
    area: Rect,
    theme: &Theme,
    g: &Glyphs,
    hits: &mut HitMap,
) {
    let dual = area.width >= INBOX_DUAL_MIN_COLS;
    s.dual = dual;
    if !dual {
        // Only one pane is on screen, so only it gets a rect (issue #549).
        s.list_rect = Rect::default();
        s.view_rect = Rect::default();
        match s.focus {
            InboxPane::List => {
                s.list_rect = area;
                render_inbox_list(s, f, area, theme, g, true, hits)
            }
            InboxPane::View => match &mut s.view {
                Some(view) => {
                    s.view_rect = area;
                    render_inbox_view_panel(view, f, area, theme, g, true, hits)
                }
                None => {
                    s.list_rect = area;
                    render_inbox_list(s, f, area, theme, g, true, hits)
                }
            },
        }
        return;
    }
    let [left, right] =
        Layout::horizontal([Constraint::Length(INBOX_PANE_COLS), Constraint::Min(0)]).areas(area);
    // What the wheel routes by (issue #549).
    s.list_rect = left;
    s.view_rect = right;
    render_inbox_list(s, f, left, theme, g, s.focus == InboxPane::List, hits);
    match &mut s.view {
        Some(view) => render_inbox_view_panel(
            view,
            f,
            right,
            theme,
            g,
            s.focus == InboxPane::View,
            hits,
        ),
        None => {
            f.render_widget(chrome::panel(theme, g, "Inbox", false, None, None), right);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn render_inbox_list(
    s: &mut InboxState,
    f: &mut ratatui::Frame,
    area: Rect,
    theme: &Theme,
    g: &Glyphs,
    focused: bool,
    hits: &mut HitMap,
) {
    hits.push(area, Hit::Pane(HitPane::List));
    let bottom = match s.tab {
        InboxTab::Conversations => conversations_range(&s.convos),
        InboxTab::Alerts => alerts_range(&s.alerts),
    };
    let block = chrome::panel(theme, g, "Inbox", focused, None, bottom.as_deref());
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let [tabs_area, body_area] =
        Layout::vertical([Constraint::Length(2), Constraint::Min(0)]).areas(inner);
    let tabs = tab_row(theme, s);
    // The chips' own cell ranges, measured off the spans that were drawn:
    // ` `, Conversations chip, ` `, Alerts chip (`tab_row`).
    if let Some(first) = tabs.first() {
        let mut x = tabs_area.x;
        for (i, span) in first.spans.iter().enumerate() {
            let w = span.width() as u16;
            let tab = match i {
                1 => Some(InboxTab::Conversations),
                3 => Some(InboxTab::Alerts),
                _ => None,
            };
            if let Some(tab) = tab {
                hits.push(Rect::new(x, tabs_area.y, w, 1), Hit::Tab(tab));
            }
            x = x.saturating_add(w);
        }
    }
    f.render_widget(Paragraph::new(tabs), tabs_area);

    match s.tab {
        InboxTab::Conversations => {
            render_conversations_rows(&mut s.convos, f, body_area, theme, g, focused, hits)
        }
        InboxTab::Alerts => {
            render_alerts_rows(&mut s.alerts, f, body_area, theme, g, focused, hits)
        }
    }
}

/// The `Conversations N | Alerts N` tab row: the active tab's chip in
/// `accent_bg`, the other neutral. The counts are unread counts (matching the
/// header's own `Inbox`/`Alerts` badges), not the number of rows loaded.
fn tab_row(theme: &Theme, s: &InboxState) -> Vec<Line<'static>> {
    let unread_convos = s
        .convos
        .conversations
        .iter()
        .filter(|c| c.is_unread_conv())
        .count();
    let unread_alerts = s.alerts.alerts.iter().filter(|a| !a.viewed()).count();
    let conv_label = format!("Conversations {unread_convos}");
    let alert_label = format!("Alerts {unread_alerts}");
    let (conv_chip, alert_chip) = match s.tab {
        InboxTab::Conversations => (
            chrome::chip_active(theme, &conv_label),
            chrome::chip(theme, &alert_label),
        ),
        InboxTab::Alerts => (
            chrome::chip(theme, &conv_label),
            chrome::chip_active(theme, &alert_label),
        ),
    };
    vec![
        Line::from(vec![Span::raw(" "), conv_chip, Span::raw(" "), alert_chip]),
        Line::from(Span::raw("")),
    ]
}

/// `1–5 of 18`, the same math as the thread list's bottom footer (browse.rs's
/// `list_range` — private there, so reimplemented here).
fn conversations_range(s: &ConversationsState) -> Option<String> {
    if s.conversations.is_empty() {
        return None;
    }
    if s.total == 0 {
        return Some(format!("{} conversations", s.conversations.len()));
    }
    let len = s.conversations.len() as u64;
    let (start, end) = if s.page >= s.last_page.max(1) && s.total >= len {
        (s.total - len + 1, s.total)
    } else {
        let start = (s.page.max(1) as u64 - 1) * len + 1;
        (start, start + len - 1)
    };
    Some(format!("{start}\u{2013}{end} of {}", s.total))
}

fn alerts_range(s: &AlertsState) -> Option<String> {
    if s.alerts.is_empty() {
        return None;
    }
    let unread = s.alerts.iter().filter(|a| !a.viewed()).count();
    Some(format!("{unread} of {} unread", s.alerts.len()))
}

/// Two-line conversation rows: unread glyph + bold title, then dim
/// `participants · N replies · age`.
#[allow(clippy::too_many_arguments)]
fn render_conversations_rows(
    s: &mut ConversationsState,
    f: &mut ratatui::Frame,
    area: Rect,
    theme: &Theme,
    g: &Glyphs,
    focused: bool,
    hits: &mut HitMap,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    if s.loading && s.conversations.is_empty() {
        f.render_widget(
            Paragraph::new("Loading conversations…").style(theme.dim()),
            area,
        );
        return;
    }
    if let Some(err) = &s.error {
        f.render_widget(
            Paragraph::new(format!("Error: {err}")).style(Style::new().fg(theme.error)),
            area,
        );
        return;
    }
    if s.conversations.is_empty() {
        f.render_widget(Paragraph::new("No conversations yet.").style(theme.dim()), area);
        return;
    }

    let tw = (area.width as usize).saturating_sub(3);
    let items: Vec<ListItem> = s
        .conversations
        .iter()
        .map(|c| {
            let unread = c.is_unread_conv();
            let title_style = if unread {
                theme.base().add_modifier(Modifier::BOLD)
            } else {
                theme.base()
            };
            let marker = if unread {
                Span::styled(g.unread.to_string(), Style::new().fg(theme.accent))
            } else {
                Span::raw(" ")
            };
            let line1 = Line::from(vec![
                Span::raw(" "),
                marker,
                Span::raw(" "),
                Span::styled(truncate(&c.title, tw), title_style),
            ]);
            let meta = format!(
                "{} \u{00B7} {} replies \u{00B7} {}",
                c.participants_display(),
                c.reply_count,
                fmt_age(c.last_message_date)
            );
            let line2 = Line::from(vec![
                Span::raw("   "),
                Span::styled(truncate(&meta, tw), theme.dim()),
            ]);
            ListItem::new(vec![line1, line2])
        })
        .collect();
    let mut state = ListState::default().with_selected(Some(s.sel.min(s.conversations.len() - 1)));
    let list = List::new(items);
    let list = if focused {
        list.highlight_style(theme.selected())
    } else {
        list
    };
    f.render_stateful_widget(list, area, &mut state);
    // Two rows per conversation (title + meta), so both lines are the row.
    hits.list(
        area,
        state.offset(),
        &vec![2u16; s.conversations.len()],
        Hit::Row,
    );
}

/// Alert rows: the kind glyph, username bold when unread, dim age.
#[allow(clippy::too_many_arguments)]
fn render_alerts_rows(
    s: &mut AlertsState,
    f: &mut ratatui::Frame,
    area: Rect,
    theme: &Theme,
    g: &Glyphs,
    focused: bool,
    hits: &mut HitMap,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    if s.loading && s.alerts.is_empty() {
        f.render_widget(Paragraph::new("Loading alerts…").style(theme.dim()), area);
        return;
    }
    if let Some(err) = &s.error {
        f.render_widget(
            Paragraph::new(format!("Error: {err}")).style(Style::new().fg(theme.error)),
            area,
        );
        return;
    }
    if s.alerts.is_empty() {
        f.render_widget(Paragraph::new("No alerts yet.").style(theme.dim()), area);
        return;
    }

    let items: Vec<ListItem> = s
        .alerts
        .iter()
        .map(|a| {
            let (marker, style) = if a.viewed() {
                (Span::raw(" "), theme.dim())
            } else {
                (
                    Span::styled(g.unread.to_string(), Style::new().fg(theme.accent)),
                    theme.base().add_modifier(Modifier::BOLD),
                )
            };
            // XF carries the alert KIND in `action` (`quote`/`mention`/
            // `reaction`/`insert`/`award`/…); `content_type` is the CONTENT's
            // type (`post`/`trophy`/`user`/…) and never contains "quote" or
            // "mention" (issue #601).
            let action = a.action.to_ascii_lowercase();
            let kind_glyph = if action.contains("quote") {
                g.quote_alert
            } else if action.contains("mention") {
                g.mention
            } else {
                // Reaction/insert/award/etc. share the reply glyph — the
                // glyph set has no dedicated reaction mark.
                g.reply_alert
            };
            // `alert_text` is the server-rendered human-readable body (built
            // from the alert handler's push template); fall back to
            // `content_type action` for a payload that hasn't populated it.
            let body = if a.alert_text.is_empty() {
                format!("{} {}", a.content_type, a.action).trim().replace('_', " ")
            } else {
                a.alert_text.clone()
            };
            ListItem::new(Line::from(vec![
                Span::raw(" "),
                marker,
                Span::raw(" "),
                Span::styled(format!("{kind_glyph} "), theme.dim()),
                Span::styled(a.username.clone(), style),
                Span::styled(format!("  {body} "), theme.dim()),
                Span::styled(fmt_age(a.event_date), theme.dim()),
            ]))
        })
        .collect();
    let mut state = ListState::default().with_selected(Some(s.sel.min(s.alerts.len() - 1)));
    let list = List::new(items);
    let list = if focused {
        list.highlight_style(theme.selected())
    } else {
        list
    };
    f.render_stateful_widget(list, area, &mut state);
    hits.rows(area, state.offset(), s.alerts.len(), Hit::Row);
}

/// The inline view pane: title = the conversation, right segment `r reply`
/// (DESIGN.md's Inbox artboard — the page-position segment other panels show
/// there is replaced by the one key this pane actually wants advertised).
/// Then `with <participants> · started <age> · N messages`, a rule, and
/// message cards.
#[allow(clippy::too_many_arguments)]
fn render_inbox_view_panel(
    view: &mut ConversationViewState,
    f: &mut ratatui::Frame,
    area: Rect,
    theme: &Theme,
    g: &Glyphs,
    focused: bool,
    hits: &mut HitMap,
) {
    hits.push(area, Hit::Pane(HitPane::View));
    let title = truncate(&view.conversation.title, 40);
    let block = chrome::panel(theme, g, &title, focused, Some("r reply"), None);
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    if view.loading && view.messages.is_empty() {
        f.render_widget(Paragraph::new("Loading messages…").style(theme.dim()), inner);
        return;
    }
    if let Some(err) = &view.error {
        f.render_widget(
            Paragraph::new(format!("Error: {err}")).style(Style::new().fg(theme.error)),
            inner,
        );
        return;
    }

    let [header_area, body_area] =
        Layout::vertical([Constraint::Length(2), Constraint::Min(1)]).areas(inner);

    // Issue #582: `view.messages` holds only the current page (replaced
    // wholesale by `Msg::ConversationLoaded`), so on page 2+ its first
    // message is NOT the conversation's start — it's whatever the pager
    // landed on. XF's own `start_date` is the real answer; fall back to the
    // first loaded message only on page 1 with no `start_date` at all (an
    // older cached fixture, or a server that omitted the field).
    let started = if view.conversation.start_date > 0 {
        view.conversation.start_date
    } else if view.page <= 1 {
        view.messages.first().map(|m| m.message_date).unwrap_or(0)
    } else {
        0
    };
    let total_messages = view.conversation.reply_count + 1;
    let mut meta = vec![
        Span::styled("with ", theme.dim()),
        Span::styled(view.participants_display(), theme.base()),
    ];
    let mut tail = String::new();
    if started > 0 {
        // "ago" only reads right on the relative rungs ("14h", "6d"); once
        // the ladder falls back to a calendar date or bare year (issue
        // #578), the phrase must be "started on Jul 26" / "started on
        // 2025", not "started Jul 26 ago".
        let (age_text, is_relative) = fmt_age_parts(started);
        if age_text == "now" {
            // "started now ago" is nonsense the same way "started Jul 26
            // ago" is (issue #578) — the under-a-minute rung just needs its
            // own phrasing rather than the generic "<rung> ago" tail
            // (issue #583).
            tail.push_str(" \u{00B7} started just now");
        } else if is_relative {
            tail.push_str(&format!(" \u{00B7} started {age_text} ago"));
        } else {
            tail.push_str(&format!(" \u{00B7} started on {age_text}"));
        }
    }
    tail.push_str(&format!(" \u{00B7} {total_messages} messages"));
    meta.push(Span::styled(tail, theme.dim()));
    let rule = if g.ascii { "-" } else { "\u{2500}" };
    f.render_widget(
        Paragraph::new(vec![
            Line::from(meta),
            Line::from(Span::styled(rule.repeat(inner.width as usize), theme.faint())),
        ]),
        header_area,
    );

    view.rebuild_message_lines(theme, g, body_area.width);
    let max_scroll = view.lines.len().saturating_sub(body_area.height as usize);
    if view.scroll > max_scroll {
        view.scroll = max_scroll;
    }
    // Sliced rather than `Paragraph::scroll`, whose offset is a `u16`
    // (issue #558).
    f.render_widget(
        Paragraph::new(crate::editor::visible_window(&view.lines, view.scroll, body_area.height)),
        body_area,
    );
    message_hits(view, body_area, hits);
}

/// One `Hit::Post` per drawn row of a message card, so a click anywhere in a
/// DM selects the message it landed in — the same thing `n`/`N` do, and the
/// same `msg_line_offsets` they navigate by.
fn message_hits(view: &ConversationViewState, area: Rect, hits: &mut HitMap) {
    for row in 0..area.height {
        let line = view.scroll + row as usize;
        if line >= view.lines.len() {
            break;
        }
        let Some(msg) = view
            .msg_line_offsets
            .iter()
            .rposition(|&start| start <= line)
        else {
            continue;
        };
        hits.push(Rect::new(area.x, area.y + row, area.width, 1), Hit::Post(msg));
    }
}

/// Left content, then padding, then right-aligned content — a small local
/// cousin of ThreadView's `justify` (private to `browse`), used for the
/// message-card header's right-aligned age.
fn justify_line(left: Vec<Span<'static>>, right: Vec<Span<'static>>, width: usize) -> Line<'static> {
    let lw: usize = left.iter().map(Span::width).sum();
    let rw: usize = right.iter().map(Span::width).sum();
    if lw + rw < width {
        let mut spans = left;
        spans.push(Span::raw(" ".repeat(width - lw - rw)));
        spans.extend(right);
        Line::from(spans)
    } else {
        Line::from(left)
    }
}

/// Greedy word-wrap over styled spans for the message-card body — a smaller
/// cousin of ThreadView's wrapper (also private to `browse`). Good enough for
/// a compact preview card; it does not try to preserve exact inter-word
/// spacing beyond a single space.
///
/// `width` is terminal *cells* — measured with `chrome::cell_width`, not
/// `chars().count()` — because the body is drawn via `Paragraph::scroll`
/// with no `Wrap`, so a line this function judges to fit is never re-checked
/// by ratatui: a CJK/emoji body billed one cell per (2-cell) character would
/// silently lose roughly half of every line to the panel's own clipping
/// (issue #515).
fn wrap_line(spans: &[Span<'static>], width: usize) -> Vec<Vec<Span<'static>>> {
    let width = width.max(1);
    let mut out: Vec<Vec<Span<'static>>> = Vec::new();
    let mut line: Vec<Span<'static>> = Vec::new();
    let mut used = 0usize;
    for sp in spans {
        for word in sp.content.split_whitespace() {
            let wlen = chrome::cell_width(word);
            if wlen > width {
                if used > 0 {
                    out.push(std::mem::take(&mut line));
                }
                let mut rest = word;
                while chrome::cell_width(rest) > width {
                    let mut head = chrome::take_cells(rest, width);
                    if head.is_empty() {
                        // A single character wider than the whole line: take
                        // it anyway so the loop always makes progress
                        // (mirrors browse::wrap_spans's identical hard-split
                        // case).
                        if let Some(c) = rest.chars().next() {
                            head.push(c);
                        }
                    }
                    let bytes = head.len();
                    out.push(vec![Span::styled(head, sp.style)]);
                    rest = &rest[bytes..];
                }
                line.push(Span::styled(rest.to_string(), sp.style));
                used = chrome::cell_width(rest);
                continue;
            }
            if used > 0 && used + 1 + wlen > width {
                out.push(std::mem::take(&mut line));
                used = 0;
            } else if used > 0 {
                line.push(Span::raw(" "));
                used += 1;
            }
            line.push(Span::styled(word.to_string(), sp.style));
            used += wlen;
        }
    }
    if !line.is_empty() || out.is_empty() {
        out.push(line);
    }
    out
}

// ================= conversation view (standalone + shared) =================

impl ConversationViewState {
    pub fn all_participants(&self) -> Vec<String> {
        let mut names = self.conversation.participant_names();
        for msg in &self.messages {
            if !msg.username.is_empty()
                && !names.iter().any(|n| n.eq_ignore_ascii_case(&msg.username))
            {
                names.push(msg.username.clone());
            }
        }
        names
    }

    pub fn participants_display(&self) -> String {
        let starter = self.conversation.starter();
        let all = self.all_participants();
        if all.is_empty() {
            return "No participants listed".to_string();
        }
        let mut parts = Vec::new();
        if !starter.is_empty() {
            parts.push(format!("{starter} (starter)"));
        }
        for name in all {
            if !starter.eq_ignore_ascii_case(&name) {
                parts.push(name);
            }
        }
        parts.join(", ")
    }

    /// Message-card lines for the Inbox's inline view pane: initials chip,
    /// bold username, right-aligned age, then the body behind a gutter glyph
    /// (accent when this is the selected message, per DESIGN.md). Rebuilt on
    /// every render — a conversation is a handful of messages, so this is
    /// cheap — which is simpler than width-caching and keeps the `n`/`N`
    /// navigation in `conversation_view_key_inner` working off always-fresh
    /// offsets. Unlike `rebuild_lines` (the standalone screen's banner-style
    /// dump), this has no "CONVERSATION PARTICIPANTS" header — the Inbox pane
    /// draws its own compact `with … · started … · N messages` line above it.
    pub fn rebuild_message_lines(&mut self, theme: &Theme, g: &Glyphs, width: u16) {
        // Defensive clamp: sel_msg is written by the n/N keys against the
        // PREVIOUS page's message count, and paging/reload can shrink the
        // list out from under it before this next rebuild runs (issue #534).
        self.sel_msg = self
            .sel_msg
            .min(self.messages.len().saturating_sub(1));
        if self.built == Some((width, self.sel_msg)) {
            return;
        }
        self.built = Some((width, self.sel_msg));
        let width = width.max(1) as usize;
        let body_w = width.saturating_sub(3).max(4);
        let mut lines: Vec<Line<'static>> = Vec::new();
        let mut offsets: Vec<usize> = Vec::new();
        for (i, msg) in self.messages.iter().enumerate() {
            offsets.push(lines.len());
            let header_left = vec![
                chrome::initials_chip(theme, &msg.username),
                Span::raw("  "),
                Span::styled(
                    msg.username.clone(),
                    theme.base().add_modifier(Modifier::BOLD),
                ),
            ];
            let header_right = vec![Span::styled(fmt_age(msg.message_date), theme.dim())];
            lines.push(justify_line(header_left, header_right, width));

            let gutter_style = if i == self.sel_msg {
                Style::new().fg(theme.accent)
            } else {
                theme.faint()
            };
            let mut sink: Vec<Line<'static>> = Vec::new();
            let mut links = Vec::new();
            // Hidden spoilers in DMs stay hidden: the reveal key lives on
            // the thread view (#621).
            push_bbcode(&mut sink, &mut links, &msg.message, theme, false);
            for logical in sink {
                for wrapped in wrap_line(&logical.spans, body_w) {
                    let mut spans = vec![
                        Span::raw(" "),
                        Span::styled(g.gutter.to_string(), gutter_style),
                        Span::raw(" "),
                    ];
                    spans.extend(wrapped);
                    lines.push(Line::from(spans));
                }
            }
            lines.push(Line::from(Span::raw("")));
        }
        self.lines = lines;
        self.msg_line_offsets = offsets;
    }
}

pub fn conversation_view_key(s: &mut ConversationViewState, key: KeyEvent) -> Action {
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('h') | KeyCode::Left => {
            Action::PopScreen
        }
        _ => conversation_view_key_inner(s, key),
    }
}

/// Scrolling, paging, and the content actions (reply, next/prev message,
/// author/starter profile) — shared by the standalone `ConversationView`
/// screen and the Inbox's inline view pane. Navigation-away keys (Esc/q/h/
/// Left) differ by context (pop the screen vs. return focus to the Inbox
/// list), so callers handle those themselves before delegating here.
fn conversation_view_key_inner(s: &mut ConversationViewState, key: KeyEvent) -> Action {
    match key.code {
        KeyCode::Up | KeyCode::Char('k') => {
            s.scroll = s.scroll.saturating_sub(1);
            Action::None
        }
        KeyCode::Down | KeyCode::Char('j') => {
            s.scroll = s.scroll.saturating_add(1);
            Action::None
        }
        KeyCode::PageUp => {
            s.scroll = s.scroll.saturating_sub(20);
            Action::None
        }
        KeyCode::PageDown => {
            s.scroll = s.scroll.saturating_add(20);
            Action::None
        }
        KeyCode::Char('[') => {
            if s.loading {
                return Action::Notice("Already loading — one moment.".into());
            }
            if s.page > 1 {
                s.loading = true;
                Action::LoadConversation(s.conversation.conversation_id, s.page - 1)
            } else {
                Action::None
            }
        }
        KeyCode::Char(']') => {
            if s.loading {
                return Action::Notice("Already loading — one moment.".into());
            }
            if s.page < s.last_page {
                s.loading = true;
                Action::LoadConversation(s.conversation.conversation_id, s.page + 1)
            } else {
                Action::None
            }
        }
        KeyCode::Char('n') => {
            if s.sel_msg + 1 < s.msg_line_offsets.len()
                && let Some(&line) = s.msg_line_offsets.get(s.sel_msg + 1)
            {
                s.sel_msg += 1;
                s.scroll = line;
            }
            Action::None
        }
        KeyCode::Char('N') => {
            if s.sel_msg > 0
                && let Some(&line) = s.msg_line_offsets.get(s.sel_msg - 1)
            {
                s.sel_msg -= 1;
                s.scroll = line;
            }
            Action::None
        }
        KeyCode::Char('p') => {
            if let Some(msg) = s.messages.get(s.sel_msg).or_else(|| s.messages.first()) {
                Action::OpenProfile(msg.user_id, msg.username.clone())
            } else {
                // Advertised on the bar with nothing to act on — refuse out
                // loud, never silently (#658).
                Action::Notice("No message to open a profile for.".into())
            }
        }
        KeyCode::Char('P') => {
            let uid = s.conversation.starter_id();
            let name = s.conversation.starter().to_string();
            if !name.is_empty() {
                Action::OpenProfile(uid, name)
            } else {
                Action::None
            }
        }
        KeyCode::Char('r') => Action::StartReplyConversation(s.conversation.clone()),
        _ => Action::None,
    }
}

pub fn conversation_view_hints() -> Hints {
    Hints::with_short(
        &[
            ("r", "reply"),
            ("j/k", "scroll"),
            ("n/N", "next/prev msg"),
            ("p", "author profile"),
            ("P", "starter profile"),
            ("[/]", "page"),
            ("Esc", "back"),
        ],
        &[
            ("r", "reply"),
            ("j/k", ""),
            ("n/N", "msg"),
            ("p", "profile"),
            ("[/]", "page"),
            ("Esc", "back"),
        ],
        0,
    )
}

pub fn conversation_view_crumb(s: &ConversationViewState) -> String {
    s.conversation.title.clone()
}

pub fn render_conversation_view(
    s: &mut ConversationViewState,
    f: &mut ratatui::Frame,
    area: ratatui::layout::Rect,
    theme: &Theme,
    g: &Glyphs,
    hits: &mut HitMap,
) {
    let right = format!("page {} of {}", s.page.max(1), s.last_page.max(1));
    let bottom = if s.messages.is_empty() {
        None
    } else {
        Some(format!("{} messages", s.messages.len()))
    };
    let block = super::solo_panel(
        theme,
        g,
        &truncate(&s.conversation.title, 40),
        Some(&right),
        bottom.as_deref(),
    );
    let inner = block.inner(area);
    f.render_widget(block, area);

    if s.loading && s.lines.is_empty() {
        f.render_widget(Paragraph::new("Loading messages…"), inner);
        return;
    }
    if let Some(err) = &s.error {
        f.render_widget(
            Paragraph::new(format!("Error: {err}")).style(Style::new().fg(theme.error)),
            inner,
        );
        return;
    }

    let body = Layout::vertical([
        Constraint::Length(2), // Persistent participant header
        Constraint::Min(1),    // Scrollable message body
    ])
    .split(inner);

    // Persistent participant header bar
    let rule = if g.ascii { "-" } else { "\u{2500}" };
    let header_lines = vec![
        Line::from(vec![
            Span::styled("With: ", theme.dim()),
            Span::styled(
                s.participants_display(),
                theme.base().add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(Span::styled(
            rule.repeat(inner.width as usize),
            theme.faint(),
        )),
    ];
    f.render_widget(Paragraph::new(header_lines), body[0]);

    let view = body[1];
    // The same pre-wrapped message cards the Inbox's inline pane draws
    // (issue #528): `Paragraph`'s own `Wrap` used to add visual rows the
    // scroll clamp — computed from *logical* lines — knew nothing about, so
    // below 110 columns the tail of a long DM could not be scrolled to, and
    // the `n`/`N` offsets pointed at the wrong rows. Rebuilt only when the
    // width or the selected message changes.
    s.rebuild_message_lines(theme, g, view.width);
    let max_scroll = s.lines.len().saturating_sub(view.height as usize);
    if s.scroll > max_scroll {
        s.scroll = max_scroll;
    }

    f.render_widget(
        Paragraph::new(crate::editor::visible_window(&s.lines, s.scroll, view.height)),
        view,
    );
    message_hits(s, view, hits);
}

// ================= new conversation =================

pub fn new_conversation_key(s: &mut super::NewConversationState, key: KeyEvent) -> Action {
    if s.busy {
        return Action::None;
    }
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
            KeyCode::Char('y') | KeyCode::Char('v') => return Action::PasteClipboard,
            KeyCode::Char('a') => {
                let (text, cursor) = match s.field {
                    0 => (&mut s.recipients, &mut s.recipients_cursor),
                    1 => (&mut s.title, &mut s.title_cursor),
                    _ => (&mut s.body, &mut s.body_cursor),
                };
                crate::editor::move_home(text, cursor);
                return Action::None;
            }
            KeyCode::Char('e') => {
                let (text, cursor) = match s.field {
                    0 => (&mut s.recipients, &mut s.recipients_cursor),
                    1 => (&mut s.title, &mut s.title_cursor),
                    _ => (&mut s.body, &mut s.body_cursor),
                };
                crate::editor::move_end(text, cursor);
                return Action::None;
            }
            KeyCode::Char('w') => {
                let (text, cursor) = match s.field {
                    0 => (&mut s.recipients, &mut s.recipients_cursor),
                    1 => (&mut s.title, &mut s.title_cursor),
                    _ => (&mut s.body, &mut s.body_cursor),
                };
                crate::editor::delete_word_back(text, cursor);
                return Action::None;
            }
            KeyCode::Char('u') => {
                let (text, cursor) = match s.field {
                    0 => (&mut s.recipients, &mut s.recipients_cursor),
                    1 => (&mut s.title, &mut s.title_cursor),
                    _ => (&mut s.body, &mut s.body_cursor),
                };
                crate::editor::kill_to_start(text, cursor);
                return Action::None;
            }
            KeyCode::Char('k') => {
                let (text, cursor) = match s.field {
                    0 => (&mut s.recipients, &mut s.recipients_cursor),
                    1 => (&mut s.title, &mut s.title_cursor),
                    _ => (&mut s.body, &mut s.body_cursor),
                };
                crate::editor::kill_to_end(text, cursor);
                return Action::None;
            }
            _ => {}
        }
    }

    match key.code {
        KeyCode::Tab => {
            s.field = (s.field + 1) % 3;
            Action::None
        }
        KeyCode::Backspace => {
            let (text, cursor) = match s.field {
                0 => (&mut s.recipients, &mut s.recipients_cursor),
                1 => (&mut s.title, &mut s.title_cursor),
                _ => (&mut s.body, &mut s.body_cursor),
            };
            crate::editor::delete_back(text, cursor);
            Action::None
        }
        KeyCode::Delete => {
            let (text, cursor) = match s.field {
                0 => (&mut s.recipients, &mut s.recipients_cursor),
                1 => (&mut s.title, &mut s.title_cursor),
                _ => (&mut s.body, &mut s.body_cursor),
            };
            crate::editor::delete_forward(text, cursor);
            Action::None
        }
        KeyCode::Left => {
            let (text, cursor) = match s.field {
                0 => (&mut s.recipients, &mut s.recipients_cursor),
                1 => (&mut s.title, &mut s.title_cursor),
                _ => (&mut s.body, &mut s.body_cursor),
            };
            crate::editor::move_left(text, cursor);
            Action::None
        }
        KeyCode::Right => {
            let (text, cursor) = match s.field {
                0 => (&mut s.recipients, &mut s.recipients_cursor),
                1 => (&mut s.title, &mut s.title_cursor),
                _ => (&mut s.body, &mut s.body_cursor),
            };
            crate::editor::move_right(text, cursor);
            Action::None
        }
        KeyCode::Home => {
            let (text, cursor) = match s.field {
                0 => (&mut s.recipients, &mut s.recipients_cursor),
                1 => (&mut s.title, &mut s.title_cursor),
                _ => (&mut s.body, &mut s.body_cursor),
            };
            crate::editor::move_home(text, cursor);
            Action::None
        }
        KeyCode::End => {
            let (text, cursor) = match s.field {
                0 => (&mut s.recipients, &mut s.recipients_cursor),
                1 => (&mut s.title, &mut s.title_cursor),
                _ => (&mut s.body, &mut s.body_cursor),
            };
            crate::editor::move_end(text, cursor);
            Action::None
        }
        KeyCode::Enter => {
            if s.field == 2 {
                s.submit()
            } else {
                s.field += 1;
                Action::None
            }
        }
        // Vertical motion in the message field moves by visual row, like the
        // Reply editor; in the single-line fields it steps between fields.
        KeyCode::Up | KeyCode::Down | KeyCode::PageUp | KeyCode::PageDown if s.field == 2 => {
            let page = (s.body_height.saturating_sub(1)).max(1) as isize;
            let delta = match key.code {
                KeyCode::Up => -1,
                KeyCode::Down => 1,
                KeyCode::PageUp => -page,
                _ => page,
            };
            let width = if s.body_width == 0 { 1 } else { s.body_width as usize };
            crate::editor::move_vertical(
                &s.body,
                width,
                &mut s.body_cursor,
                &mut s.body_desired_col,
                delta,
            );
            Action::None
        }
        KeyCode::Char(c)
            if !key.modifiers.contains(KeyModifiers::CONTROL)
                && !key.modifiers.contains(KeyModifiers::ALT) =>
        {
            let (text, cursor) = match s.field {
                0 => (&mut s.recipients, &mut s.recipients_cursor),
                1 => (&mut s.title, &mut s.title_cursor),
                _ => (&mut s.body, &mut s.body_cursor),
            };
            crate::editor::insert_char(text, cursor, c);
            Action::None
        }
        _ => Action::None,
    }
}

impl NewConversationState {
    fn submit(&mut self) -> Action {
        if self.busy {
            return Action::None;
        }
        let names: Vec<String> = self
            .recipients
            .split(',')
            .map(|p| p.trim().to_string())
            .filter(|p| !p.is_empty())
            .collect();
        if names.is_empty() || self.title.trim().is_empty() || self.body.trim().is_empty() {
            // Replace, never stack: three Enters on an empty form used to pile
            // three identical rows over the body (issue #597).
            self.errors = vec!["Fill in recipients, title and message.".into()];
            return Action::None;
        }
        self.busy = true;
        self.sending = false;
        self.resolved_ids.clear();
        self.errors.clear();
        self.resolving = names.len();
        Action::ResolveRecipients(names, self.title.clone(), self.body.clone())
    }
}

pub fn new_conversation_hints() -> Hints {
    Hints::new(
        &[
            ("Enter", "next/send"),
            ("Tab", "next field"),
            ("Esc", "cancel"),
        ],
        0,
    )
}

/// The two single-line fields' labels. Their cell widths are the caret's
/// offset both when drawing and when a click asks where it landed, so they
/// are named once (issue #535's lesson) rather than counted twice.
const TO_LABEL: &str = "To (usernames, comma-separated): ";
const TITLE_LABEL: &str = "Title: ";

/// Focus field `field` and put the caret under the pointer (`Hit::Field`).
pub(crate) fn new_conversation_click_field(
    s: &mut super::NewConversationState,
    field: usize,
    col: u16,
    row: u16,
) {
    if s.busy {
        return;
    }
    s.field = field.min(2);
    match field {
        0 => {
            let label = crate::chrome::cell_width(TO_LABEL);
            let room = (s.to_rect.width as usize).saturating_sub(label);
            let x = col.saturating_sub(s.to_rect.x).saturating_sub(label as u16);
            s.recipients_cursor =
                crate::editor::field_caret_at(&s.recipients, s.recipients_cursor, room, x as usize);
        }
        1 => {
            let label = crate::chrome::cell_width(TITLE_LABEL);
            let room = (s.title_rect.width as usize).saturating_sub(label);
            let x = col
                .saturating_sub(s.title_rect.x)
                .saturating_sub(label as u16);
            s.title_cursor =
                crate::editor::field_caret_at(&s.title, s.title_cursor, room, x as usize);
        }
        _ => {
            let line = s.body_scroll + row.saturating_sub(s.body_rect.y) as usize;
            let x = col.saturating_sub(s.body_rect.x) as usize;
            s.body_cursor =
                crate::editor::caret_at_cell(&s.body, s.body_width as usize, line, x);
            // Any non-vertical move clears the sticky column (issue #523).
            s.body_desired_col = None;
        }
    }
}

pub fn render_new_conversation(
    s: &mut super::NewConversationState,
    f: &mut ratatui::Frame,
    area: ratatui::layout::Rect,
    theme: &Theme,
    g: &Glyphs,
    hits: &mut HitMap,
) {
    let block = super::solo_panel(theme, g, "New conversation", None, None);
    let inner = block.inner(area);
    f.render_widget(block, area);

    let [to_area, title_area, label_area, body_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(1),
    ])
    .areas(inner);

    s.to_rect = to_area;
    s.title_rect = title_area;
    s.body_rect = body_area;
    hits.push(to_area, Hit::Field(0));
    hits.push(title_area, Hit::Field(1));
    hits.push(body_area, Hit::Field(2));
    // Both single-line fields scroll horizontally inside the room their label
    // leaves them, and the caret comes from the same window (issue #606) —
    // they used to be drawn whole and clipped by the pane, so a long
    // recipients list or title was typed blind against the border.
    let to_room = (to_area.width as usize).saturating_sub(crate::chrome::cell_width(TO_LABEL));
    let (to_visible, to_caret) =
        crate::chrome::field_window(&s.recipients, s.recipients_cursor, to_room);
    let title_room =
        (title_area.width as usize).saturating_sub(crate::chrome::cell_width(TITLE_LABEL));
    let (title_visible, title_caret) =
        crate::chrome::field_window(&s.title, s.title_cursor, title_room);
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(TO_LABEL, theme.dim()),
            Span::styled(to_visible, theme.base()),
        ])),
        to_area,
    );
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(TITLE_LABEL, theme.dim()),
            Span::styled(title_visible, theme.base()),
        ])),
        title_area,
    );
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "Message (Tab to next field):",
            theme.dim(),
        ))),
        label_area,
    );

    // Same contract as the Reply editor (issues #519/#523): the message is
    // pre-wrapped into visual rows, the caret is tracked in those rows, and
    // the offset follows it — never `Paragraph`'s own `Wrap`, which would
    // put the caret a row off for every wrap above it and leave anything
    // past the pane's last row unreachable.
    s.body_width = body_area.width;
    s.body_height = body_area.height;
    let body_chars: Vec<char> = s.body.chars().collect();
    let rows = crate::editor::visual_rows_of(&body_chars, body_area.width as usize);
    let (caret_row, caret_col) = crate::editor::caret_in_rows(&body_chars, &rows, s.body_cursor);
    s.body_scroll = crate::editor::follow_caret(
        s.body_scroll,
        caret_row,
        rows.len(),
        body_area.height as usize,
    );
    let body_lines: Vec<Line<'static>> = rows
        .iter()
        .map(|r| {
            Line::from(Span::styled(
                body_chars[r.start..r.end].iter().collect::<String>(),
                theme.base(),
            ))
        })
        .collect();
    f.render_widget(
        Paragraph::new(crate::editor::visible_window(
            &body_lines,
            s.body_scroll,
            body_area.height,
        )),
        body_area,
    );

    let cur_pos = match s.field {
        0 => {
            // Measured from the label itself (cells, not a hand-counted
            // constant) so the caret can't drift off by however many cells
            // someone gets wrong re-copying the string (issue #535), and from
            // the same window the field was drawn with (issue #606).
            let prefix_len = crate::chrome::cell_width(TO_LABEL) as u16;
            Some((
                (to_area.x + prefix_len + to_caret)
                    .min(to_area.x + to_area.width.saturating_sub(1)),
                to_area.y,
            ))
        }
        1 => {
            let prefix_len = crate::chrome::cell_width(TITLE_LABEL) as u16;
            Some((
                (title_area.x + prefix_len + title_caret)
                    .min(title_area.x + title_area.width.saturating_sub(1)),
                title_area.y,
            ))
        }
        2 => Some((
            (body_area.x + caret_col as u16).min(body_area.x + body_area.width.saturating_sub(1)),
            // Subtract in `usize`, narrow afterwards: the difference is at
            // most one pane height, however many rows the draft has (#558).
            (body_area.y + caret_row.saturating_sub(s.body_scroll).min(u16::MAX as usize) as u16)
                .min(body_area.y + body_area.height.saturating_sub(1)),
        )),
        _ => None,
    };
    if let Some((x, y)) = cur_pos {
        f.set_cursor_position((x, y));
    }

    let mut status_lines: Vec<Line> = Vec::new();
    for e in &s.errors {
        status_lines.push(Line::from(Span::styled(e.clone(), Style::new().fg(theme.error))));
    }
    if s.sending {
        status_lines.push(Line::from(Span::styled("Sending…", theme.dim())));
    } else if s.busy {
        status_lines.push(Line::from(Span::styled("Resolving recipients…", theme.dim())));
    }
    if !status_lines.is_empty() {
        let status_area = ratatui::layout::Rect::new(
            inner.x,
            inner.y + inner.height.saturating_sub(status_lines.len() as u16),
            inner.width,
            status_lines.len() as u16,
        );
        f.render_widget(Paragraph::new(status_lines), status_area);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::models::{Alert, Conversation, ConversationMessage, ConversationRecipient};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn sample_conversation() -> Conversation {
        Conversation {
            conversation_id: 3,
            title: "Terminal client beta — first impressions".into(),
            start_user_id: 1,
            start_username: "Mike".into(),
            reply_count: 6,
            last_message_date: 1_700_000_000,
            conversation_unread: true,
            recipients: vec![ConversationRecipient {
                user_id: 2,
                username: "kemical".into(),
            }],
            ..Default::default()
        }
    }

    fn sample_inbox_state() -> InboxState {
        let conv = sample_conversation();
        let messages = vec![
            ConversationMessage {
                message_id: 1,
                conversation_id: 3,
                user_id: 2,
                username: "kemical".into(),
                message: "Ran it over SSH from the Windows Terminal preview.".into(),
                message_date: 1_700_000_000,
            },
            ConversationMessage {
                message_id: 2,
                conversation_id: 3,
                user_id: 1,
                username: "Mike".into(),
                message: "Kitty too?".into(),
                message_date: 1_700_003_600,
            },
        ];
        InboxState {
            convos: ConversationsState {
                conversations: vec![conv.clone(); 5],
                page: 1,
                last_page: 4,
                total: 18,
                ..Default::default()
            },
            view: Some(ConversationViewState {
                conversation: conv,
                messages,
                page: 1,
                last_page: 1,
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn render_rows_inbox(s: &mut InboxState, w: u16, h: u16) -> Vec<String> {
        let theme = Theme::truecolor();
        let mut term =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).expect("terminal");
        term.draw(|f| {
            let area = f.area();
            render_inbox(s, f, area, &theme, &crate::glyph::UNICODE, &mut crate::hit::HitMap::default());
        })
        .expect("draw");
        let buf = term.backend().buffer().clone();
        (0..h)
            .map(|y| (0..w).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>())
            .collect()
    }

    /// Issue #535: the recipients caret used a hand-counted `32` for the
    /// 33-cell label `"To (usernames, comma-separated): "`, landing one cell
    /// left of the actual insertion point. Pin the caret to where the typed
    /// text itself renders, so a wrong constant fails regardless of panel
    /// border internals.
    #[test]
    fn recipients_caret_aligns_with_the_actual_label_width() {
        let mut s = super::super::NewConversationState {
            field: 0,
            recipients: "abc".to_string(),
            recipients_cursor: 3,
            ..Default::default()
        };
        let theme = Theme::truecolor();
        let mut term =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).expect("terminal");
        term.draw(|f| {
            let area = f.area();
            render_new_conversation(&mut s, f, area, &theme, &crate::glyph::UNICODE, &mut crate::hit::HitMap::default());
        })
        .expect("draw");
        let pos = term.get_cursor_position().expect("cursor");
        let buf = term.backend().buffer().clone();
        // Per-cell symbols, one per column — unlike a joined `String`, this
        // keeps the index aligned to the buffer's x coordinate even though
        // the border glyphs are multi-byte UTF-8.
        let cells: Vec<String> = (0..80).map(|x| buf[(x, pos.y)].symbol().to_string()).collect();
        let text_start = cells
            .windows(3)
            .position(|w| w[0] == "a" && w[1] == "b" && w[2] == "c")
            .expect("recipients text on screen") as u16;
        assert_eq!(
            pos.x,
            text_start + 3,
            "caret must land right after the typed text, not one cell short"
        );
    }

    /// Issue #569: both single-line fields on this screen placed their
    /// caret with `chars().take(cursor).count()` — one column per
    /// *character*, not per cell — so a CJK recipient list or title drifted
    /// the caret left by however many double-width characters preceded it.
    #[test]
    fn recipients_and_title_carets_measure_cjk_in_cells_not_chars() {
        let theme = Theme::truecolor();

        let mut s = super::super::NewConversationState {
            field: 0,
            recipients: "\u{6f22}\u{5b57}".into(), // 漢字, two double-width chars
            recipients_cursor: 2,
            ..Default::default()
        };
        let mut term =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).expect("terminal");
        term.draw(|f| {
            let area = f.area();
            render_new_conversation(&mut s, f, area, &theme, &crate::glyph::UNICODE, &mut crate::hit::HitMap::default());
        })
        .expect("draw");
        let pos = term.get_cursor_position().expect("cursor");
        let buf = term.backend().buffer().clone();
        let cells: Vec<String> = (0..80).map(|x| buf[(x, pos.y)].symbol().to_string()).collect();
        let text_start = cells
            .iter()
            .position(|c| c == "\u{6f22}")
            .expect("recipients text on screen") as u16;
        assert_eq!(pos.x, text_start + 4, "recipients caret must move 4 cells, not 2");

        let mut s = super::super::NewConversationState {
            field: 1,
            title: "\u{6f22}\u{5b57}".into(),
            title_cursor: 2,
            ..Default::default()
        };
        let mut term =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).expect("terminal");
        term.draw(|f| {
            let area = f.area();
            render_new_conversation(&mut s, f, area, &theme, &crate::glyph::UNICODE, &mut crate::hit::HitMap::default());
        })
        .expect("draw");
        let pos = term.get_cursor_position().expect("cursor");
        let buf = term.backend().buffer().clone();
        let cells: Vec<String> = (0..80).map(|x| buf[(x, pos.y)].symbol().to_string()).collect();
        let text_start = cells
            .iter()
            .position(|c| c == "\u{6f22}")
            .expect("title text on screen") as u16;
        assert_eq!(pos.x, text_start + 4, "title caret must move 4 cells, not 2");
    }

    /// Issue #606: the DM recipients field scrolls horizontally too — a long
    /// comma-separated list used to run off the pane with the caret pinned to
    /// the border.
    #[test]
    fn new_conversation_recipients_scroll_horizontally() {
        let mut s = super::super::NewConversationState {
            field: 0,
            recipients: format!("{}, Zed", "someone_with_a_long_name, ".repeat(4)),
            ..Default::default()
        };
        s.recipients_cursor = s.recipients.chars().count();

        let theme = Theme::truecolor();
        let mut term =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).expect("terminal");
        term.draw(|f| {
            let area = f.area();
            render_new_conversation(&mut s, f, area, &theme, &crate::glyph::UNICODE, &mut crate::hit::HitMap::default());
        })
        .expect("draw");
        let pos = term.get_cursor_position().expect("cursor");
        let buf = term.backend().buffer().clone();
        let row: String = (0..80).map(|x| buf[(x, pos.y)].symbol().to_string()).collect();
        assert!(row.contains("Zed"), "the tail being typed must be visible: {row}");
        let z = row.chars().position(|c| c == 'Z').expect("Z on screen") as u16;
        assert_eq!(pos.x, z + 3, "caret sits just past the text: {row}");
        assert!(pos.x < 79, "caret must stay inside the panel: {}", pos.x);
    }

    /// Issue #597: the validation row used to be *pushed* on every Enter
    /// while only the valid path cleared the list, so three Enters on an
    /// empty form stacked three identical rows over the body.
    #[test]
    fn empty_form_enter_twice_leaves_one_validation_row() {
        let mut s = super::super::NewConversationState { field: 2, ..Default::default() };
        for _ in 0..3 {
            let action = new_conversation_key(
                &mut s,
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            );
            assert!(matches!(action, Action::None));
        }
        assert_eq!(s.errors.len(), 1, "{:?}", s.errors);
        assert!(!s.busy, "an invalid form never goes busy");
    }

    /// Issue #519/#523 for the DM composer: the message body is pre-wrapped
    /// into visual rows, the caret is placed in those rows, the pane follows
    /// it, and Up/Down move between them.
    #[test]
    fn new_conversation_body_scrolls_and_places_the_caret_on_visual_rows() {
        let body: String = (0..40).map(|i| format!("row {i:02}\n")).collect();
        let mut s = super::super::NewConversationState {
            field: 2,
            body: body.trim_end_matches('\n').to_string(),
            ..Default::default()
        };
        s.body_cursor = s.body.chars().count();

        let theme = Theme::truecolor();
        let mut term =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).expect("terminal");
        term.draw(|f| {
            let area = f.area();
            render_new_conversation(&mut s, f, area, &theme, &crate::glyph::UNICODE, &mut crate::hit::HitMap::default());
        })
        .expect("draw");
        let pos = term.get_cursor_position().expect("cursor");
        let buf = term.backend().buffer().clone();
        let rows: Vec<String> = (0..24)
            .map(|y| (0..80).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>())
            .collect();
        let screen = rows.join("\n");
        assert!(screen.contains("row 39"), "the caret's line is off screen:\n{screen}");
        assert!(!screen.contains("row 00"), "the pane did not scroll:\n{screen}");
        let caret_row = rows.iter().position(|r| r.contains("row 39")).expect("last row") as u16;
        assert_eq!((pos.x, pos.y), (1 + 6, caret_row));

        // Up now moves a visual row instead of doing nothing.
        new_conversation_key(&mut s, KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        // Each row is "row NN" + "\n" = 7 chars; column 6 of row 38.
        assert_eq!(s.body_cursor, 7 * 38 + 6, "Up lands at column 6 of the row above");
    }

    /// Issue #528: the standalone DM screen used to clamp its scroll by
    /// *logical* line count while `Paragraph` wrapped the text, so on an
    /// 80-column terminal the tail of a long conversation could not be
    /// scrolled to at all. It now draws the pre-wrapped message cards.
    #[test]
    fn standalone_conversation_view_scrolls_to_the_end_of_a_wrapped_dm() {
        let body = |n: usize| {
            format!(
                "{} paragraph {n} that is quite long and will wrap several times over.",
                "filler words ".repeat(12)
            )
        };
        let messages: Vec<ConversationMessage> = (0..6)
            .map(|i| ConversationMessage {
                message_id: i as u32 + 1,
                conversation_id: 3,
                user_id: 2,
                username: "kemical".into(),
                message: if i == 5 {
                    format!("{} LASTWORD", body(i))
                } else {
                    body(i)
                },
                message_date: 1_700_000_000,
            })
            .collect();
        let mut state = ConversationViewState {
            conversation: sample_conversation(),
            messages,
            page: 1,
            last_page: 1,
            scroll: usize::MAX, // `G` / a long run of `j`
            ..Default::default()
        };

        let theme = Theme::truecolor();
        let mut term =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).expect("terminal");
        term.draw(|f| {
            let area = f.area();
            render_conversation_view(&mut state, f, area, &theme, &crate::glyph::UNICODE, &mut crate::hit::HitMap::default());
        })
        .expect("draw");
        let buf = term.backend().buffer().clone();
        let rows: Vec<String> = (0..24)
            .map(|y| (0..80).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>())
            .collect();
        let screen = rows.join("\n");
        assert!(
            screen.contains("LASTWORD"),
            "the end of the conversation is unreachable:\n{screen}"
        );
        // Pre-wrapped, not `Wrap`: every body row fits inside the panel, so
        // the scroll clamp counts exactly what is drawn.
        assert!(
            state.lines.iter().all(|l| l.width() <= 78),
            "a line is wider than the panel: the pane is not pre-wrapped"
        );
    }

    /// Issue #522: the message cards were re-parsed and re-wrapped on every
    /// frame (20 fps). They are now derived once per (width, selection).
    #[test]
    fn message_lines_are_cached_until_the_width_or_selection_changes() {
        let mut state = sample_inbox_state().view.take().expect("view");
        let theme = Theme::truecolor();
        let g = crate::glyph::UNICODE;
        state.rebuild_message_lines(&theme, &g, 60);
        assert!(!state.lines.is_empty());

        // A sentinel survives a second call with the same key: no rebuild.
        state.lines = vec![Line::from(Span::raw("SENTINEL"))];
        state.rebuild_message_lines(&theme, &g, 60);
        assert_eq!(state.lines.len(), 1, "the cards were rebuilt for an unchanged frame");

        // A width change rebuilds...
        state.rebuild_message_lines(&theme, &g, 40);
        assert!(state.lines.len() > 1, "a resize must rebuild");

        // ...so does moving the selection (the gutter is styled by it)...
        state.lines = vec![Line::from(Span::raw("SENTINEL"))];
        state.sel_msg = 1;
        state.rebuild_message_lines(&theme, &g, 40);
        assert!(state.lines.len() > 1, "a new selection must rebuild");

        // ...and so does new data, which clears the key.
        state.lines = vec![Line::from(Span::raw("SENTINEL"))];
        state.built = None;
        state.rebuild_message_lines(&theme, &g, 40);
        assert!(state.lines.len() > 1, "fresh messages must rebuild");
    }

    fn conv_messages(n: usize) -> Vec<ConversationMessage> {
        (0..n)
            .map(|i| ConversationMessage {
                message_id: i as u32 + 1,
                conversation_id: 3,
                user_id: 2,
                username: "kemical".into(),
                message: format!("message {i}"),
                message_date: 1_700_000_000,
            })
            .collect()
    }

    /// Issue #534: a page change (or the post-reply reload) used to leave
    /// `sel_msg` pointing past the new, shorter page. `rebuild_message_lines`
    /// must clamp it before indexing, and `N` must never index past the
    /// clamped bound — repro is page 1 with 6 messages at sel_msg=5, then a
    /// page swap down to 2 messages, then `N`.
    #[test]
    fn stale_sel_msg_after_a_page_change_is_clamped_not_indexed() {
        let mut state = ConversationViewState {
            conversation: sample_conversation(),
            messages: conv_messages(6),
            page: 1,
            last_page: 2,
            sel_msg: 5,
            ..Default::default()
        };
        let theme = Theme::truecolor();
        let g = crate::glyph::UNICODE;
        state.rebuild_message_lines(&theme, &g, 60);
        assert_eq!(state.sel_msg, 5);

        // Simulate the shorter page 2 landing without an app.rs-level reset
        // (the defense-in-depth this test pins): sel_msg is still 5.
        state.messages = conv_messages(2);
        state.built = None;
        state.rebuild_message_lines(&theme, &g, 60);
        assert_eq!(state.sel_msg, 1, "sel_msg must clamp to the new last index");

        // `N` must not panic and must stop at 0, using the clamped offsets.
        conversation_view_key_inner(&mut state, key('N'));
        assert_eq!(state.sel_msg, 0);
        conversation_view_key_inner(&mut state, key('N'));
        assert_eq!(state.sel_msg, 0, "N past the top must be a no-op, not a panic");
    }

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    /// A CJK DM body has no whitespace, so it takes the hard-break path;
    /// every returned line must still fit in `width` *cells* (issue #515) —
    /// a char-count budget would let a 20-ideograph body overflow to twice
    /// the requested width and get silently clipped by the un-`Wrap`ped
    /// Paragraph that renders it.
    #[test]
    fn wrap_line_measures_cjk_bodies_in_cells_not_chars() {
        let body = "视频编辑软件推荐帮助教程升级指南论坛社区管理".to_string(); // 22 ideographs, no whitespace
        let spans = vec![Span::raw(body)];
        for width in [10usize, 20, 30, 45] {
            let lines = wrap_line(&spans, width);
            for line in &lines {
                let w: usize = line.iter().map(Span::width).sum();
                assert!(w <= width, "width {width}: line is {w} cells: {line:?}");
            }
        }
    }

    /// `q` in the inline view pane is one level out, not quit — the same step
    /// the pushed `ConversationView` takes below the dual-layout threshold.
    #[test]
    fn q_leaves_the_inline_conversation_pane_like_h_and_left() {
        for k in [
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Left, KeyModifiers::NONE),
        ] {
            let mut s = sample_inbox_state();
            s.focus = InboxPane::View;
            let action = inbox_key(&mut s, k);
            assert!(matches!(action, Action::None), "{:?} should not act", k.code);
            assert_eq!(s.focus, InboxPane::List, "{:?} should return focus", k.code);
        }
        // ...and from the list `q` still leaves the Inbox entirely.
        let mut s = sample_inbox_state();
        s.focus = InboxPane::List;
        assert!(matches!(inbox_key(&mut s, key('q')), Action::PopScreen));
    }

    /// `r` is reply, so refresh is `R` — and it must reach both lists rather
    /// than being swallowed by the Inbox's own `r` interception.
    #[test]
    fn shift_r_refreshes_whichever_inbox_list_is_showing() {
        let shift_r = KeyEvent::new(KeyCode::Char('R'), KeyModifiers::SHIFT);
        let mut s = sample_inbox_state();
        s.convos.page = 3;
        assert!(matches!(
            inbox_key(&mut s, shift_r),
            Action::LoadConversations(3)
        ));
        assert!(s.convos.loading);

        let mut s = sample_inbox_state();
        s.tab = InboxTab::Alerts;
        assert!(matches!(inbox_key(&mut s, shift_r), Action::LoadAlerts));
        assert!(s.alerts.loading);

        // F5 is the same key for terminals that send it.
        let mut s = sample_inbox_state();
        s.tab = InboxTab::Alerts;
        let f5 = KeyEvent::new(KeyCode::F(5), KeyModifiers::NONE);
        assert!(matches!(inbox_key(&mut s, f5), Action::LoadAlerts));

        // Lowercase `r` still means reply, not refresh.
        let mut s = sample_inbox_state();
        assert!(matches!(
            inbox_key(&mut s, key('r')),
            Action::StartReplyConversation(_)
        ));

        // The key bar advertises it.
        assert!(inbox_hints(&s).keys.iter().any(|(k, _)| *k == "R"));
    }

    /// #657: a fetch sets `loading` until its reply lands, and the page and
    /// refresh keys must refuse out loud instead of firing a duplicate
    /// request into that window — with the 3 s spacing on the shared gates,
    /// duplicates delay the user's next real request. The reply clears
    /// `loading`; here the refused action stands in for the fetch.
    #[test]
    fn inbox_page_and_refresh_keys_do_not_double_fire_while_loading() {
        let mut s = sample_inbox_state();
        s.convos.page = 2;
        s.convos.last_page = 5;

        assert!(matches!(
            inbox_key(&mut s, key(']')),
            Action::LoadConversations(3)
        ));
        assert!(
            matches!(inbox_key(&mut s, key(']')), Action::Notice(_)),
            "] must refuse while loading"
        );
        let shift_r = KeyEvent::new(KeyCode::Char('R'), KeyModifiers::SHIFT);
        assert!(
            matches!(inbox_key(&mut s, shift_r), Action::Notice(_)),
            "R must refuse while loading"
        );
        s.convos.loading = false;
        assert!(matches!(
            inbox_key(&mut s, key('[')),
            Action::LoadConversations(1)
        ));
        assert!(s.convos.loading);

        // The alerts list's own refresh refuses the same way (#666).
        let mut s = sample_inbox_state();
        s.tab = InboxTab::Alerts;
        s.alerts.loading = true;
        let shift_r = KeyEvent::new(KeyCode::Char('R'), KeyModifiers::SHIFT);
        assert!(
            matches!(inbox_key(&mut s, shift_r), Action::Notice(_)),
            "alerts R must refuse while loading"
        );

        // The open conversation's pager behaves the same way.
        let mut s = sample_inbox_state();
        s.focus = InboxPane::View;
        {
            let Some(view) = s.view.as_mut() else { panic!("test setup: a view is open") };
            view.page = 1;
            view.last_page = 3;
        }
        assert!(matches!(
            inbox_key(&mut s, key(']')),
            Action::LoadConversation(_, 2)
        ));
        assert!(
            matches!(inbox_key(&mut s, key(']')), Action::Notice(_)),
            "] must refuse while the conversation page is loading"
        );
    }

    #[test]
    fn inbox_dual_layout_shows_tabs_and_both_tabs_bottom_titles() {
        let mut s = sample_inbox_state();
        let rows = render_rows_inbox(&mut s, 120, 36);
        assert!(s.dual, "120 cols should be dual");
        let text = rows.join("\n");
        assert!(text.contains("Conversations"), "{text}");
        assert!(text.contains("Alerts"), "{text}");
        assert!(text.contains("of 18"), "conversations footer missing: {text}");
        assert!(text.contains("r reply"), "view panel right segment: {text}");
        assert!(text.contains("kemical"), "message card author: {text}");

        s.tab = InboxTab::Alerts;
        s.alerts.alerts = vec![Alert {
            alert_id: 1,
            username: "kemical".into(),
            content_type: "post".into(),
            action: "quote".into(),
            view_date: 0,
            ..Default::default()
        }];
        let rows = render_rows_inbox(&mut s, 120, 36);
        let text = rows.join("\n");
        assert!(text.contains("unread"), "alerts footer missing: {text}");
    }

    /// Issue #605: with the view pane focused, `inbox_key` routes `n`/`m`/
    /// `Enter`/`R` to `conversation_view_key_inner`, where they either do
    /// something else (`n` moves to the next message) or nothing (`m`/
    /// `Enter`/`R` have no arm there) — the bar must advertise the keys that
    /// actually work in that pane (`n/N` message, `p/P` profile, `[/]` page),
    /// not the list pane's own bar.
    #[test]
    fn inbox_hints_show_the_view_pane_bar_once_it_has_focus() {
        let mut s = sample_inbox_state();
        let list_hints = inbox_hints(&s);
        assert!(list_hints.keys.iter().any(|(_, d)| *d == "new message"));

        s.focus = InboxPane::View;
        assert!(s.view.is_some(), "test setup: sample state has a view open");
        let view_hints = inbox_hints(&s);
        let names: Vec<&str> = view_hints.keys.iter().map(|(k, _)| *k).collect();
        assert!(names.contains(&"n/N"), "{names:?}");
        assert!(names.contains(&"p/P"), "{names:?}");
        assert!(names.contains(&"[/]"), "{names:?}");
        assert!(
            !view_hints.keys.iter().any(|(_, d)| *d == "new message"),
            "the list pane's own bar must not show once the view has focus: {names:?}"
        );
    }

    /// `r` replies to whatever the view pane has open, so with `view: None`
    /// (a narrow terminal, or the Alerts tab where nothing auto-primes the
    /// pane) the cap must vanish — advertising it there was a silent no-op
    /// (issue class #561/#605).
    #[test]
    fn inbox_hints_hide_reply_while_no_conversation_is_open() {
        let mut s = sample_inbox_state();
        assert!(inbox_hints(&s).keys.iter().any(|(k, _)| *k == "r"));

        s.view = None;
        assert!(
            !inbox_hints(&s).keys.iter().any(|(k, _)| *k == "r"),
            "no open conversation means no reply cap"
        );
        // The key itself stays inert — it is simply no longer advertised.
        assert!(matches!(inbox_key(&mut s, key('r')), Action::None));
    }

    /// Issue #601: the alert kind glyph must come from `action` (XF's real
    /// kind column), not `content_type` (the CONTENT's type, `post`/
    /// `trophy`/`user`, which never contains "quote"/"mention" in real data)
    /// — shapes taken straight from `xf_user_alert` live rows.
    #[test]
    fn alert_glyph_is_chosen_from_action_not_content_type() {
        let g = crate::glyph::UNICODE;
        let alert = |content_type: &str, action: &str| Alert {
            alert_id: 1,
            username: "kemical".into(),
            content_type: content_type.into(),
            action: action.into(),
            view_date: 0,
            ..Default::default()
        };

        let mut s = InboxState {
            tab: InboxTab::Alerts,
            alerts: AlertsState { alerts: vec![alert("post", "quote")], ..Default::default() },
            ..Default::default()
        };
        let rows = render_rows_inbox(&mut s, 120, 36);
        assert!(
            rows.join("\n").contains(g.quote_alert),
            "content_type=post, action=quote must render the quote glyph"
        );

        let mut s = InboxState {
            tab: InboxTab::Alerts,
            alerts: AlertsState { alerts: vec![alert("user", "mention")], ..Default::default() },
            ..Default::default()
        };
        let rows = render_rows_inbox(&mut s, 120, 36);
        assert!(
            rows.join("\n").contains(g.mention),
            "content_type=user, action=mention must render the mention glyph"
        );

        // A real "trophy award" row must NOT pick up the quote/mention
        // glyph off `content_type` alone.
        let mut s = InboxState {
            tab: InboxTab::Alerts,
            alerts: AlertsState { alerts: vec![alert("trophy", "award")], ..Default::default() },
            ..Default::default()
        };
        let rows = render_rows_inbox(&mut s, 120, 36);
        let text = rows.join("\n");
        assert!(!text.contains(g.quote_alert), "trophy/award must not render the quote glyph");
        assert!(!text.contains(g.mention), "trophy/award must not render the mention glyph");
    }

    /// Issue #578: the view panel's header used to append "ago"
    /// unconditionally (`"started {fmt_age} ago"`), which only reads right
    /// on `fmt_age`'s relative rungs (`14h`, `6d`). Past 30 days the ladder
    /// falls back to a calendar date or a bare year, and "started Jul 26
    /// ago" / "started 2025 ago" is nonsense. A conversation whose first
    /// message is 90 days old must read "started on <date>" instead.
    #[test]
    fn inbox_view_header_says_started_on_for_a_message_past_the_relative_rungs() {
        let now = time::OffsetDateTime::now_utc();
        let ninety_days_ago = now.unix_timestamp() - 90 * 86_400;
        // Confirm the fixture actually lands on a calendar/year rung, not a
        // relative one — otherwise this test would pass for the wrong
        // reason regardless of the fix.
        let (age_text, is_relative) = crate::theme::fmt_age_parts(ninety_days_ago);
        assert!(!is_relative, "test setup: 90 days must land past the relative rungs");

        let mut s = sample_inbox_state();
        if let Some(view) = &mut s.view {
            view.messages[0].message_date = ninety_days_ago;
        }
        let rows = render_rows_inbox(&mut s, 120, 36);
        let text = rows.join("\n");
        assert!(
            text.contains(&format!("started on {age_text}")),
            "expected calendar-date wording: {text}"
        );
        assert!(
            !text.contains(&format!("started {age_text} ago")),
            "must not say '... ago' once the ladder is past its relative rungs: {text}"
        );

        // Issue #583: the same "ago" tail is also wrong at the OTHER end of
        // the ladder — the under-a-minute "now" rung produces "started now
        // ago", reachable the moment a member opens a conversation created
        // within the last minute.
        let five_seconds_ago = now.unix_timestamp() - 5;
        let (now_text, now_is_relative) = crate::theme::fmt_age_parts(five_seconds_ago);
        assert_eq!(now_text, "now", "test setup: must land on the under-a-minute rung");
        assert!(now_is_relative);

        let mut s = sample_inbox_state();
        if let Some(view) = &mut s.view {
            view.messages[0].message_date = five_seconds_ago;
        }
        let rows = render_rows_inbox(&mut s, 120, 36);
        let text = rows.join("\n");
        assert!(text.contains("started just now"), "expected \"just now\" wording: {text}");
        assert!(!text.contains("started now ago"), "\"started now ago\" is nonsense: {text}");
    }

    /// Issue #582: `Msg::ConversationLoaded` replaces `view.messages`
    /// wholesale with whichever page was requested, so on page 2+ the first
    /// loaded message is NOT when the conversation started — it's just
    /// wherever the pager landed. The header must read XF's own
    /// `start_date` instead of re-deriving "started" from the current
    /// page's first message.
    #[test]
    fn inbox_view_header_uses_start_date_not_the_current_pages_first_message() {
        let now = time::OffsetDateTime::now_utc();
        let ninety_days_ago = now.unix_timestamp() - 90 * 86_400;
        let five_seconds_ago = now.unix_timestamp() - 5;
        let (age_text, _) = crate::theme::fmt_age_parts(ninety_days_ago);

        let mut s = sample_inbox_state();
        if let Some(view) = &mut s.view {
            // The conversation really started 90 days ago...
            view.conversation.start_date = ninety_days_ago;
            // ...but page 2's first loaded message is recent — a year-old
            // thread paged to its latest messages, exactly the repro.
            view.page = 2;
            view.messages[0].message_date = five_seconds_ago;
        }
        let rows = render_rows_inbox(&mut s, 120, 36);
        let text = rows.join("\n");
        assert!(
            text.contains(&format!("started on {age_text}")),
            "expected the header to read the conversation's start_date: {text}"
        );
        assert!(
            !text.contains("started now ago") && !text.contains("started just now"),
            "must not derive \"started\" from page 2's first message: {text}"
        );
    }

    #[test]
    fn inbox_renders_one_panel_when_narrow() {
        let mut s = sample_inbox_state();
        let rows = render_rows_inbox(&mut s, 80, 24);
        assert!(!s.dual, "80 cols should not be dual");
        let text = rows.join("\n");
        assert!(text.contains("Conversations"), "{text}");
        // No inline view pane at this width, so its right segment is absent.
        assert!(!text.contains("r reply"), "{text}");
    }

    #[test]
    fn tab_key_switches_tabs_from_the_list_and_returns_from_the_view() {
        let mut s = InboxState::default();
        assert_eq!(s.tab, InboxTab::Conversations);
        inbox_key(&mut s, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(s.tab, InboxTab::Alerts);
        inbox_key(&mut s, KeyEvent::new(KeyCode::BackTab, KeyModifiers::NONE));
        assert_eq!(s.tab, InboxTab::Conversations);

        s.view = Some(ConversationViewState::default());
        s.focus = InboxPane::View;
        inbox_key(&mut s, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(s.focus, InboxPane::List, "Tab from the view returns to the list");
        assert_eq!(s.tab, InboxTab::Conversations, "Tab from the view must not also flip tabs");

        s.focus = InboxPane::View;
        inbox_key(&mut s, KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE));
        assert_eq!(s.focus, InboxPane::List, "h also returns focus from the view");
    }

    #[test]
    fn enter_opens_a_conversation_and_marks_an_alert_read_by_tab() {
        let mut s = sample_inbox_state();
        let act = inbox_key(&mut s, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(act, Action::OpenConversation(c) if c.conversation_id == 3));

        s.tab = InboxTab::Alerts;
        s.alerts.alerts = vec![Alert {
            alert_id: 9,
            username: "kemical".into(),
            content_type: "post".into(),
            action: "quote".into(),
            ..Default::default()
        }];
        let act = inbox_key(&mut s, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(act, Action::MarkAlertRead(id) if id == 9));
    }

    #[test]
    fn n_and_r_work_from_the_list_regardless_of_active_tab() {
        let mut s = sample_inbox_state();
        s.tab = InboxTab::Alerts;
        let act = inbox_key(&mut s, KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
        assert!(matches!(act, Action::StartNewConversation(None)));

        let act = inbox_key(&mut s, KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE));
        assert!(matches!(act, Action::StartReplyConversation(c) if c.conversation_id == 3));
    }

    #[test]
    fn m_marks_read_on_whichever_tab_is_active() {
        let mut s = sample_inbox_state();
        let act = inbox_key(&mut s, KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE));
        assert!(matches!(act, Action::MarkConversationRead(id) if id == 3));

        s.tab = InboxTab::Alerts;
        s.alerts.alerts = vec![Alert {
            alert_id: 9,
            username: "kemical".into(),
            content_type: "post".into(),
            action: "quote".into(),
            ..Default::default()
        }];
        let act = inbox_key(&mut s, KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE));
        assert!(matches!(act, Action::MarkAlertRead(id) if id == 9));
    }

    #[test]
    fn conversation_participants_and_view_actions() {
        let conv = Conversation {
            conversation_id: 55,
            title: "Security Review".into(),
            start_user_id: 1,
            start_username: "Alice".into(),
            recipients: vec![
                ConversationRecipient {
                    user_id: 2,
                    username: "Bob".into(),
                },
                ConversationRecipient {
                    user_id: 3,
                    username: "Charlie".into(),
                },
            ],
            ..Default::default()
        };

        let messages = vec![
            ConversationMessage {
                message_id: 501,
                conversation_id: 55,
                user_id: 1,
                username: "Alice".into(),
                message: "Shall we audit the firewall?".into(),
                ..Default::default()
            },
            ConversationMessage {
                message_id: 502,
                conversation_id: 55,
                user_id: 4,
                username: "Dave_Auditor".into(),
                message: "I can help review the logs.".into(),
                ..Default::default()
            },
        ];

        let mut state = ConversationViewState {
            conversation: conv,
            messages,
            page: 1,
            last_page: 1,
            ..Default::default()
        };

        // Check that all participants include starter, recipients, and new message senders
        let all = state.all_participants();
        assert_eq!(all, vec!["Alice", "Bob", "Charlie", "Dave_Auditor"]);
        assert_eq!(
            state.participants_display(),
            "Alice (starter), Bob, Charlie, Dave_Auditor"
        );

        // The standalone screen now draws the same pre-wrapped message cards
        // the Inbox pane does (issue #528); the participant roll-call it used
        // to bake into `lines` is the panel's own persistent header.
        let theme = Theme::dark();
        state.rebuild_message_lines(&theme, &crate::glyph::UNICODE, 70);
        let text: String = state
            .lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert!(text.contains("Alice"));
        assert!(text.contains("Dave_Auditor"));

        // 'p': open profile of active message author (msg 0: Alice)
        let act = conversation_view_key(
            &mut state,
            KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE),
        );
        assert!(matches!(act, Action::OpenProfile(uid, name) if uid == 1 && name == "Alice"));

        // 'n': advance to next message (msg 1: Dave_Auditor)
        let act = conversation_view_key(
            &mut state,
            KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE),
        );
        assert!(matches!(act, Action::None));
        assert_eq!(state.sel_msg, 1);

        // 'p': open profile of Dave_Auditor
        let act = conversation_view_key(
            &mut state,
            KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE),
        );
        assert!(matches!(act, Action::OpenProfile(uid, name) if uid == 4 && name == "Dave_Auditor"));

        // 'P': open starter profile (Alice)
        let act = conversation_view_key(
            &mut state,
            KeyEvent::new(KeyCode::Char('P'), KeyModifiers::SHIFT),
        );
        assert!(matches!(act, Action::OpenProfile(uid, name) if uid == 1 && name == "Alice"));

        // 'r': start reply to conversation
        let act = conversation_view_key(
            &mut state,
            KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE),
        );
        assert!(matches!(act, Action::StartReplyConversation(c) if c.conversation_id == 55));
    }
}
