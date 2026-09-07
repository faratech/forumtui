# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

# wftui_app — WindowsForum.com terminal client (Linux + Windows)

Rust TUI for windowsforum.com: OAuth login, forum/thread/news browsing, replies,
DMs (conversations), alerts, search, member profiles, inline images. Pure client —
the only server-side pieces it needs are stock XenForo 2.3's OAuth server plus the
tiny `WindowsForum/TuiLink` addon (short login links + callback relay), both already
deployed. `DESIGN.md` beside this file is the approved UI contract (palette, glyphs,
three-zone chrome, row grammar, screens, graphics tiers); read it before touching a
renderer.

## Commands

```bash
cd /web/wftui_app
export WFTUI_CONFIG_DIR=/tmp/wftui-test-cfg          # ALWAYS before cargo test (see Testing)
cargo check                                          # while iterating
cargo test --workspace                               # unit + wiremock, both crates
cargo test -p wftui --no-default-features            # the no-images build must stay green too
cargo test -p wftui thread_row_keeps_exact_width     # one test (substring match on the name)
cargo test -p common bbcode::tests::                 # one module
cargo clippy --all-targets --release -- -D warnings                              # gate: 0 warnings
cargo clippy -p wftui --no-default-features --all-targets --release -- -D warnings
cargo build --release && cp target/release/wftui bin/wftui   # bin/wftui is committed
cp bin/wftui /usr/local/bin/wftui                    # deploy on this server (on PATH)
```

The "gates" every change must pass before it is committed: both test invocations,
both clippy invocations, a release build, and `/usr/bin/grep -rn $'\x1b' wftui/src`
printing nothing (hard rule 1). The release binary is committed at `bin/wftui` so
machines without a toolchain can grab it. Windows is a native `cargo build
--release`; there is no CI and no cross-build (`scripts/` is reserved for one).

## Git

The repo root is `/web` (a monorepo); this crate is `wftui_app/`. Stage explicit
paths under `wftui_app/` only — never `git add -A`, never `target/`. Commit messages
are `feat(wftui): …` / `fix(wftui): …` with a body of plain-word bullets, `Fixes #N`
lines, the test/clippy result line, and the `Co-Authored-By` trailer. Pushes go
straight to `origin main`. Bugs are tracked as GitHub issues labelled
`area:wftui` (+ `p0`–`p3`, `sev:*`); an issue is closed only with the fixing commit
and the name of the test that pins it.

## Architecture

Two-crate workspace (house style from `services/mirror`: resolver 2, edition 2024,
plain-enum errors, no anyhow/thiserror, inline `#[cfg(test)]` tests, `rust-version
1.98` = the toolchain it is released against, nothing in the graph needs it).

### `common/` — everything testable without a terminal

- `api.rs` — `WfApi` trait (the seam screens consume; tests substitute
  `RecordingApi`) and `WfApiClient`: Bearer auth, silent refresh-on-expiry, and
  the politeness gates (`ratelimit.rs`): `api_gate` 250 ms, `search_gate` 3 s,
  `write_gate` 30 s (180 s after a new thread), `image_gate` 250 ms — a separate
  lane for thumbnails so a screenful of avatars never queues ahead of the user's
  next navigation. ALL traffic goes through a gate. XF replies its own error
  envelope (`{"errors":[{"code":..}]}`) even on the OAuth token endpoint —
  `error_from_response` and `oauth::token_request` both parse it; a raw-body
  `"http_error"` code means "not JSON" and is treated as transient.
- `oauth.rs` — PKCE (S256) login through the TuiLink addon
  (`/api/wf-tuilink/register` → short link; `/api/wf-tuilink/poll` → code) and
  `/api/oauth2/token`; refresh; revoke (client half only — see Known gaps). Every
  call takes the origin as an argument; nothing reads `WFTUI_BASE_URL` at request
  time. `browser_command` builds the opener argv: **on Windows it is
  `rundll32 url.dll,FileProtocolHandler <url>`, never `cmd /C start`** (cmd parses
  `&`/`%` inside forum URLs — command injection).
