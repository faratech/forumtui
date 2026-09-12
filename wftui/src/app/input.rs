//! Keyboard, mouse and selection handling.
//!
//! Split out of `app.rs` (#714). A continuation `impl App` block, so no
//! type, signature or call site changed.

use super::*;

impl App {
    pub(super) fn handle_key(&mut self, k: KeyEvent) {
        if k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('l') {
            // The sign-in screen is a gate, not a session to end (issue
            // #570): with no session already, Ctrl+L on the Login screen
            // has nothing to sign out of. `logout()` -> `end_session()`
            // used to tear the screen down and push a brand-new Idle one
            // anyway, discarding a short link the user was about to open on
            // their phone and leaving the still-running poll orphaned.
            if self.me.is_none() && matches!(self.screens.last(), Some(Screen::Login(_))) {
                return;
            }
            self.logout();
            return;
        }
        // A transient bootstrap failure (issue #551) leaves the session
        // unrestored with no login screen up; `r` re-runs the check instead
        // of falling through to whatever the Home screen would otherwise do
        // with it. `self.me.is_none()` is belt-and-braces (issue #589): the
        // `Msg::Bootstrap { Err }` arm no longer arms `bootstrap_retry_needed`
        // for a live session in the first place, but a live-session `r`
        // must never be hijacked into `restore_session()` even if that
        // invariant is ever broken elsewhere.
        if self.bootstrap_retry_needed
            && self.me.is_none()
            && k.modifiers.is_empty()
            && k.code == KeyCode::Char('r')
            && !self.input_active()
        {
            self.bootstrap_retry_needed = false;
            self.restore_session();
            return;
        }
        // The palette owns every key while it is up, including `?` and Esc.
        // The event is taken first so the arms can borrow `self` again.
        if let Some(event) = self.palette.as_mut().map(|p| p.key(k)) {
            match event {
                PaletteEvent::None => {}
                PaletteEvent::Close => {
                    self.palette = None;
                    self.status.clear();
                }
                PaletteEvent::Run(target) => {
                    self.palette = None;
                    self.status.clear();
                    self.run_palette_target(target);
                }
            }
            return;
        }
        if k.code == KeyCode::Char('?') && !self.input_active() {
            self.show_help = !self.show_help;
            return;
        }
        if self.show_help {
            // The card is modal like the palette: "any key closes" means the
            // key is CONSUMED by closing, never also dispatched to the
            // screen underneath — `q` used to quit the client and `j`/`k`
            // moved the hidden list through the card (#612).
            self.show_help = false;
            return;
        }
        // The key after `g` resolves the chord or cancels it; either way it is
        // consumed, so a mistyped chord never fires a stray command.
        if self.prefix.armed() {
            if let PrefixEvent::Go(target) = self.prefix.resolve(k) {
                self.go(target);
            }
            return;
        }
        let capture = self.screens.last().map(|s| s.input_capture()).unwrap_or(false);
        // `Ctrl+K` / `:` / `g` / `G` only when no input field owns the keyboard
        // (`^K` is BBCode `[ICODE]` in the composer) and only once there is a
        // session to navigate — the login screen is a gate, not a place.
        let plain = k.modifiers.difference(KeyModifiers::SHIFT).is_empty();
        if self.me.is_some() && !capture && !self.input_active() {
            let palette_key = (k.modifiers.contains(KeyModifiers::CONTROL)
                && k.code == KeyCode::Char('k'))
                || (plain && k.code == KeyCode::Char(':'));
            if palette_key {
                self.open_palette();
                return;
            }
            if plain && k.code == KeyCode::Char('g') {
                self.prefix.arm();
                return;
            }
            if plain && k.code == KeyCode::Char('G') {
                if let Some(screen) = self.screens.last_mut() {
                    screen.goto_bottom();
                }
                return;
            }
        }
        // Global navigation only when no input field owns the keyboard, and
        // only once there is a session — the sign-in screen is a gate, not a
        // place these can push screens over (issue #556).
        if self.me.is_some() && k.modifiers.is_empty() && !capture && !self.input_active() {
            match k.code {
                KeyCode::Char('c') => {
                    self.open_inbox(screens::InboxTab::Conversations);
                    return;
                }
                KeyCode::Char('a') => {
                    self.open_inbox(screens::InboxTab::Alerts);
                    return;
                }
                KeyCode::Char('s') | KeyCode::Char('/') => {
                    self.push_screen(screens::search_state());
                    return;
                }
                _ => {}
            }
        }
        if k.code == KeyCode::Esc && !capture {
            // Screens get first refusal (issue #520): a busy composer refuses
            // Esc out loud rather than being popped out from under an
            // in-flight post, and a screen with its own Esc arm (Search's
            // edit mode, the composers' discard) runs it.
            match self.screens.last().map(Screen::esc_intent) {
                Some(screens::EscIntent::Blocked(hint)) => {
                    self.set_hint(hint);
                    return;
                }
                Some(screens::EscIntent::Screen) => {
                    let action = match self.screens.last_mut() {
                        Some(screen) => screen.on_key(k),
                        None => Action::None,
                    };
                    self.execute_action(action);
                    return;
                }
                _ => {}
            }
            // Home is the root screen, so a bare Esc there would quit. In the
            // list pane it means "back to the forums", which is what Esc means
            // everywhere else in the client.
            if let Some(Screen::Home(h)) = self.screens.last_mut()
                && h.focus == screens::Pane::List
            {
                h.focus = screens::Pane::Tree;
                return;
            }
            // Same idea for the Inbox: Esc from the inline view pane returns
            // to the tabbed list rather than leaving the screen entirely.
            if let Some(Screen::Inbox(ib)) = self.screens.last_mut()
                && ib.focus == screens::InboxPane::View
            {
                ib.focus = screens::InboxPane::List;
                return;
            }
            if self.pop_screen() {
                self.status.clear();
            } else {
                self.should_quit = true;
            }
            return;
        }
        tracing::debug!("key: {:?} mods {:?}", k.code, k.modifiers);
        let action = match self.screens.last_mut() {
            Some(screen) => screen.on_key(k),
            None => Action::None,
        };
        self.execute_action(action);
    }

