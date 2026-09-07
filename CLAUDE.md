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
# Deploy on this server. Install-then-rename, NOT `cp`: a plain copy over a
# running wftui fails with "Text file busy", while a rename swaps the path
# atomically and leaves the running process on its old inode.
install -m755 bin/wftui /usr/local/bin/wftui.new && mv -f /usr/local/bin/wftui.new /usr/local/bin/wftui
```

The "gates" every change must pass before it is committed: both test invocations,
both clippy invocations, a release build, and `/usr/bin/grep -rn $'\x1b' wftui/src`
printing nothing (hard rule 1).

Two harnesses exist for checking this client against the real site rather
than against a belief about it, and both are worth reaching for:

```bash
# The quote block the client would write for a post, for the site's own
# ContentIntegrity analyzer to judge (see "Quoting", below).
cargo run -q -p common --example quote_probe -- <message-file> <username> <post_id> <user_id>

# Any endpoint's real shape, before writing a model for it. An API key
# bypasses OAuth scopes, so it proves shapes, NOT permissions (#695).
KEY=$(grep -m1 '^XF_API_KEY=' /web/.env | cut -d= -f2-)
curl -s -H "XF-Api-Key: $KEY" 'https://windowsforum.com/api/<path>' | jq .
``` The release binary is committed at `bin/wftui` so
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
  file instead of bricking startup. `models.rs` — XF API shapes. Field names come from the
  entity classes under `/web/public_html/src/XF/Entity/*.php`
  (`setupApiResultData` / `getStructure`) or from a captured response, never
  from a guess — and **the fixture must be a whole captured body**. Three
  bugs shipped behind hand-written fixtures that agreed with the wrong guess:
  `rating_average` for `rating_avg` (#696), a map for `tags` when the wire
  sends an array (#698), and both halves of the attachment upload (#709).
  Every field here is `#[serde(default)]`-tolerant, which is what makes a
  wrong name silent rather than an error; `null_default` covers the other
  half, since `default` applies to a *missing* field and not to an explicit
  `null`. `bbcode.rs` — BBCode →
  styled `Chunk`s, golden-tested against real posts (XF's parser semantics:
  `[tag="v]"]` quoted values may contain `]`, `[i words]` without `key=` options is
  literal text, `[CODE]`/`[ICODE]` bodies are verbatim, `[USER]` already carries
  the `@`). `osc.rs` — OSC 8 hyperlinks + OSC 52 clipboard with tmux DCS
  passthrough (a `screen*` TERM without `$TMUX` is tmux-over-ssh: dual-deliver).
  `drafts.rs` — unsent composer drafts, stored like the token set (one JSON
  file, `0600`, temp-file-plus-rename) but with the opposite failure policy: a
  corrupt store is quarantined and read as *no drafts* rather than surfaced as
  an error, because a convenience file must never stand between the user and
  the composer, and an unknown key kind is dropped so a newer build's store
  cannot brick an older one. `config.rs` — constants + the `WFTUI_*` env
  overrides.

### `wftui/` — the binary

- `main.rs` runs graphics detection (reads stdin — before the reader thread), then
  `app::run`. `event.rs` is the dedicated blocking reader thread (hard rule 3).
  `tty.rs` snapshots termios before detection and re-applies it on every exit path.
- `app/` is one `App` with an event loop ticking every 50 ms, a `Msg` pump
  (every async result is a `Msg` variant handled in `handle_msg`),
  `handle_key` / `handle_mouse`, and `draw`. It is split across continuation
  `impl App` blocks (#714), so a method's home is a filing decision and
  nothing else: `mod.rs` holds `App`/`Msg`/`TaskError`/`TerminalGuard`,
  `event_loop`, `draw`, the screen stack and the tests; `msg.rs` holds
  `handle_msg`; `actions.rs` holds `execute_action` and the fetches it
  spawns; `input.rs` keys, mouse and selection; `session.rs` bootstrap,
  recovery, pollers, teardown and logout. A method moved out of `mod.rs` is
  `pub(super)` — the same scope a private `fn` in `app` always had, just
  spelled out. **A source-scan test that reads `include_str!` must list every
  one of these files**; `every_app_module_is_covered_by_the_source_scans`
  fails when a new module is not added to that list, because a scan of one
  file would otherwise keep passing while covering almost nothing.
  Screens never see `App`: they own
  their state, render into a `Rect`, and return `Action`s from `on_key`; the app
  executes actions (`execute_action`) by spawning tasks that send `Msg`s back.
  Loads carry their identity and stale replies are dropped — copy that
  pattern for any new fetch. `ThreadLoaded{id}` and `SearchDone{key}` name
  their content; `Bootstrap{generation}` and `ForumLoaded{seq}` carry a
  generation instead, because a thread list is one screen that shows many
  forums in turn: matching on `node_id` alone let a page from the forum the
  reader had just left land in the list that replaced it (#705). A fresh load
  mints the generation; a fill inherits it.
- `screens/mod.rs` — `Screen` enum + dispatch (`render`, `on_key`, `hints`,
  `crumb`, `esc_intent`, `goto_top/bottom`, `web_url`, `selected_index`,
  `select_index`, `select_post`, `focus_pane_at`, `click_field`,
  `set_image_policy`, `image_requests`, `keys_group`), state structs, `Action`
  enum. Three screens are two-pane with a renderer-set `dual` flag: `Home`
  (Forums tree + thread list) and `Inbox` (Conversations/Alerts + view) at
  ≥ 110 cols, `MediaGallery` (categories + media) at ≥ 90;
  `ThreadList`/`ConversationView` are the narrow pushables. Renderers live in
  `screens/{browse,social,misc,library}.rs` — `library.rs` holds the Media
  Gallery, the Resource Manager, the resource page and the image viewer.
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
  for single-line fields). Both composer bodies keep an `editor::WrapCache`
  (#678): how a logical line wraps depends only on that line and the width, so
  a keystroke re-wraps just the line it touched, and the renderer builds
  `Line`s only for the visible window rather than one per row of the draft.
  `visual_rows_of` stays the reference implementation the cache is tested
  against. `hit.rs` — the per-frame mouse/touch hit map.
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

## What the client can do, and the contracts behind it

The sections below are the behaviour a change is most likely to break. Each
one exists because the obvious implementation was wrong in a way the server
only tells you about later — read the one you are touching before you touch
it.

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

## Images, reading state and navigation

- **Images render where the message puts them** (#693). `ThreadViewState::rebuild_lines`
  walks the post's chunk stream and lifts each resolvable image reference out
  into a caption plus reserved rows at that point; only attachments the
  message never referenced are listed underneath, as XenForo does it. The
  numbering (`n of N`, and the `1`-`9` digits) follows display order, so the
  digit under a caption is the picture above it. `post_images()` is the one
  place that order is decided — renderer and key handler both read it.
- **Enter expands a picture** into `Screen::ImageView` (the standing
  "Enter-to-expand" gap). An image attachment the API gave no usable URL for
  still earns its caption row and its number; it simply has nothing to draw.
- **Reading marks read** (#694). A thread is marked read up to the newest
  post that was actually on screen (`seen_date`, stamped by the renderer),
  sent once when the view leaves the stack — never to "now" on open, so a
  half-read thread stays half unread. XF refuses to move the marker
  backwards, which makes a re-read idempotent. `App::pop_screen` is the ONE
  way a screen leaves the stack, so no exit path can forget it. Showing the
  Alerts tab marks alerts *viewed* (`/alerts/mark-all`), which is what clears
  the counter; Enter/`m` on a row still marks that one read.
- **Pagination is the client's own model** (#699/#700): a list holds a window
  of consecutive server pages (`page ..= page + pages_loaded - 1`), because
  XF fixes page size server-side and ignores `per_page`/`limit`. The window
  fills to the pane and then grows as the reader approaches its end
  (`autoload_more`, once per loop tick so keys, wheel, `G` and clicks share
  one path). `[`/`]` step by the window, not by one page.
- **The breadcrumb is navigation** (#700): each crumb pops back to the screen
  it names, measured off the spans the header actually drew (the row elides
  its own middle, so a recomputed position points at the wrong place).

## Visibility and content state

**The server is the gate; the client is the label** (#704). XF's API only
sends content the token's user may see — a deleted post is simply absent for
a reader without `viewDeleted`, and a hidden node's threads never arrive — so
the client must never invent visibility rules of its own, and must never
refuse to draw something the server chose to send (that would hide a
moderator's own queue from them).

What the client owes is saying *which state* a thing is in, because a
moderator gets deleted and awaiting-approval items in the same lists as
everything else:

- `Thread::discussion_state` / `Post::message_state` map to
  `models::ContentState` (`Visible` / `Moderated` / `Deleted`); anything
  unrecognised, absent or null is Visible.
- A deleted thread's row is struck through and dimmed; a moderated one is
  italic. Both carry a chip naming the state, ahead of the prefix.
- A non-visible post says so on its own row above the body ("deleted ·
  visible to moderators", "awaiting approval · not yet public").

## BBCode parity — the styling tags

`common/src/bbcode.rs` carries `color`, `size`, `heading`, `align`,
`highlight` and `mono` on `Style`; `screens::style_from` turns them into
terminal attributes and `wrap_spans_aligned` does the alignment, because only
the renderer knows a line's width.

**Semantics come from `XF\BbCode\Renderer\Html`, not from intuition** — two
of them go the opposite way to the obvious reading, and both are common in
this site's posts:

- `getTextSize`: a *pure integer* maps onto XF's 9/10/12/15/18/22/26-px
  ladder, and anything **above 7 is the top of it** (`[SIZE=200]` is the
  largest, not an error); an integer ≤ 0 is no size; otherwise only a bare
  `Npx` counts, clamped 8–36. `[SIZE=+2]`, `[SIZE=-1]`, `1.5em` and `120%`
  therefore render with **no size change at all** — and note Rust's own
  `parse` accepts `"+2"` as 2, so the naive reading draws "two steps larger"
  as near-smallest.
- `getHeadingTagMap`: `[HEADING=1/2/3]` are h2/h3/h4 and **everything else is
  a plain `div`** — no value, `0`, `9` or a word carries no heading weight.
  This is the site's most-used tag (100k+ posts), so its edges matter.

Colours accept what CSS accepts (named, `#rgb`, `#rrggbb`, `rgb()`/`rgba()`,
including `%` channels); a value CSS would reject leaves the enclosing colour
in force, as CSS does. `theme::quantize` brings a colour down to the
terminal's tier — exact on TrueColor, nearest cube/grey-ramp entry on 256,
nearest basic on 16, and dropped entirely on mono.

Terminals cannot scale glyphs, so size reads as emphasis (5–7 bold, 1–3 dim)
and headings as bold + accent + underline by level. `[HIGHLIGHT]` is
`REVERSED` (legible over any colour, on every tier), not bold.

`real_posts_parse_completely_and_carry_their_styles` runs the parser over 40
captured live post bodies in `common/src/testdata/`; that corpus is what
caught the `SIZE` reading. Re-capture it when the parser changes.

## Writing: reply, quote, edit, delete, solution

The thread view's write keys are `r` reply, `Q` quote (see below), `e` edit,
`D` delete, `S` mark solution. Three rules hold across them:

- **The API's own permission flags decide what is offered.** Posts carry
  `can_edit` / `can_soft_delete` / `can_hard_delete`; a key absent from the
  bar is a key the server would refuse. The server enforces regardless — the
  flags are for the key bar, never for safety.
- **Delete is soft and takes two presses.** Soft is what XF's own UI does and
  leaves the post recoverable; the first `D` arms, the second deletes, and
  any other key disarms (`ThreadViewState::confirm_delete`). No hard delete
  from a keystroke.
- **A failed write keeps the draft.** The editor stays open with its text and
  the error; losing a rewritten post to a 403 is worse than the 403.
- **So does Esc.** Esc closes the composer instantly — no confirm prompt, so
  the ordinary empty-composer case pays nothing — and the draft is saved and
  offered back the next time *that same composer* opens (#715). `^X` discards
  a resumed draft, and puts back whatever the composer would have shown
  without one: for an edit that is the post's current text, so discarding a
  draft must never empty the post.

Draft rules worth knowing before touching `pop_screen` or `push_screen`:

- **Restore lives in `push_screen`, save in `pop_screen`.** Every composer
  reaches the stack through `push_screen`, so a new opener cannot forget to
  restore. Do not add a composer that bypasses it.
- **The successful-write arms must not go through `pop_screen`,** which
  *saves*. They call `close_sent_composer`, which removes the screen and
  forgets the draft: the post is on the site, so the next reply to that
  thread must not come up pre-filled with it.
- **Drafts are keyed by `ComposeTarget` identity**, so an edit of post 5 and
  a reply to the thread holding it never share one.
- **Drafts are cleared on an explicit sign-out and on an identity change**
  (the #581 teardown), because an unsent post is private writing and the next
  person to sign in on that machine must not be offered it. A session that
  merely *expires* keeps them — that user is coming back.

## Quoting — the ContentIntegrity contract

`Q` in the thread view replies with the selected post quoted, and the block
it writes has to satisfy **WindowsForum's own `ContentIntegrity` addon**
(`public_html/src/addons/WindowsForum/ContentIntegrity/Analyzer.php`), which
re-derives every quote's fingerprint on save and compares it against the
source post. A quote that does not match is recorded as forged or altered.

The hash is **derived, never supplied** — there is nothing for the client to
sign. What `bbcode::quote_block` owes is a block the analyzer re-derives the
same way:

- `[QUOTE="<username>, post: <id>, member: <user_id>"]` — both keys plain
  digits, each appearing once (anything else is `quote_malformed`), and
  `member` matching the source's real author (`quote_author_altered`). A
  comma in the username would read as an attribute separator, so it is
  replaced.
- The body is the source message with **every nested `[QUOTE]` stripped**,
  which is what `XF\Str\Formatter::getBbCodeForQuote` (via
  `ProcessorAction\StripQuotes`) produces. A nested quote inside an
  attributed quote is `nested_quote_in_attributed_quote` on its own, whatever
  the text says.
- Nothing else may change: the analyzer requires the quote's semantic text
  (whitespace collapsed, NFC-normalised) to be a **substring** of the
  source's, and any link in the quote that is not in the source is
  `quote_link_injected`.

Verify changes here against the analyzer itself, not against this note —
`common/examples/quote_probe.rs` prints the block for a real post and the
addon can be run over it directly. That is how the current implementation was
confirmed (`valid=true, violations: 0`), and how the three failure modes
above were confirmed to fire.

## Videos, and why they are not played here (#710, #711)

A video renders as a **row you can press** — `▶ YouTube video — open`, where
the message put it — rather than a link with a `[n]` marker. Click it or
press `W`, and it opens in a browser (with the URL on the clipboard, which is
what makes it work from a remote session). 25,839 posts here carry
`[MEDIA=youtube]`, which the parser already resolves to a watch URL, so
`browse::post_videos` recognises those URLs rather than re-parsing the tag —
which also picks up a plain link somebody pasted without one. The rows join
the same block walk as inline images (#693), and `video_lines` maps each row
to its own index so clicking the second video opens the second.

**Playing video in the terminal was built twice and removed.** Not because it
does not work — it does — but because of what it costs, and the numbers are
here so nobody has to rediscover them:

| approach | result |
|---|---|
| #710: suspend the UI, hand the terminal to `mpv` | worked; the screen goes away and comes back, which is not the app |
| #711: decode in-app and paint frames in a pane | worked; **7.2 Mbit/s** of terminal traffic on half-blocks, **26 Mbit/s** on kitty |

The decode was never the problem: ffmpeg runs ~32x realtime (5 s of 360p in
0.15 s) and encoding a frame costs ~1.1 ms. The problem is the wire. A
terminal has no video codec, so every frame crosses the connection as either
per-cell SGR colour runs or a base64 image — 74 KB and 266 KB per frame
respectively at a normal pane size. For a 360p video the reader could have
watched at about 1 Mbit/s, and on this deployment every byte of it flows out
of the production web server, which also downloads the video a second time.

So: a video opens where video is cheap. If this is revisited, revisit the
arithmetic first — it is the whole argument, and it does not improve with a
better implementation.

## Attachments

`^F` in the composer opens a path prompt — a terminal has no file picker, so
the path is typed (`~` expands) — and the file is uploaded, inserted at the
caret as `[ATTACH]id[/ATTACH]`, and remembered.

The **attachment key** is the whole mechanism: an upload is attached to
nothing until a write carries the same key, and XF checks the key's context
against that write (`context[thread_id]` for a reply, `context[node_id]` for
a new thread, `context[post_id]` for an edit — `AttachContext`). One key per
draft: the first file mints it, later files reuse it, and
`ComposeState::attachment_key` is what `reply`/`create_thread`/`edit_post`
carry. Conversations take attachments under a different content type this
client does not upload to, so `^F` is not offered there at all.

Two wire shapes worth remembering, both of which the old code had wrong while
its tests passed on invented fixtures:
`POST /attachments/new-key` returns **`{"key": …}`**, not `attachment_key`,
and `POST /attachments/` returns **`{"attachment": {…}}`** — decoding that
envelope as a bare `Attachment` yields a silently *blank* one, because every
field on that model is `#[serde(default)]`.

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

- **The politeness gates are real time in tests too.** A second write on one
  client sleeps the full 30 s `WRITE_COOLDOWN_MS`, and one test doing that was
  30.6 s of the `common` suite's 36.8 s — 36.8 s of wall clock for 1.9 s of
  CPU (#712). A test about *what* goes on the wire calls `open_gates` before
  **each** write (a successful write `penalize`s the gate back to the full
  cool-down, so once up front is not enough).
  `write_gate_is_opened_in_every_test_that_writes_twice` fails when a new
  multi-write test forgets. Tests whose subject *is* the spacing —
  `upload_attachment_re_anchors_the_write_gate_on_success`, the `ratelimit`
  module's own, and the image-lane test that queues ten fetches — keep
  waiting on purpose and are allowlisted. `start_paused = true` works only for
  pure-timer tests: a wiremock server over loopback stalls on a paused clock.
- **A fixture is a whole captured body, in a file.** They live in
  `common/src/testdata/*.json` and come in through `include_str!`; capture
  them with `cargo run -p common --example capture_fixture`, which never
  selects fields. Three bugs shipped behind fixtures that disagreed with the
  wire (#696, #698, #709) — #698 specifically because a re-capture was piped
  through `jq`, which silently dropped `tags`. Note the root `.gitignore`
  excludes `*.json`; `common/src/testdata/.gitignore` negates it for that
  directory, so a new fixture there is committable — that is why the older
  `styled_posts.txt` is a `.txt`.
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
  `common/src/testdata/styled_posts.txt` is a captured corpus of real bodies
  that `real_posts_parse_completely_and_carry_their_styles` runs over — it is
  what caught the `[SIZE=+2]` misreading. Re-capture it when the parser
  changes:
  ```bash
  mariadb -N --raw wf_wf -e "SELECT CONCAT(message, '\n===WFTUI-POST-BOUNDARY===')
    FROM xf_post WHERE message REGEXP '\\[(SIZE|COLOR|HEADING|CENTER|FONT|HIGHLIGHT)='
    ORDER BY post_id DESC LIMIT 40" > common/src/testdata/styled_posts.txt
  ```
- **A wiremock fixture must be a captured body, not a written one, and the
  WHOLE body.** Three separate bugs shipped because a hand-written fixture
  agreed with the wrong guess (#696, #698, #709) — and one of those survived a
  round of "capture it properly" because the capture was filtered through `jq`
  to the fields the model already believed in, which dropped the one field
  whose shape broke it.

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

The breadcrumb is navigation, not decoration (#700): each crumb pops back to
the screen it names, and the brand at the far left is Home. Its hit boxes are
measured off the spans `header_line_hits` actually drew — the row elides its
own middle and truncates its last crumb to fit, so a recomputed position
points at the wrong place. The crumb for the screen you are on is inert, and
no crumb may pop the sign-in gate.

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
Five surfaces draw them: the thread view (post images where the message puts
them, avatars 2×5), the compose Preview pane (`Chunk::Image` / `Chunk::Attach`
from the draft, cached in `PreviewCache`, fetched only after the draft has
been still for 500 ms), the Media Gallery's row thumbnails, a resource's icon,
and the full-size viewer. `images::fit` is the inline case and hard-codes the
thumbnail caps; `fit_within` fits an arbitrary box and is what the viewer and
the gallery rows use. Inline thumbnails ≤ 40 % of the panel and ≤ 12 rows;
fetches stream through `fetch_bytes` with a hard byte cap — 2 MiB for
thumbnails, 8 MiB for the viewer (`Request::full`) — then the disk cache under
`<config dir>/cache/img/` (32 MiB, 0600, atomic). `fetch_bytes` sends the
bearer token **only** to our own API base, which the gallery's
`/api/media/{id}/data` needs and no third-party `[IMG]` host may ever see.
Decoding is capped at 4096 px per axis and a 64 MiB allocation budget (#679);
an image past either returns `Err` and falls back to the placeholder.

## Known gaps

- Profile posts, watched/unread feeds and moderator actions beyond
  delete/solution (move, change type, approve) are not built. XF's REST API
  exposes no watched-content or unread-thread endpoint at all, so that one
  needs a TuiLink addon route like the login relay does.
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
- The library screens are read-only: no album browsing or media comments
  (`XFMG:Albums`, `XFMG:Comments`), and a resource shows its description but
  not its updates, reviews or versions (`XFRM:ResourceUpdates`,
  `ResourceReviews`, `ResourceVersions`). All of those endpoints exist.
- There is no @mention completion.
