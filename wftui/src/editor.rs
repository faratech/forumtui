//! Reusable text editing primitives for single-line and multi-line inputs.
//!
//! All operations work with character indices (0..chars_len) to ensure
//! Unicode safety and never slice across UTF-8 boundaries. Cursor *steps*
//! and deletions move whole grapheme clusters, not lone chars: Backspace on
//! "☝️" (base + U+FE0F) removes both, never the modifier alone (#651).

use unicode_segmentation::UnicodeSegmentation;

/// Insert a single character at the character cursor position.
pub fn insert_char(text: &mut String, cursor: &mut usize, c: char) {
    let mut chars: Vec<char> = text.chars().collect();
    *cursor = (*cursor).min(chars.len());
    chars.insert(*cursor, c);
    *cursor += 1;
    *text = chars.into_iter().collect();
}

/// Insert a string slice at the character cursor position.
pub fn insert_str(text: &mut String, cursor: &mut usize, s: &str) {
    let mut chars: Vec<char> = text.chars().collect();
    *cursor = (*cursor).min(chars.len());
    let insert_chars: Vec<char> = s.chars().collect();
    let count = insert_chars.len();
    chars.splice(*cursor..*cursor, insert_chars);
    *cursor += count;
    *text = chars.into_iter().collect();
}

/// Delete the grapheme cluster immediately preceding the cursor (Backspace).
pub fn delete_back(text: &mut String, cursor: &mut usize) {
    let mut chars: Vec<char> = text.chars().collect();
    *cursor = (*cursor).min(chars.len());
    if *cursor == 0 {
        return;
    }
    let start = cluster_start_before(&chars, *cursor);
    chars.drain(start..*cursor);
    *cursor = start;
    *text = chars.into_iter().collect();
}

/// Delete the grapheme cluster immediately at the cursor (Delete).
pub fn delete_forward(text: &mut String, cursor: &mut usize) {
    let mut chars: Vec<char> = text.chars().collect();
    *cursor = (*cursor).min(chars.len());
    let end = next_cluster_end(&chars, *cursor);
    if end > *cursor {
        chars.drain(*cursor..end);
        *text = chars.into_iter().collect();
    }
}

/// Delete the word preceding the cursor (Ctrl+W / Alt+Backspace).
pub fn delete_word_back(text: &mut String, cursor: &mut usize) {
    let mut chars: Vec<char> = text.chars().collect();
    // Clamp FIRST and use the clamped value for the rest of this call
    // (including the final drain) — a `*cursor` past `chars.len()` (e.g. the
    // recipients field seeded with a byte length, issue #535) used to make
    // `chars.drain(start..*cursor)` panic even though `end`/`start` were
    // themselves clamped.
    *cursor = (*cursor).min(chars.len());
    if *cursor == 0 {
        return;
    }
    let mut end = *cursor;

    // Skip trailing whitespace before cursor
    while end > 0 && chars[end - 1].is_whitespace() {
        end -= 1;
    }
    let mut start = end;
    // Skip word characters
    while start > 0 && !chars[start - 1].is_whitespace() {
        start -= 1;
    }

    if start < *cursor {
        chars.drain(start..*cursor);
        *cursor = start;
        *text = chars.into_iter().collect();
    }
}

/// Kill (delete) text from the cursor to the end of the current line (Ctrl+K).
pub fn kill_to_end(text: &mut String, cursor: &mut usize) {
    let mut chars: Vec<char> = text.chars().collect();
    *cursor = (*cursor).min(chars.len());
    let start = *cursor;
    let mut end = start;
    while end < chars.len() && chars[end] != '\n' {
        end += 1;
    }
    if start == end && end < chars.len() && chars[end] == '\n' {
        // At newline: delete the newline character itself
        end += 1;
    }
    if start < end {
        chars.drain(start..end);
        *text = chars.into_iter().collect();
    }
}

/// Kill (delete) text from the start of the current line to the cursor (Ctrl+U).
pub fn kill_to_start(text: &mut String, cursor: &mut usize) {
    let mut chars: Vec<char> = text.chars().collect();
    *cursor = (*cursor).min(chars.len());
    if *cursor == 0 {
        return;
    }
    let end = *cursor;
    let mut start = end;
    while start > 0 && chars[start - 1] != '\n' {
        start -= 1;
    }
    if start < end {
        chars.drain(start..end);
        *cursor = start;
        *text = chars.into_iter().collect();
    }
}

/// Move cursor left by one grapheme cluster.
pub fn move_left(text: &str, cursor: &mut usize) {
    let chars: Vec<char> = text.chars().collect();
    *cursor = (*cursor).min(chars.len());
    *cursor = cluster_start_before(&chars, *cursor);
}

/// Move cursor right by one grapheme cluster.
pub fn move_right(text: &str, cursor: &mut usize) {
    let chars: Vec<char> = text.chars().collect();
    *cursor = (*cursor).min(chars.len());
    *cursor = next_cluster_end(&chars, *cursor);
}

/// Char index of the first char of the cluster that ends at `at` (#651).
fn cluster_start_before(chars: &[char], at: usize) -> usize {
    if at == 0 {
        return 0;
    }
    let prefix: String = chars[..at].iter().collect();
    match prefix.grapheme_indices(true).next_back() {
        Some((_, cluster)) => at - cluster.chars().count(),
        None => at - 1, // unreachable: the prefix is non-empty
    }
}

/// Char index just past the cluster that starts at `at` (#651).
fn next_cluster_end(chars: &[char], at: usize) -> usize {
    if at >= chars.len() {
        return chars.len();
    }
    let suffix: String = chars[at..].iter().collect();
    match suffix.graphemes(true).next() {
        Some(cluster) => at + cluster.chars().count(),
        None => chars.len(), // unreachable: the suffix is non-empty
    }
}

/// Move cursor to the beginning of the current line (Home / Ctrl+A).
pub fn move_home(text: &str, cursor: &mut usize) {
    let chars: Vec<char> = text.chars().collect();
    let mut pos = (*cursor).min(chars.len());
    while pos > 0 && chars[pos - 1] != '\n' {
        pos -= 1;
    }
    *cursor = pos;
}

/// Move cursor to the end of the current line (End / Ctrl+E).
pub fn move_end(text: &str, cursor: &mut usize) {
    let chars: Vec<char> = text.chars().collect();
    let mut pos = (*cursor).min(chars.len());
    while pos < chars.len() && chars[pos] != '\n' {
        pos += 1;
    }
    *cursor = pos;
}

/// Calculate the 2D cursor coordinate (col, row) for a multi-line string.
pub fn cursor_coords(text: &str, cursor: usize) -> (u16, u16) {
    let chars: Vec<char> = text.chars().collect();
    let target = grapheme_boundary(&chars, cursor);
    let mut row = 0u16;
    let mut col = 0u16;
    let mut i = 0usize;
    while i < target {
        if chars[i] == '\n' {
            row += 1;
            col = 0;
            i += 1;
        } else {
            let end = next_cluster_end(&chars, i).min(target);
            col = col.saturating_add(span_cells(&chars, i, end) as u16);
            i = end;
        }
    }
    (col, row)
}

// ---------- visual (wrapped) rows ----------
//
// The compose editors draw their body with `Paragraph::scroll` and no
// `Wrap`, so *they* own the line breaking: a widget-level wrap would fold a
// long logical line into rows the caret arithmetic knows nothing about, and
// the cursor would drift a row down for every wrap above it (issue #519).
// Everything below works in terminal *cells* (`chrome::cell_width`), never
// `char`s, so a CJK/emoji character is billed the two columns it renders as.

/// One visual row: the half-open character range of the body it covers.
/// Rows are contiguous and cover the whole text, so a cursor index always
/// lands in exactly one of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VisualRow {
    pub start: usize,
    pub end: usize,
}