- `token.rs` — 0600 atomic token store; `quarantine_corrupt` renames an unreadable
  file instead of bricking startup. `models.rs` — XF API shapes; field names come
  from the entity classes under `/web/public_html/src/XF/Entity/*.php`
  (`setupApiResultData` / `getStructure`), never guessed. `bbcode.rs` — BBCode →
  styled `Chunk`s, golden-tested against real posts (XF's parser semantics:
  `[tag="v]"]` quoted values may contain `]`, `[i words]` without `key=` options is
  literal text, `[CODE]`/`[ICODE]` bodies are verbatim, `[USER]` already carries
  the `@`). `osc.rs` — OSC 8 hyperlinks + OSC 52 clipboard with tmux DCS
  passthrough (a `screen*` TERM without `$TMUX` is tmux-over-ssh: dual-deliver).
  `config.rs` — constants + the `WFTUI_*` env overrides.

### `wftui/` — the binary

- `main.rs` runs graphics detection (reads stdin — before the reader thread), then
  `app::run`. `event.rs` is the dedicated blocking reader thread (hard rule 3).
  `tty.rs` snapshots termios before detection and re-applies it on every exit path.
- `app.rs` (the largest file) is one `App` with an event loop ticking every 50 ms,
  a `Msg` pump (every async result is a `Msg` variant handled in `handle_msg`),
  `handle_key` / `handle_mouse`, and `draw`. Screens never see `App`: they own
  their state, render into a `Rect`, and return `Action`s from `on_key`; the app
  executes actions (`execute_action`) by spawning tasks that send `Msg`s back.
  Loads carry their identity (`ForumLoaded{node_id}`, `ThreadLoaded{id}`,
  `SearchDone{key}`, `Bootstrap{generation}`) and stale replies are dropped —
  copy that pattern for any new fetch.
- `screens/mod.rs` — `Screen` enum + dispatch (`render`, `on_key`, `hints`,
  `crumb`, `esc_intent`, `goto_top/bottom`, `web_url`, `selected_index`,
  `select_index`, `focus_pane_at`, `click_field`), state structs, `Action` enum.
  `Home` (Forums tree + thread list, dual pane ≥ 110 cols) and `Inbox`
  (Conversations/Alerts tabs + view pane) are two-pane screens with a
  renderer-set `dual` flag; `ThreadList`/`ConversationView` are the narrow
  pushables. Renderers live in `screens/{browse,social,misc}.rs`.
- `chrome.rs` — the three zones (`header_line` with breadcrumb + badges,
  `key_bar` with exactly one primary cap, `status_line` with gate countdowns),
  `panel`, key caps/chips, and the cell-width helpers (`cell_width`,
  `take_cells`, `take_cells_end`). `theme.rs` — role palette with
  TrueColor/Ansi256/Ansi16/Mono tiers (`NO_COLOR` → Mono; body text is always
  `Reset`). `glyph.rs` — the single-cell glyph table with an ASCII set
  (`WFTUI_ASCII=1` or a non-UTF-8 locale); **no emoji anywhere** (double-width or
  blank in many terminals). `overlay.rs` — go-to palette (`Ctrl+K` / `:`), `g`
  which-key prefix, `?` keys card. `editor.rs` — the text editor primitives on
  char indices with a cell-width visual-row model (caret, wrapping, `hwindow`
  for single-line fields). `hit.rs` — the per-frame mouse/touch hit map.
  `images.rs` — graphics tiers, sizing, LRU + disk cache.

### Width, text and keys — the contracts tests pin

- Every width measurement is in **terminal cells**, cut on **grapheme clusters**:
  use `chrome::cell_width` / `take_cells`, never `chars().count()` or
  `chars().take(n)` (CJK, emoji and `U+FE0F` sequences broke every one of those).
  Row builders pad with `saturating_sub`.
