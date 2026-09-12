//! Inline graphics: the five tiers of DESIGN.md, the sizing math, the decoded
//! protocol store, and the on-disk thumbnail cache.
//!
//! Tiers (DESIGN.md "Graphics tiers"), highest first:
//!
//! | # | tier | how it draws |
//! |---|---|---|
//! | 1 | kitty | unicode placeholders, image transmitted once |
//! | 2 | sixel | one sixel payload per image rect |
//! | 3 | iTerm2 | inline base64 payload |
//! | 4 | half-blocks | plain coloured cells, no escape sequences at all |
//! | 5 | text | `▣ name · W×H` placeholder + "press N" hint |
//!
//! **Hard rule 1** (never put escape bytes in span content) is kept by never
//! writing an escape sequence in this module. The only thing that emits one is
//! `ratatui-image`'s own widget, and it does so through ratatui's sanctioned
//! diff-option path: the whole payload goes into exactly one anchor `Cell`'s
//! symbol (marked `CellDiffOption::ForcedWidth(1)`) and every other cell of
//! the image rect is marked `CellDiffOption::Skip`, so the frame diff can
//! never re-emit a fragment of it out of context. Because those
//! cells are not text, `App::capture_screen` (mouse selection) and
//! `App::paint_selection` skip them, and images are not drawn at all while an
//! overlay is up (`overlay::dim_body` would re-style the anchor cell, and the
//! overlay's `Clear` erases the rect for that frame anyway).
//!
//! **Hard rule 3**: the terminal capability query reads stdin directly, so it
//! runs in `main.rs` before `event::spawn_reader` exists. See `Images::detect`.

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};

use ratatui::Frame;
use ratatui::layout::Rect;

use common::models::{Attachment, User};

#[cfg(feature = "images")]
use ratatui::widgets::Widget;

/// The sign-in logo is embedded, not fetched: it must render before there is
/// a session (or even a network). `Request::key` carries this sentinel instead
/// of a URL.
pub const LOGO_KEY: &str = "wftui:logo";

#[cfg(feature = "images")]
const LOGO_BYTES: &[u8] = include_bytes!("../assets/wf-logo.png");

/// Attachment images take at most this share of the panel width (DESIGN.md).
pub const WIDTH_PERCENT: u16 = 40;
/// …and at most this many rows, whatever the aspect ratio says.
pub const MAX_ROWS: u16 = 12;
/// Avatar slot in a post header: 2 rows × 5 cells (DESIGN.md).
pub const AVATAR_COLS: u16 = 5;
pub const AVATAR_ROWS: u16 = 2;
/// A resource's icon on its page (#697): the same two-row shape the post
/// avatar uses, one size up, so the header block keeps its height whether
/// the icon loads or not.
pub const ICON_COLS: u16 = 8;
pub const ICON_ROWS: u16 = 4;
/// Sign-in logo: 7 rows × 16 cells (DESIGN.md).
pub const LOGO_COLS: u16 = 16;
pub const LOGO_ROWS: u16 = 7;
/// Thumbnails are small. Anything larger is a mistake or an attack, and the
/// cap is enforced by `WfApiClient::fetch_bytes` (Content-Length *and* the
/// body actually received).
pub const MAX_IMAGE_BYTES: usize = 2 * 1024 * 1024;
/// The full-size viewer's budget. A thumbnail is capped tight because a
/// screenful of them is fetched at once; one picture the reader asked to see
/// is allowed to be a real photograph.
pub const MAX_VIEW_BYTES: usize = 8 * 1024 * 1024;
/// Pixel-dimension ceiling for decode. `MAX_IMAGE_BYTES` caps the compressed
/// body, but a hostile header can still declare gigantic dimensions (a 2 MiB
/// WebP claiming 30000×30000 asks for a ~3.6 GB RGBA buffer, and some
/// decoders allocate straight from the header). The allocation failure would
/// abort the process — unwinding past `catch_unwind` and the terminal
/// restore — so refuse anything oversized before a pixel is read.
///
/// 4K per axis, not 8K (issue #679): the target is a thumbnail of at most
/// ~200×24 cells, and 8192² RGBA is a 268 MB transient for something that
/// ends up a few kilobytes of escape payload.
pub const MAX_DECODE_DIM: u32 = 4096;
/// Companion byte budget for the decoded buffer. A per-axis cap alone still
/// admits 4096×4096 (67 MB); this is what the decoder actually bills its
/// allocations against, so a lopsided 4096×2048×4 image is refused on the
/// same rule as a square one.
pub const MAX_DECODE_ALLOC: u64 = 64 * 1024 * 1024;
/// Decoded protocols kept in memory. Each is an encoded payload sized for one
/// rect, so this is bounded by roughly (visible images × panel sizes seen).
pub const LRU_CAP: usize = 32;
/// Ceiling for `cache/img/`. Pruned oldest-first back to 80 % on overflow.
pub const DISK_CAP_BYTES: u64 = 32 * 1024 * 1024;

/// The graphics tier in use, as DESIGN.md numbers them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Tier {
    Kitty,
    Sixel,
    Iterm2,
    Halfblocks,
    /// No inline graphics: text placeholders and the "press N" hint.
    #[default]
    Text,
}

impl Tier {
    /// True when this tier can paint pixels into a rect.
    pub fn inline(self) -> bool {
        !matches!(self, Tier::Text)
    }

    /// Short name for the status row / logs.
    pub fn label(self) -> &'static str {
        match self {
            Tier::Kitty => "kitty",
            Tier::Sixel => "sixel",
            Tier::Iterm2 => "iterm2",
            Tier::Halfblocks => "half-blocks",
            Tier::Text => "text only",
        }
    }
}

/// What the renderers need to know about graphics, small and `Copy` so it can
/// be stamped onto a screen's state before every frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    pub tier: Tier,
    /// Terminal cell size in pixels, as reported by the capability query.
    pub font: (u16, u16),
}

impl Default for Policy {
    fn default() -> Self {
        // The halfblocks default from ratatui-image: arbitrary but a sane 1:2.
        Policy { tier: Tier::Text, font: (10, 20) }
    }
}

impl Policy {
    pub fn inline(&self) -> bool {
        self.tier.inline()
    }

    /// True only for the tiers that paint real pixels (1-3). Half-blocks is
    /// coloured cells, so artwork the design already draws out of block glyphs
    /// — the sign-in mark — stays as it is there.
    pub fn pixels(&self) -> bool {
        matches!(self.tier, Tier::Kitty | Tier::Sixel | Tier::Iterm2)
    }
}

/// An image slot reserved by `rebuild_lines`, in the screen's own `lines`
/// coordinates. `x` is the column offset inside the panel's inner area.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Slot {
    pub line: usize,
    pub x: u16,
    pub cols: u16,
    pub rows: u16,
    pub key: String,
}

/// One image the frame just rendered wants painted, in absolute screen
/// coordinates. Screens fill these during `render`; `App::draw` paints them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    /// Absolute URL, or [`LOGO_KEY`] for the embedded sign-in logo.
    pub key: String,
    pub rect: Rect,
    /// True for the full-size viewer (#697): its bytes get the larger cap,
    /// because a thumbnail budget is not what a whole picture costs. The
    /// decode caps (#679) are what actually bound memory either way.
    pub full: bool,
}

/// An image the store does not hold yet, handed to the app to load off-thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pending {
    pub key: String,
    pub cols: u16,
    pub rows: u16,
    pub full: bool,
}

impl Pending {
    pub fn store_key(&self) -> String {
        store_key(&self.key, self.cols, self.rows)
    }
}