    /// Copy text via OSC 52 (remote terminal), local system clipboard tools, and in-app clipboard.
    pub fn copy_text(&mut self, text: &str) {
        emit_raw(&common::osc::set_clipboard(text));
        common::osc::copy_to_system_clipboard(text);
        self.clipboard = text.to_string();
    }

    // ---- login orchestration ----

    pub fn handle_paste(&mut self, text: String) {
        if text.is_empty() {
            return;
        }
        self.clipboard = text.clone();
        // The palette owns the keyboard while it is up, so it owns pastes too.
        if let Some(palette) = self.palette.as_mut() {
            let sanitized = crate::editor::normalize_control_chars(&text.replace(['\r', '\n'], " "));
            crate::editor::insert_str(&mut palette.query, &mut palette.cursor, &sanitized);
            palette.after_paste();
            return;
        }
        if let Some(screen) = self.screens.last_mut() {
            match screen {
                Screen::Compose(cs) => {
                    if cs.title_field {
                        let sanitized =
                            crate::editor::normalize_control_chars(&text.replace(['\r', '\n'], " "));
                        crate::editor::insert_str(&mut cs.title, &mut cs.title_cursor, &sanitized);
                    } else {
                        let sanitized = crate::editor::normalize_control_chars(&text.replace('\r', ""));
                        crate::editor::insert_str(&mut cs.body, &mut cs.body_cursor, &sanitized);
                    }
                    self.set_status(format!("Pasted {} characters", text.chars().count()));
                }
                Screen::Search(ss) => {
                    let sanitized = crate::editor::normalize_control_chars(&text.replace(['\r', '\n'], " "));
                    if ss.active_field == 1 {
                        crate::editor::insert_str(&mut ss.author, &mut ss.author_cursor, &sanitized);
                    } else {
                        crate::editor::insert_str(&mut ss.query, &mut ss.query_cursor, &sanitized);
                    }
                    self.set_status(format!("Pasted {} characters into search", text.chars().count()));
                }
                Screen::NewConversation(ncs) => {
                    match ncs.field {
                        0 => {
                            let sanitized = crate::editor::normalize_control_chars(
                                &text.replace(['\r', '\n'], " "),
                            );
                            crate::editor::insert_str(
                                &mut ncs.recipients,
                                &mut ncs.recipients_cursor,
                                &sanitized,
                            );
                        }
                        1 => {
                            let sanitized = crate::editor::normalize_control_chars(
                                &text.replace(['\r', '\n'], " "),
                            );
                            crate::editor::insert_str(
                                &mut ncs.title,
                                &mut ncs.title_cursor,
                                &sanitized,
                            );
                        }
                        _ => {
                            let sanitized =
                                crate::editor::normalize_control_chars(&text.replace('\r', ""));
                            crate::editor::insert_str(
                                &mut ncs.body,
                                &mut ncs.body_cursor,
                                &sanitized,
                            );
                        }
                    }
                    self.set_status(format!("Pasted {} characters", text.chars().count()));
                }
                _ => {
                    self.set_status(format!(
                        "Pasted {} characters into clipboard (press Ctrl+Y to insert)",
                        text.chars().count()
                    ));
                }
            }
        }
    }