- **Every advertised key is handled.** `screens::dispatch_tests::every_advertised_key_dispatches_to_something`
  presses every cap each screen's `hints()` shows; hints are state-aware
  (login Idle vs Waiting, compose title vs body, Inbox list vs view pane, search
  input vs browse vs member mode). If a key cannot work in a state, hide the cap
  or return `Action::Notice(..)` — never a silent no-op.
- Status writes go through `set_status` (4 s toast) or `set_hint` (persistent);
  a source-scan test (`no_direct_status_writes_bypass_set_status_or_set_hint`)
  fails on any direct `self.status =`.
- Esc is offered to the screen first (`esc_intent`): a busy composer blocks it, the
  sign-in and paywall-style gates block it with a hint, Search in input mode leaves
  input mode. Ctrl+L with no session is a no-op.

### Session lifecycle (the part that took ten rounds to get right)

`end_session(reason)` is the single teardown (pollers stopped, writes aborted,
stack cut to Home, Login pushed, overlays cleared, badges zeroed, Home list reset).
`session_error_of(&Msg)` at the top of `handle_msg` decides whether a failed
message ends the session: only `NoToken`, structured OAuth errors, 401 and the
bootstrap account-gone codes do; ordinary XF 403 refusals and `"http_error"`
bodies (Cloudflare 5xx/429 pages) never do. A mid-session `NoToken`/401 first
runs `recheck_stored_session` (adopt a sibling instance's rotated `token.json`,
then `/me`); its verdict comes back as `Msg::Bootstrap{generation}` through a
drop guard, so it can never be lost; errors that arrive inside that window are
rewritten by `mark_retryable` and still delivered to their screen so nothing stays
busy/loading; a 120 s backstop clears the window without ending a live session.
If `/me` reports a different user, the old identity is torn down and a hint names
the new one. `logout()` forgets tokens synchronously before the (slow) revoke
calls. Writes carry the session generation and are aborted by `end_session`.

## Media Gallery and Resource Manager (XFMG / XFRM)

Both add-ons ship REST list APIs on this server and the client browses them:
`GET /api/media/?page=N` → `{media, pagination}` and `GET /api/resources/?page=N`
→ `{resources, pagination}` back `Screen::MediaGallery` / `Screen::Resources`
(`screens/library.rs`). Reach them with `g m` / `g r` or the go-to palette's
"Media Gallery" / "Resources" rows; `j/k` moves, `[`/`]` pages, `R` refreshes,
Enter/`o` opens the item on the site. Both wear the house `solo_panel` with the
`page N of M` cap, and both are read-only — there is no upload path.