/// Decoded payloads are sized for one exact rect, so the store is keyed by
/// size as well as source — a resize re-encodes rather than stretching.
pub fn store_key(key: &str, cols: u16, rows: u16) -> String {
    format!("{cols}x{rows}|{key}")
}

/// The decoded, terminal-ready payload. `()` without the `images` feature, so
/// the message pump and the store compile identically either way.
#[cfg(feature = "images")]
pub type Decoded = ratatui_image::protocol::Protocol;
#[cfg(not(feature = "images"))]
pub type Decoded = ();

/// A finished load: the payload plus the source image's pixel size.
///
/// The size is not decoration. An attachment arrives from the API with
/// `width`/`height` already on it, but a bare `[IMG]url[/IMG]` in a draft
/// carries no metadata at all, so the compose preview's caption
/// (`▣ name · W×H`) can only learn its dimensions from the
/// bytes — this is how they cross back.
///
/// No `Debug`: `ratatui_image::protocol::Protocol` does not implement it.
#[derive(Clone)]
pub struct Loaded {
    pub decoded: Decoded,
    pub px: (u32, u32),
}

/// Source pixel sizes learned this session, keyed by the image's own key
/// (the URL, or [`LOGO_KEY`]) — *not* by the size-qualified store key, since
/// the pixel size of the source does not change with the box it is drawn in.
pub type Sizes = HashMap<String, (u32, u32)>;

/// The image key inside a store key (`store_key` prefixes it with the box).
pub fn source_of(store_key: &str) -> &str {
    store_key.split_once('|').map(|(_, key)| key).unwrap_or(store_key)
}

// ---------------------------------------------------------------- sizing ---

/// Fit `px` (an image's pixel size) into the caps: ≤ `WIDTH_PERCENT` of
/// `panel_cols` and ≤ `MAX_ROWS` rows, aspect kept, rounded up to whole cells.
///
/// `font` is the terminal's cell size in pixels; without it "aspect kept" is
/// meaningless, because a cell is roughly twice as tall as it is wide.
pub fn fit(panel_cols: u16, px: (u32, u32), font: (u16, u16)) -> (u16, u16) {
    if panel_cols == 0 {
        return (0, 0);
    }
    let col_cap = ((panel_cols as u32 * WIDTH_PERCENT as u32) / 100).max(1) as u16;
    let (iw, ih) = (px.0.max(1), px.1.max(1));
    // Attachment metadata is remote input. Keep ratio products wide and
    // clamp before converting back to terminal-cell integers.
    let (fw, fh) = (u128::from(font.0.max(1)), u128::from(font.1.max(1)));
    let iw = u128::from(iw);
    let ih = u128::from(ih);
    let max_px_w = u128::from(col_cap) * fw;
    let max_px_h = u128::from(MAX_ROWS) * fh;

    // Which cap bites first: compare aspect ratios without dividing.
    if iw * max_px_h >= ih * max_px_w {
        let rows = (max_px_w * ih)
            .div_ceil(iw)
            .div_ceil(fh)
            .clamp(1, u128::from(MAX_ROWS)) as u16;
        (col_cap, rows)
    } else {
        let cols = (max_px_h * iw)
            .div_ceil(ih)
            .div_ceil(fw)
            .clamp(1, u128::from(col_cap)) as u16;
        (cols, MAX_ROWS)
    }
}

/// Fit `px` into an arbitrary cell box, aspect kept — what the full-size
/// viewer and the gallery's thumbnails need (#697). `fit` exists for the
/// *inline* case and hard-codes the thumbnail caps (a share of the panel,
/// `MAX_ROWS` tall); a viewer wants the whole pane, and a gallery row wants
/// its own small square, so both caps come from the caller here.
///
/// Never returns a box larger than asked for in either axis, so a caller can
/// reserve exactly what it got back.
pub fn fit_within(box_cols: u16, box_rows: u16, px: (u32, u32), font: (u16, u16)) -> (u16, u16) {
    if box_cols == 0 || box_rows == 0 {
        return (0, 0);
    }
    let (cw, ch) = (box_cols.max(1), box_rows.max(1));
    let (iw, ih) = (px.0.max(1), px.1.max(1));
    let (fw, fh) = (u128::from(font.0.max(1)), u128::from(font.1.max(1)));
    let iw = u128::from(iw);
    let ih = u128::from(ih);
    let max_px_w = u128::from(cw) * fw;
    let max_px_h = u128::from(ch) * fh;
    // Which cap bites first: compare aspect ratios without dividing.
    if iw * max_px_h >= ih * max_px_w {
        let rows = (max_px_w * ih)
            .div_ceil(iw)
            .div_ceil(fh)
            .clamp(1, u128::from(ch)) as u16;
        (cw, rows)
    } else {
        let cols = (max_px_h * iw)
            .div_ceil(ih)
            .div_ceil(fw)
            .clamp(1, u128::from(cw)) as u16;
        (cols, ch)
    }
}

/// The box an attachment gets: `fit` of its declared pixel size, or a 16:9
/// guess when the API sent no dimensions.
pub fn attachment_box(panel_cols: u16, att: &Attachment, font: (u16, u16)) -> (u16, u16) {
    let px = match (att.width, att.height) {
        (Some(w), Some(h)) if w > 0 && h > 0 => (w, h),
        _ => (16, 9),
    };
    fit(panel_cols, px, font)
}

// ------------------------------------------------------------ url choice ---

/// Which URL to fetch for an attachment: the thumbnail if XF made one (small,
/// already the right shape), the full file only when it did not. Non-image
/// attachments are never fetched — they stay a caption line.
pub fn attachment_url(att: &Attachment) -> Option<&str> {
    if !att.is_image() {
        return None;
    }
    non_empty(att.thumbnail_url.as_deref()).or_else(|| non_empty(att.direct_url.as_deref()))
}

/// The avatar to paint in a post header's slot: `s` (48 px, exactly the 2×5
/// cell slot) falling back to `m` when a user has no small variant.
pub fn avatar_url(user: Option<&User>) -> Option<&str> {
    let urls = user?.avatar_urls.as_ref()?;
    non_empty(urls.s.as_deref()).or_else(|| non_empty(urls.m.as_deref()))
}

fn non_empty(s: Option<&str>) -> Option<&str> {
    s.filter(|v| !v.trim().is_empty())
}

// ------------------------------------------------------------------- lru ---

/// A tiny insertion-ordered LRU. Generic so the eviction contract can be
/// tested without a decoded protocol (and without the `images` feature).
pub struct Lru<T> {
    cap: usize,
    map: HashMap<String, T>,
    /// Least-recently-used first.
    order: Vec<String>,
}

impl<T> Lru<T> {
    pub fn new(cap: usize) -> Self {
        Lru { cap: cap.max(1), map: HashMap::new(), order: Vec::new() }
    }

    pub fn contains(&self, key: &str) -> bool {
        self.map.contains_key(key)
    }

    /// Read and mark as most-recently-used.
    pub fn get(&mut self, key: &str) -> Option<&T> {
        if !self.map.contains_key(key) {
            return None;
        }
        self.touch(key);
        self.map.get(key)
    }

    pub fn insert(&mut self, key: String, value: T) {
        if self.map.insert(key.clone(), value).is_some() {
            self.touch(&key);
            return;
        }
        self.order.push(key);
        while self.order.len() > self.cap {
            let evicted = self.order.remove(0);
            self.map.remove(&evicted);
        }
    }

