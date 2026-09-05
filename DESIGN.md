# wftui design system — implementation contract

Approved 2026-09-05. Reference canvas (screens + design-system sheet):
https://claude.ai/code/artifact/b25e6a15-0e3a-44e7-8c05-61c065f762fc
Local renders of every screen (open with the Read tool):
`/tmp/claude-0/-web-wftui-app/a4ba3bf4-920d-429b-aee6-d6d8f8196f11/scratchpad/wf/c1.png` … `c6.png`
(c1 Home + Thread, c2 Overlays + Reply, c3 Sign-in + Inbox, c4 Search + Keys card,
c5 80×24 Home + Sign-in, c6 alternate directions — ignore c6). The artboard sources
(`*.dc.html` in the same directory) are plain HTML with one `<div>` per terminal row;
they are the pixel-exact reference for column widths and copy.

## Principle

**Brand in the chrome, content in the reader's own colors.** The header band, key caps,
selection band and unread marks carry WindowsForum blue and the white bubble mark (a
` WF ` chip, chrome_fg on chrome_bg, echoing the rounded speech-bubble shape of
`wf-logo.png`). Body text, backgrounds and borders defer to the terminal
(`Color::Reset`), so the client looks right in light and dark terminals. Hard rules
1–7 in CLAUDE.md are unchanged.

The mark was originally a bare four-color (red/green/blue/yellow) quadrant glyph —
dropped 2026-09 for trademark reasons: shown on its own, with no bubble outline or
wordmark around it for context, it read as the Microsoft Windows logo. The `mark_r` /
`mark_g` / `mark_b` / `mark_y` theme roles that carried those colors are gone; nothing
should reintroduce a red+green+blue+yellow set as a reusable role.

## Colors by role (theme.rs)

| role | truecolor | 256 | 16-color | used for |
|---|---|---|---|---|
| text | Reset | Reset | Reset | all content |
| dim | #6F7C8B | 243 | DarkGray | meta, hints, column headers |
| faint | #313B47 | 238 | DarkGray | unfocused borders, rules, quote gutters |
| accent | #4DA3F5 | 75 | LightBlue | focused border, unread dot, links, headings, spinner |
| accent_bg | #0078D4 (fg #FFF) | 31 | Blue (fg White) | the ONE primary key cap per bar, active chip |
| selection_bg | #17304A | 236 | (REVERSED modifier) | selected row, bold on top |
| keycap_bg / keycap_fg | #232C38 / #E6EDF3 | 236 / 254 | DarkGray / White | every other key cap |
| chrome_bg / chrome_fg / chrome_dim | #0F6CBD / #FFFFFF / #BCD6EE | 24 / 231 / 153 | Blue / White / Gray | header band |
| badge_bg / badge_fg | #FFB902 / #1A1300 | 214 / 16 | Yellow / Black | unread counts in the header, only when > 0 |
| ok | #81B800 | 106 | Green | solved, gate ready, success toast |
| warn | #FFB902 | 214 | Yellow | open question, gate counting down, watching star |
| error | #F54E25 | 202 | Red | errors only |
| code_fg / code_bg | #F0C674 / #1A2028 | 221 / 234 | Yellow / Reset | [ICODE] and [CODE] |

There is no separate mark role: the header and sign-in marks are drawn straight from
`chrome_bg`/`chrome_fg` (see below). The four-color `mark (r, g, b, y)` role this table
used to list here is gone — dropped for the trademark reason above.

Tier detection: `NO_COLOR` → Mono (everything Reset, as today); `COLORTERM` =
`truecolor`/`24bit` → TrueColor; `TERM` containing `256color` → Ansi256; else Ansi16.
Keep the existing `Theme` methods/fields that screens call today (`text`, `dim`,
`accent`, `link`, `warn`, `error`, `header`, `selected_bg`, `base()`, `dim()`,
`title()`, `selected()`) working — map them onto the new roles — and add the new
roles beside them. `header` and `link` both map to `accent`.

## Glyphs (new `wftui/src/glyph.rs`)

Single-cell glyphs only. Emoji leave the client (📌 ❓ 📰 💡 💬 📁 🔗 📄 ✉ 🔔 are
double-width in some terminals and blank in others — they misalign lists today).