#[cfg(test)]
fn char_cells(c: char) -> usize {
    let mut buf = [0u8; 4];
    crate::chrome::cell_width(c.encode_utf8(&mut buf))
}

#[derive(Debug, Clone, Copy)]
struct GraphemeRange {
    start: usize,
    end: usize,
    width: usize,
}

/// Grapheme clusters with character-index ranges. The editor stores cursors
/// as character indices for cheap mutation, but every visual operation must
/// treat a cluster such as `⚠️` or a family emoji as one indivisible terminal
/// unit. `ratatui`/`unicode-width` measures the complete cluster, not the
/// individual scalar values.
fn grapheme_ranges(chars: &[char], offset: usize) -> Vec<GraphemeRange> {
    let text: String = chars.iter().collect();
    let mut char_at = offset;
    text.graphemes(true)
        .map(|grapheme| {
            let start = char_at;
            char_at += grapheme.chars().count();
            GraphemeRange {
                start,
                end: char_at,
                width: crate::chrome::cell_width(grapheme),
            }
        })
        .collect()
}

/// Clamp an editor cursor to a grapheme boundary. Normal key paths already
/// maintain this invariant, but seeded text, a mouse click, or a future
/// caller can hand the visual model an interior scalar index. Drawing a caret
/// inside a presentation sequence would make the renderer and the editor
/// disagree about its width.
fn grapheme_boundary(chars: &[char], cursor: usize) -> usize {
    let cursor = cursor.min(chars.len());
    grapheme_ranges(chars, 0)
        .into_iter()
        .find(|range| cursor > range.start && cursor < range.end)
        .map_or(cursor, |range| range.start)
}

/// Display width of `chars[a..b]` in cells. All visual ranges are grapheme
/// aligned; measuring the complete string also keeps variation selectors and
/// ZWJ sequences consistent with the terminal buffer's width calculation.
fn span_cells(chars: &[char], a: usize, b: usize) -> usize {
    let text: String = chars[a.min(chars.len())..b.min(chars.len())].iter().collect();
    crate::chrome::cell_width(&text)
}

/// Greedy word wrap of `text` at `width` cells, as visual rows.
pub fn visual_rows(text: &str, width: usize) -> Vec<VisualRow> {
    let chars: Vec<char> = text.chars().collect();
    visual_rows_of(&chars, width)
}

pub fn visual_rows_of(chars: &[char], width: usize) -> Vec<VisualRow> {
    let width = width.max(1);
    let n = chars.len();
    let mut rows: Vec<VisualRow> = Vec::new();
    let mut line_start = 0usize;
    loop {
        let mut le = line_start;
        while le < n && chars[le] != '\n' {
            le += 1;
        }
        wrap_logical_line(chars, line_start, le, width, &mut rows);
        if le >= n {
            break;
        }
        line_start = le + 1;
        if line_start >= n {
            // A trailing newline opens one more (empty) row to type on.
            rows.push(VisualRow { start: n, end: n });
            break;
        }
    }
    rows
}

/// Wrap one logical line (`[start, end)`, no newline inside) into rows.
///
/// Greedy: fill until the next character would overflow, then break after
/// the run of whitespace at that point (a break the eye already sees) or,
/// failing that, after the last whitespace before it; a word longer than the
/// whole line is hard-split. Whitespace that follows a break is absorbed
/// into the row that ends there — trailing blanks hang past the margin
/// rather than opening a row of their own.
fn wrap_logical_line(
    chars: &[char],
    start: usize,
    end: usize,
    width: usize,
    rows: &mut Vec<VisualRow>,
) {
    let clusters = grapheme_ranges(&chars[start..end], start);
    let mut first = 0;
    while first < clusters.len() {
        let mut fit = first;
        let mut used = 0;
        let mut whitespace = None;
        while fit < clusters.len() && used + clusters[fit].width <= width {
            used += clusters[fit].width;
            if chars[clusters[fit].start].is_whitespace() {
                whitespace = Some(fit + 1);
            }
            fit += 1;
        }
        if fit == clusters.len() {
            rows.push(VisualRow { start: clusters[first].start, end });
            return;
        }
        let mut next = if chars[clusters[fit].start].is_whitespace() {
            fit + 1
        } else {
            whitespace.unwrap_or(fit)
        };
        while next < clusters.len() && chars[clusters[next].start].is_whitespace() {
            next += 1;
        }
        // Even a cluster wider than the pane must remain whole.
        next = next.max(first + 1);
        rows.push(VisualRow { start: clusters[first].start, end: clusters[next - 1].end });
        first = next;
    }
    if clusters.is_empty() {
        rows.push(VisualRow { start, end });
    }
}

/// An incrementally maintained wrap of one editor body (issue #678).
///
/// `visual_rows_of` wraps the whole buffer, and the composer called it from
/// the caret model *and* the renderer on every keystroke — O(draft) per key,
/// on a screen that explicitly accepts a pasted 70 000-row CBS.log. How a
/// logical line wraps depends only on that line and the width, so this keeps
/// the rows per logical line and re-wraps only the lines an edit touched.
///
/// The rows it yields are identical to `visual_rows_of`'s — that is the
/// contract `the_wrap_cache_agrees_with_the_reference_wrap` pins in both
/// directions, because a caret model that disagrees with the renderer is
/// worse than a slow one.
#[derive(Default)]
pub struct WrapCache {
    width: usize,
    chars: Vec<char>,
    /// Absolute char index each logical line starts at, plus a sentinel at
    /// the end of the text (`lines + 1` entries).
    line_starts: Vec<usize>,
    /// Rows per logical line, offsets *relative* to that line's start — so
    /// an edit shifts `line_starts`, not every row in the tail.
    line_rows: Vec<Vec<VisualRow>>,
    line_metrics: Vec<LineMetrics>,
    /// Rows before each logical line, plus the total (`lines + 1` entries).
    row_prefix: Vec<usize>,
    /// Rows the last `sync` actually re-wrapped. One assignment per sync,
    /// and it is what pins the cache's whole reason to exist
    /// (`a_keystroke_rewraps_one_logical_line_not_the_whole_draft`).
    rewrapped: usize,
    /// The caller's body version at the last `sync`. The renderer syncs
    /// every frame, but a frame is not an edit: with the epoch unchanged
    /// (and the width), the cache is returned without even the
    /// common-prefix walk that locates an edit — which on a 70 000-line
    /// draft is itself 1.5 ms of char compares per frame. Over-bumping an
    /// epoch only costs a walk; under-bumping would show stale rows, so
    /// callers bump coarsely (any key, a paste, an attachment insert)
    /// rather than finely.
    epoch: u64,
    /// The text's byte length at the last `sync` — the fast path's safety
    /// net. Production mutators all bump the epoch, but anything that swaps
    /// the body behind the composer's back (a test, a future bulk edit)
    /// with a different length still misses the fast path instead of
    /// wrapping stale rows; only a same-length mutation could, and the
    /// editors have no overtype mode.
    text_len: usize,
    /// How many `sync` calls took the epoch fast path (`#[cfg(test)]`).
    #[cfg(test)]
    fast_paths: usize,
}

/// Grapheme boundaries and cumulative cell widths, relative to one line.
/// Rebuilt only for changed lines; cursor movement does not segment text.
#[derive(Default)]
struct LineMetrics {
    clusters: Vec<GraphemeRange>,
    cells: Vec<usize>,
}

impl LineMetrics {
    fn new(chars: &[char]) -> Self {
        let clusters = grapheme_ranges(chars, 0);
        let mut cells = vec![0];
        for cluster in &clusters {
            cells.push(cells.last().copied().unwrap_or(0) + cluster.width);
        }
        Self { clusters, cells }
    }