    fn touch(&mut self, key: &str) {
        if let Some(i) = self.order.iter().position(|k| k == key) {
            let k = self.order.remove(i);
            self.order.push(k);
        }
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.map.len()
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

// ------------------------------------------------------------ disk cache ---

/// Raw thumbnail bytes under `<config dir>/cache/img/`, so a restart (or a
/// scroll back up) does not re-hit the site. Files are written atomically and
/// `0600`, the same contract as `common::token::Store`.
#[derive(Debug, Clone)]
pub struct DiskCache {
    dir: PathBuf,
    max_bytes: u64,
}

impl Default for DiskCache {
    fn default() -> Self {
        Self::new()
    }
}

impl DiskCache {
    pub fn new() -> Self {
        DiskCache { dir: default_cache_dir(), max_bytes: DISK_CAP_BYTES }
    }

    pub fn with_dir(dir: PathBuf, max_bytes: u64) -> Self {
        DiskCache { dir, max_bytes }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn path_for(&self, url: &str) -> PathBuf {
        self.dir.join(format!("{}.img", digest(url)))
    }

    pub fn get(&self, url: &str) -> Option<Vec<u8>> {
        let path = self.path_for(url);
        // A planted or simply oversized file is never slurped whole: one
        // entry can never legitimately exceed the cache's own byte cap
        // (#654).
        let len = std::fs::metadata(&path).ok()?.len();
        if len > self.max_bytes {
            return None;
        }
        std::fs::read(path).ok()
    }

    /// Best effort: a cache that cannot be written is a slower client, not a
    /// broken one.
    pub fn put(&self, url: &str, bytes: &[u8]) {
        if let Err(e) = self.write_atomic(url, bytes) {
            tracing::warn!("image cache write failed for {url}: {e}");
            return;
        }
        self.prune();
    }

    fn write_atomic(&self, url: &str, bytes: &[u8]) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let final_path = self.path_for(url);
        // One tmp per write (#654): the inflight dedupe is keyed by the
        // size-qualified store key, so two concurrent loads of one URL (a
        // resize while a load is in flight) shared a single
        // `<digest>.img.tmp` and could interleave their writes before
        // either rename landed.
        static TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp = final_path.with_extension(format!("tmp.{}.{}", std::process::id(), seq));
        {
            #[allow(unused_mut)]
            let mut opts = std::fs::OpenOptions::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(0o600);
            }
            let mut f = opts.write(true).create(true).truncate(true).open(&tmp)?;
            f.write_all(bytes)?;
            f.sync_all().ok();
        }
        restrict_permissions(&tmp);
        std::fs::rename(&tmp, &final_path)?;
        Ok(())
    }

    /// Oldest-first eviction back to 80 % of the cap once it is exceeded.
    pub fn prune(&self) {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return;
        };
        let mut files: Vec<(std::time::SystemTime, u64, PathBuf)> = Vec::new();
        let mut total: u64 = 0;
        for entry in entries.flatten() {
            let Ok(meta) = entry.metadata() else { continue };
            if !meta.is_file() {
                continue;
            }
            let mtime = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
            total += meta.len();
            files.push((mtime, meta.len(), entry.path()));
        }
        if total <= self.max_bytes {
            return;
        }
        files.sort_by_key(|(mtime, _, _)| *mtime);
        let target = self.max_bytes / 5 * 4;
        for (_, len, path) in files {
            if total <= target {
                break;
            }
            if std::fs::remove_file(&path).is_ok() {
                total = total.saturating_sub(len);
            }
        }
    }
}

/// `<config dir>/cache/img`. Derived from `token_path()`'s parent so this
/// module honours `WFTUI_CONFIG_DIR` exactly like everything else, without
/// duplicating `common::config`'s resolution rules.
fn default_cache_dir() -> PathBuf {
    common::config::token_path()
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("cache")
        .join("img")
}

/// Cache file name. A hash, not the URL: URLs contain `/` and are longer than
/// most filename limits. SHA-256 rather than a cheap hash so two thumbnails
/// can never collide onto one another's bytes.
fn digest(url: &str) -> String {
    use sha2::{Digest, Sha256};
    let out = Sha256::digest(url.as_bytes());
    out.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(unix)]
fn restrict_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &Path) {}

// --------------------------------------------------------------- runtime ---

/// The env veto, split from `detect` so it is testable without a terminal.
/// `WFTUI_NO_IMAGES=1` is the operator's off switch; `NO_COLOR` selects the
/// Mono theme tier, where painting a colour image would contradict the rest of
/// the screen.
pub fn env_disables_images<F>(var: F) -> bool
where
    F: Fn(&str) -> Option<String>,
{
    if var("NO_COLOR").is_some() {
        return true;
    }
    matches!(
        var("WFTUI_NO_IMAGES").map(|v| v.trim().to_ascii_lowercase()).as_deref(),
        Some("1") | Some("true") | Some("yes") | Some("on")
    )
}

/// The `WFTUI_GRAPHICS` operator override, parsed. Unrecognised values are
/// `None` (the caller warns and falls back to automatic detection).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraphicsChoice {
    /// Detect as usual.
    Auto,
    /// Tier 5, same as `WFTUI_NO_IMAGES=1`.
    Off,
    /// Use this tier without asking the terminal anything.
    Tier(Tier),
}

/// Parse one `WFTUI_GRAPHICS` value. Case- and space-insensitive.
pub fn parse_graphics_choice(raw: &str) -> Option<GraphicsChoice> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "" | "auto" => Some(GraphicsChoice::Auto),
        "none" | "off" | "text" => Some(GraphicsChoice::Off),
        "kitty" => Some(GraphicsChoice::Tier(Tier::Kitty)),
        "sixel" => Some(GraphicsChoice::Tier(Tier::Sixel)),
        "iterm2" | "iterm" => Some(GraphicsChoice::Tier(Tier::Iterm2)),
        "halfblocks" | "half-blocks" | "blocks" => Some(GraphicsChoice::Tier(Tier::Halfblocks)),
        _ => None,
    }
}

/// How `detect` should establish the tier. Split out of `detect` so the policy
/// is a pure function of the environment and can be pinned by unit tests with
/// no terminal in sight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetectPlan {
    /// Tier 5: no picker, no query.
    TextOnly,
    /// Build a picker for this tier without touching stdin.
    Forced(Tier),
    /// Ask the terminal: writes capability escapes and READS STDIN.
    Query,
    /// No query: half-blocks with the fallback font size.
    Fallback,
}

/// Decide the plan. The stdio query is the dangerous step (issue #532:
/// `ratatui-image` leaks a thread blocked in `read()` with `ICANON`/`ECHO`
/// cleared whenever the terminal does not answer within 2 s), so it is only
/// ever chosen when it can plausibly succeed:
///
/// * an explicit `WFTUI_GRAPHICS` wins over everything — that is the escape
///   hatch for slow SSH links, which answer the DSR *after* the timeout;
/// * Windows never queries. `ratatui-image`'s own source documents ConPTY as
///   a terminal that does not reliably deliver the reply, so the query there
///   is a guaranteed 2 s stall plus a leaked reader that eats the user's first
///   keystrokes and re-enables `ENABLE_PROCESSED_INPUT` behind the TUI;
/// * a non-terminal stdin (pipe, `< /dev/null`) can never answer either.
pub fn detect_plan<F>(var: F, windows: bool, stdin_is_tty: bool) -> DetectPlan
where
    F: Fn(&str) -> Option<String>,
{
    if env_disables_images(&var) {
        return DetectPlan::TextOnly;
    }
    if let Some(raw) = var("WFTUI_GRAPHICS") {
        match parse_graphics_choice(&raw) {
            Some(GraphicsChoice::Off) => return DetectPlan::TextOnly,
            Some(GraphicsChoice::Tier(t)) => return DetectPlan::Forced(t),
            Some(GraphicsChoice::Auto) => {}
            None => {
                tracing::warn!(
                    "ignoring WFTUI_GRAPHICS={raw:?}: expected kitty|sixel|iterm2|halfblocks|none"
                );
            }
        }
    }
    if windows || !stdin_is_tty {
        DetectPlan::Fallback
    } else {
        DetectPlan::Query
    }
}