    // ---- mouse: selection + multi-click + wheel scrolling ----

    /// Pointer and touch. Terminals deliver a tap as a left click, a
    /// two-finger scroll as a wheel and a long press as a right click, so
    /// there is one layer here for all three.
    ///
    /// Press+release in the same cell is a **click**: it resolves against the
    /// frame's `HitMap` and does what that target says. Anything that moves
    /// is a **drag**, which is the selection it always was — including across
    /// a list row, so a thread title is still copyable. The one thing a UI
    /// target changes is what a *repeat* press means: on text it is the
    /// double-click word / triple-click line select, on a row or a cap it is
    /// "open".
    pub(super) fn handle_mouse(&mut self, me: ratatui::crossterm::event::MouseEvent) {
        use ratatui::crossterm::event::{KeyCode, MouseButton, MouseEventKind as K};

        // `WFTUI_MOUSE=0`: nothing captured the mouse, so anything that
        // arrives here anyway (a terminal that reports without being asked)
        // is not ours to act on.
        if !self.hits.enabled() {
            return;
        }
        let pos = (me.column, me.row);
        match me.kind {
            K::Down(MouseButton::Left, ..) => {
                let now = std::time::Instant::now();
                let is_rapid = self.last_click_instant.is_some_and(|t| {
                    now.duration_since(t) < Duration::from_millis(400)
                }) && self.last_click_pos.0.abs_diff(pos.0) <= 2
                    && self.last_click_pos.1 == pos.1;

                if is_rapid {
                    self.click_count = (self.click_count % 3) + 1;
                } else {
                    self.click_count = 1;
                }
                self.last_click_instant = Some(now);
                self.last_click_pos = pos;
                self.press_pos = Some(pos);

                // On a UI target (a row, a cap, a tab, a link, a field) the
                // SECOND press means "open" — never a word select, which is
                // what a double click means on text. A single press starts
                // the band either way: dragging across a list row to copy a
                // thread title is still a selection, and the release decides
                // which gesture it was.
                let ui = self
                    .hits
                    .at(pos.0, pos.1)
                    .is_some_and(|h| !h.is_text_like());
                if ui && self.click_count >= 2 {
                    self.selection = None;
                    self.click_hit(pos, true);
                    return;
                }
                if ui {
                    self.selection = Some(Selection {
                        anchor: pos,
                        end: pos,
                    });
                    return;
                }

                if self.click_count == 2 {
                    // Double-click: select word
                    if let Some((start_col, end_col)) = self.find_word_bounds(pos.0, pos.1) {
                        self.selection = Some(Selection {
                            anchor: (start_col, pos.1),
                            end: (end_col, pos.1),
                        });
                        let text = self.extract_selection_text(start_col, pos.1, end_col, pos.1);
                        if !text.is_empty() {
                            let n = text.chars().count();
                            self.copy_text(&text);
                            self.set_status(format!(
                                "Copied word ({n} chars) to clipboard (and Ctrl+Y)"
                            ));
                        }
                    }
                } else if self.click_count == 3 {
                    // Triple-click: select line
                    if let Some((start_col, end_col)) = self.find_line_bounds(pos.1) {
                        self.selection = Some(Selection {
                            anchor: (start_col, pos.1),
                            end: (end_col, pos.1),
                        });
                        let text = self.extract_selection_text(start_col, pos.1, end_col, pos.1);
                        if !text.is_empty() {
                            let n = text.chars().count();
                            self.copy_text(&text);
                            self.set_status(format!(
                                "Copied line ({n} chars) to clipboard (and Ctrl+Y)"
                            ));
                        }
                    }
                } else {
                    self.selection = Some(Selection {
                        anchor: pos,
                        end: pos,
                    });
                }
            }
            K::Drag(MouseButton::Left, ..) => {
                if let Some(sel) = &mut self.selection {
                    sel.end = pos;
                }
            }
            K::Up(MouseButton::Left, ..) => {
                let press = self.press_pos.take();
                if self.click_count > 1 {
                    // The word/line select (or the double-click "open") ran
                    // on the press; the release has nothing left to do.
                    return;
                }
                let selection = self.selection.take();
                // Press and release in the same cell, with the band never
                // having left it, is a click — anything else is a drag.
                let still = selection.is_none_or(|sel| {
                    let (x0, y0, x1, y1) = sel.rect();
                    (x0, y0) == (x1, y1)
                });
                if press == Some(pos) && still {
                    self.click_hit(pos, false);
                    return;
                }
                if let Some(sel) = selection {
                    let (x0, y0, x1, y1) = sel.rect();
                    let text = self.extract_selection_text(x0, y0, x1, y1);
                    if !text.is_empty() {
                        let n = text.chars().count();
                        self.copy_text(&text);
                        self.set_status(format!(
                            "Copied {n} chars to your clipboard (and Ctrl+Y)"
                        ));
                    }
                }
            }
            // Long press on a phone terminal, right button on a desktop:
            // "open this on the site".
            K::Down(MouseButton::Right, ..) => self.right_click_hit(pos),
            K::ScrollUp => self.handle_wheel(pos, KeyCode::Up),
            K::ScrollDown => self.handle_wheel(pos, KeyCode::Down),
            _ => {}
        }
    }

