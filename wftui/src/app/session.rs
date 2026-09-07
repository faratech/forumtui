//! Session lifecycle: bootstrap, recovery, pollers, teardown, logout.
//!
//! Split out of `app.rs` (#714). A continuation `impl App` block, so no
//! type, signature or call site changed.

use super::*;

impl App {
    pub(super) async fn bootstrap(&mut self) {
        self.screens.push(screens::home_state(true));
        if self.client.has_tokens().await {
            self.restore_session();
        } else {
            // No session: start the browser login immediately — zero keys.
            self.screens.push(screens::login_state());
            self.begin_login();
        }
    }

    /// Spawn the `api.me()` check that restores (or invalidates) a stored
    /// session. Split out of `bootstrap` so a transient failure's "press r
    /// to retry" (issue #551) can re-run exactly this step without pushing a
    /// second Home screen.
    pub(super) fn restore_session(&mut self) {
        self.set_hint("Restoring session…");
        let api = self.api.clone();
        let tx = self.tx.clone();
        let generation = self.bootstrap_generation;
        tokio::spawn(async move {
            let result = api.me().await.map_err(|e| TaskError::of(&e));
            tx.send(Msg::Bootstrap { generation, result }).ok();
        });
    }

    /// A live session lost its token (`Error::NoToken`). Before ending it,
    /// re-read `token.json` once: XF rotates the refresh token on every
    /// refresh, so a second instance sharing the config dir invalidates this
    /// one's in-memory grant while leaving a perfectly good token set on
    /// disk. Either outcome reports back through `Msg::Bootstrap` under this
    /// recheck's `generation` — success as `Ok`, "nothing to adopt" as an
    /// `Err(NoToken)` that ends the session exactly as a bare `SessionLost`
    /// used to (issue #557). Reporting through `Bootstrap` rather than
    /// `SessionLost` is deliberate (issue #587): a poller's own
    /// `Msg::SessionLost(NoToken | OAuth)` arriving in this same window
    /// describes the very grant this recheck is already replacing, and must
    /// be swallowed as stale rather than ending the session out from under
    /// the recovery — see the boundary in `handle_msg` and
    /// `session_recovery_pending`. Distinguishing the two message shapes is
    /// what lets that swallow apply to a poller's report without also
    /// swallowing this recheck's own verdict.
    pub(super) fn recheck_stored_session(&mut self, reason: String) {
        self.session_recovery_tried = true;
        self.session_recovery_pending = true;
        self.session_recovery_started_at = Some(std::time::Instant::now());
        self.set_hint("Session token changed elsewhere — re-checking…");
        let client = self.client.clone();
        let api = self.api.clone();
        let tx = self.tx.clone();
        let generation = self.bootstrap_generation;
        tokio::spawn(async move {
            // Issue #591: make the recheck's report infallible instead of
            // racing a short timer. If this task exits — normally, on a
            // panic, or dropped by a runtime shutdown — without having sent
            // its own `Msg::Bootstrap`, this guard's `Drop` sends a fallback
            // `Err(NoToken)` under the same generation so the recovery
            // window is never left open by a task that silently died.
            struct ReportGuard {
                tx: mpsc::UnboundedSender<Msg>,
                generation: u64,
                reported: bool,
            }
            impl Drop for ReportGuard {
                fn drop(&mut self) {
                    if !self.reported {
                        self.tx
                            .send(Msg::Bootstrap {
                                generation: self.generation,
                                result: Err(TaskError {
                                    message: "session re-check ended unexpectedly".into(),
                                    code: None,
                                    max_page: None,
                                    kind: TaskErrorKind::NoToken,
                                }),
                            })
                            .ok();
                    }
                }
            }
            let mut guard = ReportGuard { tx: tx.clone(), generation, reported: false };
            if client.adopt_stored_tokens().await {
                let result = api.me().await.map_err(|e| TaskError::of(&e));
                guard.reported = true;
                tx.send(Msg::Bootstrap { generation, result }).ok();
            } else {
                guard.reported = true;
                tx.send(Msg::Bootstrap {
                    generation,
                    result: Err(TaskError {
                        message: reason,
                        code: None,
                        max_page: None,
                        kind: TaskErrorKind::NoToken,
                    }),
                })
                .ok();
            }
        });
    }