| name | unicode | ascii | meaning |
|---|---|---|---|
| unread | ● | * | unread thread / conversation |
| question | ? | ? | open question (warn) |
| solved | ✓ | v | solved question (ok) |
| sticky | » | > | sticky (accent) |
| article | ▪ | - | article thread (mark blue) |
| locked | ⊘ | x | closed thread (dim) |
| gutter | ┃ | \| | post body gutter (accent when selected, faint otherwise) |
| heading | ▌ | # | [HEADING] prefix (accent bold) |
| watching | ★ | * | watching (warn) |
| like | ♡ | <3 | reaction count |
| vote | ▲ | ^ | vote score |
| spinner | ◐ ◓ ◑ ◒ | - \ \| / | loading |
| gate_on / gate_off | ▮ / ▯ | = / . | write-gate countdown bar (10 cells) |
| crumb | › | > | breadcrumb separator |
| more | ▾ | v | "N more" scroll indicator |
| external | ↗ | ^ | link forum / page |
| image | ▣ | [img] | attachment placeholder |
| reply_alert / mention / quote_alert | ↩ / @ / ❝ | -> / @ / " | alert kinds |
| bubble corners | ╭ ╮ ╰ ╯ (BorderType::Rounded) | + (BorderType::Plain) | all panels |

`glyph::detect()` returns the ASCII set when `WFTUI_ASCII=1` or the locale
(`LC_ALL`/`LC_CTYPE`/`LANG`) is not UTF-8.

## Three zones (app.rs `draw`)

```
row 0        header band: "  WF  WindowsForum  ›  crumb  ›  crumb(bold) … right: user   Inbox [2]   Alerts [5] "
rows 1..n-3  body (screen renders here; one or two panels)
row n-2      key bar:  " [Enter] open  [j/k] move  [Tab] pane  … "  — exactly one primary (accent_bg) cap
row n-1      status:   " left text / toast                                   write gate ● ready "
```

* The mark is a single ` WF ` chip (4 cells): chrome_fg (white) on chrome_bg (brand
  blue), bold — the white bubble chip, same in both glyph sets since it is letters
  rather than block-drawing characters. Then `Windows` bold + `Forum` regular. (The
  mark used to be two `▀` cells painting a four-color red/green/blue/yellow quadrant;
  that reads as the Microsoft Windows logo out of context, so it was dropped.)
* Crumbs = titles of the screen stack (Login excluded). Overflow: drop the online
  count, then replace middle crumbs with `…`, then clip the last crumb with `…`.
* Badges render only when the count is > 0; otherwise `Inbox 0` in chrome_dim.
* Key caps are ` Enter `, ` j/k `, ` ^S ` — words, not symbols. Key bar clips with `…`
  from the right; below 90 columns screens supply a short hint set.
* Status right side shows the write gate: `● ready` (ok) or `● 24 s` (warn) plus a
  10-cell ▮▯ bar. `Gate::pending_wait()` in common/ratelimit.rs supplies the wait.
  Toasts (success/error) replace the left text and clear after 4 s.

## Panels (new `wftui/src/chrome.rs`)

One panel style everywhere: rounded borders, title ` Title ` bold-white when focused,
dim-bold when not; border accent when focused, faint when not; optional right title
segment `─┤ page 1 of 22 ├─` and bottom-left title ` 1–20 of 431 `. Two panels side by
side at ≥ 110 columns (Forums 37 wide + list; Inbox list 50 + view; editor 72 +
preview 48); one panel below that. `Tab` moves focus between panels.

Public API (phase 1 delivers these; later phases only consume them):

```rust
pub type Hint = (&'static str, &'static str);            // ("Enter", "open")
pub struct Hints { pub keys: Vec<Hint>, pub primary: usize }
pub fn header_line(theme:&Theme, g:&Glyphs, crumbs:&[String], user:Option<&str>,
                   inbox:u32, alerts:u32, width:u16) -> Line<'static>;
pub fn key_bar(theme:&Theme, hints:&Hints, width:u16) -> Line<'static>;
pub enum GateState { Ready, Waiting { left: Duration, total: Duration } }
pub fn status_line(theme:&Theme, g:&Glyphs, left:&str, left_style:Style,
                   gate:GateState, width:u16) -> Line<'static>;
pub fn panel(theme:&Theme, g:&Glyphs, title:&str, focused:bool,
             right:Option<&str>, bottom:Option<&str>) -> Block<'static>;
pub fn keycap(theme:&Theme, key:&str, primary:bool) -> Span<'static>;
pub fn chip(theme:&Theme, text:&str) -> Span<'static>;            // " Win11 " on keycap_bg
pub fn initials_chip(theme:&Theme, username:&str) -> Span<'static>; // 2 letters, hashed bg
pub fn spinner(g:&Glyphs, tick:usize) -> &'static str;
pub fn short_prefix(prefix:&str) -> String;  // "Windows 11" -> "Win11", "Windows Server" -> "Server"
```

`Screen` gains `fn hints(&self) -> Hints` and `fn crumb(&self) -> String` (dispatch per
screen, like `render`/`on_key`). Per-screen hint lines drawn inside panels are removed;
the app draws the key bar. `app::footer_line` goes away.

## Row grammar (thread lists, ≥ 90 cols)