    /// Act on the cell the pointer clicked, per the frame's hit map.
    ///
    /// Everything that has a key does it *through* `handle_key`, never by
    /// duplicating the handler: a click on a cap is a keypress, so every
    /// gate a keypress passes (a busy composer, the sign-in gate, an armed
    /// `g` chord, the write gate) applies to it unchanged.
    pub(super) fn click_hit(&mut self, pos: (u16, u16), double: bool) {
        let Some(hit) = self.hits.at(pos.0, pos.1).cloned() else {
            return;
        };
        match hit {
            Hit::CloseOverlay => self.close_overlay(),
            Hit::Key(k) | Hit::Cap(k) => {
                if let Some(key) = hit::key_event_for_label(k) {
                    self.handle_key(key);
                }
            }
            Hit::Badge(tab) => self.open_inbox(tab),
            Hit::Tab(tab) => {
                self.open_inbox(tab);
                if let Some(Screen::Inbox(inbox)) = self.screens.last_mut() {
                    inbox.focus = screens::InboxPane::List;
                }
            }
            Hit::PaletteRow(i) => {
                let target = self.palette.as_mut().and_then(|p| {
                    p.select(i);
                    p.selected().map(|item| item.target.clone())
                });
                if let Some(target) = target {
                    self.palette = None;
                    self.status.clear();
                    self.run_palette_target(target);
                }
            }
            Hit::Link(url) => {
                // A link row inside the thread view's popup closes it, the
                // same as Enter there.
                if let Some(Screen::ThreadView(v)) = self.screens.last_mut() {
                    v.link_popup = false;
                }
                self.open_url(&url);
            }
            // #710: clicking a play row plays THAT video — the post it
            // belongs to is selected first, the same rule images follow.
            Hit::Video(n) => {
                if let Some(post) = self.hits.post_at(pos.0, pos.1)
                    && let Some(screen) = self.screens.last_mut()
                {
                    screen.select_post(post);
                }
                let video = match self.screens.last() {
                    Some(Screen::ThreadView(v)) => {
                        screens::browse::post_videos(v.posts.get(v.sel_post))
                            .into_iter()
                            .nth(n)
                    }
                    _ => None,
                };
                if let Some((url, site)) = video {
                    self.execute_action(Action::PlayVideo {
                        url,
                        title: format!("{site} video"),
                    });
                }
            }
            Hit::Image(n) => {
                // Select the post the picture belongs to, then press its
                // digit: `1`-`9` is the one path that opens an attachment,
                // and it reads the SELECTED post (issue #542's rule).
                if let Some(post) = self.hits.post_at(pos.0, pos.1)
                    && let Some(screen) = self.screens.last_mut()
                {
                    screen.select_post(post);
                }
                if (1..=9).contains(&n)
                    && let Some(digit) = char::from_digit(n as u32, 10)
                {
                    self.handle_key(KeyEvent::new(
                        KeyCode::Char(digit),
                        KeyModifiers::NONE,
                    ));
                }
            }
            Hit::Field(field) => {
                if let Some(screen) = self.screens.last_mut() {
                    screen.click_field(field, pos.0, pos.1);
                }
            }
            Hit::Row(i) => {
                // The pane under the pointer takes the keyboard first, so the
                // index lands in the list the user is actually looking at.
                if let Some(screen) = self.screens.last_mut() {
                    screen.focus_pane_at(pos.0, pos.1);
                }
                let was = self.screens.last().and_then(Screen::selected_index);
                if let Some(screen) = self.screens.last_mut() {
                    screen.select_index(i);
                }
                // Clicking the row that was already selected, or a
                // double-click, is Enter — a first click on any other row
                // only moves the selection there.
                if double || was == Some(i) {
                    self.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
                }
            }
            Hit::Post(i) => {
                if let Some(screen) = self.screens.last_mut() {
                    screen.focus_pane_at(pos.0, pos.1);
                    screen.select_post(i);
                }
            }
            Hit::Crumb(index) => self.go_to_crumb(index),
            Hit::Pane(_) => {
                if let Some(screen) = self.screens.last_mut() {
                    screen.focus_pane_at(pos.0, pos.1);
                }
            }
        }
    }