/// `Picker::font_size` returns `ratatui_image::FontSize` (a struct since
/// ratatui-image 10; it was a bare `(u16, u16)` in 9). `Policy.font` stays a
/// tuple because `fit`/`attachment_box` and every screen do arithmetic on it.
#[cfg(feature = "images")]
fn font_pair(picker: &ratatui_image::picker::Picker) -> (u16, u16) {
    let fs = picker.font_size();
    (fs.width, fs.height)
}

/// The inverse of `tier_of`. `Tier::Text` has no protocol (it never reaches a
/// picker).
#[cfg(feature = "images")]
pub fn protocol_of(t: Tier) -> Option<ratatui_image::picker::ProtocolType> {
    use ratatui_image::picker::ProtocolType;
    match t {
        Tier::Kitty => Some(ProtocolType::Kitty),
        Tier::Sixel => Some(ProtocolType::Sixel),
        Tier::Iterm2 => Some(ProtocolType::Iterm2),
        Tier::Halfblocks => Some(ProtocolType::Halfblocks),
        Tier::Text => None,
    }
}

#[cfg(feature = "images")]
pub fn tier_of(p: ratatui_image::picker::ProtocolType) -> Tier {
    use ratatui_image::picker::ProtocolType;
    match p {
        ProtocolType::Kitty => Tier::Kitty,
        ProtocolType::Sixel => Tier::Sixel,
        ProtocolType::Iterm2 => Tier::Iterm2,
        ProtocolType::Halfblocks => Tier::Halfblocks,
    }
}

/// The app's graphics runtime: the detected policy, the decoded-protocol LRU,
/// the in-flight/failed sets that keep a missing thumbnail from becoming a
/// fetch storm, and the disk cache.
pub struct Images {
    policy: Policy,
    #[cfg(feature = "images")]
    picker: Option<ratatui_image::picker::Picker>,
    /// Only ever read on a tier that can paint, i.e. with the feature on.
    #[cfg_attr(not(feature = "images"), allow(dead_code))]
    cache: Lru<Decoded>,
    inflight: HashSet<String>,
    failed: HashSet<String>,
    /// Source pixel size per image key, learned when a load finishes. Grows
    /// only; captions read it for their `W×H` segment.
    sizes: Sizes,
    disk: DiskCache,
}

impl Default for Images {
    fn default() -> Self {
        Self::text_only()
    }
}

impl Images {
    /// Tier 5: no query, no decoding, placeholders everywhere.
    pub fn text_only() -> Self {
        Images {
            policy: Policy::default(),
            #[cfg(feature = "images")]
            picker: None,
            cache: Lru::new(LRU_CAP),
            inflight: HashSet::new(),
            failed: HashSet::new(),
            sizes: Sizes::new(),
            disk: DiskCache::new(),
        }
    }

    /// Tier 5 with the disk cache pointed somewhere explicit. Test-only:
    /// the default cache dir is derived from `config::token_path()`, i.e.
    /// the machine owner's real config dir, and no test may write there
    /// (issue #565).
    #[cfg(test)]
    pub fn text_only_at(dir: PathBuf) -> Self {
        let mut images = Self::text_only();
        images.disk = DiskCache::with_dir(dir, DISK_CAP_BYTES);
        images
    }

    /// Where this instance's disk cache lives (test guard).
    #[cfg(test)]
    pub fn disk_dir(&self) -> &Path {
        self.disk.dir()
    }

    /// Query the terminal for its graphics protocol and cell size.
    ///
    /// **On the `DetectPlan::Query` path this reads stdin directly** (it writes
    /// capability-query escapes and blocks on the replies), so it MUST complete
    /// before
    /// `event::spawn_reader` starts — CLAUDE.md hard rule 3: input comes from
    /// one dedicated blocking reader thread, and two readers racing on stdin
    /// would split the terminal's reply the same way the login-corruption
    /// incident split escape sequences. `main.rs` calls this before it hands
    /// control to `app::run`, which is what spawns the reader. The query also
    /// runs before raw mode and the alternate screen (it manages termios
    /// itself, and this is the ordering `ratatui-image`'s own binary uses).
    ///
    /// `detect_plan` decides whether the query runs at all: never on Windows,
    /// never without a terminal on stdin, never when `WFTUI_GRAPHICS` names a
    /// tier. `main.rs` brackets this call with `tty::snapshot()` /
    /// `tty::restore()` so even the query path cannot poison the exit state.
    #[cfg(feature = "images")]
    pub fn detect() -> Self {
        use std::io::IsTerminal;
        let mut me = Self::text_only();
        let plan = detect_plan(
            |k| std::env::var(k).ok(),
            cfg!(windows),
            std::io::stdin().is_terminal(),
        );
        let picker = match plan {
            DetectPlan::TextOnly => {
                tracing::debug!("inline images disabled by environment");
                return me;
            }
            DetectPlan::Forced(tier) => {
                let mut p = ratatui_image::picker::Picker::halfblocks();
                if let Some(proto) = protocol_of(tier) {
                    p.set_protocol_type(proto);
                }
                tracing::debug!("graphics tier forced by WFTUI_GRAPHICS: {}", tier.label());
                p
            }
            DetectPlan::Fallback => {
                tracing::debug!("skipping the graphics capability query; half-blocks fallback");
                ratatui_image::picker::Picker::halfblocks()
            }
            DetectPlan::Query => match ratatui_image::picker::Picker::from_query_stdio() {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!("graphics capability query failed ({e}); staying on text tier");
                    return me;
                }
            },
        };
        let tier = tier_of(picker.protocol_type());
        let font = font_pair(&picker);
        me.policy = Policy { tier, font };
        tracing::info!("graphics tier: {} font {font:?}", tier.label());
        me.picker = Some(picker);
        me
    }

    /// Without the `images` feature there is nothing to query.
    #[cfg(not(feature = "images"))]
    pub fn detect() -> Self {
        Self::text_only()
    }

    pub fn policy(&self) -> Policy {
        self.policy
    }

    pub fn disk(&self) -> DiskCache {
        self.disk.clone()
    }

    /// What the store has learned about the images it loaded: source pixel
    /// size per image key. Stamped onto the screens that caption images
    /// before every frame.
    pub fn sizes(&self) -> &Sizes {
        &self.sizes
    }

    #[cfg(feature = "images")]
    pub fn picker(&self) -> Option<ratatui_image::picker::Picker> {
        self.picker.clone()
    }

    /// Paint every request whose payload is already decoded and return the
    /// ones that are not, for the app to load off-thread. Never blocks and
    /// never decodes: this runs inside `draw`.
    #[cfg_attr(not(feature = "images"), allow(unused_variables))]
    pub fn paint(&mut self, f: &mut Frame, reqs: &[Request]) -> Vec<Pending> {
        let mut pending: Vec<Pending> = Vec::new();
        if !self.policy.inline() {
            return pending;
        }
        for req in reqs {
            if req.rect.width == 0 || req.rect.height == 0 {
                continue;
            }
            let sk = store_key(&req.key, req.rect.width, req.rect.height);
            #[cfg(feature = "images")]
            if let Some(proto) = self.cache.get(&sk) {
                // The crate's own widget is the ONLY thing that writes image
                // escapes, and it does it through the skip-cell path.
                ratatui_image::Image::new(proto).render(req.rect, f.buffer_mut());
                continue;
            }
            if self.failed.contains(&sk) || !self.inflight.insert(sk) {
                continue;
            }
            pending.push(Pending {
                key: req.key.clone(),
                cols: req.rect.width,
                rows: req.rect.height,
                full: req.full,
            });
        }
        pending
    }

    /// A background load finished (or failed). A failure is remembered so a
    /// dead thumbnail is fetched once per session, not once per frame.
    pub fn on_loaded(&mut self, key: String, result: Result<Loaded, String>) {
        self.inflight.remove(&key);
        match result {
            Ok(loaded) => {
                self.sizes.insert(source_of(&key).to_string(), loaded.px);
                #[cfg(feature = "images")]
                self.cache.insert(key, loaded.decoded);
                #[cfg(not(feature = "images"))]
                {
                    let _ = (key, loaded);
                }
            }
            Err(e) => {
                tracing::warn!("image load failed ({key}): {e}");
                self.failed.insert(key);
            }
        }
    }
}