XFRM serves `rating_average` as a decimal *string* (`"4.50"`), so
`models::deserialize_opt_f64` accepts either shape. Search's type cycler already
covers the same content (`xfmg_media`, `resource`, issue #673), and
`[GALLERY=media, <id>]caption` embeds in posts render as the caption linked to
the media page.

## Hard rules (each closes a real bug — do not regress)

1. **Never embed OSC/escape sequences in ratatui span content.** Ratatui re-emits
   cells on diff; escape bytes get written out of context and the terminal eats
   surrounding text. All escape sequences go through `app::emit_raw`, written
   directly to stdout between frames. Image escapes are the one exception and are
   written only by the ratatui-image widget (see Inline graphics).
2. **Every spawned child process must null its stdin** (`oauth::open_browser`
   does). xdg-open chains into browser-probe scripts that read stdin — they eat
   keystrokes and split escape sequences.
3. **Input comes from the dedicated blocking reader thread** (`event.rs`), not
   `poll(timeout)` in the UI loop. Anything else that reads stdin (the graphics
   capability query) runs before that thread starts.
4. **`panic = "unwind"` in the release profile** — the run loop is wrapped in
   `catch_unwind`; `TerminalGuard::Drop` and the panic hook call one shared
   `restore_terminal()` that issues each crossterm command in its own `execute!`
   (a failing `PopKeyboardEnhancementFlags` on Windows must not skip
   `LeaveAlternateScreen`).
5. **Mouse selection extracts text from `App::screen_rows`** (the mirror captured
   in `draw`), never from `terminal.current_buffer_mut()`; image cells and
   wide-char continuation cells are skipped.
6. **The UA is `wftui/<ver> (+https://windowsforum.com)`.** Cloudflare's bot rule
   403s bare library UAs; never "fix" a blocked request by spoofing a browser UA.
7. **Self-throttling is not optional** — the zone's flood ceiling is shared with
   all visitors and the gate constants mirror XF's own flood checks.

## Testing without touching production

- **No test may resolve the real config dir or the live site.** The suite once
  overwrote the operator's `~/.config/wftui/token.json` with a fixture and
  `GET /api/me`'d the live site with the real bearer. Build test clients with
  `WfApiClient::with_store(token::Store::with_path(<scratch>), <unreachable
  base>)` — never `WfApiClient::new()`; `wftui`'s `test_app()` also stubs
  `App::api` with `RecordingApi` and points the image cache at the scratch dir.
  Two guard tests are the contract and must stay green:
  `common::api::tests::guard_no_test_can_reach_the_real_config_dir_or_the_live_site`
  and `wftui::app::tests::guard_no_test_touches_the_real_config_dir_or_the_live_site`.
  Belt-and-braces: export `WFTUI_CONFIG_DIR` to a scratch dir before any
  `cargo test`.
- Wiremock tests serialize on `common::config::ENV_LOCK` because the `WFTUI_*`
  variables are process-global — every test that mutates one must hold it and
  restore (never remove) the variable.
- Headless render tests use ratatui's `TestBackend` and drive the real
  `App::draw` / `handle_key` / `handle_mouse` (click at a cell, assert the
  action). Real PTY verification (python `pty` + `pyte`, SGR mouse sequences
  `\x1b[<0;x;yM`) is how the render bugs above were found; a raw byte dump is not
  enough because frames are diffed.
- Live-data checks are read-only SQL against `wf_wf` (`mariadb -N wf_wf -e`);
  verify parser changes against real posts, that is where the BBCode bugs were.

## Runtime environment variables

`WFTUI_BASE_URL`, `WFTUI_OAUTH_CLIENT_ID`, `WFTUI_CONFIG_DIR`, `WFTUI_LOG`,
`WFTUI_ASCII=1` (glyph fallback), `WFTUI_MOUSE=0` (no mouse capture — the
permanent form of Shift+drag), `WFTUI_NO_IMAGES=1` (text tier),
`WFTUI_GRAPHICS=kitty|sixel|iterm2|halfblocks|none` (skip the capability query),
`NO_COLOR` (mono theme and text tier).

## Mouse and touch

Terminals deliver touch as mouse events (tap = click, two-finger scroll = wheel,
long press = right click on most mobile terminals), so there is exactly one
hit-testing layer (`hit.rs`). `App::draw` clears a `HitMap` each frame; renderers
register `Rect -> Hit` as they draw; `handle_mouse` resolves the pointer cell.
Load-bearing rules: last registered wins (overlays clear the map first, header and
key bar register last); indices are screen-local and a click focuses the pane
first; press+release in one cell is a click, any movement is the drag-select;
a second click on the selected row opens it, right click opens it on the site;
anything with a key is dispatched through `handle_key` so every gate applies to
the click too — never duplicate a handler in `click_hit`; list hits are read from
`ListState::offset()` after the widget rendered.

## Login flow (works over SSH, no copy-paste)

No token → `begin_login` registers a PKCE challenge and shows the short link
`https://windowsforum.com/tui-start/<id>`, also pushed to the clipboard (OSC 52)
and saved to `<config dir>/login-url.txt`. The user approves on the real site
(Turnstile, 2FA); the redirect to `/tui-done` captures the code; the TUI polls
every 2 s, exchanges it, and persists the token set (access 2 h / refresh 90 d,
silent refresh). Enter restarts a stuck flow (flows are generation-stamped). The
OAuth client is a public PKCE client — no secret anywhere — registered by
`/web/ops/wftui_oauth_client.php`.

## tmux notes

tmux swallows apps' OSC sequences; `osc.rs` wraps them in the DCS passthrough when
`$TMUX` is set and dual-delivers when only a `screen*` TERM is visible (tmux on
the other side of ssh). On the machine running the client:

```tmux
set -g allow-passthrough on    # tmux >= 3.3 — deliver OSC 52/8 and image payloads
set -g mouse on                # forward mouse events to the TUI
```

## Windows build

Pure Rust, no C toolchain: TLS is rustls with the *ring* provider (pre-generated
windows-msvc objects). reqwest 0.13's plain `rustls` feature would pull
aws-lc-rs (a vendored C tree needing NASM), so `common/` takes
`rustls-no-provider` and installs ring itself in `http::build` — don't "simplify"
that back. `unix`-only crates (`libc` for `tty.rs`) are cfg-gated. The graphics
capability query is skipped on Windows (ConPTY never answers it reliably).

## Inline graphics (cargo feature `images`, default on)

`images.rs` maps DESIGN.md's five tiers (kitty, sixel, iTerm2, half-blocks, text)
onto `ratatui-image` 11.x with `default-features = false, features =
["crossterm", "image-defaults"]` — the only pure-Rust feature set (`chafa-*`
probes a C library in `build.rs`). Exactly one `ratatui` and one `crossterm` must
resolve (`cargo tree -i ratatui`, `cargo tree -i crossterm`); a second copy will
not link the widget against our buffer. `--no-default-features` drops the decoder
and leaves the text tier.

The capability query reads stdin, so it runs in `main.rs` before the reader
thread, only on unix with a terminal on stdin and no `WFTUI_GRAPHICS` override
(ratatui-image leaks a blocked reader thread on timeout, which `tty.rs`'s termios
snapshot neutralizes on exit). Image payloads are written only by the crate's
widget through ratatui's diff-option path (anchor cell `ForcedWidth(1)`, covered
cells `Skip`; ratatui 0.30 deprecated `Cell::skip`, so read `diff_option`).
`app::is_image_cell` is the shared probe used by `capture_screen`,
`paint_selection` and the overlays; images are suppressed while an overlay is up.
Two surfaces draw them: the thread view (attachments sized from the API's
width/height, avatars 2×5) and the compose Preview pane (`Chunk::Image` /
`Chunk::Attach` from the draft, cached in `PreviewCache`, fetched only after the
draft has been still for 500 ms). Thumbnails ≤ 40 % of the panel and ≤ 12 rows;
fetches stream through `fetch_bytes` with a hard byte cap, then the disk cache
under `<config dir>/cache/img/` (32 MiB, 0600, atomic).

## Known gaps

- Attachment upload is implemented in `common` but not wired into compose, so the
  preview resolves `[IMG]` URLs only. Enter-to-expand an image is not implemented.
- Logout revokes only client-side: stock XF's `/api/oauth2/revoke` needs a client
  secret a public client cannot hold, so the 90-day refresh token outlives a
  sign-out until the TuiLink addon gains a revoke relay (#527). The client warns
  persistently when the revoke fails (issue #527: the server-side relay that
  would make the revoke succeed is accepted-deferred — it needs a production
  TuiLink addon change).
- TuiLink cannot yet signal a denied browser approval; the client only offers a
  restart. The Premium Supporter gate (#572) is designed but deliberately not
  built (accepted-deferred product feature).
- Search's chip row is keyboard-only; Windows packaging is documentation-only.
  (`[SPOILER]` bodies are hidden black-on-black until `x` reveals them in the
  thread view — issue #621.)
