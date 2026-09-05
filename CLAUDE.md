# wftui_app — WindowsForum.com terminal client (Linux + Windows)

Rust TUI for windowsforum.com: OAuth login, forum/thread/news browsing, replies,
DMs (conversations), alerts, search, member profiles. Pure client — the only
server-side pieces it needs are stock XenForo 2.3's OAuth server plus the tiny
`WindowsForum/TuiLink` addon (short login links + callback relay), both already
deployed.

## Commands

```bash
cd /web/wftui_app
cargo build --release                 # release binary
cargo test                            # unit + wiremock (49 tests)
cargo clippy --all-targets --release -- -D warnings   # gate — must stay at 0
cp target/release/wftui bin/wftui     # stable artifact location
cp bin/wftui /usr/local/bin/wftui     # deploy on the server (also on PATH)
```

Release binary is also committed at `bin/wftui` so non-developer machines can
grab it without a toolchain. `scripts/` is reserved for an optional
cargo-xwin Windows cross-build — the supported Windows story is a native
`cargo build --release` (the dep set is pure-Rust: rustls, not OpenSSL).

## Architecture

Two-crate workspace (house style from `services/mirror`: resolver 2, edition
2024, plain-enum errors, no anyhow/thiserror, inline `#[cfg(test)]` tests):

- `common/` — everything testable without a terminal:
  - `oauth.rs` — PKCE (S256) login. Registers a short link via the TuiLink
    addon (`POST /api/wf-tuilink/register`), polls for the authorization code
    (`POST /api/wf-tuilink/poll`), exchanges it at `/api/oauth2/token`.
    Also: token refresh, revoke, loopback-listener legacy path (unused).
  - `api.rs` — `WfApi` trait (the seam screens consume; tests substitute it)
    and `WfApiClient`: Bearer auth, silent refresh-on-expiry, and three
    politeness gates (`api_gate` 250ms, `search_gate` 3s, `write_gate` 30s —
    180s after new threads). ALL traffic goes through these gates.
  - `ratelimit.rs` — the gates. `token.rs` — 0600 atomic token store.
  - `bbcode.rs` — BBCode → styled chunks (UI-agnostic; golden-tested).
  - `osc.rs` — OSC 8 hyperlinks + OSC 52 clipboard, with tmux DCS passthrough.
  - `config.rs` — constants + `WFTUI_*` env overrides (`WFTUI_BASE_URL`,
    `WFTUI_OAUTH_CLIENT_ID`, `WFTUI_CONFIG_DIR`, `WFTUI_LOG`).
- `wftui/` — the binary: `app.rs` (event loop, message pump, mouse selection),
  `event.rs` (dedicated blocking reader thread), `theme.rs`,
  `screens/` (browse/social/misc renderers + key handlers).

## Hard rules (each closes a real bug — do not regress)

1. **Never embed OSC/escape sequences in ratatui span content.** Ratatui
   re-emits cells on diff; escape bytes get written out of context and the
   terminal eats surrounding text (this corrupted the whole login screen
   once). All escape sequences go through `app::emit_raw`, written directly
   to stdout between frames.
2. **Every spawned child process must null its stdin** (`oauth::open_browser`
   does). xdg-open chains into browser-probe scripts that inherit and READ
   stdin — they eat the user's keystrokes and split escape sequences, which
   looks like random key loss. This caused the "completely bugged" incident.
3. **Input comes from the dedicated blocking reader thread** (`event.rs::spawn_reader`),
   not `poll(timeout)` in the UI loop — the loop raced and dropped events.
4. **`panic = "unwind"` in the release profile** — the run loop is wrapped in
   `catch_unwind` and `TerminalGuard::Drop` restores raw mode/alternate
   screen during unwind. `abort` leaves the user's terminal broken.
5. **Mouse selection extracts text from `App::screen_rows`** (the mirror
   captured in `draw`), never from `terminal.current_buffer_mut()` — between
   frames that is the blank next-frame buffer.
6. **The UA is `wftui/<ver> (+https://windowsforum.com)`.** Cloudflare's bot
   rule 403s bare library UAs (reqwest/hyper/...); never "fix" a blocked
   request by spoofing a browser UA.
7. **Self-throttling is not optional** — the zone's flood ceiling is shared
   with all visitors and the gate constants mirror XF's own flood checks
   (30s posts / 180s threads).

## Login flow (works over SSH, no copy-paste)

1. No token at start → `begin_login` registers a PKCE challenge+state and
   shows the short link `https://windowsforum.com/tui-start/<id>` (~40 chars).
   It is also pushed to the clipboard (OSC 52, tmux-wrapped) and saved to
   `<config dir>/login-url.txt`.
2. The user opens it anywhere (browser/phone), approves on the real site —
   real Turnstile, real 2FA. The OAuth redirect goes to `/tui-done` (registered
   redirect URI), which captures the code.
3. The TUI polls every 2s, exchanges the code (PKCE verifier never leaves the
   client), and persists the token set (access 2h / refresh 90d, silent
   refresh). Logout = `POST /api/oauth2/revoke` + local erase.

The OAuth client is a **public PKCE client** — no secret anywhere. Registered
by `/web/ops/wftui_oauth_client.php` (idempotent, entity-system save).

## tmux notes

tmux swallows apps' OSC sequences; `osc.rs` wraps them in the DCS passthrough
when `$TMUX` is set. The user needs, in `~/.tmux.conf` **on the machine that
runs the client**:

```tmux
set -g allow-passthrough on    # tmux >= 3.3 — deliver app OSC 52/8 to the outer terminal
set -g mouse on                # forward mouse events to the TUI (drag-select, wheel)
```

## Testing without touching production

- Unit/wiremock tests: `cargo test`. Mock-server tests serialize on
  `common::config::ENV_LOCK` because `WFTUI_BASE_URL` is process-global.
- Real PTY verification (how the render bugs above were found): run the binary
  under a Python `pty` + `pyte` screen, inject SGR mouse sequences
  (`\x1b[<0;x;yM` / `m`) and keys, and assert on the rendered screen. A raw
  byte dump is NOT enough — frames are diffed.

## Known gaps

- Attachment upload is implemented in `common` but not wired into the compose
  screen; attachment viewing is via URLs only.
- Double-click word-select / triple-click line-select not implemented (drag +
  release = copy is).
- Windows packaging is documentation-only (native build; no CI).