    /// Arm the "no session, transient failure" retry state: `bootstrap_retry_needed`
    /// (so a plain `r` re-runs `restore_session()`), the Forums panel's own
    /// retry hint (so it stops spinning on "Loading forums…" for a load that
    /// will never come), and a length-capped status line (issue #561: an
    /// uncapped raw HTTP body pushed "press r to retry." off the end of a
    /// 120-column terminal). Shared by the startup restore's `Msg::Bootstrap
    /// { Err }` arm and, since issue #594, `Msg::LoginComplete { Err }` for a
    /// transient failure right after a successful token exchange — both
    /// leave `self.me` unset and a stored token that already works.
    pub(super) fn arm_bootstrap_retry(&mut self, e: &TaskError) {
        self.bootstrap_retry_needed = true;
        // The Forums panel must not spin on "Loading forums…" forever for a
        // failure that will never resolve on its own (no `NodesLoaded` is
        // coming — bootstrap never got that far) — show the same retry hint
        // there `render_forum_panel` already knows how to draw for
        // `NodesLoaded`'s own Err arm, instead of leaving the priming
        // spinner up with no session and no way to tell the user anything
        // went wrong (issue #561).
        if let Some(tree) = self.tree_mut() {
            tree.loading = false;
            tree.error = Some(cap_message(&e.message, RAW_MESSAGE_CAP));
        }
        // `chrome::status_line` clips an overlong `left` string from the
        // *right* to keep the write-gate widget on screen — so an uncapped
        // raw HTTP body here is exactly what pushed " — press r to retry."
        // off the end of a 120-column terminal (issue #561, the reported
        // bug). `TaskError::of` already caps the synthetic `"http_error"`
        // fallback at construction (`RAW_MESSAGE_CAP` = 80), but this
        // wrapper's own fixed text already spends ~50 cells before the
        // message even starts, so re-cap tighter here: this line's tail is
        // load-bearing and must survive regardless of how the `TaskError`
        // was built.
        const BOOTSTRAP_STATUS_MSG_CAP: usize = 30;
        self.set_hint(format!(
            "Can't reach windowsforum.com ({}) — press r to retry.",
            cap_message(&e.message, BOOTSTRAP_STATUS_MSG_CAP)
        ));
    }

    /// Force-clears a `session_recovery_pending` window that has run past
    /// `SESSION_RECOVERY_TIMEOUT_SECS` without the recheck task reporting
    /// back — called once per event-loop tick. Without this, a recheck task
    /// that dies silently (panics, is dropped, its channel send fails) would
    /// leave the window open forever, and with it every poller's
    /// `SessionLost` inside the window swallowed as "stale" forever: a
    /// genuinely signed-out session would then never reach the Login screen
    /// (issue #587).
    /// Returns true when the backstop fired this tick — the caller owes the
    /// screen one redraw (#674).
    pub(super) fn expire_session_recovery_timeout(&mut self) -> bool {
        if self.session_recovery_pending
            && let Some(started) = self.session_recovery_started_at
            && started.elapsed() >= Duration::from_secs(SESSION_RECOVERY_TIMEOUT_SECS)
        {
            self.session_recovery_pending = false;
            self.session_recovery_started_at = None;
            // Issue #591: `recheck_stored_session` is only ever started with
            // a live session (`self.me.is_some()` — the boundary that calls
            // it checks this), so a token is still adopted and in use even
            // though this backstop never heard back from the recheck. Ending
            // the session here would be exactly the bug this fix closes —
            // a working session torn down by a timer. Prefer the same
            // "still signed in" hint the transient-failure arm uses and
            // leave the pollers (or the next call) to decide if the session
            // is really gone.
            if let Some(user) = self.me.as_ref() {
                let username = user.username.clone();
                self.set_hint(format!(
                    "Token re-check could not reach the site; still signed in as {username}."
                ));
            } else {
                self.end_session("Session check timed out; log in again.");
            }
            return true;
        }
        false
    }

