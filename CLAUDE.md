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
cargo test --workspace                # unit + wiremock (321 tests; 318 with --no-default-features)
cargo clippy --all-targets --release -- -D warnings   # gate — must stay at 0
cp target/release/wftui bin/wftui     # stable artifact location
cp bin/wftui /usr/local/bin/wftui     # deploy on the server (also on PATH)
```

Release binary is also committed at `bin/wftui` so non-developer machines can
grab it without a toolchain. `scripts/` is reserved for an optional
cargo-xwin Windows cross-build — the supported Windows story is a native
`cargo build --release` with no external C library to find: TLS is rustls, not
OpenSSL, and its crypto provider is *ring*, which ships pre-generated
windows-msvc objects. reqwest 0.13's plain `rustls` feature would instead
hard-wire **aws-lc-rs**, whose `aws-lc-sys` vendors a ~70 MB AWS-LC C source
tree and needs NASM on windows-msvc (its `prebuilt-nasm` escape hatch is
opt-in), so `common/` takes `rustls-no-provider` and installs the ring provider
itself in `http::build`. Don't "simplify" that back to `rustls`.

Both crates carry `rust-version = "1.98"`. Nothing in the graph forces that —
the highest transitive MSRV is 1.90 (`quantette`/`ordered-float`, via the image
decoder chain), with ratatui 0.30, image and time at 1.88 — it is simply the
toolchain the client is developed and released against.

## Architecture

Two-crate workspace (house style from `services/mirror`: resolver 2, edition
2024, plain-enum errors, no anyhow/thiserror, inline `#[cfg(test)]` tests):

- `common/` — everything testable without a terminal:
  - `oauth.rs` — PKCE (S256) login. Registers a short link via the TuiLink
    addon (`POST /api/wf-tuilink/register`), polls for the authorization code
    (`POST /api/wf-tuilink/poll`), exchanges it at `/api/oauth2/token`.
    Also: token refresh, revoke, loopback-listener legacy path (unused).
  - `api.rs` — `WfApi` trait (the seam screens consume; tests substitute it)
    and `WfApiClient`: Bearer auth, silent refresh-on-expiry, and four
    politeness gates (`api_gate` 250ms, `search_gate` 3s, `write_gate` 30s —
    180s after new threads — and `image_gate` 250ms, the decoration-only lane
    `fetch_bytes` uses so a screenful of thumbnails can never queue in front of
    the user's next navigation, issue #543; `app.rs` also caps image loads at
    `IMAGE_LOAD_CONCURRENCY` in flight). ALL traffic goes through these gates.
  - `ratelimit.rs` — the gates. `token.rs` — 0600 atomic token store.
  - `bbcode.rs` — BBCode → styled chunks (UI-agnostic; golden-tested).
  - `osc.rs` — OSC 8 hyperlinks + OSC 52 clipboard, with tmux DCS passthrough.
  - `config.rs` — constants + `WFTUI_*` env overrides (`WFTUI_BASE_URL`,
    `WFTUI_OAUTH_CLIENT_ID`, `WFTUI_CONFIG_DIR`, `WFTUI_LOG`).
- `wftui/` — the binary: `app.rs` (event loop, message pump, mouse selection),
  `event.rs` (dedicated blocking reader thread), `theme.rs`,
  `images.rs` (graphics tiers, sizing, LRU + `cache/img/` disk cache),
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

- **No test may resolve the real config dir or the live site** (issue #565:
  the suite used to read the operator's own `~/.config/wftui/token.json`,
  really `GET /api/me`'d windowsforum.com with that bearer, and overwrote the
  store with a fixture). A test client is built with
  `WfApiClient::with_store(token::Store::with_path(<scratch>), <unreachable
  base>)` — never `WfApiClient::new()`, which reads `WFTUI_CONFIG_DIR` /
  `WFTUI_BASE_URL`. The store and the origin are captured once at
  construction, and every OAuth call (`register_link`, `poll_link`,
  `exchange_code`, `refresh`, `revoke`) takes the origin as an argument, so a
  client can never disagree with itself about which site it is talking to.
  `wftui`'s `test_app()` also stubs `App::api` (`RecordingApi`, everything
  `NoToken`) so no handler that spawns a call can reach the network, and
  points the image disk cache at the scratch dir.
  The two guard tests are the contract:
  `common::api::tests::guard_no_test_can_reach_the_real_config_dir_or_the_live_site`
  and `wftui::app::tests::guard_no_test_touches_the_real_config_dir_or_the_live_site`
  (plus the assertion inside `EnvGuard::hold`/`offline_client`, which fires in
  every test, not just those two).
- Unit/wiremock tests: `cargo test`. Mock-server tests serialize on
  `common::config::ENV_LOCK` because `WFTUI_BASE_URL` is process-global —
  **every** test that mutates a `WFTUI_*` variable must hold it (`logging`'s
  did not, and its `remove_var` is what let an `api` test's client resolve the
  real store).