    fn boundary(&self, cursor: usize) -> (usize, usize) {
        let index = self.clusters.partition_point(|c| c.end <= cursor);
        (index, self.clusters.get(index).map_or(cursor, |c| c.start))
    }
}

fn segment_metrics(chars: &[char], starts: &[usize], end: usize) -> Vec<LineMetrics> {
    starts.iter().enumerate().map(|(i, &start)| {
        let end = starts.get(i + 1).map_or(end, |next| next.saturating_sub(1));
        LineMetrics::new(&chars[start..end])
    }).collect()
}

impl WrapCache {
    /// Bring the cache up to date with `text` at `width` cells. A width
    /// change (or the first call) rebuilds; otherwise the edit is located by
    /// common prefix + common suffix and only the logical lines it spans are
    /// re-wrapped. Every accessor below reads the state this leaves.
    ///
    /// `epoch` is the caller's body version, bumped by every path that may
    /// have mutated the text. The renderer calls this on frames where
    /// nothing was typed; the epoch compare lets those frames return
    /// immediately instead of re-walking the whole draft to discover the
    /// edit they do not contain.
    pub fn sync(&mut self, text: &str, width: usize, epoch: u64) {
        let width = width.max(1);
        if width == self.width
            && epoch == self.epoch
            && text.len() == self.text_len
            && !self.line_starts.is_empty()
        {
            #[cfg(test)]
            {
                self.fast_paths += 1;
            }
            self.rewrapped = 0;
            return;
        }
        self.epoch = epoch;
        if width != self.width || self.line_starts.is_empty() {
            self.width = width;
            self.chars.clear();
            self.chars.extend(text.chars());
            self.text_len = text.len();
            self.rebuild();
            self.rewrapped = self.row_count();
            return;
        }

        // The edit, found as a common prefix + common suffix against the
        // chars already held. Deriving it from the text rather than from an
        // edit hint keeps every mutation site — typing, delete, paste —
        // unchanged and correct, and this walks `text` in place: collecting
        // it into a fresh `Vec<char>` first was itself an O(draft) cost per
        // keystroke.
        let mut p = 0usize;
        let mut pb = 0usize;
        let mut it = text.char_indices();
        while p < self.chars.len() {
            let Some((b, c)) = it.next() else { break };
            if self.chars[p] != c {
                break;
            }
            pb = b + c.len_utf8();
            p += 1;
        }
        if p == self.chars.len() && pb == text.len() {
            self.text_len = text.len();
            self.rewrapped = 0;
            return;
        }

        let mut old_end = self.chars.len();
        let mut sb = text.len();
        let mut back = text[pb..].char_indices().rev();
        while old_end > p {
            let Some((b, c)) = back.next() else { break };
            if self.chars[old_end - 1] != c {
                break;
            }
            old_end -= 1;
            sb = pb + b;
        }
        let new_end = p + text[pb..sb].chars().count();

        // The logical lines the edit touches, in old coordinates (a binary
        // search, so this stays cheap on a 70 000-line draft).
        let first = self.line_of(p);
        let last = self.line_of(old_end);
        let start = self.line_starts[first];

        self.chars.splice(p..old_end, text[pb..sb].chars());

        // Re-wrap from that first line's start through the end of the line
        // the edit now ends in — every other line keeps the rows it had.
        let mut end = new_end.max(start);
        while end < self.chars.len() && self.chars[end] != '\n' {
            end += 1;
        }
        let (starts, rows) = wrap_segment(&self.chars, start, end, width);

        let delta = new_end as isize - old_end as isize;
        let replaced = starts.len();
        self.rewrapped = rows.iter().map(Vec::len).sum();
        self.line_metrics.splice(first..=last, segment_metrics(&self.chars, &starts, end));
        self.line_starts.splice(first..=last, starts);
        self.line_rows.splice(first..=last, rows);
        // Everything after the replaced span keeps its wrap; only its
        // absolute position moved (the end sentinel moves with it).
        for st in self.line_starts.iter_mut().skip(first + replaced) {
            *st = (*st as isize + delta).max(0) as usize;
        }
        self.text_len = text.len();
        self.rebuild_prefix();
    }

    fn rebuild(&mut self) {
        let (starts, rows) = wrap_segment(&self.chars, 0, self.chars.len(), self.width);
        self.line_metrics = segment_metrics(&self.chars, &starts, self.chars.len());
        self.line_starts = starts;
        self.line_rows = rows;
        self.line_starts.push(self.chars.len());
        self.rebuild_prefix();
    }

    /// The row prefix is a pass of integer adds over the logical lines —
    /// cheap next to re-wrapping them, which is the point of the cache.
    fn rebuild_prefix(&mut self) {
        self.row_prefix.clear();
        self.row_prefix.reserve(self.line_rows.len() + 1);
        let mut acc = 0usize;
        for rows in &self.line_rows {
            self.row_prefix.push(acc);
            acc += rows.len();
        }
        self.row_prefix.push(acc);
    }

    /// The logical line `cursor` falls in.
    fn line_of(&self, cursor: usize) -> usize {
        let lines = self.line_rows.len();
        self.line_starts
            .partition_point(|&s| s <= cursor)
            .saturating_sub(1)
            .min(lines.saturating_sub(1))
    }

    /// Sync, then report how many rows that sync re-wrapped.
    #[cfg(test)]
    fn rewrapped_rows_for_test(&mut self, text: &str, width: usize, epoch: u64) -> usize {
        self.sync(text, width, epoch);
        self.rewrapped
    }

    /// How many `sync` calls took the epoch fast path — the pin that says an
    /// unchanged frame does no work.
    #[cfg(test)]
    fn fast_paths(&self) -> usize {
        self.fast_paths
    }

    /// The wrapped text, for slicing a row's characters out of.
    pub fn chars(&self) -> &[char] {
        &self.chars
    }

    pub fn row_count(&self) -> usize {
        *self.row_prefix.last().unwrap_or(&0)
    }

    /// Row `i`, in absolute char offsets.
    pub fn row(&self, i: usize) -> Option<VisualRow> {
        if i >= self.row_count() {
            return None;
        }
        let li = self
            .row_prefix
            .partition_point(|&r| r <= i)
            .saturating_sub(1);
        let r = self.line_rows[li][i - self.row_prefix[li]];
        let base = self.line_starts[li];
        Some(VisualRow {
            start: base + r.start,
            end: base + r.end,
        })
    }

    /// `height` rows from `scroll` — what a renderer needs, instead of
    /// building a `Line` for every row of a draft to then throw all but a
    /// screenful away.
    pub fn window(&self, scroll: usize, height: usize) -> Vec<VisualRow> {
        (scroll..scroll.saturating_add(height))
            .map_while(|i| self.row(i))
            .collect()
    }

    /// The caret's `(row, col)` — the answer `caret_in_rows` gives over the
    /// whole row list, found by two binary searches instead of a scan.
    pub fn caret(&self, cursor: usize) -> (usize, usize) {
        if self.line_rows.is_empty() {
            return (0, 0);
        }
        let cursor = cursor.min(self.chars.len());
        let li = self.line_of(cursor);
        let base = self.line_starts[li];
        let metrics = &self.line_metrics[li];
        let (cluster, rel) = metrics.boundary(cursor.saturating_sub(base));
        let rows = &self.line_rows[li];
        let ri = rows
            .partition_point(|r| r.start <= rel)
            .saturating_sub(1)
            .min(rows.len().saturating_sub(1));
        let row = rows[ri];
        (
            self.row_prefix[li] + ri,
            metrics.cells[cluster] - metrics.cells[metrics.boundary(row.start).0],
        )
    }