    /// Right button / long press: open what is under the pointer on the site.
    /// A thread row opens that thread, a post opens the thread it is in, and
    /// a link is a link either way.
    pub(super) fn right_click_hit(&mut self, pos: (u16, u16)) {
        match self.right_click_url(pos) {
            Some(url) => self.open_url(&url),
            None => self.set_status("Nothing here to open on the site."),
        }
    }

    /// The URL a right click at `pos` means, moving the selection onto the
    /// row it landed on first (opening row 5 in a browser while row 2 stays
    /// selected would be its own bug). Split from `right_click_hit` so the
    /// resolution is testable without spawning the user's browser.
    pub(super) fn right_click_url(&mut self, pos: (u16, u16)) -> Option<String> {
        let hit = self.hits.at(pos.0, pos.1).cloned()?;
        match hit {
            Hit::Link(url) => Some(url),
            Hit::Row(i) => {
                if let Some(screen) = self.screens.last_mut() {
                    screen.focus_pane_at(pos.0, pos.1);
                    screen.select_index(i);
                }
                self.screens.last().and_then(Screen::web_url)
            }
            Hit::Post(i) => {
                if let Some(screen) = self.screens.last_mut() {
                    screen.select_post(i);
                }
                self.screens.last().and_then(Screen::web_url)
            }
            Hit::Pane(_) | Hit::Image(_) => self.screens.last().and_then(Screen::web_url),
            _ => None,
        }
    }