    /// The single place a session ends. Every session-ending failure routes
    /// here from the message pump's boundary (and `logout` calls it too), so
    /// a client that has lost its grant can never keep rendering a signed-in
    /// header over panels that all say "not logged in" (issue #557).
    pub(super) fn end_session(&mut self, reason: &str) {
        self.stop_pollers();
        // A write (reply/thread/DM) still waiting on the politeness gates
        // belongs to the session that started it: it must never be sent
        // once that session is over — least of all with the *next*
        // account's token (issue #567).
        self.abort_writes();
        // Anything already in flight belongs to the session being ended.
        self.bootstrap_generation = self.bootstrap_generation.wrapping_add(1);
        self.me = None;
        self.alerts_unread = 0;
        self.convos_unread = 0;
        // Every overlay/chord layer that owns the keyboard ahead of the
        // screen stack must be torn down too — otherwise it stays armed over
        // the freshly-pushed Login screen and can still act (issue #560):
        // the palette's `Enter` still runs `run_palette_target` with no
        // session, and an armed `g` chord still resolves on whatever key
        // follows.
        self.palette = None;
        self.show_help = false;
        self.prefix = Prefix::default();
        self.selection = None;
        self.keep_thread_position = None;
        self.bootstrap_retry_needed = false;
        self.session_recovery_tried = false;
        self.session_recovery_pending = false;
        self.session_recovery_started_at = None;
        // A `Screen::Login` already on top survives instead of being
        // dropped and replaced with a fresh Idle one (issue #570): a late
        // session-ending message can arrive while the sign-in screen is
        // already up mid-flow (a link showing, or a poll in flight), and
        // that flow's own link/stage must not be reset out from under the
        // user. Login can only ever be the top of the stack (the gate
        // invariant — nothing pushes over a session-less Login), so keeping
        // whichever one is already there is enough; only push a new one
        // when none survived.
        self.screens
            .retain(|s| matches!(s, Screen::Home(_) | Screen::ForumTree(_) | Screen::Login(_)));
        if !matches!(self.screens.last(), Some(Screen::Login(_))) {
            self.screens.push(screens::login_state());
        }
        // Issue #600: like the #590 identity-change teardown, the next
        // sign-in (same account or a different one) must not inherit this
        // session's Home list — its threads/unread marks (from nodes the
        // next account may not even be allowed to see), or a `loading` flag
        // stuck true forever if a load was in flight when the session ended
        // (its `ForumLoaded` arrives with nowhere to clear it — see the
        // boundary's `mark_retryable` fallthrough below). Resetting to
        // default also puts `prime_home_list`'s gate back to "empty, not
        // loading, node 0", so the next sign-in reloads Latest.
        if let Some(Screen::Home(h)) = self.screens.first_mut() {
            h.list = screens::ThreadListState::default();
            h.focus = screens::Pane::Tree;
        }
        self.set_hint(reason);
    }