    /// The char index a click at visual `(row, col)` lands on — the cached
    /// twin of `caret_at_cell`, with the same past-the-end behaviour.
    pub fn caret_at_cell(&self, row: usize, col: usize) -> usize {
        let Some(r) = self
            .row(row)
            .or_else(|| self.row(self.row_count().saturating_sub(1)))
        else {
            return 0;
        };
        let li = self.line_of(r.start);
        let base = self.line_starts[li];
        let metrics = &self.line_metrics[li];
        let first = metrics.boundary(r.start - base).0;
        let last = metrics.boundary(r.end - base).0;
        let target = metrics.cells[first].saturating_add(col);
        let index = metrics.cells[first..=last].partition_point(|&cell| cell <= target)
            .saturating_sub(1) + first;
        base + metrics.clusters.get(index).map_or(r.end - base, |c| c.start)

    }

    /// Move the caret `delta` visual rows, keeping the desired column — the
    /// cached twin of `move_vertical`.
    pub fn move_vertical(&self, cursor: &mut usize, desired: &mut Option<usize>, delta: isize) {
        let rows = self.row_count();
        if rows == 0 {
            return;
        }
        let (row, col) = self.caret(*cursor);
        let want = (*desired).unwrap_or(col);
        *desired = Some(want);
        let target = (row as isize + delta).clamp(0, rows as isize - 1) as usize;
        if target == row {
            return;
        }
        *cursor = self.caret_at_cell(target, want);
    }
}

/// Split `chars[start..end]` on newlines and wrap each logical line, with
/// row offsets relative to that line's start.
fn wrap_segment(
    chars: &[char],
    start: usize,
    end: usize,
    width: usize,
) -> (Vec<usize>, Vec<Vec<VisualRow>>) {
    let mut starts = Vec::new();
    let mut rows = Vec::new();
    let mut ls = start;
    loop {
        let mut le = ls;
        while le < end && chars[le] != '\n' {
            le += 1;
        }
        starts.push(ls);
        let mut line = Vec::new();
        wrap_logical_line(chars, ls, le, width.max(1), &mut line);
        for r in &mut line {
            r.start -= ls;
            r.end -= ls;
        }
        rows.push(line);
        if le >= end {
            return (starts, rows);
        }
        ls = le + 1;
    }
}

/// The caret's `(row, col)` in `rows`, col measured in cells.
pub fn caret_in_rows(chars: &[char], rows: &[VisualRow], cursor: usize) -> (usize, usize) {
    let cursor = grapheme_boundary(chars, cursor);
    let idx = rows.iter().rposition(|r| r.start <= cursor).unwrap_or(0);
    let Some(row) = rows.get(idx) else {
        return (0, 0);
    };
    (idx, span_cells(chars, row.start, cursor.min(row.end)))
}

/// The caret's visual `(row, col)` for `text` wrapped at `width` cells.
pub fn caret_position(text: &str, width: usize, cursor: usize) -> (usize, usize) {
    let chars: Vec<char> = text.chars().collect();
    let rows = visual_rows_of(&chars, width);
    caret_in_rows(&chars, &rows, cursor)
}

/// Cell width of `text`'s first `cursor` *characters* — the caret column
/// for a single-line field (a title, a search query, a recipients list).
/// `cursor` is a char index, same as every other cursor in this module;
/// billing it in `chars().count()` instead of cells put the caret one
/// column off per CJK/emoji character already typed, since those draw two
/// cells wide (issue #569 — the single-line sibling of `caret_in_rows`,
/// which multi-line editors already measure this way).
pub fn prefix_cells(text: &str, cursor: usize) -> usize {
    let chars: Vec<char> = text.chars().collect();
    let cursor = grapheme_boundary(&chars, cursor);
    span_cells(&chars, 0, cursor)
}

/// The horizontal viewport of a **single-line** field: which char the visible
/// window starts at, and where the caret sits inside it (both in the field's
/// own coordinates — chars in, cells out).
///
/// Single-line fields (the new-thread Title, the search query and author, the
/// DM recipients and title) used to be drawn as one unwindowed `Line` with the
/// caret clamped to the last column, so past the pane's width the typist was
/// typing blind with the caret pinned to the border (issue #606). This is the
/// single-line sibling of `follow_caret`: it is a pure function of the text,
/// the caret and the room, deriving the window per frame rather than storing a
/// scroll offset, and it prefers the tail — the caret keeps the last usable
/// column while typing, and the window snaps back to the head as soon as the
/// whole prefix fits again.
pub fn hwindow(text: &str, cursor: usize, width: usize) -> (usize, usize) {
    let chars: Vec<char> = text.chars().collect();
    let cursor = grapheme_boundary(&chars, cursor);
    if width == 0 {
        return (cursor, 0);
    }
    let head = span_cells(&chars, 0, cursor);
    if head < width {
        return (0, head);
    }
    // Reserve the caret's own cell so it is always inside the field.
    let budget = width - 1;
    let mut start = cursor;
    let mut used = 0usize;
    while start > 0 {
        let previous = cluster_start_before(&chars, start);
        let w = span_cells(&chars, previous, start);
        if used + w > budget {
            break;
        }
        used += w;
        start = previous;
    }
    (start, used)
}

/// The character index a click at visual row `row`, cell column `col`, lands
/// on — the inverse of [`caret_in_rows`], and the reason a click in a
/// composer lands where the eye says it did.
///
/// Cells, never chars: clicking the left half of a wide character puts the
/// caret before it and the right half puts it after, the same arithmetic
/// `caret_in_rows` uses to draw it. A click past the end of a row lands at
/// that row's end (for a soft-wrapped row that is the next row's first
/// character — the same text position), and a click past the last row lands
/// at the end of the text.
pub fn caret_at_cell(text: &str, width: usize, row: usize, col: usize) -> usize {
    let chars: Vec<char> = text.chars().collect();
    let rows = visual_rows_of(&chars, width.max(1));
    let Some(r) = rows.get(row.min(rows.len().saturating_sub(1))) else {
        return 0;
    };
    let mut used = 0usize;
    let mut i = r.start;
    while i < r.end {
        let end = next_cluster_end(&chars, i).min(r.end);
        let cw = span_cells(&chars, i, end);
        if used + cw > col {
            break;
        }
        used += cw;
        i = end;
    }
    i
}

/// The single-line sibling of [`caret_at_cell`]: the character index a click
/// `col` cells into a field drawn through [`hwindow`] lands on. The field
/// scrolls horizontally, so the window the click is measured against is the
/// one the frame drew — derived from the same `(text, cursor, room)`.
pub fn field_caret_at(text: &str, cursor: usize, room: usize, col: usize) -> usize {
    let (start, _) = hwindow(text, cursor, room);
    let chars: Vec<char> = text.chars().collect();
    let mut used = 0usize;
    let mut i = start.min(chars.len());
    while i < chars.len() {
        let end = next_cluster_end(&chars, i);
        let cw = span_cells(&chars, i, end);
        if used + cw > col {
            break;
        }
        used += cw;
        i = end;
    }
    i
}

