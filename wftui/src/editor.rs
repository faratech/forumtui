//! Reusable text editing primitives for single-line and multi-line inputs.
//!
//! All operations work with character indices (0..chars_len) to ensure
//! Unicode safety and never slice across UTF-8 boundaries.

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

/// Delete the character immediately preceding the cursor (Backspace).
pub fn delete_back(text: &mut String, cursor: &mut usize) {
    let mut chars: Vec<char> = text.chars().collect();
    *cursor = (*cursor).min(chars.len());
    if *cursor == 0 {
        return;
    }
    chars.remove(*cursor - 1);
    *cursor -= 1;
    *text = chars.into_iter().collect();
}

/// Delete the character immediately at the cursor (Delete).
pub fn delete_forward(text: &mut String, cursor: &mut usize) {
    let mut chars: Vec<char> = text.chars().collect();
    *cursor = (*cursor).min(chars.len());
    if *cursor < chars.len() {
        chars.remove(*cursor);
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

/// Move cursor left by 1 character.
pub fn move_left(cursor: &mut usize) {
    *cursor = cursor.saturating_sub(1);
}

/// Move cursor right by 1 character.
pub fn move_right(text: &str, cursor: &mut usize) {
    let max = text.chars().count();
    *cursor = (*cursor).min(max);
    *cursor = (*cursor + 1).min(max);
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
    let target = cursor.min(chars.len());
    let mut row = 0u16;
    let mut col = 0u16;
    for (i, &c) in chars.iter().enumerate() {
        if i == target {
            break;
        }
        if c == '\n' {
            row += 1;
            col = 0;
        } else {
            col += 1;
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

fn char_cells(c: char) -> usize {
    let mut buf = [0u8; 4];
    crate::chrome::cell_width(c.encode_utf8(&mut buf))
}

/// Display width of `chars[a..b]` in cells.
fn span_cells(chars: &[char], a: usize, b: usize) -> usize {
    chars[a..b].iter().copied().map(char_cells).sum()
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
    let mut pos = start;
    loop {
        let mut used = 0usize;
        let mut fit = pos;
        while fit < end {
            let cw = char_cells(chars[fit]);
            if used + cw > width {
                break;
            }
            used += cw;
            fit += 1;
        }
        if fit >= end {
            rows.push(VisualRow { start: pos, end });
            return;
        }
        let mut brk = if chars[fit].is_whitespace() {
            fit
        } else {
            match (pos..fit).rev().find(|&i| chars[i].is_whitespace()) {
                Some(ws) => ws + 1,
                None => fit, // one unbreakable word: hard-split it
            }
        };
        while brk < end && chars[brk].is_whitespace() {
            brk += 1;
        }
        if brk <= pos {
            brk = pos + 1; // the loop must always make progress
        }
        rows.push(VisualRow { start: pos, end: brk });
        pos = brk;
    }
}

/// The caret's `(row, col)` in `rows`, col measured in cells.
pub fn caret_in_rows(chars: &[char], rows: &[VisualRow], cursor: usize) -> (usize, usize) {
    let cursor = cursor.min(chars.len());
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
    text.chars().take(cursor).map(char_cells).sum()
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
    let cursor = cursor.min(chars.len());
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
        let w = char_cells(chars[start - 1]);
        if used + w > budget {
            break;
        }
        used += w;
        start -= 1;
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
        let cw = char_cells(chars[i]);
        if used + cw > col {
            break;
        }
        used += cw;
        i += 1;
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
        let cw = char_cells(chars[i]);
        if used + cw > col {
            break;
        }
        used += cw;
        i += 1;
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
        let cw = char_cells(chars[i]);
        if used + cw > want {
            break;
        }
        used += cw;
        i += 1;
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
        i -= 1;
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

/// Normalise control characters before they ever reach the buffer (issue
/// #559): expand `\t` to spaces up to the next 4-column stop (column tracked
/// across embedded newlines) and drop every other C0 control character.
/// Nothing between the API and the terminal handles a raw tab — ratatui's
/// own grapheme filter drops the byte outright when it draws a span — so
/// leaving one in the buffer both fuses adjacent text together on screen
/// (`"a\tb"` renders `"ab"`) and, because the wrap/caret math still bills it
/// one cell, desyncs the caret from the drawn text by one column per tab.
/// Used both for received text (`chunk_lines`) and pasted composer input
/// (`App::handle_paste`) — the two places raw text crosses into a span or a
/// text buffer.
pub fn normalize_control_chars(s: &str) -> String {
    if !s.bytes().any(|b| b < 0x20 && b != b'\n') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut col = 0usize;
    for c in s.chars() {
        match c {
            '\n' => {
                out.push('\n');
                col = 0;
            }
            '\t' => {
                let next_stop = (col / 4 + 1) * 4;
                for _ in col..next_stop {
                    out.push(' ');
                }
                col = next_stop;
            }
            c if (c as u32) < 0x20 => {
                // Drop every other C0 control character (issue #559).
            }
            c => {
                out.push(c);
                col += 1;
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