- Real PTY verification (how the render bugs above were found): run the binary
  under a Python `pty` + `pyte` screen, inject SGR mouse sequences
  (`\x1b[<0;x;yM` / `m`) and keys, and assert on the rendered screen. A raw
  byte dump is NOT enough — frames are diffed.

## Inline graphics (cargo feature `images`, default on)

`images.rs` maps DESIGN.md's five tiers onto `ratatui-image` 11.x, which tracks
`ratatui = "^0.30.1"` — the same major we pin — so exactly one ratatui
resolves, and its `crossterm` feature routes through `ratatui-crossterm`'s
default `crossterm_0_29`, so exactly one crossterm resolves. Assert both with
`cargo tree -i ratatui` and `cargo tree -i crossterm`; a second copy of either
is a build that will not link the widget against our buffer.

`default-features = false` is load-bearing, and it is what keeps the dep set
pure-Rust: `chafa-dyn` and `chafa-static` are the only ratatui-image features
that reach for a C library (they add a `pkg-config` build-dependency whose
`build.rs` probes libchafa and panics the build without it), and `chafa-dyn` is
on by default. `crossterm` + `image-defaults` is the whole feature set we want:
kitty, sixel (pure-Rust `icy_sixel`), iTerm2 and half-blocks are not
feature-gated — they are always compiled and chosen at runtime by `Picker` —
and `image-defaults` is what gives us PNG/GIF/WEBP instead of JPEG only.
`cargo build --no-default-features` drops the decoder entirely and leaves tier 5.

Two surfaces draw images. The thread view sizes each post attachment from the
API's own `width`/`height` and reserves the rows in `rebuild_lines`. The
compose **Preview** pane does the same for the image references in the draft
itself: `bbcode::Chunk::Image` / `Chunk::Attach` (their own chunk variants, so
an image is never guessed from a `[image]`-shaped label) become a `▣ caption`
line plus reserved rows, laid out by `screens::misc::build_preview` and cached
in `PreviewCache` — the event loop redraws every 50 ms, so re-parsing the draft
per frame would re-derive the same picture twenty times a second. A bare
`[IMG]` URL carries no dimensions, so `images::Loaded` brings the source pixel
size back with the decoded payload and `Images::sizes()` is stamped onto the
screen beside the tier; the caption reads `▣ loading…` until then.

The capability query reads stdin, so it runs in `main.rs` **before**
`app::run` spawns the reader thread (hard rule 3). Image escapes are written
only by the crate's widget, through ratatui's sanctioned diff-option path: the
whole payload goes in one anchor `Cell`'s symbol, flagged
`CellDiffOption::ForcedWidth(1)` so ratatui bills it one column instead of its
byte width, and every cell the image covers is flagged `CellDiffOption::Skip`.
(ratatui 0.30 replaced the old `Cell::skip` bool with `Cell::diff_option`, and
`Cell::skip`/`set_skip` are deprecated — reading `skip` now silently misses
every image cell.) That is why `capture_screen`, `paint_selection` and the
overlays all have to leave those cells alone: `app::is_image_cell` is the
shared probe and it tests `diff_option != None`, the kitty-tier test in
`images.rs` pins the marking, and images are suppressed entirely while an
overlay is up. `WFTUI_NO_IMAGES=1` or `NO_COLOR` forces tier 5.

The query itself is opt-in by circumstance (`images::detect_plan`, issue #532):
ratatui-image gives up after 2 s but leaks the thread it left blocked in
`read()` with `ICANON`/`ECHO` cleared, so it is only run on unix, with a
terminal on stdin, and no override. Windows (ConPTY never answers reliably) and
a piped stdin take the half-blocks fallback picker instead, and
`WFTUI_GRAPHICS=kitty|sixel|iterm2|halfblocks|none` names the tier outright —
that is the escape hatch for a link whose DSR reply arrives after the timeout.
`tty.rs` snapshots the line discipline with `tcgetattr` before `detect()` and
re-applies it after `disable_raw_mode()` in `app::restore_terminal`, so a late
restore from the leaked thread can never be the last word on the exit state.

## Known gaps

- Attachment upload is implemented in `common` but not wired into the compose
  screen; attachment *viewing* is inline on tiers 1-4 (thumbnails, ≤ 40 % of
  the panel and ≤ 12 rows) and `1`–`9` opens the nth image in a browser on
  every tier. Because nothing fills `ComposeState::attachments` yet, the
  preview pane can only resolve `[IMG]` URLs — `[ATTACH]id[/ATTACH]` stays a
  text placeholder until upload is wired up (`resolve_image` already handles
  it, and is tested). Enter-to-expand is not implemented: it needs a second, larger
  encode inside an overlay, and overlays deliberately suppress image draws.
- Double-click word-select / triple-click line-select not implemented (drag +
  release = copy is).
- Windows packaging is documentation-only (native build; no CI).