/// Decode bytes and encode them for `cols`×`rows`. Blocking and CPU-bound —
/// callers run it on a blocking task, never on the UI thread.
#[cfg(feature = "images")]
pub fn decode(
    picker: &ratatui_image::picker::Picker,
    bytes: &[u8],
    cols: u16,
    rows: u16,
) -> Result<Loaded, String> {
    use ratatui::layout::Size;
    use ratatui_image::{FilterType, Resize};
    let mut limits = image::Limits::default();
    // Enforce the caps before any decoder allocates from the header.
    limits.max_image_width = Some(MAX_DECODE_DIM);
    limits.max_image_height = Some(MAX_DECODE_DIM);
    limits.max_alloc = Some(MAX_DECODE_ALLOC);
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes));
    reader.limits(limits);
    let img = reader
        .with_guessed_format()
        .map_err(|e| e.to_string())?
        .decode()
        .map_err(|e| e.to_string())?;
    let px = (img.width(), img.height());
    // ratatui-image 11 takes the target box as a `Size`, not a `Rect` (9 took
    // a Rect and ignored its origin).
    let decoded = picker
        .new_protocol(img, Size::new(cols, rows), Resize::Fit(Some(FilterType::Triangle)))
        .map_err(|e| e.to_string())?;
    Ok(Loaded { decoded, px })
}

/// Fetch (disk cache first, then the network through `api_gate`) and decode.
/// The whole thing runs on a background task; only the finished payload
/// crosses back to the UI, as `Msg::ImageLoaded`.
#[cfg(feature = "images")]
pub async fn load(
    client: &common::api::WfApiClient,
    disk: &DiskCache,
    picker: ratatui_image::picker::Picker,
    pending: &Pending,
) -> Result<Loaded, String> {
    let bytes = if pending.key == LOGO_KEY {
        LOGO_BYTES.to_vec()
    } else if let Some(cached) = disk.get(&pending.key) {
        cached
    } else {
        let fetched = client
            .fetch_bytes(
                &pending.key,
                if pending.full { MAX_VIEW_BYTES } else { MAX_IMAGE_BYTES },
            )
            .await
            .map_err(|e| e.to_string())?;
        disk.put(&pending.key, &fetched);
        fetched
    };
    let (cols, rows) = (pending.cols, pending.rows);
    tokio::task::spawn_blocking(move || decode(&picker, &bytes, cols, rows))
        .await
        .map_err(|e| e.to_string())?
}