/// Move the caret `delta` visual rows, keeping the sticky desired column:
/// a run of Up/Down keeps aiming at the column the caret started from, so
/// passing through a short line does not shorten the next move (`desired` is
/// `None` after any other key — every caller clears it).
pub fn move_vertical(
    text: &str,
    width: usize,
    cursor: &mut usize,
    desired: &mut Option<usize>,
    delta: isize,
) {
    let chars: Vec<char> = text.chars().collect();
    let rows = visual_rows_of(&chars, width);
    if rows.is_empty() {
        return;
    }
    let (row, col) = caret_in_rows(&chars, &rows, *cursor);
    let want = (*desired).unwrap_or(col);
    *desired = Some(want);
    let target = (row as isize + delta).clamp(0, rows.len() as isize - 1) as usize;
    if target == row {
        return;
    }
    let r = rows[target];
    let mut used = 0usize;
    let mut i = r.start;
    while i < r.end {
        let end = next_cluster_end(&chars, i).min(r.end);
        let cw = span_cells(&chars, i, end);
        if used + cw > want {
            break;
        }
        used += cw;
        i = end;
    }
    // A CONTINUATION row (soft-wrapped — a hard-split word, or hanging
    // whitespace absorbed into the row before a break) shares its `end` with
    // the next row's `start`; a true logical-line end does not (the next
    // row starts one past a real newline). `caret_in_rows` resolves an index
    // equal to a row's `end` to that FOLLOWING row via `rposition`, so
    // landing exactly on `r.end` here — the sticky column reaching or
    // exceeding the row's width — would draw the caret at column 0 of the
    // row after the target: Up/Down moves sideways instead, and since
    // `desired` stays sticky, every further press repeats the same non-move
    // (issue #544). Clamp to the row's last real character instead.
    if i == r.end
        && i > r.start
        && rows.get(target + 1).is_some_and(|next| next.start == r.end)
    {
        i = cluster_start_before(&chars, i);
    }
    *cursor = i;
}

/// Follow the caret with a viewport `height` rows tall: the smallest scroll
/// offset that keeps `caret_row` on screen, never past the last row.
///
/// Everything here is `usize`, deliberately (issue #558). This site's
/// `messageMaxLength` is 0 — unlimited — so a pasted CBS.log/DISM log is a
/// draft the client both accepts and can post; past 65,536 visual rows the
/// old `u16` cast wrapped and scrolled the composer to `n mod 65536`,
/// leaving the typist looking at the wrong window with the caret drawn in
/// it. The row model is `usize` end to end now, and the only narrowing to
/// `u16` happens at the terminal-coordinate boundary, after subtraction.
pub fn follow_caret(scroll: usize, caret_row: usize, rows: usize, height: usize) -> usize {
    let height = height.max(1);
    let mut top = scroll;
    if caret_row < top {
        top = caret_row;
    } else if caret_row >= top.saturating_add(height) {
        top = caret_row + 1 - height;
    }
    let max_top = rows.saturating_sub(height);
    top.min(max_top)
}

/// The slice of pre-wrapped lines a viewport shows — what every scrolled
/// pane hands to `Paragraph` instead of using `Paragraph::scroll` (issue
/// #558). `scroll` is a `usize` row offset (`Paragraph`'s is a `u16`, which
/// is exactly the truncation this avoids), and slicing also drops ratatui's
/// per-frame walk over every skipped line.
pub fn visible_window<T: Clone>(lines: &[T], scroll: usize, height: u16) -> Vec<T> {
    let start = scroll.min(lines.len());
    let end = start.saturating_add(height as usize).min(lines.len());
    lines[start..end].to_vec()
}

/// The `Line`s a viewport shows of a cached body: the visible rows of the
/// wrap, then whatever trailing lines the screen appends (an error, a
/// "Sending…" note). Only the window is materialised — building a `Line`
/// per row cost a `String` per row of the whole draft, every frame (#678).
pub fn window_lines(
    wrap: &WrapCache,
    tail: &[ratatui::text::Line<'static>],
    scroll: usize,
    height: u16,
    style: ratatui::style::Style,
) -> Vec<ratatui::text::Line<'static>> {
    use ratatui::text::{Line, Span};
    let rows = wrap.row_count();
    let total = rows + tail.len();
    let start = scroll.min(total);
    let end = start.saturating_add(height as usize).min(total);
    let chars = wrap.chars();
    (start..end)
        .map(|i| {
            if i < rows {
                let r = wrap.row(i).unwrap_or(VisualRow { start: 0, end: 0 });
                Line::from(Span::styled(
                    chars[r.start..r.end].iter().collect::<String>(),
                    style,
                ))
            } else {
                tail[i - rows].clone()
            }
        })
        .collect()
}

