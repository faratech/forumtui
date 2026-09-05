//! Reusable text editing primitives for single-line and multi-line inputs.
//!
//! All operations work with character indices (0..chars_len) to ensure
//! Unicode safety and never slice across UTF-8 boundaries.

/// Insert a single character at the character cursor position.
pub fn insert_char(text: &mut String, cursor: &mut usize, c: char) {
    let mut chars: Vec<char> = text.chars().collect();
    let idx = (*cursor).min(chars.len());
    chars.insert(idx, c);
    *cursor = idx + 1;
    *text = chars.into_iter().collect();
}

/// Insert a string slice at the character cursor position.
pub fn insert_str(text: &mut String, cursor: &mut usize, s: &str) {
    let mut chars: Vec<char> = text.chars().collect();
    let idx = (*cursor).min(chars.len());
    let insert_chars: Vec<char> = s.chars().collect();
    let count = insert_chars.len();
    chars.splice(idx..idx, insert_chars);
    *cursor = idx + count;
    *text = chars.into_iter().collect();
}

/// Delete the character immediately preceding the cursor (Backspace).
pub fn delete_back(text: &mut String, cursor: &mut usize) {
    if *cursor == 0 {
        return;
    }
    let mut chars: Vec<char> = text.chars().collect();
    if *cursor <= chars.len() {
        chars.remove(*cursor - 1);
        *cursor -= 1;
        *text = chars.into_iter().collect();
    }
}

/// Delete the character immediately at the cursor (Delete).
pub fn delete_forward(text: &mut String, cursor: &mut usize) {
    let mut chars: Vec<char> = text.chars().collect();
    if *cursor < chars.len() {
        chars.remove(*cursor);
        *text = chars.into_iter().collect();
    }
}

/// Delete the word preceding the cursor (Ctrl+W / Alt+Backspace).
pub fn delete_word_back(text: &mut String, cursor: &mut usize) {
    if *cursor == 0 {
        return;
    }
    let mut chars: Vec<char> = text.chars().collect();
    let mut end = (*cursor).min(chars.len());

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
    let start = (*cursor).min(chars.len());
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
    if *cursor == 0 {
        return;
    }
    let mut chars: Vec<char> = text.chars().collect();
    let end = (*cursor).min(chars.len());
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