#[cfg(not(feature = "images"))]
pub async fn load(
    _client: &common::api::WfApiClient,
    _disk: &DiskCache,
    _picker: (),
    _pending: &Pending,
) -> Result<Loaded, String> {
    Err("built without the `images` feature".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "images")]
    use ratatui::buffer::CellDiffOption;

    fn att(name: &str, w: Option<u32>, h: Option<u32>) -> Attachment {
        Attachment {
            attachment_id: 1,
            filename: name.into(),
            width: w,
            height: h,
            ..Default::default()
        }
    }

    // ---------- sizing ----------

    #[test]
    fn fit_keeps_aspect_under_the_forty_percent_and_twelve_row_caps() {
        // 120-col terminal: the thread panel's inner area is 118 wide.
        let (cols, rows) = fit(118, (1152, 720), (10, 20));
        assert_eq!(rows, MAX_ROWS, "a 16:10 shot is height-bound at 118 cols");
        assert_eq!(cols, 39);
        assert!(cols <= 118 * WIDTH_PERCENT / 100, "{cols} exceeds the 40 % cap");

        // 80-col terminal: inner area 78 wide, so the width cap bites first.
        let (cols, rows) = fit(78, (1152, 720), (10, 20));
        assert_eq!(cols, 78 * WIDTH_PERCENT / 100, "width-bound: exactly the cap");
        assert_eq!(cols, 31);
        assert_eq!(rows, 10);
        assert!(rows <= MAX_ROWS);
    }

    #[test]
    fn fit_never_returns_a_zero_or_over_cap_box() {
        for panel in [1u16, 2, 8, 40, 118, 400] {
            for px in [(1u32, 10_000u32), (10_000, 1), (1, 1), (0, 0)] {
                let (cols, rows) = fit(panel, px, (10, 20));
                assert!(cols >= 1 && rows >= 1, "{panel} {px:?} -> {cols}x{rows}");
                assert!(rows <= MAX_ROWS, "{panel} {px:?} -> {rows} rows");
                let cap = ((panel as u32 * WIDTH_PERCENT as u32) / 100).max(1) as u16;
                assert!(cols <= cap, "{panel} {px:?} -> {cols} cols over cap {cap}");
            }
        }
        assert_eq!(fit(0, (100, 100), (10, 20)), (0, 0));
    }

    #[test]
    fn fit_handles_extreme_remote_dimensions_without_overflow_or_wrap() {
        for px in [(u32::MAX, u32::MAX), (1, u32::MAX), (u32::MAX, 1)] {
            let (cols, rows) = fit(118, px, (u16::MAX, u16::MAX));
            assert!((1..=47).contains(&cols), "{px:?} -> {cols}x{rows}");
            assert!((1..=MAX_ROWS).contains(&rows), "{px:?} -> {cols}x{rows}");
        }
    }

    #[test]
    fn a_very_tall_image_is_row_bound_and_a_very_wide_one_width_bound() {
        let (cols, rows) = fit(118, (100, 4000), (10, 20));
        assert_eq!(rows, MAX_ROWS);
        assert!(cols < 47, "a column of pixels must not claim the width cap: {cols}");

        let (cols, rows) = fit(118, (4000, 100), (10, 20));
        assert_eq!(cols, 47);
        assert!(rows < MAX_ROWS, "a banner must not claim 12 rows: {rows}");
    }

    #[test]
    fn attachment_box_falls_back_to_sixteen_by_nine_without_dimensions() {
        let with = attachment_box(118, &att("a.png", Some(1152), Some(720)), (10, 20));
        let without = attachment_box(118, &att("a.png", None, None), (10, 20));
        assert_eq!(with, (39, 12));
        assert_eq!(without, fit(118, (16, 9), (10, 20)));
    }

    // ---------- decode limits ----------


    /// A minimal uncompressed 24-bpp BMP: `w`/`h` go into the header
    /// verbatim, followed by `pixels` bytes of (possibly zero) pixel data.
    /// BMP needs no fixture file because its dimensions are plain header
    /// fields — the same place a hostile WebP/PNG carries its bomb.
    #[cfg(feature = "images")]
    fn bmp(w: i32, h: i32, pixels: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(b"BM");
        v.extend_from_slice(&(54 + pixels.len() as u32).to_le_bytes());
        v.extend_from_slice(&0u32.to_le_bytes());
        v.extend_from_slice(&54u32.to_le_bytes());
        v.extend_from_slice(&40u32.to_le_bytes());
        v.extend_from_slice(&w.to_le_bytes());
        v.extend_from_slice(&h.to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes());
        v.extend_from_slice(&24u16.to_le_bytes());
        v.extend_from_slice(&0u32.to_le_bytes());
        v.extend_from_slice(&0u32.to_le_bytes());
        v.extend_from_slice(&2835i32.to_le_bytes());
        v.extend_from_slice(&2835i32.to_le_bytes());
        v.extend_from_slice(&0u32.to_le_bytes());
        v.extend_from_slice(&0u32.to_le_bytes());
        v.extend_from_slice(pixels);
        v
    }

    /// A 68-byte body can declare a 3.6 GB framebuffer. Decode must refuse
    /// it at the header — an allocation abort would skip `catch_unwind` and
    /// `TerminalGuard`, leaving the user's terminal in raw mode.
    #[cfg(feature = "images")]
    #[test]
    fn decode_refuses_a_header_declaring_monstrous_dimensions() {
        let picker = ratatui_image::picker::Picker::halfblocks();
        let bomb = bmp(60_000, 60_000, &[]);
        let err = match decode(&picker, &bomb, 39, 12) {
            Ok(_) => panic!("the bomb must be refused"),
            Err(e) => e,
        };
        assert!(
            err.to_lowercase().contains("dimension")
                || err.to_lowercase().contains("large")
                || err.to_lowercase().contains("limit"),
            "expected a limits rejection, got: {err}"
        );
    }

    /// Issue #679: the old ceiling was 8K per axis, so a 6000×6000 image —
    /// perfectly decodable, and pure waste for a thumbnail — cost a 144 MB
    /// RGBA transient. It must now be refused and fall back to the
    /// placeholder like any other unreadable image.
    #[cfg(feature = "images")]
    #[test]
    fn decode_refuses_an_image_far_larger_than_any_thumbnail_needs() {
        let picker = ratatui_image::picker::Picker::halfblocks();
        for (w, h) in [(6_000, 6_000), (4_000, 8_000)] {
            let big = bmp(w, h, &[]);
            assert!(
                decode(&picker, &big, 39, 12).is_err(),
                "{w}x{h} must be refused by the decode caps"
            );
        }
    }

    #[cfg(feature = "images")]
    #[test]
    fn decode_still_accepts_a_small_image_and_reports_its_pixel_size() {
        let picker = ratatui_image::picker::Picker::halfblocks();
        // 2×2 24-bpp: 6 bytes per row padded to 8, bottom-up.
        let ok = bmp(2, 2, &[10, 20, 30, 0, 0, 40, 50, 60, 0, 0, 70, 80, 90, 0, 0, 100]);
        let loaded = decode(&picker, &ok, 39, 12).expect("a 2x2 bmp decodes");
        assert_eq!(loaded.px, (2, 2));
    }

    /// #697: the viewer's fit never exceeds the box it was given, keeps the
    /// aspect within a cell of exact, and fills the axis that binds.
    #[test]
    fn fit_within_never_exceeds_its_box_and_keeps_aspect() {
        let font = (10u16, 20u16);
        for (px, box_wh) in [
            ((1536u32, 1024u32), (100u16, 30u16)),
            ((400, 1200), (100, 30)),
            ((1, 1), (80, 24)),
            ((4000, 3000), (20, 5)),
        ] {
            let (cols, rows) = fit_within(box_wh.0, box_wh.1, px, font);
            assert!(cols <= box_wh.0 && rows <= box_wh.1, "{px:?} in {box_wh:?} -> {cols}x{rows}");
            assert!(cols >= 1 && rows >= 1);
            // One axis must be filled, or the image would float smaller than
            // it needs to.
            assert!(
                cols == box_wh.0 || rows == box_wh.1,
                "{px:?} in {box_wh:?} -> {cols}x{rows} fills neither axis"
            );
        }
        assert_eq!(fit_within(0, 24, (100, 100), font), (0, 0));
        assert_eq!(fit_within(80, 0, (100, 100), font), (0, 0));
    }

    #[test]
    fn fit_within_handles_extreme_remote_dimensions_without_overflow_or_wrap() {
        for px in [(u32::MAX, u32::MAX), (1, u32::MAX), (u32::MAX, 1)] {
            let (cols, rows) = fit_within(u16::MAX, u16::MAX, px, (u16::MAX, u16::MAX));
            assert!((1..=u16::MAX).contains(&cols), "{px:?} -> {cols}x{rows}");
            assert!((1..=u16::MAX).contains(&rows), "{px:?} -> {cols}x{rows}");
        }
    }

    // ---------- url choice ----------

    #[test]
    fn thumbnail_wins_direct_wins_nothing_and_non_images_are_skipped() {
        let mut a = att("screenshot.png", Some(800), Some(600));
        a.thumbnail_url = Some("https://wf/thumb.png".into());
        a.direct_url = Some("https://wf/full.png".into());
        assert_eq!(attachment_url(&a), Some("https://wf/thumb.png"));

        a.thumbnail_url = None;
        assert_eq!(attachment_url(&a), Some("https://wf/full.png"));

        // An empty string is XF saying "no thumbnail", not a URL.
        a.thumbnail_url = Some("  ".into());
        assert_eq!(attachment_url(&a), Some("https://wf/full.png"));

        a.direct_url = None;
        a.thumbnail_url = None;
        assert_eq!(attachment_url(&a), None);

        // A log file is openable but never decodable.
        let mut z = att("dump.txt", None, None);
        z.thumbnail_url = Some("https://wf/thumb.png".into());
        z.direct_url = Some("https://wf/dump.txt".into());
        assert!(!z.is_image());
        assert_eq!(attachment_url(&z), None);
    }

    #[test]
    fn avatar_prefers_the_small_variant_then_medium() {
        let mut u = User { user_id: 7, username: "Mike".into(), ..Default::default() };
        assert_eq!(avatar_url(Some(&u)), None);

        u.avatar_urls = Some(common::models::AvatarUrls {
            s: Some("https://wf/s.jpg".into()),
            m: Some("https://wf/m.jpg".into()),
            ..Default::default()
        });
        assert_eq!(avatar_url(Some(&u)), Some("https://wf/s.jpg"));

        u.avatar_urls = Some(common::models::AvatarUrls {
            s: None,
            m: Some("https://wf/m.jpg".into()),
            ..Default::default()
        });
        assert_eq!(avatar_url(Some(&u)), Some("https://wf/m.jpg"));

        // Guests and deleted authors carry no user object at all.
        assert_eq!(avatar_url(None), None);
    }

    // ---------- tier policy from env ----------

    #[test]
    fn env_switches_images_off() {
        let none = |_: &str| None;
        assert!(!env_disables_images(none));

        let off = |k: &str| (k == "WFTUI_NO_IMAGES").then(|| "1".to_string());
        assert!(env_disables_images(off));

        let off_word = |k: &str| (k == "WFTUI_NO_IMAGES").then(|| "YES".to_string());
        assert!(env_disables_images(off_word));

        // An explicit 0 means "leave them on", not "the var exists".
        let on = |k: &str| (k == "WFTUI_NO_IMAGES").then(|| "0".to_string());
        assert!(!env_disables_images(on));

        // NO_COLOR selects the Mono theme tier; a colour image contradicts it.
        let mono = |k: &str| (k == "NO_COLOR").then(String::new);
        assert!(env_disables_images(mono));
    }

    #[test]
    fn graphics_override_parsing_covers_every_documented_value() {
        use GraphicsChoice::*;
        assert_eq!(parse_graphics_choice("kitty"), Some(Tier(super::Tier::Kitty)));
        assert_eq!(parse_graphics_choice(" SIXEL "), Some(Tier(super::Tier::Sixel)));
        assert_eq!(parse_graphics_choice("iTerm2"), Some(Tier(super::Tier::Iterm2)));
        assert_eq!(parse_graphics_choice("halfblocks"), Some(Tier(super::Tier::Halfblocks)));
        assert_eq!(parse_graphics_choice("half-blocks"), Some(Tier(super::Tier::Halfblocks)));
        assert_eq!(parse_graphics_choice("none"), Some(Off));
        assert_eq!(parse_graphics_choice("off"), Some(Off));
        assert_eq!(parse_graphics_choice("auto"), Some(Auto));
        assert_eq!(parse_graphics_choice(""), Some(Auto));
        // Unrecognised is not an error and not a tier: the caller warns and
        // detects normally.
        assert_eq!(parse_graphics_choice("chafa"), None);
    }

    #[test]
    fn the_stdio_query_only_runs_on_a_unix_terminal_with_no_override() {
        let none = |_: &str| None;
        // The one case that may touch stdin.
        assert_eq!(detect_plan(none, false, true), DetectPlan::Query);
        // Windows never queries (ConPTY does not answer; the leaked reader
        // then eats keystrokes and re-enables ENABLE_PROCESSED_INPUT).
        assert_eq!(detect_plan(none, true, true), DetectPlan::Fallback);
        // Neither does a piped/redirected stdin.
        assert_eq!(detect_plan(none, false, false), DetectPlan::Fallback);
        assert_eq!(detect_plan(none, true, false), DetectPlan::Fallback);

        // The override is honoured BEFORE any query, on both platforms.
        let g = |v: &'static str| move |k: &str| (k == "WFTUI_GRAPHICS").then(|| v.to_string());
        assert_eq!(detect_plan(g("kitty"), false, true), DetectPlan::Forced(Tier::Kitty));
        assert_eq!(detect_plan(g("sixel"), true, false), DetectPlan::Forced(Tier::Sixel));
        assert_eq!(detect_plan(g("halfblocks"), false, true), DetectPlan::Forced(Tier::Halfblocks));
        assert_eq!(detect_plan(g("none"), false, true), DetectPlan::TextOnly);
        // Auto and garbage both fall through to automatic detection.
        assert_eq!(detect_plan(g("auto"), false, true), DetectPlan::Query);
        assert_eq!(detect_plan(g("chafa"), false, true), DetectPlan::Query);
        assert_eq!(detect_plan(g("chafa"), true, true), DetectPlan::Fallback);

        // The existing off switches still win over an override that asks for
        // pixels.
        let both = |k: &str| match k {
            "WFTUI_NO_IMAGES" => Some("1".to_string()),
            "WFTUI_GRAPHICS" => Some("kitty".to_string()),
            _ => None,
        };
        assert_eq!(detect_plan(both, false, true), DetectPlan::TextOnly);
    }

    #[test]
    fn text_tier_paints_nothing_and_asks_for_nothing() {
        let mut images = Images::text_only();
        assert_eq!(images.policy().tier, Tier::Text);
        assert!(!images.policy().inline());

        let backend = ratatui::backend::TestBackend::new(40, 10);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let reqs = vec![Request {
            key: "https://wf/thumb.png".into(),
            rect: Rect::new(0, 0, 8, 4),
                full: false,
        }];
        let mut pending = Vec::new();
        terminal
            .draw(|f| {
                pending = images.paint(f, &reqs);
            })
            .unwrap();
        assert!(pending.is_empty(), "the text tier must not queue fetches");
    }

    #[cfg(feature = "images")]
    #[test]
    fn an_inline_tier_asks_once_per_key_and_stops_after_a_failure() {
        let mut images = Images::text_only();
        images.policy = Policy { tier: Tier::Halfblocks, font: (10, 20) };

        let backend = ratatui::backend::TestBackend::new(40, 10);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let reqs = vec![Request {
            key: "https://wf/thumb.png".into(),
            rect: Rect::new(0, 0, 8, 4),
                full: false,
        }];

        let mut first = Vec::new();
        terminal.draw(|f| first = images.paint(f, &reqs)).unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].store_key(), "8x4|https://wf/thumb.png");

        // Still in flight: the next frame must not queue it again.
        let mut second = Vec::new();
        terminal.draw(|f| second = images.paint(f, &reqs)).unwrap();
        assert!(second.is_empty(), "a second fetch for an in-flight image");

        images.on_loaded(first[0].store_key(), Err("404".into()));
        let mut third = Vec::new();
        terminal.draw(|f| third = images.paint(f, &reqs)).unwrap();
        assert!(third.is_empty(), "a failed image must not be retried every frame");
    }

    // ---------- end to end, no terminal involved ----------

    /// The FRAME buffer, not the backend's: `CellDiffOption::Skip` cells are
    /// filtered out of the diff on the way to the terminal, so the image's
    /// cell marking is only observable here.
    #[cfg(feature = "images")]
    fn painted(images: &mut Images, rect: Rect, w: u16, h: u16) -> ratatui::buffer::Buffer {
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
        let reqs = vec![Request { key: LOGO_KEY.to_string(), rect, full: false }];
        let mut pending = Vec::new();
        let mut frame = ratatui::buffer::Buffer::empty(Rect::new(0, 0, w, h));
        terminal
            .draw(|f| {
                pending = images.paint(f, &reqs);
                frame = f.buffer_mut().clone();
            })
            .unwrap();
        assert!(pending.is_empty(), "a decoded image must not be requested again");
        frame
    }

    #[cfg(feature = "images")]
    fn loaded(tier: Tier) -> Images {
        let mut picker = ratatui_image::picker::Picker::halfblocks();
        picker.set_protocol_type(match tier {
            Tier::Kitty => ratatui_image::picker::ProtocolType::Kitty,
            Tier::Sixel => ratatui_image::picker::ProtocolType::Sixel,
            Tier::Iterm2 => ratatui_image::picker::ProtocolType::Iterm2,
            _ => ratatui_image::picker::ProtocolType::Halfblocks,
        });
        let proto = decode(&picker, LOGO_BYTES, LOGO_COLS, LOGO_ROWS).expect("decode the logo");
        let mut images = Images::text_only();
        images.policy = Policy { tier, font: font_pair(&picker) };
        images.picker = Some(picker);
        images.on_loaded(store_key(LOGO_KEY, LOGO_COLS, LOGO_ROWS), Ok(proto));
        images
    }

    #[cfg(feature = "images")]
    #[test]
    fn the_halfblock_tier_paints_plain_cells_and_emits_no_escape_at_all() {
        let mut images = loaded(Tier::Halfblocks);
        let rect = Rect::new(2, 1, LOGO_COLS, LOGO_ROWS);
        let buf = painted(&mut images, rect, 40, 12);

        let mut ink = 0usize;
        for y in 0..12u16 {
            for x in 0..40u16 {
                let cell = &buf[(x, y)];
                assert!(
                    !cell.symbol().contains('\u{1b}'),
                    "tier 4 must be plain coloured cells: {:?} at {x},{y}",
                    cell.symbol()
                );
                if x >= rect.x && x < rect.right() && y >= rect.y && y < rect.bottom() {
                    assert!(
                        matches!(cell.diff_option, CellDiffOption::None),
                        "half-blocks are real cells, never skipped"
                    );
                    if cell.symbol() != " " {
                        ink += 1;
                    }
                }
            }
        }
        assert!(ink > 0, "the logo painted nothing");
    }

    /// Pins the mechanism CLAUDE.md hard rule 1 depends on: the escape payload
    /// lives in exactly one anchor cell per row — flagged `ForcedWidth(1)`, so
    /// ratatui bills it one column rather than its byte width — and every
    /// other cell of the rect is `Skip`, so the frame diff can never re-emit a
    /// fragment of it out of context. A crate upgrade that changed this would
    /// break the login-corruption fix (and `app::is_image_cell`, which reads
    /// exactly these flags), so it is asserted rather than assumed.
    #[cfg(feature = "images")]
    #[test]
    fn the_kitty_tier_keeps_its_payload_in_skip_protected_anchor_cells() {
        let mut images = loaded(Tier::Kitty);
        let rect = Rect::new(2, 1, LOGO_COLS, LOGO_ROWS);
        let buf = painted(&mut images, rect, 40, 12);

        let mut anchors = 0usize;
        for y in rect.y..rect.bottom() {
            let mut row_anchors = 0usize;
            for x in rect.x..rect.right() {
                let cell = &buf[(x, y)];
                if cell.symbol().contains('\u{1b}') {
                    assert_eq!(x, rect.x, "the payload must live in the row's first cell");
                    assert!(
                        matches!(cell.diff_option, CellDiffOption::ForcedWidth(w) if w.get() == 1),
                        "the anchor must be billed one column, not its byte width"
                    );
                    row_anchors += 1;
                    anchors += 1;
                    continue;
                }
                // Everything else is either skipped (the image covers it) or
                // untouched (the fitted image is narrower than the reserved
                // box). What must never happen is a cell carrying a fragment
                // of the payload, or text the image would sit on top of.
                assert!(
                    !matches!(cell.diff_option, CellDiffOption::None) || cell.symbol() == " ",
                    "a kitty image cell carries text at {x},{y}: {:?}",
                    cell.symbol()
                );
            }
            assert_eq!(row_anchors, 1, "row {y} carries {row_anchors} escape payloads");
        }
        assert_eq!(anchors, rect.height as usize, "one payload per image row");

        // Nothing outside the rect was touched.
        for y in 0..12u16 {
            for x in 0..40u16 {
                if x >= rect.x && x < rect.right() && y >= rect.y && y < rect.bottom() {
                    continue;
                }
                let cell = &buf[(x, y)];
                assert!(
                    matches!(cell.diff_option, CellDiffOption::None)
                        && !cell.symbol().contains('\u{1b}'),
                    "spill at {x},{y}"
                );
            }
        }
    }

    // ---------- lru ----------

    #[test]
    fn lru_evicts_the_least_recently_used() {
        let mut lru: Lru<u32> = Lru::new(3);
        lru.insert("a".into(), 1);
        lru.insert("b".into(), 2);
        lru.insert("c".into(), 3);
        assert_eq!(lru.len(), 3);

        // Touch "a" so "b" becomes the oldest.
        assert_eq!(lru.get("a"), Some(&1));
        lru.insert("d".into(), 4);

        assert_eq!(lru.len(), 3);
        assert!(lru.contains("a"), "the touched entry must survive");
        assert!(!lru.contains("b"), "the least-recently-used must be gone");
        assert!(lru.contains("c") && lru.contains("d"));
    }

    #[test]
    fn lru_overwrite_does_not_grow_or_evict() {
        let mut lru: Lru<u32> = Lru::new(2);
        lru.insert("a".into(), 1);
        lru.insert("b".into(), 2);
        lru.insert("a".into(), 9);
        assert_eq!(lru.len(), 2);
        assert_eq!(lru.get("a"), Some(&9));
        assert!(lru.contains("b"));

        // "a" was refreshed by the overwrite, so "b" goes first.
        lru.insert("c".into(), 3);
        assert!(lru.contains("a") && lru.contains("c"));
        assert!(!lru.contains("b"));
    }

    #[test]
    fn lru_capacity_never_collapses_to_zero() {
        let mut lru: Lru<u32> = Lru::new(0);
        lru.insert("a".into(), 1);
        assert_eq!(lru.len(), 1);
    }

    // ---------- disk cache ----------

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("wftui-img-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn disk_cache_round_trips_and_is_owner_only() {
        let dir = scratch("roundtrip");
        let cache = DiskCache::with_dir(dir.clone(), DISK_CAP_BYTES);
        assert_eq!(cache.get("https://wf/a.png"), None, "a cold cache must miss");

        let bytes = vec![0x89, b'P', b'N', b'G', 1, 2, 3, 4];
        cache.put("https://wf/a.png", &bytes);
        assert_eq!(cache.get("https://wf/a.png"), Some(bytes.clone()));
        // Two URLs never share a file.
        assert_eq!(cache.get("https://wf/b.png"), None);

        // No temp file survives an atomic write (tmp names now carry a
        // per-write sequence: `<digest>.tmp.<pid>.<n>`, #654).
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temp files left behind: {leftovers:?}");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let entry = std::fs::read_dir(&dir).unwrap().flatten().next().unwrap();
            let mode = entry.metadata().unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "cache files must be 0600 like the token store");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn disk_cache_prunes_back_under_its_cap() {
        let dir = scratch("prune");
        // 4 KiB cap, 1 KiB entries: the fifth write must evict.
        let cache = DiskCache::with_dir(dir.clone(), 4096);
        for i in 0..6 {
            cache.put(&format!("https://wf/{i}.png"), &vec![i as u8; 1024]);
            // Distinct mtimes so "oldest first" is well defined.
            std::thread::sleep(std::time::Duration::from_millis(12));
        }
        let total: u64 = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter_map(|e| e.metadata().ok())
            .map(|m| m.len())
            .sum();
        assert!(total <= 4096, "cache grew past its cap: {total}");
        assert!(
            cache.get("https://wf/5.png").is_some(),
            "the newest entry must survive a prune"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// #654: concurrent writes to one URL use one tmp file per write, so
    /// the final entry is always a whole write — never an interleaving of
    /// two — and no tmp file is left behind.
    #[test]
    fn racing_writes_to_one_url_leave_whole_writes_and_no_tmp() {
        let dir = scratch("race");
        let cache = std::sync::Arc::new(DiskCache::with_dir(dir.clone(), DISK_CAP_BYTES));
        let a = vec![b'a'; 4096];
        let b = vec![b'b'; 8192];
        let mut handles = Vec::new();
        for payload in [&a, &b] {
            let cache = cache.clone();
            let payload = payload.clone();
            handles.push(std::thread::spawn(move || {
                for _ in 0..25 {
                    cache.put("https://wf/same.png", &payload);
                }
            }));
        }
        for h in handles {
            h.join().expect("writers must not panic");
        }
        let got = cache.get("https://wf/same.png").expect("a final entry exists");
        assert!(
            got == a || got == b,
            "the entry must be one whole write, not an interleaving: {} bytes, first={:?}",
            got.len(),
            got.first()
        );
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "tmp files left behind: {leftovers:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// #654: a cache read over the store's own byte cap — a planted or
    /// oversized file — is refused, never slurped into memory.
    #[test]
    fn a_cache_read_over_the_cap_is_refused() {
        let dir = scratch("bigread");
        let cache = DiskCache::with_dir(dir.clone(), 1024);
        cache.put("https://wf/ok.png", &[1u8; 512]);
        assert_eq!(cache.get("https://wf/ok.png"), Some(vec![1u8; 512]));
        let big = cache.path_for("https://wf/huge.png");
        std::fs::write(&big, vec![0u8; 8192]).unwrap();
        assert!(cache.get("https://wf/huge.png").is_none(), "an oversized entry must not be read");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cache_dir_lives_under_the_config_dir() {
        let cache = DiskCache::new();
        assert!(cache.dir().ends_with("cache/img"), "{:?}", cache.dir());
    }

    #[test]
    fn store_key_separates_sizes() {
        assert_ne!(store_key("u", 10, 4), store_key("u", 11, 4));
        assert_eq!(store_key("u", 10, 4), "10x4|u");
    }
}