/// Normalise control characters before they ever reach the buffer (issue
/// #559): expand `\t` to spaces up to the next 4-column stop (column tracked
/// in terminal *cells*, so a CJK character covers two of them) and drop every
/// other control character — C0, DEL (0x7F) and the C1 block U+0080–U+009F
/// (#652), the latter two being undefined in ratatui's renderer. Nothing
/// between the API and the terminal handles a raw tab — ratatui's own
/// grapheme filter drops the byte outright when it draws a span — so leaving
/// one in the buffer both fuses adjacent text together on screen (`"a\tb"`
/// renders `"ab"`) and, because the wrap/caret math still bills it one cell,
/// desyncs the caret from the drawn text by one column per tab. Used both
/// for received text (`chunk_lines`) and pasted composer input
/// (`App::handle_paste`) — the two places raw text crosses into a span or a
/// text buffer.
pub fn normalize_control_chars(s: &str) -> String {
    fn needs_work(s: &str) -> bool {
        s.bytes().any(|b| (b < 0x20 && b != b'\n') || b == 0x7f)
            || s.chars().any(|c| ('\u{80}'..='\u{9f}').contains(&c))
    }
    if !needs_work(s) {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut col = 0usize;
    for grapheme in s.graphemes(true) {
        match grapheme {
            "\n" => {
                out.push('\n');
                col = 0;
            }
            "\t" => {
                let next_stop = (col / 4 + 1) * 4;
                for _ in col..next_stop {
                    out.push(' ');
                }
                col = next_stop;
            }
            grapheme
                if grapheme
                    .chars()
                    .any(|c| (c as u32) < 0x20 || c == '\u{7f}' || ('\u{80}'..='\u{9f}').contains(&c)) =>
            {
                // DEL and C1: ratatui's rendering of them is undefined (#652).
            }
            grapheme => {
                out.push_str(grapheme);
                // The stop column counts the complete grapheme's terminal
                // cells. Billing only its first scalar shifted every tab stop
                // after a variation sequence or ZWJ emoji (#718).
                col += crate::chrome::cell_width(grapheme);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_and_backspace_basic() {
        let mut s = "hello".to_string();
        let mut c = 5;
        insert_char(&mut s, &mut c, '!');
        assert_eq!(s, "hello!");
        assert_eq!(c, 6);

        delete_back(&mut s, &mut c);
        assert_eq!(s, "hello");
        assert_eq!(c, 5);
    }

    #[test]
    fn insert_and_delete_unicode() {
        let mut s = "🦀 Rust".to_string();
        let mut c = 1; // After crab emoji
        insert_str(&mut s, &mut c, " is cool");
        assert_eq!(s, "🦀 is cool Rust");
        assert_eq!(c, 9);

        // Delete forward
        c = 0;
        delete_forward(&mut s, &mut c);
        assert_eq!(s, " is cool Rust");
        assert_eq!(c, 0);
    }

    #[test]
    fn word_deletion_and_line_killing() {
        let mut s = "foo bar baz".to_string();
        let mut c = 11;
        delete_word_back(&mut s, &mut c);
        assert_eq!(s, "foo bar ");
        assert_eq!(c, 8);

        delete_word_back(&mut s, &mut c);
        assert_eq!(s, "foo ");
        assert_eq!(c, 4);

        // Kill to end
        let mut s2 = "first line\nsecond line".to_string();
        let mut c2 = 3;
        kill_to_end(&mut s2, &mut c2);
        assert_eq!(s2, "fir\nsecond line");

        // Kill to start
        let mut c3 = 8; // in "second line" after 'o' (pos 4 on line 2, chars 4..8 "seco" deleted)
        kill_to_start(&mut s2, &mut c3);
        assert_eq!(s2, "fir\nnd line");
    }

    /// Issue #535: a cursor seeded from a BYTE length (e.g. the new-DM
    /// recipients field prefilled with a non-ASCII username) can sit past
    /// `chars.len()`. `delete_word_back` used to clamp `start`/`end` but then
    /// drain `start..*cursor` with the still-unclamped cursor, panicking.
    /// Every editor.rs helper now clamps `*cursor` up front and uses that
    /// clamped value everywhere, including the final drain.
    #[test]
    fn cursor_past_char_len_is_clamped_not_drained_raw() {
        let mut s = "é".to_string(); // 1 char, 2 bytes
        let mut c = s.len(); // the exact bug: byte length used as a char cursor
        delete_word_back(&mut s, &mut c);
        assert_eq!(s, "");
        assert_eq!(c, 0);

        // Backspace/Delete/kill-to-start must likewise clamp rather than
        // panic or silently no-op forever.
        let mut s2 = "ab".to_string();
        let mut c2 = s2.len() + 5; // absurdly past the end
        delete_back(&mut s2, &mut c2);
        assert_eq!(s2, "a");
        assert_eq!(c2, 1);

        let mut s3 = "ab".to_string();
        let mut c3 = 100;
        delete_forward(&mut s3, &mut c3);
        assert_eq!(s3, "ab", "cursor past the end: nothing to delete forward");

        let mut s4 = "ab".to_string();
        let mut c4 = 100;
        kill_to_start(&mut s4, &mut c4);
        assert_eq!(s4, "");
    }

    // ---------- the incremental wrap cache (issue #678) ----------

    /// A deterministic pseudo-random stream, so the corpus below is varied
    /// but the failure is always the same failure.
    fn lcg(seed: &mut u64) -> u64 {
        *seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        *seed >> 33
    }

    /// The contract, in both directions: whatever sequence of edits the
    /// cache has seen, its rows are exactly `visual_rows_of`'s. The caret
    /// model and the renderer read the cache, so a disagreement here is a
    /// caret drawn on the wrong row.
    #[test]
    fn the_wrap_cache_agrees_with_the_reference_wrap() {
        let alphabet: Vec<char> = "ab \n\u{6f22}c  \nde".chars().collect();
        let mut seed = 0x5eed_1234u64;
        for width in [1usize, 3, 7, 12, 40] {
            let mut cache = WrapCache::default();
            let mut epoch = 0u64;
            let mut text = String::new();
            for step in 0..200 {
                // Insert, delete a span, or paste a block — the three shapes
                // a composer actually produces.
                match lcg(&mut seed) % 3 {
                    0 => {
                        let at = if text.is_empty() {
                            0
                        } else {
                            (lcg(&mut seed) as usize) % (text.chars().count() + 1)
                        };
                        let c = alphabet[(lcg(&mut seed) as usize) % alphabet.len()];
                        let mut chars: Vec<char> = text.chars().collect();
                        chars.insert(at, c);
                        text = chars.into_iter().collect();
                    }
                    1 => {
                        let n = text.chars().count();
                        if n > 0 {
                            let at = (lcg(&mut seed) as usize) % n;
                            let len = 1 + (lcg(&mut seed) as usize) % 5.min(n - at).max(1);
                            let mut chars: Vec<char> = text.chars().collect();
                            chars.drain(at..(at + len).min(n));
                            text = chars.into_iter().collect();
                        }
                    }
                    _ => {
                        let n = text.chars().count();
                        let at = if n == 0 { 0 } else { (lcg(&mut seed) as usize) % (n + 1) };
                        let block: String = (0..8)
                            .map(|_| alphabet[(lcg(&mut seed) as usize) % alphabet.len()])
                            .collect();
                        let mut chars: Vec<char> = text.chars().collect();
                        for (i, c) in block.chars().enumerate() {
                            chars.insert(at + i, c);
                        }
                        text = chars.into_iter().collect();
                    }
                }

                epoch += 1;
                cache.sync(&text, width, epoch);
                let chars: Vec<char> = text.chars().collect();
                let want = visual_rows_of(&chars, width);
                assert_eq!(
                    cache.window(0, cache.row_count()),
                    want,
                    "width {width}, step {step}, text {text:?}"
                );
                assert_eq!(cache.row_count(), want.len(), "row count, step {step}");

                // The caret model must agree at every position, including
                // the very end of the text.
                for cursor in [0usize, chars.len() / 3, chars.len() / 2, chars.len()] {
                    assert_eq!(
                        cache.caret(cursor),
                        caret_in_rows(&chars, &want, cursor),
                        "caret at {cursor}, width {width}, step {step}, text {text:?}"
                    );
                }
            }
        }
    }

    /// The epoch fast path (#26): a sync whose epoch and width are unchanged
    /// does no work at all — not even the prefix walk that locates an edit —
    /// while a bumped epoch still lands in the edit path.
    #[test]
    fn an_unchanged_epoch_takes_the_fast_path_and_a_bump_does_not() {
        let text = "line one\nline two\n";
        let mut cache = WrapCache::default();
        cache.sync(text, 40, 0);
        assert_eq!(cache.fast_paths(), 0, "the first sync has no cache to match");

        cache.sync(text, 40, 0);
        assert_eq!(cache.fast_paths(), 1, "the unchanged frame returns immediately");

        cache.sync(text, 40, 1);
        assert_eq!(cache.fast_paths(), 1, "a bumped epoch is an edit and re-locates");
        assert_eq!(
            cache.rewrapped_rows_for_test(text, 40, 1),
            0,
            "the bump's own walk reported the edit it did (not) find"
        );
    }

    /// A width change re-wraps from scratch rather than reusing rows cut for
    /// the old width — the one case the prefix/suffix diff cannot see.
    #[test]
    fn the_wrap_cache_rewraps_when_the_width_changes() {
        let text = "the quick brown fox jumps over the lazy dog";
        let chars: Vec<char> = text.chars().collect();
        let mut cache = WrapCache::default();
        for (epoch, width) in [40usize, 9, 80, 5].into_iter().enumerate() {
            cache.sync(text, width, epoch as u64);
            assert_eq!(
                cache.window(0, cache.row_count()),
                visual_rows_of(&chars, width),
                "width {width}"
            );
        }
    }

    /// The point of the cache (#678): a one-character edit in a large draft
    /// re-wraps the line it touched, not the whole buffer. Counted through
    /// the rows the cache rebuilt — before this, every keystroke rebuilt all
    /// 20 000 of them.
    #[test]
    fn a_keystroke_rewraps_one_logical_line_not_the_whole_draft() {
        let mut text: String = (0..20_000)
            .map(|i| format!("line {i} of a pasted log file\n"))
            .collect();
        let mut cache = WrapCache::default();
        cache.sync(&text, 40, 0);
        let rows_before = cache.row_count();
        assert!(rows_before >= 20_000, "test setup: a big draft");

        // Type one character into the middle line.
        let at = text.char_indices().nth(text.chars().count() / 2).expect("a midpoint").0;
        text.insert(at, 'x');
        let touched = cache.rewrapped_rows_for_test(&text, 40, 1);
        assert!(
            touched <= 4,
            "a one-character edit re-wrapped {touched} rows; it must only re-wrap the line it touched"
        );

        // …and the result is still exactly the reference wrap.
        let chars: Vec<char> = text.chars().collect();
        assert_eq!(cache.window(0, cache.row_count()), visual_rows_of(&chars, 40));
    }

    #[test]
    fn visual_rows_wrap_greedily_and_cover_every_char() {
        let rows = visual_rows("hello world", 5);
        let chars: Vec<char> = "hello world".chars().collect();
        let text: String = rows
            .iter()
            .flat_map(|r| chars[r.start..r.end].iter().copied())
            .collect();
        assert_eq!(text, "hello world", "wrapping must not drop a character");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].start, 6, "the break absorbs the space");

        // Blank rows for blank lines, and one to type on after a trailing \n.
        let rows = visual_rows("a\n\nb\n", 10);
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[3], VisualRow { start: 5, end: 5 });

        // A word longer than the line is hard-split, never dropped.
        let rows = visual_rows(&"x".repeat(25), 10);
        assert_eq!(rows.len(), 3);
    }

    /// The wrap budget and the caret column are terminal *cells*: a CJK body
    /// billed one cell per character puts the caret half a line to the left
    /// and builds rows twice the pane width (issue #519).
    #[test]
    fn visual_rows_and_caret_measure_cells_not_chars() {
        let cjk: String = "\u{6f22}".repeat(10); // 10 ideographs = 20 cells
        let rows = visual_rows(&cjk, 10);
        assert_eq!(rows.len(), 2, "20 cells over a 10-cell line");
        assert_eq!(rows[0], VisualRow { start: 0, end: 5 });
        assert_eq!(caret_position(&cjk, 10, 3), (0, 6));
        assert_eq!(caret_position(&cjk, 10, 7), (1, 4));

        // An odd width never splits a double-width character across the edge.
        let rows = visual_rows(&cjk, 9);
        for r in &rows {
            let w: usize = cjk.chars().collect::<Vec<_>>()[r.start..r.end]
                .iter()
                .map(|&c| char_cells(c))
                .sum();
            assert!(w <= 9, "row is {w} cells wide");
        }
    }

    #[test]
    fn visual_model_measures_complete_grapheme_clusters() {
        // U+26A0 + U+FE0F is two characters but one presentation cluster;
        // the terminal bills the complete cluster as two cells.
        let warning = "\u{26a0}\u{fe0f}";
        assert_eq!(crate::chrome::cell_width(warning), 2);
        let text = format!("{warning}{warning}");
        assert_eq!(visual_rows(&text, 2), vec![VisualRow { start: 0, end: 2 }, VisualRow { start: 2, end: 4 }]);
        assert_eq!(caret_position(&text, 2, 2), (1, 0), "the boundary belongs to the following visual row");
        assert_eq!(caret_position(&text, 2, 1), (0, 0), "an interior scalar clamps before the cluster");

        // A ZWJ family must also stay whole when the pane is narrow.
        let family = "👨\u{200d}👩\u{200d}👦";
        assert_eq!(crate::chrome::cell_width(family), 2);
        let family_text = format!("{family}{family}");
        assert_eq!(visual_rows(&family_text, 2), vec![VisualRow { start: 0, end: 5 }, VisualRow { start: 5, end: 10 }]);
        assert_eq!(hwindow(&text, text.chars().count(), 3), (2, 2));
        assert_eq!(normalize_control_chars(&format!("{warning}\tx")), format!("{warning}  x"));
    }

    #[test]
    fn vertical_motion_keeps_the_desired_column_and_the_view_follows() {
        let text = "aaaaaaaa\nbb\ncccccccc";
        let mut cursor = 20usize;
        let mut want: Option<usize> = None;
        move_vertical(text, 40, &mut cursor, &mut want, -1);
        assert_eq!(cursor, 11, "clamped to the short line's end");
        move_vertical(text, 40, &mut cursor, &mut want, -1);
        assert_eq!(cursor, 8, "the desired column is sticky");

        // follow_caret: the smallest offset that keeps the caret on screen.
        assert_eq!(follow_caret(0, 0, 40, 10), 0);
        assert_eq!(follow_caret(0, 12, 40, 10), 3);
        assert_eq!(follow_caret(20, 5, 40, 10), 5);
        assert_eq!(follow_caret(0, 39, 40, 10), 30);
        assert_eq!(follow_caret(9, 0, 3, 10), 0, "never scrolls past the last row");
    }

    /// Issue #558: the row model must be `usize` end to end. A pasted
    /// CBS.log is a legal post here (`messageMaxLength` is 0), and with the
    /// old `caret_row as u16` the 70,001st row scrolled to 70_000 mod
    /// 65_536 = 4_464 — the typist looked at the wrong window while the
    /// caret was drawn 18 rows into it.
    #[test]
    fn follow_caret_survives_a_draft_past_the_u16_row_ceiling() {
        assert_eq!(follow_caret(0, 70_000, 70_001, 18), 70_000 + 1 - 18);
        // Already scrolled there: no further movement, and never past the end.
        assert_eq!(follow_caret(69_983, 70_000, 70_001, 18), 69_983);
        assert_eq!(follow_caret(usize::MAX, 70_000, 70_001, 18), 70_001 - 18);
        // Scrolling back up to the top of the same draft still works.
        assert_eq!(follow_caret(69_983, 0, 70_001, 18), 0);

        // And the window handed to `Paragraph` is the caret's, not row
        // 4_464's: `visible_window` slices in `usize`.
        let lines: Vec<usize> = (0..70_001).collect();
        let scroll = follow_caret(0, 70_000, lines.len(), 18);
        let window = visible_window(&lines, scroll, 18);
        assert_eq!(window.len(), 18);
        assert_eq!(*window.first().expect("window"), 69_983);
        assert_eq!(*window.last().expect("window"), 70_000);
        // A scroll past the end yields an empty window rather than panicking.
        assert!(visible_window(&lines, 999_999, 18).is_empty());
    }

    /// Issue #544: a vertical move that lands exactly on a soft-wrapped
    /// (continuation) row's `end` used to leave the caret one row down and
    /// at column 0 instead of at the end of the target row — and because
    /// `desired` stays sticky, every further Up repeated the same non-move.
    /// Hard-split word case: "xxxxxxxxxx" at width 5 -> rows {0,5},{5,10}.
    #[test]
    fn vertical_motion_does_not_stick_at_a_hard_split_continuation_boundary() {
        let text = "x".repeat(10);
        let rows = visual_rows(&text, 5);
        assert_eq!(rows, vec![VisualRow { start: 0, end: 5 }, VisualRow { start: 5, end: 10 }]);

        let mut cursor = 10usize; // end of text: row 1, col 5
        let mut desired: Option<usize> = None;
        move_vertical(&text, 5, &mut cursor, &mut desired, -1);
        assert_eq!(cursor, 4, "must land on row 0's last real character, not row 1 col 0");

        // A second Up must actually do nothing more (already at row 0) —
        // not repeat the same sideways non-move.
        move_vertical(&text, 5, &mut cursor, &mut desired, -1);
        assert_eq!(cursor, 4, "already at the top row: Up is a no-op");
    }

    /// Hanging-whitespace case: "hello world" at width 5 wraps as
    /// {0,6} (absorbs the space after "hello"), {6,11} ("world"). A sticky
    /// column reaching the full absorbed width (6) must clamp inside row 0,
    /// not spill into row 1.
    #[test]
    fn vertical_motion_does_not_stick_at_a_hanging_whitespace_continuation_boundary() {
        let text = "hello world";
        let rows = visual_rows(text, 5);
        assert_eq!(rows, vec![VisualRow { start: 0, end: 6 }, VisualRow { start: 6, end: 11 }]);

        let mut cursor = 11usize; // end of "world": row 1, col 5
        let mut desired: Option<usize> = None;
        move_vertical(text, 5, &mut cursor, &mut desired, -1);
        assert!(cursor < 6, "must stay inside row 0, not spill onto row 1's start: got {cursor}");
    }

    #[test]
    fn multi_line_cursor_coords() {
        let s = "hello\nworld\nfoo";
        assert_eq!(cursor_coords(s, 0), (0, 0));
        assert_eq!(cursor_coords(s, 5), (5, 0));
        assert_eq!(cursor_coords(s, 6), (0, 1));
        assert_eq!(cursor_coords(s, 11), (5, 1));
        assert_eq!(cursor_coords(s, 12), (0, 2));
        assert_eq!(cursor_coords(s, 15), (3, 2));
    }

    /// Issue #606: the single-line viewport keeps the caret inside the field
    /// and shows the text around it, at every width and for wide characters.
    #[test]
    fn hwindow_keeps_the_caret_inside_the_field() {
        let text = "abcdefghij"; // 10 cells
        // The whole prefix fits: the window stays at the head.
        assert_eq!(hwindow(text, 4, 10), (0, 4));
        assert_eq!(hwindow(text, 9, 10), (0, 9));
        // Caret at the end of a full line: the window scrolls by one so the
        // caret has a cell of its own.
        assert_eq!(hwindow(text, 10, 10), (1, 9));
        // A narrow field shows the tail.
        assert_eq!(hwindow(text, 10, 4), (7, 3));
        // Wide characters are billed in cells, never chars.
        let cjk = "\u{6f22}\u{5b57}\u{6f22}\u{5b57}"; // 4 chars, 8 cells
        let (start, caret) = hwindow(cjk, 4, 5);
        assert_eq!((start, caret), (2, 4));
        // Degenerate widths never panic and never place the caret outside.
        for width in 0..12usize {
            for cursor in 0..=text.chars().count() {
                let (start, caret) = hwindow(text, cursor, width);
                assert!(start <= cursor);
                assert!(caret < width.max(1), "caret {caret} outside width {width}");
            }
        }
    }

    /// Issue #651: Left/Right/Backspace/Delete move whole grapheme clusters,
    /// never a lone base or modifier char — the editing siblings of the
    /// width contract that already bills clusters in cells.
    #[test]
    fn steps_and_deletes_move_whole_grapheme_clusters() {
        // ☝️ = U+261D U+FE0F: two chars, one cluster.
        let seq = "\u{261d}\u{fe0f}";
        let s = format!("{seq}ok");
        let mut c = 0usize;
        move_right(&s, &mut c);
        assert_eq!(c, 2, "Right steps the whole presentation sequence");
        move_right(&s, &mut c);
        assert_eq!(c, 3);
        move_left(&s, &mut c);
        assert_eq!(c, 2);
        move_left(&s, &mut c);
        assert_eq!(c, 0, "Left stops before the cluster, not on its modifier");

        // Backspace removes the whole cluster, never the modifier alone.
        let mut s2 = format!("a{seq}b");
        let mut c2 = 3usize;
        delete_back(&mut s2, &mut c2);
        assert_eq!(s2, "ab");
        assert_eq!(c2, 1);

        // A ZWJ family (👨‍👩‍👦 = 5 chars, one cluster) goes in one press.
        let fam = "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f466}";
        let mut s3 = format!("x{fam}");
        let mut c3 = 1 + fam.chars().count();
        delete_back(&mut s3, &mut c3);
        assert_eq!(s3, "x");
        assert_eq!(c3, 1);

        // Delete forward removes the cluster at the cursor.
        let mut s4 = format!("a{seq}b");
        let mut c4 = 1usize;
        delete_forward(&mut s4, &mut c4);
        assert_eq!(s4, "ab");
        assert_eq!(c4, 1);
    }

    /// Issue #652: DEL and C1 controls are dropped like the other C0s, and
    /// tab stops count terminal cells — a double-width character covers two
    /// columns, so the next stop sits two spaces after it, not three.
    #[test]
    fn normalize_strips_del_and_c1_and_expands_tabs_in_cells() {
        assert_eq!(normalize_control_chars("a\u{7f}b"), "ab");
        // U+0085 (NEL) lives in the C1 block.
        assert_eq!(normalize_control_chars("a\u{85}b"), "ab");
        // U+6F22 is two cells wide: col 2 -> the next stop is 4, two spaces.
        let cjk = "\u{6f22}";
        assert_eq!(normalize_control_chars(&format!("{cjk}\tx")), format!("{cjk}  x"));
    }

    /// Issue #559: a tab expands to spaces up to the next 4-column stop, and
    /// every other C0 control character is dropped outright.
    #[test]
    fn normalize_control_chars_expands_tabs_and_drops_other_controls() {
        assert_eq!(normalize_control_chars("a\tb"), "a   b");
        assert_eq!(normalize_control_chars("ab\tcd\te"), "ab  cd  e");
        // A tab already sitting on a 4-column stop takes a full stop.
        assert_eq!(normalize_control_chars("abcd\te"), "abcd    e");
        // Column tracking resets at each embedded newline.
        assert_eq!(normalize_control_chars("ab\tc\nd\te"), "ab  c\nd   e");
        // Other C0 controls (e.g. a stray bell/backspace byte) vanish; `\n`
        // itself is preserved.
        assert_eq!(normalize_control_chars("a\u{7}b\u{8}c"), "abc");
        // The common case (nothing to normalise) is untouched.
        assert_eq!(normalize_control_chars("plain text"), "plain text");
    }

    /// A click has to land where the eye says it did, which means the same
    /// wrapped-row, cell-measured model `caret_in_rows` draws the caret with
    /// — the two are inverses, and this pins the round trip.
    #[test]
    fn caret_at_cell_is_the_inverse_of_caret_in_rows() {
        let text = "one two three four five six seven eight nine ten";
        let width = 16;
        for (row, col) in [(0usize, 0usize), (0, 5), (1, 0), (1, 3), (2, 6)] {
            let cursor = caret_at_cell(text, width, row, col);
            assert_eq!(
                caret_position(text, width, cursor),
                (row, col),
                "a click at row {row} col {col} must come back as row {row} col {col}"
            );
        }
        // Past the end of a row lands at that row's end, and past the last
        // row at the end of the text — never out of bounds.
        assert_eq!(
            caret_at_cell(text, width, 0, 999),
            visual_rows(text, width)[0].end
        );
        // ...and a click past the last row lands in the last row, at the
        // column it names.
        let last = *visual_rows(text, width).last().expect("a row");
        assert_eq!(caret_at_cell(text, width, 99, 0), last.start);
        assert_eq!(caret_at_cell(text, width, 99, 999), text.chars().count());
        assert_eq!(caret_at_cell("", width, 0, 4), 0);
    }

    /// Cells, not characters: a double-width character is two columns, so a
    /// click on its right half lands after it and one on its left half
    /// before it (issue #569's rule, from the other direction).
    #[test]
    fn caret_at_cell_counts_wide_characters_as_two_columns() {
        let text = "\u{6f22}\u{5b57}\u{30c6}"; // 3 ideographs, 6 cells
        assert_eq!(caret_at_cell(text, 20, 0, 0), 0);
        assert_eq!(caret_at_cell(text, 20, 0, 1), 0, "the left half is still that character");
        assert_eq!(caret_at_cell(text, 20, 0, 2), 1);
        assert_eq!(caret_at_cell(text, 20, 0, 4), 2);
        assert_eq!(caret_at_cell(text, 20, 0, 6), 3);
    }

    /// The single-line sibling: a field that has scrolled horizontally must
    /// measure the click against the window that was drawn, not the whole
    /// string.
    #[test]
    fn field_caret_at_measures_inside_the_window_the_field_was_drawn_with() {
        let text = "abcdefghijklmnop";
        // Window at the head: column 3 is the fourth character.
        assert_eq!(field_caret_at(text, 0, 10, 3), 3);
        // Caret at the end with room for 10 cells: `hwindow` scrolled the
        // field, so column 0 is no longer character 0.
        let (start, _) = hwindow(text, text.chars().count(), 10);
        assert!(start > 0, "the field must have scrolled for this to mean anything");
        assert_eq!(field_caret_at(text, text.chars().count(), 10, 0), start);
        assert_eq!(field_caret_at(text, text.chars().count(), 10, 2), start + 2);
        // A click past the text lands at its end.
        assert_eq!(field_caret_at(text, 0, 40, 99), text.chars().count());
    }
}
