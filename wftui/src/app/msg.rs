//! `Msg` handling: every async result comes back here (see `mod.rs`).
//!
//! Split out of `app.rs` (#714), where `handle_msg` alone was 1,220 lines
//! inside a 4,175-line `impl App`. A continuation `impl App` block, so no
//! type, signature or call site changed.

use super::*;

impl App {
    pub(super) fn handle_msg(&mut self, mut msg: Msg) {
        // A stored-session check from a session that has since ended (the
        // user pressed Ctrl+L while "Restoring session…" was in flight) must
        // not sign anyone back in against an erased token store — issue #557,
        // the same generation discipline the Login* messages use.
        if let Msg::Bootstrap { generation, .. } = &msg
            && *generation != self.bootstrap_generation
        {
            return;
        }
        // The recheck this window belongs to reports back with exactly one
        // `Msg::Bootstrap` under this generation — `Ok` on success, or
        // `Err(NoToken)` when there was nothing new to adopt (issue #587:
        // deliberately NOT `Msg::SessionLost`, so that message shape is free
        // to mean "a poller's report" below). Only that message may close
        // the window: a poller's own `Msg::SessionLost(NoToken | OAuth)`
        // landing inside it describes the same grant the recheck is already
        // replacing (round 7, issue #580) and must be swallowed as stale by
        // the `session_recovery_pending` check further down, not mistaken
        // here for the recheck's own verdict.
        if matches!(msg, Msg::Bootstrap { .. }) {
            self.session_recovery_pending = false;
            self.session_recovery_started_at = None;
        }
        // The one session boundary: any background failure that means the
        // stored session itself is gone ends it here, rather than each
        // handler stashing "not logged in" in its own panel while the header
        // keeps showing a user who is no longer signed in (issue #557).
        if let Some((reason, kind)) =
            session_error_of(&msg).map(|err| (err.message.clone(), err.kind))
        {
            let is_bootstrap = matches!(msg, Msg::Bootstrap { .. });
            // A live session that lost its token may just be sharing
            // `token.json` with a second instance that rotated it: re-read
            // the store once before giving up on the session.
            //
            // Issue #593: XenForo's refresh grant revokes the OLD access
            // token in the SAME request, so a sibling's refresh can surface
            // here as `Api(401)` — a request already in flight, or one sent
            // during the clock-skew window before this instance's own 60 s
            // margin trips — just as easily as it surfaces as the `NoToken`
            // the refresh call itself produces. Treat the two identically:
            // this cannot mask a genuine revoke, because the recheck's own
            // `/me` (issue #581) verifies identity before anything is
            // adopted, and `adopt_stored_tokens()` returning false (nothing
            // new on disk) still reports `Err(NoToken)` and ends the session
            // exactly as before.
            let recheckable = matches!(kind, TaskErrorKind::NoToken | TaskErrorKind::Api(401));
            if recheckable && self.me.is_some() && !self.session_recovery_tried {
                self.recheck_stored_session(reason);
                // The recheck's own report is the recovery machinery's
                // business and has no owning screen — consume it here.
                if is_bootstrap {
                    return;
                }
                // Everything else belongs to a screen that is waiting on it:
                // hand it on as a retryable failure (issue #588). See
                // `mark_retryable`.
                mark_retryable(&mut msg);
            } else if self.session_recovery_pending
                && matches!(kind, TaskErrorKind::OAuth | TaskErrorKind::NoToken | TaskErrorKind::Api(401))
            {
                // Every caller that was queued behind the refresh which
                // failed reports the same rejection, so an OAuth error, a
                // second NoToken (once `valid_token` clears the in-memory
                // guard for every other queued caller — round 7 regression,
                // issue #580), or an `Api(401)` from the revoked old access
                // token (issue #593) arriving while the recheck is still in
                // flight describes the grant that recheck is already
                // replacing — stale. Ending the session on it would kick a
                // member whose sibling instance merely rotated the shared
                // `token.json` (issue #568); the recheck's own outcome —
                // always a `Msg::Bootstrap`, `Ok` or `Err` (issue #587) —
                // decides, one way or the other. (`Msg::Bootstrap` can never
                // reach here: it cleared the window just above.) The message
                // is still delivered, retryable, to the screen that owns it
                // (issue #588).
                mark_retryable(&mut msg);
            } else {
                self.end_session(&format!("Session expired ({reason}); log in again."));
                // Issue #600: a load already in flight when the session
                // ended (e.g. `ForumLoaded` for the Home list that `Screen`
                // retain above just kept) still needs its `Err` arm to run
                // so `loading` clears rather than sticking forever — same
                // discipline as the recheckable branches above (issue #588).
                // `Msg::Bootstrap` is the one exception: its `Err` arm is
                // documented as unreachable once the session boundary has
                // already handled a session-ending failure (issue #557).
                if is_bootstrap {
                    return;
                }
                mark_retryable(&mut msg);
            }
        }
        match msg {
            Msg::LoginReady { generation, url } => {
                if generation != self.login_generation {
                    return; // a flow the user already restarted (issue #547)
                }
                // Hand the short link over every channel we have: OSC 8 on
                // screen (rendered by the login screen), OSC 52 clipboard,
                // and a file for plain `cat`.
                emit_raw(&common::osc::set_clipboard(&url));
                // Beside the client's own store, not `config::token_path()`:
                // a test client is pinned to a scratch dir and must not be
                // able to write into the real config dir (issue #565). 0600
                // like the rest of the store's files (#655) — the URL is
                // not a credential, but a shared machine has no business
                // reading it either.
                let path = self.client.store_path().with_file_name("login-url.txt");
                #[allow(unused_mut)]
                let mut opts = std::fs::OpenOptions::new();
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt;
                    opts.mode(0o600);
                }
                let _ = opts.write(true).create(true).truncate(true).open(&path)
                    .and_then(|mut f| std::io::Write::write_all(&mut f, url.as_bytes()));
                self.set_hint("Login link → clipboard + login-url.txt");
                if let Some(Screen::Login(ls)) = self.screens.last_mut() {
                    ls.busy = false;
                    ls.url = url;
                    ls.stage = crate::screens::LoginStage::Waiting;
                }
            }
            Msg::LoginFailed { generation, message } => {
                if generation != self.login_generation {
                    return; // the restarted flow owns the screen now
                }
                self.login_task = None;
                if let Some(Screen::Login(ls)) = self.screens.last_mut() {
                    ls.busy = false;
                    ls.error = Some(message);
                    if matches!(
                        ls.stage,
                        crate::screens::LoginStage::Waiting
                    ) {
                        ls.stage = crate::screens::LoginStage::Idle;
                    }
                }
            }
            Msg::LoginComplete { generation, result } => {
                if generation != self.login_generation {
                    return; // a superseded flow must not sign anyone in
                }
                self.login_task = None;
                match result {
                    Ok(user) => {
                        self.me = Some(user);
                        if self.screens.len() > 1
                            && matches!(self.screens.last(), Some(Screen::Login(_)))
                        {
                            self.screens.pop();
                        }
                        self.set_status(format!(
                            "Welcome, {}.",
                            self.me.as_ref().map(|u| u.username.as_str()).unwrap_or("")
                        ));
                        self.start_pollers();
                        self.load_nodes();
                        self.prime_home_list();
                    }
                    Err(e) => {
                        // Issue #594: `finish_login` persists the exchanged
                        // tokens BEFORE calling `client.me()` — so a
                        // transient failure on that single verification call
                        // (a timeout, a Cloudflare 5xx, an XF 500) must not
                        // force the user through an entirely new browser
                        // authorization when the tokens it just stored
                        // already work: quitting and restarting proves it by
                        // going straight through `bootstrap`/`restore_session`
                        // silently. A session-ending rejection (bad grant) or
                        // an account-gone one (banned/rejected/disabled) is
                        // not this case — the Login screen's own error is
                        // still the right place for those.
                        if !e.ends_session() && !e.is_account_gone() {
                            if self.screens.len() > 1
                                && matches!(self.screens.last(), Some(Screen::Login(_)))
                            {
                                self.screens.pop();
                            }
                            self.arm_bootstrap_retry(&e);
                        } else if let Some(Screen::Login(ls)) = self.screens.last_mut() {
                            ls.busy = false;
                            ls.error = Some(e.message);
                            ls.stage = crate::screens::LoginStage::Idle;
                        }
                    }
                }
            }
            Msg::ImageLoaded { key, result } => {
                self.images.on_loaded(key, result);
            }
            Msg::PostToggled { verb, result } => {
                match result {
                    Ok(toggle) => {
                        self.set_status(verb.notice(toggle));
                        // The ♡/▲ counts live in the baked post lines, so the
                        // page has to come back from the server; keep the
                        // reader where they were while it does (issue #538).
                        // A toggle landing after its session ended (the abort
                        // cannot stop one already in the channel) must not
                        // reload anything as a signed-out client.
                        if self.me.is_some()
                            && let Some((id, page, sel_post, scroll)) =
                                open_thread_position(&self.screens)
                        {
                            self.keep_thread_position = Some((id, page, sel_post, scroll));
                            self.load_thread(id, page);
                        }
                    }
                    Err(e) => {
                        self.set_status(format!("{}: {}", verb.failure(), e.message));
                    }
                }
            }
            Msg::Notice(n) => {
                if n == "quit" {
                    self.should_quit = true;
                } else if let Some(v) = n.strip_prefix("alerts:") {
                    self.alerts_unread = v.parse().unwrap_or(0);
                } else if let Some(v) = n.strip_prefix("convos:") {
                    self.convos_unread = v.parse().unwrap_or(0);
                } else {
                    self.set_status(n);
                }
            }
            Msg::SessionLost(_) => {
                // Nothing left to do: the boundary above either ended the
                // session or rewrote this poller report as a stale,
                // retryable one (issue #588) — and a poller's failure has no
                // screen of its own waiting on it.
            }
            Msg::Bootstrap { result: Ok(user), .. } => {
                self.bootstrap_retry_needed = false;
                self.session_recovery_tried = false;
                self.session_recovery_pending = false;
                self.session_recovery_started_at = None;
                // Issue #581: the token recheck (issue #573) re-verifies
                // identity via `/me` before adopting a foreign token set,
                // but this arm used to act on it unconditionally — an open
                // Inbox showing the OLD user's DMs, a Compose draft written
                // as them, a write still waiting on the politeness gates,
                // all carried straight over to the new identity with only
                // "Session restored." A different `user_id` here means the
                // token store now holds a different account, not just a
                // rotated grant for the same one — tear the session-shaped
                // state down the way `end_session` would (short of actually
                // ending it: the new identity IS signed in).
                let previous_user_id = self.me.as_ref().map(|u| u.user_id);
                let identity_changed = previous_user_id.is_some_and(|id| id != user.user_id);
                if identity_changed {
                    self.abort_writes();
                    self.screens.retain(|s| matches!(s, Screen::Home(_) | Screen::ForumTree(_)));
                    if self.screens.is_empty() {
                        self.screens.push(screens::home_state(false));
                    }
                    self.keep_thread_position = None;
                    self.palette = None;
                    self.prefix = Prefix::default();
                    // Issue #590: unlike `end_session`, this teardown left
                    // the OLD identity's unread badges and its already-loaded
                    // Home thread list on screen under the NEW username —
                    // the header kept reading "Inbox [A's count] Alerts
                    // [A's count]" until the pollers' first tick (up to 90s),
                    // and `prime_home_list` below only loads when the list is
                    // already empty, so A's titles/unread marks (from nodes B
                    // may not even be allowed to see) just sat there.
                    self.alerts_unread = 0;
                    self.convos_unread = 0;
                    if let Some(Screen::Home(h)) = self.screens.first_mut() {
                        h.list = screens::ThreadListState::default();
                    }
                    // #715: and the saved drafts, which are unsent posts
                    // written as the old identity. Offering one back to a
                    // different account would be this comment's own
                    // "Compose draft written as them", one restart later.
                    self.clear_all_drafts();
                }
                let username = user.username.clone();
                self.me = Some(user);
                if identity_changed {
                    self.set_hint(format!(
                        "Token file changed elsewhere \u{2014} now signed in as {username}."
                    ));
                } else {
                    self.set_status("Session restored.");
                }
                self.start_pollers();
                if identity_changed {
                    // The pollers themselves sleep before their first fetch
                    // (45s/90s), so without this the zeroed badges above
                    // would just sit at 0 — not obviously wrong, but not the
                    // new identity's real counts either — for up to 90s.
                    self.poll_unread_now();
                }
                if matches!(self.screens.last(), Some(Screen::Login(_))) {
                    self.screens.pop();
                }
                self.load_nodes();
                self.prime_home_list();
            }
            Msg::Bootstrap { result: Err(e), .. } => {
                // Only a failure that means the stored session itself is
                // invalid (no token, OAuth refused, 401/403) may send the
                // user to the login screen — and that case never reaches
                // here, the session boundary at the top of `handle_msg` took
                // it (issue #557). What is left is transport failures and
                // server 5xx, which must not throw away a token that would
                // work the moment the network/server recovers (issue #551).
                if self.me.is_some() {
                    // Issue #589: `recheck_stored_session` (issue #557/#587)
                    // reports through this same arm from a LIVE session — a
                    // sibling instance merely rotated `token.json` and the
                    // recheck's own `/me` hit a transient failure (a
                    // timeout, a Cloudflare 5xx). This branch was written
                    // for the startup restore, where `self.me` is never set;
                    // here the session is still perfectly usable (the
                    // pollers are running on whatever token is held), so
                    // arming `bootstrap_retry_needed` would hijack the next
                    // plain `r` outside a text field into `restore_session()`
                    // instead of, say, opening the reply composer in a
                    // thread view, and `tree.error` would show a "press r to
                    // retry" banner over a working session. Just say so —
                    // the pollers (or the next call) will surface a real
                    // session loss if there is one.
                    //
                    // Issue #592: the poller that first noticed the `NoToken`
                    // and triggered this recheck already sent its
                    // `SessionLost` and returned (the alerts/conversations
                    // poller loops `return` right after reporting) — nothing
                    // else restarts it, so without this the Alerts/Inbox
                    // badges would freeze for the rest of the session. The
                    // recheck's `adopt_stored_tokens()` already ran (and
                    // succeeded — only its later `/me` failed transiently),
                    // so the adopted token is live and safe to poll on.
                    // `session_recovery_tried` must also go back to `false`:
                    // left `true`, the NEXT sibling rotation's `NoToken`
                    // would skip the recheck gate entirely and fall straight
                    // to `end_session` with a perfectly good token set on
                    // disk — re-arming the exact #557/#568 sign-out this
                    // machinery exists to prevent, off one network blip. This
                    // is safe against a loop: a recheck whose
                    // `adopt_stored_tokens()` finds nothing new to adopt
                    // reports `Err(NoToken)` and ends the session as usual.
                    self.start_pollers();
                    self.session_recovery_tried = false;
                    let username = self.me.as_ref().map(|u| u.username.as_str()).unwrap_or("");
                    self.set_hint(format!(
                        "Token re-check could not reach the site; still signed in as {username}."
                    ));
                } else {
                    self.arm_bootstrap_retry(&e);
                }
            }
            Msg::NodesLoaded(result) => {
                let tree = self.tree_mut();
                if let Some(tree) = tree {
                    match result {
                        Ok(nodes) => {
                            // See ForumLoaded (issue #537).
                            tree.error = None;
                            if tree.sel == 0
                                && !nodes.is_empty()
                                && let Some(idx) = nodes.iter().position(|n| n.node_type == "Forum")
                            {
                                tree.sel = idx;
                            }
                            tree.nodes = nodes;
                            tree.loading = false;
                            tree.sel = tree.sel.min(tree.nodes.len().saturating_sub(1));
                        }
                        Err(e) => {
                            tree.loading = false;
                            tree.error = Some(e.message);
                        }
                    }
                }
            }
            Msg::ForumLoaded { node_id, page, append, seq, result } => {
                let mut fill_next: Option<(u32, u32, u64)> = None;
                let list = self
                    .list_mut_for(node_id)
                    // #705: a reply for a load this list is no longer waiting
                    // on is not this list's reply. Clicking through forums
                    // fast left pages from the forum just left behind landing
                    // in the list that replaced it — appended into it, even,
                    // when the page number happened to line up.
                    .filter(|list| list.load_seq == seq);
                if let Some(list) = list {
                    match result {
                        Ok(reply) => {
                            // A previous failed load must not keep rendering
                            // "Error: ... Press r to retry" forever once a
                            // later load succeeds (issue #537).
                            list.error = None;
                            if !reply.forum.title.is_empty() {
                                list.title = reply.forum.title;
                            }
                            // Sticky threads arrive in their own array (XF
                            // excludes them from `threads`/`pagination` so
                            // they don't shift pagination); prepend them so
                            // they render first without inflating the count.
                            if append && page == list.page + list.pages_loaded {
                                // A fill page: extend, and never re-prepend
                                // the sticky rows (they belong to page 1).
                                list.threads.extend(reply.threads);
                                list.pages_loaded += 1;
                            } else {
                                list.sticky_count = reply.sticky.len();
                                list.threads = reply.sticky;
                                list.threads.extend(reply.threads);
                                list.page = page;
                                list.pages_loaded = 1;
                            }
                            list.last_page = reply.pagination.last_page.max(1);
                            list.total = reply.pagination.total;
                            if reply.pagination.per_page > 0 {
                                list.per_page = reply.pagination.per_page;
                            }
                            list.loading = false;
                            list.sel = list.sel.min(list.threads.len().saturating_sub(1));
                            // Top up while the pane has room and pages remain
                            // (#699). One page in flight at a time, so this
                            // walks forward a page per reply rather than
                            // firing a burst at the gate.
                            let rows = list.threads.len();
                            let next = list.page + list.pages_loaded;
                            // The automatic fill: top the pane up to what it
                            // can show. `visible` is zero until a render has
                            // stamped it for THIS list (#705) — filling
                            // against a figure measured on the previous
                            // screen is what walked a 233-page forum — and
                            // the budget bounds it even then.
                            if list.visible > 0
                                && rows < list.visible
                                && next <= list.last_page
                                && list.fill_budget > 0
                            {
                                list.fill_budget -= 1;
                                list.loading = true;
                                fill_next = Some((node_id, next, seq));
                            }
                        }
                        Err(e) => {
                            list.loading = false;
                            // A fill page that fails leaves what is already
                            // on screen alone: the rows the reader is looking
                            // at are still good.
                            if !append {
                                list.error = Some(e.message);
                            }
                        }
                    }
                }
                if let Some((node_id, page, seq)) = fill_next {
                    self.load_forum_page(node_id, page, true, seq);
                }
            }
            Msg::ThreadLoaded { id, page, result } => {
                // One-shot: a reload that only exists to refresh the ♡/▲
                // counts must not scroll the reader back to the top or move
                // the selection out from under the next `l`/`v` (issue #538).
                let keep = self
                    .keep_thread_position
                    .take()
                    .filter(|(kid, kpage, _, _)| *kid == id && *kpage == page);
                let view = self.screens.iter_mut().rev().find_map(|s| match s {
                    Screen::ThreadView(view) if view.thread.thread_id == id => Some(view),
                    _ => None,
                });
                if let Some(view) = view {
                    match result {
                        Ok(reply) => {
                            // See ForumLoaded: clear a stale error on success
                            // (issue #537).
                            view.error = None;
                            if reply.thread.thread_id > 0 || !reply.thread.title.is_empty() {
                                view.thread = reply.thread;
                            }
                            view.posts = reply.posts;
                            view.page = page;
                            view.last_page = reply.pagination.last_page.max(1);
                            view.total = reply.pagination.total;
                            view.loading = false;
                            view.scroll = 0;
                            view.sel_post = 0;
                            if let Some((_, _, sel_post, scroll)) = keep {
                                view.sel_post =
                                    sel_post.min(view.posts.len().saturating_sub(1));
                                view.scroll = scroll;
                            }
                            view.rebuild_lines(&self.theme, &self.glyphs);
                        }
                        Err(e) => {
                            view.loading = false;
                            // Clamp to the server-reported max page.
                            if let Some(max) = e.max_page {
                                view.last_page = max;
                                let id = view.thread.thread_id;
                                view.loading = true;
                                self.load_thread(id, max);
                            } else {
                                view.error = Some(e.message);
                            }
                        }
                    }
                }
            }
            Msg::ReplySent(result) => {
                let (compose_idx, target) = self
                    .screens
                    .iter()
                    .enumerate()
                    .rev()
                    .find_map(|(idx, s)| match s {
                        Screen::Compose(c) if matches!(c.target, Some(ComposeTarget::ThreadReply { .. })) => {
                            Some((idx, c.target.clone()))
                        }
                        _ => None,
                    })
                    .unzip();
                match result {
                    Ok(_) => {
                        let thread_id = match target.flatten() {
                            Some(ComposeTarget::ThreadReply { thread_id, .. }) => thread_id,
                            _ => 0,
                        };
                        if let Some(idx) = compose_idx {
                            self.close_sent_composer(idx);
                        }
                        self.set_status("Reply posted.");
                        if thread_id > 0 {
                            // Land on the page the new reply actually lands
                            // on, not page 1 (issue #529). The API gives no
                            // `per_page`, so this client can't compute that
                            // page from `post.position` alone — instead ask
                            // one past the last page we knew about and let
                            // `Msg::ThreadLoaded`'s existing `max_page` clamp
                            // (below) settle on the true last page, whether
                            // or not the reply pushed the thread onto a page
                            // that didn't exist a moment ago. A single-page
                            // thread clamps straight back to page 1.
                            let known_last = known_thread_last_page(&self.screens, thread_id);
                            self.load_thread(thread_id, known_last.saturating_add(1));
                        }
                    }
                    Err(e) => {
                        if let Some(idx) = compose_idx
                            && let Screen::Compose(compose) = &mut self.screens[idx]
                        {
                            compose.busy = false;
                            compose.error = Some(e.message);
                        } else {
                            // The composer is gone (popped, or the screen
                            // stack moved on): the failure has nowhere to
                            // render, so say it in the status line instead of
                            // dropping it (issue #520).
                            self.set_status(format!("Reply failed: {}", e.message));
                        }
                    }
                }
            }
            Msg::ThreadCreated(result) => {
                let compose_idx = self.screens.iter().rposition(|s| match s {
                    Screen::Compose(c) => matches!(c.target, Some(ComposeTarget::NewThread { .. })),
                    _ => false,
                });
                match result {
                    Ok(thread) => {
                        if let Some(idx) = compose_idx {
                            self.close_sent_composer(idx);
                        }
                        self.set_status("Thread created.");
                        // The list we are about to refresh may be showing a
                        // different forum (Home's pane, say) — point it at the
                        // new thread's forum before the reply lands in it.
                        let node_id = thread.node_id;
                        if let Some(list) = self.list_mut()
                            && list.node_id != node_id
                        {
                            list.node_id = node_id;
                            list.page = 1;
                            list.total = 0;
                        }
                        self.load_forum(node_id, 1);
                    }
                    Err(e) => {
                        if let Some(idx) = compose_idx
                            && let Screen::Compose(compose) = &mut self.screens[idx]
                        {
                            compose.busy = false;
                            compose.error = Some(e.message);
                        } else {
                            self.set_status(format!("Thread failed: {}", e.message));
                        }
                    }
                }
            }
            Msg::MarkedRead(Ok(())) => self.set_status("Marked read."),
            Msg::MarkedRead(Err(e)) => self.set_status(format!("Mark-read failed: {e}")),
            Msg::ConversationsLoaded { page, result } => {
                let mut new_unread: Option<u32> = None;
                let mut auto_load: Option<u32> = None;
                if let Some(inbox) = self.inbox_mut() {
                    match result {
                        Ok(reply) => {
                            // See ForumLoaded (issue #537).
                            inbox.convos.error = None;
                            inbox.convos.conversations = reply.conversations;
                            inbox.convos.page = page;
                            inbox.convos.last_page = reply.pagination.last_page.max(1);
                            inbox.convos.total = reply.pagination.total;
                            inbox.convos.loading = false;
                            inbox.convos.sel = inbox
                                .convos
                                .sel
                                .min(inbox.convos.conversations.len().saturating_sub(1));
                            new_unread = Some(common::models::count_unread_conversations(
                                &inbox.convos.conversations,
                            ));
                            // Prime the view pane with the first conversation
                            // so a dual Inbox never opens onto an empty right
                            // panel.
                            if inbox.dual
                                && inbox.view.is_none()
                                && inbox.tab == screens::InboxTab::Conversations
                                && let Some(first) = inbox.convos.conversations.first().cloned()
                            {
                                let cid = first.conversation_id;
                                inbox.view = Some(screens::ConversationViewState {
                                    conversation: first,
                                    page: 1,
                                    loading: true,
                                    ..Default::default()
                                });
                                auto_load = Some(cid);
                            }
                        }
                        Err(e) => {
                            inbox.convos.loading = false;
                            inbox.convos.error = Some(e.message);
                        }
                    }
                }
                if let Some(n) = new_unread {
                    self.convos_unread = n;
                }
                if let Some(cid) = auto_load {
                    // Primed, not opened: never mark this one read (#541).
                    self.load_conversation(cid, 1, false);
                }
            }
            Msg::ConversationLoaded { id, page, mark_read: user_opened, result } => {
                let mut mark_read: Option<u32> = None;
                if let Some(view) = self.conversation_view_mut(id) {
                    match result {
                        Ok(reply) => {
                            // See ForumLoaded/ThreadLoaded (issue #537).
                            view.error = None;
                            if reply.conversation.conversation_id > 0 {
                                view.conversation = reply.conversation;
                            }
                            view.messages = reply.messages;
                            view.page = page;
                            view.last_page = reply.pagination.last_page.max(1);
                            view.loading = false;
                            // A freshly loaded page is a different set of
                            // messages (paging, or a post-reply reload) —
                            // sel_msg/scroll from the previous page no longer
                            // refer to anything here. rebuild_message_lines
                            // also clamps defensively, but resetting here
                            // means the view opens at the top of the new page
                            // instead of some carried-over offset (issue #534).
                            view.sel_msg = 0;
                            view.scroll = 0;
                            // The lines are derived by the renderer (which is
                            // the only place that knows the pane width); this
                            // just invalidates them.
                            view.built = None;
                            if user_opened {
                                mark_read = Some(view.conversation.conversation_id);
                            }
                        }
                        Err(e) => {
                            view.loading = false;
                            // Clamp to the server-reported max page, exactly
                            // as `Msg::ThreadLoaded` does (issue #550) — a
                            // reply-triggered reload asking for "known last
                            // page + 1" lands here when that guess overshot.
                            if let Some(max) = e.max_page {
                                view.last_page = max;
                                view.loading = true;
                                self.load_conversation(id, max, user_opened);
                            } else {
                                view.error = Some(e.message);
                            }
                        }
                    }
                }
                if let Some(cid) = mark_read {
                    // Keep the client's own picture in step with the server:
                    // the Inbox row keeps its unread glyph (and the header
                    // badge its count) otherwise, until some later poll
                    // happens to contradict them (issue #541).
                    if let Some(inbox) = self.inbox_mut() {
                        if let Some(row) = inbox
                            .convos
                            .conversations
                            .iter_mut()
                            .find(|c| c.conversation_id == cid)
                        {
                            row.is_unread = false;
                            row.conversation_unread = false;
                        }
                        let unread =
                            common::models::count_unread_conversations(&inbox.convos.conversations);
                        self.convos_unread = unread;
                    }
                    let api = self.api.clone();
                    let tx = self.tx.clone();
                    tokio::spawn(async move {
                        let _ = api.mark_conversation_read(cid).await;
                        let _ = tx;
                    });
                }
            }
            Msg::ConvoReplySent(result) => {
                let (compose_idx, target) = self
                    .screens
                    .iter()
                    .enumerate()
                    .rev()
                    .find_map(|(idx, s)| match s {
                        Screen::Compose(c) if matches!(c.target, Some(ComposeTarget::ConversationReply { .. })) => {
                            Some((idx, c.target.clone()))
                        }
                        _ => None,
                    })
                    .unzip();
                match result {
                    Ok(()) => {
                        let cid = match target.flatten() {
                            Some(ComposeTarget::ConversationReply {
                                conversation_id, ..
                            }) => conversation_id,
                            _ => 0,
                        };
                        if let Some(idx) = compose_idx {
                            self.close_sent_composer(idx);
                        }
                        self.set_status("Message sent.");
                        if cid > 0 {
                            // Mirror the thread-reply reload (issue #529): a
                            // conversation with more than one page of
                            // messages must not jump back to page 1 and hide
                            // the reply just sent. Ask one past the last
                            // page we knew about and let the `max_page`
                            // clamp in `Msg::ConversationLoaded` settle on
                            // the true last page (issue #550).
                            let known_last = known_conversation_last_page(&self.screens, cid);
                            self.load_conversation(cid, known_last.saturating_add(1), true);
                        }
                    }
                    Err(e) => {
                        if let Some(idx) = compose_idx
                            && let Screen::Compose(compose) = &mut self.screens[idx]
                        {
                            compose.busy = false;
                            compose.error = Some(e.message);
                        } else {
                            self.set_status(format!("Message failed: {}", e.message));
                        }
                    }
                }
            }
            Msg::RecipientResolved { name, id } => {
                let nc = self.screens.iter_mut().rev().find_map(|s| match s {
                    Screen::NewConversation(nc) => Some(nc),
                    _ => None,
                });
                let done = if let Some(nc) = nc.filter(|nc| nc.resolving > 0) {
                    // `resolving == 0` means nobody is waiting on this answer
                    // (the form was already handed back, or the write is
                    // already in flight) — a stale report must never spawn a
                    // second `create_conversation` (issue #597).
                    nc.resolving = nc.resolving.saturating_sub(1);
                    match id {
                        Ok(Some(id)) => nc.resolved_ids.push(id),
                        Ok(None) => nc.errors.push(format!("{name}: not found")),
                        // A failed lookup is not an absent member (issue
                        // #597): saying "not found" for a 500 or a dropped
                        // connection sends the user renaming a correct
                        // recipient.
                        Err(e) => nc
                            .errors
                            .push(format!("{name}: lookup failed \u{2014} {}", e.message)),
                    }
                    nc.resolving == 0
                } else {
                    false
                };
                if done {
                    let nc = self.screens.iter_mut().rev().find_map(|s| match s {
                        Screen::NewConversation(nc) => Some(nc),
                        _ => None,
                    });
                    let (ids, title, body) = if let Some(nc) = nc {
                        if nc.resolved_ids.is_empty() || !nc.errors.is_empty() {
                            // Nothing will be spawned: the form is the user's
                            // again.
                            nc.busy = false;
                            nc.sending = false;
                            (Vec::new(), String::new(), String::new())
                        } else {
                            // `busy` stays set from `submit()` right through
                            // the write (issue #597): the create waits on the
                            // api + write gates (up to 30 s), and in that
                            // window a second Enter used to queue a second
                            // conversation and Esc used to pop the screen out
                            // from under the in-flight write.
                            nc.sending = true;
                            (
                                nc.resolved_ids.clone(),
                                nc.title.clone(),
                                nc.body.clone(),
                            )
                        }
                    } else {
                        (Vec::new(), String::new(), String::new())
                    };
                    if !ids.is_empty() {
                        let api = self.api.clone();
                        let tx = self.tx.clone();
                        self.spawn_write(async move {
                            let result = api
                                .create_conversation(&ids, &title, &body)
                                .await
                                .map_err(|e| TaskError::of(&e));
                            tx.send(Msg::ConvoCreated(result)).ok();
                        });
                    }
                }
            }
            Msg::ConvoCreated(result) => match result {
                Ok(conv) => {
                    // Unwind back to the Inbox (or the root, if this started
                    // from somewhere that never opened one — e.g. a Profile's
                    // "message" action), then show the new conversation.
                    while self.screens.len() > 1
                        && !matches!(self.screens.last(), Some(Screen::Inbox(_)))
                    {
                        self.screens.pop();
                    }
                    self.set_status("Conversation started.");
                    if let Some(inbox) = self.inbox_mut() {
                        inbox.tab = screens::InboxTab::Conversations;
                    }
                    self.open_conversation(conv);
                }
                Err(e) => {
                    let nc = self.screens.iter_mut().rev().find_map(|s| match s {
                        Screen::NewConversation(nc) => Some(nc),
                        _ => None,
                    });
                    if let Some(new) = nc {
                        new.busy = false;
                        new.sending = false;
                        new.errors.push(e.message);
                    } else {
                        // The screen is gone (Esc raced the write, or the
                        // stack was unwound): the failure still has to be
                        // said out loud — same discipline as `ReplySent` /
                        // `ConvoReplySent` (issues #520/#597).
                        self.set_status(format!("Conversation failed: {}", e.message));
                    }
                }
            },
            Msg::AlertsLoaded(result) => {
                let mut new_unread: Option<u32> = None;
                if let Some(inbox) = self.inbox_mut() {
                    let alerts = &mut inbox.alerts;
                    match result {
                        Ok(page) => {
                            // See ForumLoaded (issue #537).
                            alerts.error = None;
                            new_unread =
                                Some(page.alerts.iter().filter(|a| !a.viewed()).count() as u32);
                            alerts.alerts = page.alerts;
                            alerts.loading = false;
                            alerts.sel = alerts.sel.min(alerts.alerts.len().saturating_sub(1));
                        }
                        Err(e) => {
                            alerts.loading = false;
                            alerts.error = Some(e.message);
                        }
                    }
                }
                if let Some(n) = new_unread {
                    self.alerts_unread = n;
                }
            }
            Msg::MediaLoaded { page, result } => {
                // Topmost gallery only; the screen's loading guard keeps one
                // fetch in flight, so `page` stamping suffices (#657 family).
                let gallery = self.screens.iter_mut().rev().find_map(|s| match s {
                    Screen::MediaGallery(m) => Some(m),
                    _ => None,
                });
                if let Some(m) = gallery
                    && m.loading
                {
                    match result {
                        Ok(reply) => {
                            // See ForumLoaded (issue #537).
                            m.error = None;
                            m.items = reply.media;
                            m.page = page;
                            m.last_page = reply.pagination.last_page.max(1);
                            m.total = reply.pagination.total;
                            m.loading = false;
                            m.sel = 0;
                        }
                        Err(e) => {
                            m.loading = false;
                            m.error = Some(e.message);
                        }
                    }
                }
            }
            Msg::ResourceLoaded { page, result } => {
                let resources = self.screens.iter_mut().rev().find_map(|s| match s {
                    Screen::Resources(r) => Some(r),
                    _ => None,
                });
                if let Some(r) = resources
                    && r.loading
                {
                    match result {
                        Ok(reply) => {
                            r.error = None;
                            r.items = reply.resources;
                            r.page = page;
                            r.last_page = reply.pagination.last_page.max(1);
                            r.total = reply.pagination.total;
                            r.loading = false;
                            r.sel = 0;
                        }
                        Err(e) => {
                            r.loading = false;
                            r.error = Some(e.message);
                        }
                    }
                }
            }
            Msg::AttachmentUploaded(result) => {
                let Some(Screen::Compose(c)) = self.screens.last_mut() else {
                    return;
                };
                c.uploading = false;
                match result {
                    Ok((key, attachment)) => {
                        // One key per draft, minted by the first file.
                        c.attachment_key = Some(key);
                        // The reference goes in at the caret, which is where
                        // the writer was: an attachment belongs to the
                        // sentence that mentions it, not to the end of the
                        // post.
                        let tag = format!("[ATTACH]{}[/ATTACH]", attachment.attachment_id);
                        let mut chars: Vec<char> = c.body.chars().collect();
                        let at = c.body_cursor.min(chars.len());
                        for (i, ch) in tag.chars().enumerate() {
                            chars.insert(at + i, ch);
                        }
                        c.body = chars.into_iter().collect();
                        c.body_cursor = at + tag.chars().count();
                        let name = attachment.filename.clone();
                        c.attachments.push(attachment);
                        self.set_status(format!("Attached {name}."));
                    }
                    Err(message) => {
                        c.error = Some(message);
                    }
                }
            }
            Msg::PostEdited { post_id, result } => match result {
                Ok(()) => {
                    // Close the editor and reload the page the post is on,
                    // so the reader sees what was actually saved rather than
                    // what they typed. The thread comes from the editor's own
                    // target, not from whatever thread view happens to be on
                    // the stack.
                    let mut thread_id = 0u32;
                    if let Some(idx) = self.screens.iter().rposition(|s| {
                        matches!(
                            s,
                            Screen::Compose(c)
                                if matches!(
                                    c.target,
                                    Some(ComposeTarget::EditPost { post_id: p, .. }) if p == post_id
                                )
                        )
                    }) {
                        if let Screen::Compose(c) = &self.screens[idx]
                            && let Some(ComposeTarget::EditPost { thread_id: t, .. }) = c.target
                        {
                            thread_id = t;
                        }
                        self.close_sent_composer(idx);
                    }
                    self.set_status("Post saved.");
                    let page = self
                        .screens
                        .iter()
                        .rev()
                        .find_map(|s| match s {
                            Screen::ThreadView(v) if v.thread.thread_id == thread_id => {
                                Some(v.page.max(1))
                            }
                            _ => None,
                        })
                        .unwrap_or(1);
                    if thread_id > 0 {
                        self.keep_thread_position = Some((thread_id, page, 0, 0));
                        self.load_thread(thread_id, page);
                    }
                }
                Err(e) => {
                    if let Some(Screen::Compose(c)) = self.screens.last_mut() {
                        c.busy = false;
                        c.error = Some(e.message.clone());
                    }
                }
            },
            Msg::PostDeleted { post_id, thread_id, result } => match result {
                Ok(()) => {
                    self.set_status("Post deleted.");
                    let page = self
                        .screens
                        .iter()
                        .rev()
                        .find_map(|s| match s {
                            Screen::ThreadView(v) if v.thread.thread_id == thread_id => {
                                Some(v.page.max(1))
                            }
                            _ => None,
                        })
                        .unwrap_or(1);
                    self.keep_thread_position = Some((thread_id, page, 0, 0));
                    self.load_thread(thread_id, page);
                    let _ = post_id;
                }
                Err(e) => self.set_status(format!("Delete failed: {}", e.message)),
            },
            Msg::SolutionMarked { post_id, thread_id, result } => match result {
                Ok(()) => {
                    self.set_status("Solution updated.");
                    let page = self
                        .screens
                        .iter()
                        .rev()
                        .find_map(|s| match s {
                            Screen::ThreadView(v) if v.thread.thread_id == thread_id => {
                                Some(v.page.max(1))
                            }
                            _ => None,
                        })
                        .unwrap_or(1);
                    self.keep_thread_position = Some((thread_id, page, 0, 0));
                    self.load_thread(thread_id, page);
                    let _ = post_id;
                }
                Err(e) => self.set_status(format!("Could not mark solution: {}", e.message)),
            },
            Msg::MediaCategoriesLoaded(result) => {
                // Categories are decoration for the item pane: a failure
                // leaves "All media" working rather than failing the screen.
                if let Ok(reply) = result
                    && let Some(Screen::MediaGallery(m)) =
                        self.screens.iter_mut().rev().find(|s| {
                            matches!(s, Screen::MediaGallery(_))
                        })
                {
                    m.categories = reply.categories;
                }
            }
            Msg::ResourceViewLoaded { id, result } => {
                let view = self.screens.iter_mut().rev().find_map(|s| match s {
                    Screen::ResourceView(r) if r.id == id => Some(r),
                    _ => None,
                });
                if let Some(view) = view {
                    match result {
                        Ok(reply) => {
                            view.error = None;
                            view.resource = Some(reply.resource);
                            view.loading = false;
                            // Force the layout: the page is built from the
                            // resource that just arrived.
                            view.width = 0;
                        }
                        Err(e) => {
                            view.loading = false;
                            view.error = Some(e.message);
                        }
                    }
                }
            }
            Msg::AlertMarked(result) => match result {
                Ok(()) => {
                    self.load_alerts();
                    self.set_status("Alert marked read.");
                }
                Err(e) => self.set_status(format!("Mark failed: {e}")),
            },
            Msg::ConversationMarked(id, result) => match result {
                Ok(()) => {
                    // Flip the row in place and recount the badge, exactly
                    // like the open-a-conversation path (issue #541) does —
                    // reloading page 1 unconditionally (the old behavior)
                    // threw the user back to page 1 (and a re-clamped
                    // selection) no matter which page they marked from
                    // (issue #608).
                    if let Some(inbox) = self.inbox_mut() {
                        if let Some(row) =
                            inbox.convos.conversations.iter_mut().find(|c| c.conversation_id == id)
                        {
                            row.is_unread = false;
                            row.conversation_unread = false;
                        }
                        self.convos_unread =
                            common::models::count_unread_conversations(&inbox.convos.conversations);
                    }
                    self.set_status("Conversation marked read.");
                }
                Err(e) => self.set_status(format!("Mark failed: {e}")),
            },
            Msg::SearchDone { generation, page, result } => {
                // Only the screen waiting on *this* load adopts the reply —
                // the topmost Search may belong to a newer query or member
                // (see ForumLoaded, issue #537).
                let search = self.screens.iter_mut().rev().find_map(|s| match s {
                    Screen::Search(search) if search.generation == generation => Some(search),
                    _ => None,
                });
                if let Some(search) = search {
                    match result {
                        Ok(reply) => {
                            // See ForumLoaded (issue #537).
                            search.error = None;
                            // `set_results` also derives the snippets, so the
                            // renderer never re-parses a post (issue #522).
                            search.set_results(reply.results);
                            search.page = page;
                            search.last_page = reply.pagination.last_page.max(1);
                            search.total = reply.pagination.total;
                            search.loading = false;
                            search.sel = 0;
                        }
                        Err(e) => {
                            search.loading = false;
                            search.error = Some(e.message);
                        }
                    }
                }
            }
            Msg::PaletteMember { query, user } => {
                if let Some(found) = user
                    && let Some(palette) = self.palette.as_mut()
                    && palette.query.trim() == query
                {
                    palette.push_member(&found);
                }
            }
            Msg::ProfileLoaded { generation, result } => {
                // Only the profile screen waiting on *this* request adopts
                // the reply — the topmost Profile may belong to a newer
                // `open_profile` (see ForumLoaded, issue #537).
                let profile = self.screens.iter_mut().rev().find_map(|s| match s {
                    Screen::Profile(profile) if profile.generation == generation => Some(profile),
                    _ => None,
                });
                if let Some(profile) = profile {
                    match result {
                        Ok(user) => {
                            // See ForumLoaded (issue #537).
                            profile.error = None;
                            profile.user = Some(user);
                            profile.loading = false;
                        }
                        Err(e) => {
                            profile.loading = false;
                            profile.error = Some(e.message);
                        }
                    }
                }
            }
            Msg::LoggedOut(result) => match result {
                Ok(()) => tracing::info!("logout complete"),
                Err(e) => {
                    // The local token file is gone either way; say what did
                    // not happen instead of leaving "Logged out." standing.
                    // This is the one place the user learns their 90-day
                    // refresh token may still be live server-side (issue
                    // #527), so it must not be a toast that
                    // `expire_status_toast` blanks after `STATUS_TOAST_SECS`
                    // — a status write that vanishes on its own clock while
                    // the user is reading the sign-in link box is the same
                    // silence #527 wanted fixed (issue #575). `set_hint`
                    // keeps it up until something else replaces it, and it
                    // is also mirrored onto the Login screen's own error
                    // line (cleared by `begin_login`) so it stays beside the
                    // sign-in box rather than only on the status row.
                    tracing::warn!("logout error: {e}");
                    let message = format!("Logged out locally, but {e}.");
                    self.set_hint(message.clone());
                    if let Some(Screen::Login(ls)) = self.screens.last_mut() {
                        ls.error = Some(message);
                    }
                }
            },
        }
    }
}
