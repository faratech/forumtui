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
pub fn follow_caret(scroll: u16, caret_row: usize, rows: usize, height: u16) -> u16 {
    let height = height.max(1);
    let caret = caret_row as u16;
    let mut top = scroll;
    if caret < top {
        top = caret;
    } else if caret >= top.saturating_add(height) {
        top = caret + 1 - height;
    }
    let max_top = (rows as u16).saturating_sub(height);
    top.min(max_top)
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
}