    pub(super) fn start_pollers(&mut self) {
        // Defensive: a stray second call (there should never be one with the
        // callers below, but this keeps the invariant "at most one poller
        // pair running" regardless) stops the previous pair first rather
        // than doubling the poll rate.
        self.stop_pollers();
        // The website's composer drafts (#716). Not a poller — one fetch each
        // time a session goes live, which is also what picks up a reply
        // started in the browser since the last run.
        self.sync_drafts();
        // Alerts poller: unread count for the status bar.
        let api = self.api.clone();
        let tx = self.tx.clone();
        self.poller_handles.push(tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(common::config::ALERT_POLL_SECS)).await;
                match api.alerts(1).await {
                    Ok(page) => {
                        let unread = page.alerts.iter().filter(|a| !a.viewed()).count() as u32;
                        tx.send(Msg::Notice(format!("alerts:{unread}"))).ok();
                    }
                    // A poller is the first thing to notice a session that
                    // has quietly ended (the user is reading, nothing else
                    // is calling the API). Report that instead of dropping
                    // it, and stop — `end_session` aborts us anyway, but a
                    // send failure must not leave this loop spinning
                    // (issue #557).
                    Err(e) => {
                        let err = TaskError::of(&e);
                        if err.ends_session() {
                            tx.send(Msg::SessionLost(err)).ok();
                            return;
                        }
                    }
                }
            }
        }));
        // Conversations unread poller.
        let api = self.api.clone();
        let tx = self.tx.clone();
        self.poller_handles.push(tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(
                    common::config::CONVERSATION_POLL_SECS,
                ))
                .await;
                match api.conversations(1).await {
                    Ok(page) => {
                        let unread =
                            common::models::count_unread_conversations(&page.conversations);
                        tx.send(Msg::Notice(format!("convos:{unread}"))).ok();
                    }
                    // See the alerts poller above (issue #557).
                    Err(e) => {
                        let err = TaskError::of(&e);
                        if err.ends_session() {
                            tx.send(Msg::SessionLost(err)).ok();
                            return;
                        }
                    }
                }
            }
        }));
    }

    /// A single immediate alerts+conversations unread-count fetch, without
    /// waiting for the pollers' own first sleep (45s/90s — issue #590). Used
    /// right after `start_pollers()` when the identity behind a live session
    /// just changed, so the header shows the NEW account's real badge counts
    /// promptly instead of sitting on the zeroed placeholder for up to 90s.
    /// Best-effort: a failure here is silently dropped exactly like a single
    /// missed poller tick — the next real poll (or a session-ending error,
    /// reported the usual way) still happens on schedule.
    pub(super) fn poll_unread_now(&mut self) {
        let api = self.api.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            if let Ok(page) = api.alerts(1).await {
                let unread = page.alerts.iter().filter(|a| !a.viewed()).count() as u32;
                tx.send(Msg::Notice(format!("alerts:{unread}"))).ok();
            }
        });
        let api = self.api.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            if let Ok(page) = api.conversations(1).await {
                let unread = common::models::count_unread_conversations(&page.conversations);
                tx.send(Msg::Notice(format!("convos:{unread}"))).ok();
            }
        });
    }

    /// Spawn a write (reply, new thread, DM) tied to the current session.
    ///
    /// The handle is kept so `end_session` can abort the task: a write sits
    /// in `api_gate`/`write_gate` long before it touches the token, so
    /// without this a `^S` abandoned by `Ctrl+L` still posted the draft the
    /// user believed discarded — under whatever token the client held by
    /// the time the gate opened (issue #567). Finished handles are pruned
    /// on the way in so the list cannot grow across a long session.
    pub(super) fn spawn_write<F>(&mut self, fut: F)
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        self.write_handles.retain(|h| !h.is_finished());
        self.write_handles.push(tokio::spawn(fut).abort_handle());
    }

    /// Abort every in-flight write and forget its handle (issue #567).
    pub(super) fn abort_writes(&mut self) {
        for h in self.write_handles.drain(..) {
            h.abort();
        }
    }

    /// Abort every poller spawned by `start_pollers` and forget its handles.
    /// Called on logout so a signed-out session stops hitting `/alerts` and
    /// `/conversations` — and so the next login's `start_pollers` starts a
    /// fresh pair instead of adding to whatever was already running
    /// (issue #524).
    pub(super) fn stop_pollers(&mut self) {
        for h in self.poller_handles.drain(..) {
            h.abort();
        }
    }

    /// Start a login flow, superseding any flow already running.
    ///
    /// Enter is advertised as "restart login" on the whole Login screen, so it
    /// has to work while the client is polling too: a denied approval, a
    /// failed Turnstile/2FA or a lost link otherwise stranded the user for the
    /// link's full 10-minute TTL (issue #547). The previous task is aborted
    /// and the generation bumped, so neither its poll loop nor a message that
    /// outraced the abort can touch the new flow.
    pub(super) fn begin_login(&mut self) {
        if let Some(task) = self.login_task.take() {
            task.abort();
        }
        self.login_generation = self.login_generation.wrapping_add(1);
        let generation = self.login_generation;
        if let Some(Screen::Login(ls)) = self.screens.last_mut() {
            ls.busy = true;
            ls.error = None;
            ls.stage = crate::screens::LoginStage::Idle;
            ls.url.clear();
        }
        let tx = self.tx.clone();
        let client = self.client.clone();
        let handle = tokio::spawn(async move {
            if let Err(message) = run_login_flow(&tx, client, generation).await {
                tx.send(Msg::LoginFailed { generation, message }).ok();
            }
        });
        self.login_task = Some(handle.abort_handle());
    }

    // ---- paste handling (bracketed paste and clipboard) ----

    pub fn logout(&mut self) {
        let client = self.client.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            // Take the token set out of memory and erase the store *all at
            // once*, before either revoke round-trip — `take_tokens` holds
            // the token lock across the snapshot and the forget, so a
            // poller's refresh rotating the grant cannot interleave between
            // them, and the revoke calls below use the snapshot, never the
            // live session (issue #574's window, plus the narrower one the
            // old `token_set()` + `forget_tokens()` pair still had). Each
            // revoke call can take up to CONNECT 10s + REQUEST 30s (a slow
            // site, a CF challenge, a stalled connection — exactly the
            // conditions under which people sign out and back in), and a
            // sign-in completed inside it lands its fresh tokens via
            // `set_tokens()` afterward, which this snapshot never touches.
            let (tokens, forgotten) = match client.take_tokens().await {
                Ok(tokens) => (tokens, Ok(())),
                Err(e) => (None, Err(e)),
            };
            // Revoke the refresh token first and the access token second,
            // each with its `token_type_hint` — the endpoint defaults to
            // `access_token`, so the old single call left the 90-day refresh
            // token valid for anyone holding a copy of `token.json`
            // (issue #527). A failure here is reported, never swallowed:
            // against stock XenForo this call cannot currently succeed for a
            // public PKCE client (its revoke endpoint requires the
            // `client_secret` this client deliberately does not have), and a
            // silent "Logged out." would hide that the tokens are still
            // live. The server-side fix is a relay in the TuiLink add-on and
            // is out of this client's scope.
            let mut failed: Vec<&str> = Vec::new();
            if let Some(tokens) = tokens {
                match common::http::build() {
                    Ok(http) => {
                        for (token, hint) in [
                            (&tokens.refresh_token, "refresh_token"),
                            (&tokens.access_token, "access_token"),
                        ] {
                            if token.is_empty() {
                                continue;
                            }
                            let base = client.base_url();
                            if let Err(e) =
                                common::oauth::revoke(&http, base, token, Some(hint)).await
                            {
                                tracing::warn!("logout: {hint} was not revoked server-side: {e}");
                                failed.push(hint);
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!("logout: no HTTP client to revoke with: {e}");
                        failed.push("refresh_token");
                    }
                }
            }
            let result = match forgotten {
                Err(e) => Err(e.to_string()),
                Ok(()) if !failed.is_empty() => Err(format!(
                    "the server kept {} valid \u{2014} sign out in a browser to end the session",
                    failed.join(" and ").replace('_', " ")
                )),
                Ok(()) => Ok(()),
            };
            tx.send(Msg::LoggedOut(result)).ok();
        });
        // Same teardown as any other session end — including the generation
        // bump that drops a `Msg::Bootstrap` still in flight from a restore
        // the user just signed out of (issue #557).
        // Unsent drafts are this account's private writing: signing out must
        // not leave them on disk for whoever signs in next (#715). A session
        // that merely *expires* keeps them — that user is coming back.
        self.clear_all_drafts();
        self.end_session("Logged out.");
    }

    // ---- message handling ----
}