    /// Close whatever overlay is up, innermost first — a click off it means
    /// the same as the Esc that closes it.
    pub(super) fn close_overlay(&mut self) {
        if self.palette.is_some() {
            self.palette = None;
            self.status.clear();
            return;
        }
        if self.show_help {
            self.show_help = false;
            return;
        }
        if self.prefix.armed() {
            // Same path a stray key takes: armed -> cancelled, silently.
            self.prefix
                .resolve(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
            return;
        }
        if let Some(Screen::ThreadView(v)) = self.screens.last_mut() {
            v.link_popup = false;
        }
    }

    /// Three lines of scroll, aimed by the pointer (issue #549).
    ///
    /// Two things the old "synthesise three Up/Down keys through `handle_key`"
    /// got wrong: the dual-pane Home/Inbox layouts moved whichever pane had
    /// the keyboard, which is usually not the one under the mouse, and the
    /// three synthetic keys ran the overlay layer — closing the `?` card and
    /// cancelling an armed `g` chord. So the wheel focuses the pane it is over
    /// and then goes straight to the screen.
    pub(super) fn handle_wheel(&mut self, pos: (u16, u16), code: KeyCode) {
        let at = ratatui::layout::Position::new(pos.0, pos.1);
        if !self.body_rect.contains(at) {
            return; // the header band and the key/status rows do not scroll
        }
        // The palette is a list of its own and owns the keyboard while it is
        // up; the other overlays are not scrollable, and must survive a wheel.
        if self.palette.is_some() {
            for _ in 0..3 {
                self.handle_key(KeyEvent::new(code, KeyModifiers::NONE));
            }
            return;
        }
        if self.show_help || self.prefix.armed() {
            return;
        }
        if let Some(screen) = self.screens.last_mut() {
            screen.focus_pane_at(pos.0, pos.1);
        }
        for _ in 0..3 {
            let action = match self.screens.last_mut() {
                Some(screen) => screen.on_key(KeyEvent::new(code, KeyModifiers::NONE)),
                None => Action::None,
            };
            self.execute_action(action);
        }
    }

    pub(super) fn find_word_bounds(&self, x: u16, y: u16) -> Option<(u16, u16)> {
        let y_idx = y as usize;
        if y_idx >= self.screen_rows.len() || y_idx >= self.screen_cols.len() {
            return None;
        }
        let row = &self.screen_rows[y_idx];
        let offsets = &self.screen_cols[y_idx];
        if offsets.len() < 2 {
            return None;
        }
        let max_col = (offsets.len().saturating_sub(2)) as u16;
        let x = x.min(max_col);

        let get_char = |col: u16| -> Option<char> {
            let idx = col as usize;
            if idx + 1 >= offsets.len() {
                return None;
            }
            let s = offsets[idx];
            let e = offsets[idx + 1];
            row.get(s..e)?.chars().next()
        };
        // A column covered by the double-width character to its left carries
        // no bytes of its own (see `capture_screen`); stepping over those is
        // what lets a CJK word select as a word (issue #525).
        let is_continuation = |col: u16| -> bool {
            let idx = col as usize;
            idx + 1 < offsets.len() && offsets[idx] == offsets[idx + 1]
        };
        // Clicking the right half of a wide character means the character.
        let mut x = x;
        while x > 0 && is_continuation(x) {
            x -= 1;
        }

        let target = get_char(x)?;
        if target.is_whitespace() {
            return None;
        }
        let is_word_char = |c: char| {
            c.is_alphanumeric()
                || c == '_'
                || c == '-'
                || c == '.'
                || c == '/'
                || c == ':'
                || c == '@'
        };
        let target_is_word = is_word_char(target);

        let mut start_col = x;
        while start_col > 0 {
            let mut prev = start_col - 1;
            while prev > 0 && is_continuation(prev) {
                prev -= 1;
            }
            if let Some(c) = get_char(prev)
                && !c.is_whitespace()
                && is_word_char(c) == target_is_word
            {
                start_col = prev;
                continue;
            }
            break;
        }

        let mut end_col = x;
        while end_col < max_col {
            let mut next = end_col + 1;
            while next < max_col && is_continuation(next) {
                next += 1;
            }
            if let Some(c) = get_char(next)
                && !c.is_whitespace()
                && is_word_char(c) == target_is_word
            {
                end_col = next;
                continue;
            }
            break;
        }
        // Include the trailing half of a wide last character, so the band
        // covers what the eye sees and the byte slice ends after it.
        while end_col < max_col && is_continuation(end_col + 1) {
            end_col += 1;
        }

        Some((start_col, end_col))
    }

    pub(super) fn find_line_bounds(&self, y: u16) -> Option<(u16, u16)> {
        let y_idx = y as usize;
        if y_idx >= self.screen_rows.len() || y_idx >= self.screen_cols.len() {
            return None;
        }
        let offsets = &self.screen_cols[y_idx];
        if offsets.len() < 2 {
            return None;
        }
        let max_col = (offsets.len().saturating_sub(2)) as u16;
        Some((0, max_col))
    }

    pub(super) fn extract_selection_text(&self, x0: u16, y0: u16, x1: u16, y1: u16) -> String {
        if self.screen_rows.is_empty() {
            return String::new();
        }
        let max_y = (y1 as usize).min(self.screen_rows.len() - 1);
        let mut lines: Vec<String> = Vec::new();
        for y in (y0 as usize)..=max_y {
            let row = &self.screen_rows[y];
            let offsets = &self.screen_cols[y];
            let start = offsets[(x0 as usize).min(offsets.len() - 1)];
            let end = offsets[(x1 as usize + 1).min(offsets.len() - 1)];
            lines.push(row[start..end].trim_end().to_string());
        }
        while lines.last().is_some_and(|l| l.is_empty()) {
            lines.pop();
        }
        lines.join("\n")
    }

    /// Paint the live selection highlight over the finished frame.
    pub(super) fn paint_selection(&self, f: &mut Frame) {
        let Some(sel) = &self.selection else {
            return;
        };
        let (x0, y0, x1, y1) = sel.rect();
        let area = f.area();
        // Same band every other selected/focused row in the client uses
        // (issue #562): `Theme::selected()` already knows to reverse instead
        // of tint on Ansi16/Mono, where a hard-coded colour would paint
        // regardless of `NO_COLOR` — the one place this client violated
        // that contract.
        let style = self.theme.selected();
        for y in y0..=y1.min(area.height.saturating_sub(1)) {
            for x in x0..=x1.min(area.width.saturating_sub(1)) {
                let cell = &mut f.buffer_mut()[(x, y)];
                // The selection band must never touch an image: kitty encodes
                // the image id in the cell's foreground colour, so re-styling
                // the anchor cell would repaint a different image (or none).
                if is_image_cell(cell) {
                    continue;
                }
                cell.set_style(style);
            }
        }
    }

    pub fn input_active(&self) -> bool {
        // Login has no free-text field anymore (short link + polling), so
        // global keys like ? work there too.
        //
        // A Search screen only counts while a field owns the keyboard
        // (#617): on the results list `?`, the palette and `g g`/`G` were
        // swallowed, which made the screen's own goto_top/goto_bottom arms
        // unreachable. Compose/NewConversation are editors end to end.
        matches!(
            self.screens.last(),
            Some(Screen::Search(s)) if s.input_mode
        ) || matches!(
            self.screens.last(),
            Some(Screen::Compose(_)) | Some(Screen::NewConversation(_))
        ) || capture_active(self)
    }
}