```
 ● ? [Win11] Title…………………………………………………  Started by    Replies  Active
 │ │  │      45-col title incl. inline prefix chip  dim 12     right 7  right 6
 │ │  └ prefix chip (short_prefix), inline before the title
 │ └ type glyph column: ? solved sticky article locked (blank for discussion)
 └ unread column: accent ● and bold title; read rows plain
```
Column header row in dim above the list. Selected row = selection_bg across the panel
width + bold. Age: `14h` under a day, `6d` under a month, then `Jul 26`, then `2025`.
Narrow (< 90): drop Started by, author 9 wide, age 6.

## Screens (what each looks like — see the PNGs)

* **Home** (`c1.png` top): Forums panel (QUICK: `L` Latest, `1` News, `2` Security,
  `3` Tutorials; then categories as bold-dim CAPS with forums indented two spaces,
  link forums with ↗, `›` marker on the current forum, `▾ N more` when scrolled) +
  thread list panel titled with the forum name, right `page 1 of 22`, bottom
  `1–20 of 431`. Keys: Enter open · j/k move · Tab pane · N new thread · m mark read ·
  / search · g go to… · ? help · q quit.
* **Thread** (`c1.png` bottom): one panel, right `28 posts · page 1 of 2`, bottom
  `post 2 of 28`. First line: prefix chip · forum · started by · date · views ·
  replies, `★ watching` right. Post card: initials chip (or avatar image, phase 3),
  username bold, `AI` chip for the site bot (user 125694), `STAFF` chip for staff,
  date and `#n` right-aligned; second line dim meta; body lines behind a gutter
  (accent for the selected post, faint otherwise); `♡ n   ▲ n` footer; links listed
  as `[n]`. Keys: r reply · j/k scroll · n/N post · l like · v vote · o links ·
  1-9 image · u open in web · Esc back.
* **Reply** (`c2.png` bottom): editor panel `Reply` (72) + `Preview` (48) rendering
  the draft through common::bbcode; editor bottom line = BBCode caps ^B ^I ^K ^Q ^U
  and char count. Keys: ^S send · ^O preview on/off · ^Y paste · ^A attach · Tab
  field · Esc discard.
* **Sign in** (`c3.png` top, `c5.png` bottom): centered 72-wide panel with the white
  bubble mark (rounded top-left/top-right/bottom-left, square bottom-right — matching
  `wf-logo.png` — carrying bold blue `WF`), steps 1-2-3, the short link in its own box,
  spinner while polling.
* **Inbox** (`c3.png` bottom): `Inbox` panel 50 wide with tab row `Conversations 2 |
  Alerts 5`, two-line conversation rows; view panel 70 wide with message cards.
* **Search** (`c4.png` top): `/ query` line with `author` and `in` on the right,
  chip row (All/Threads/Posts · Latest/Relevance), results as two lines: kind label +
  title with hits in warn-bold, forum · age right-aligned, dim snippet.
* **Go-to palette + g which-key** (`c2.png` top): Ctrl+K or `:` opens a 60-wide
  `Go to` panel over the dimmed body (forums, actions with their key, members);
  `g` shows a small which-key panel bottom-right (n news · s security · t tutorials ·
  l latest · i inbox · a alerts · h home · p profile · g top).
* **Keys card** (`c4.png` bottom): `?` opens a 96-wide two-column card grouped
  MOVE / THIS THREAD / EVERYWHERE / MOUSE & CLIPBOARD, any key closes.

## Graphics tiers (phase 3, `wftui/src/images.rs`, cargo feature `images`, default on)

1 kitty (unicode placeholders) · 2 sixel · 3 iTerm2 · 4 half-blocks · 5 text
placeholder `▣ name · W×H` (digits 1–9 open in browser). Use the `ratatui-image`
crate; run its terminal query BEFORE `event::spawn_reader` takes stdin (hard rule 3)
and never put image bytes in span content (hard rule 1). Attachments ≤ 40 % of the
panel width and ≤ 12 rows, aspect kept; avatars 2 rows × 5 cells; the logo 7 × 16 on
sign-in. Thumbnails only, fetched through `api_gate`, cached under the config dir
(`cache/img/`, capped), disabled by `WFTUI_NO_IMAGES=1`.

## Keys stay stable

Every current binding keeps working: 1/2/3, L, N, m, r, l, v/V, o, u, p/P, c, a, s,
i, n, ?, q, Esc, Tab, Ctrl+S, Ctrl+Y, Ctrl+C, Ctrl+L, [ ], j/k, arrows, mouse.
New: `/` (search, alias of s), `g` prefix, Ctrl+K / `:` palette, `1-9` images, `w` watch.

## Gates

```bash
cd /web/wftui_app
cargo check                                            # while iterating
cargo test --workspace                                 # 188 tests today; keep them green, add yours
cargo clippy --all-targets --release -- -D warnings    # must stay at 0
cargo build --release && cp target/release/wftui bin/wftui
```
Do not commit. Do not touch `services/mirror`, `public_html`, or anything outside
`/web/wftui_app`. The reference renders and artboards are read-only.
