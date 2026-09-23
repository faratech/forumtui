//! Markdown → styled [`Chunk`]s, for Ask the AI's answers.
//!
//! The assistant answers in Markdown (the site's web client renders it with a
//! Markdown library), while everything else in this client is BBCode. Emitting
//! the same `Chunk` stream as [`crate::bbcode`] means the renderer's one
//! pipeline — `chunk_lines`, `wrap_spans_aligned`, the `[n]` link markers and
//! the digit keys that open them — serves answers unchanged.
//!
//! Deliberately small: the block forms an answer actually uses (headings,
//! lists, quotes, fenced code, rules) and the inline ones (`**bold**`,
//! `*em*`, `` `code` ``, `~~strike~~`, `[label](url)`, bare URLs). Anything
//! else — a table, raw HTML — stays literal text, which a monospace terminal
//! shows legibly anyway. It is also re-run on a half-streamed answer every
//! frame, so an unclosed `**` or code fence must degrade, never fail.

use crate::bbcode::{Chunk, Style, is_http_url};

pub fn render(src: &str) -> Vec<Chunk> {
    let mut out = Vec::new();
    let mut fence: Option<String> = None;
    for line in src.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        let trimmed = line.trim_start();

        if let Some(marker) = &fence {
            if trimmed.starts_with(marker.as_str()) && trimmed[marker.len()..].trim().is_empty() {
                fence = None;
            } else {
                push_text(&mut out, line, code_style());
                push_text(&mut out, "\n", Style::default());
            }
            continue;
        }
        if let Some(marker) = fence_open(trimmed) {
            fence = Some(marker);
            continue;
        }
        if is_rule(trimmed) {
            out.push(Chunk::Rule(Style::default()));
            push_text(&mut out, "\n", Style::default());
            continue;
        }
        if let Some((level, text)) = heading(trimmed) {
            let style = Style { heading: Some(level), ..Style::default() };
            inline(text, style, &mut out);
            push_text(&mut out, "\n", Style::default());
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix('>') {
            let style = Style { quote_depth: 1, ..Style::default() };
            inline(rest.strip_prefix(' ').unwrap_or(rest), style, &mut out);
            push_text(&mut out, "\n", Style::default());
            continue;
        }
        let indent = line.len() - trimmed.len();
        if let Some((marker, text)) = list_item(trimmed) {
            let depth = (indent / 2 + 1).min(8) as u8;
            let style = Style { list_depth: depth, ..Style::default() };
            push_text(&mut out, &format!("{}{marker}", "  ".repeat(depth as usize - 1)), style.clone());
            inline(text, style, &mut out);
            push_text(&mut out, "\n", Style::default());
            continue;
        }
        inline(line, Style::default(), &mut out);
        push_text(&mut out, "\n", Style::default());
    }
    // The trailing "\n" of the last line is noise, not a blank row.
    if let Some(Chunk::Text(t, _)) = out.last_mut()
        && t.ends_with('\n')
    {
        t.pop();
        if t.is_empty() {
            out.pop();
        }
    }
    out
}

fn code_style() -> Style {
    Style { code: true, ..Style::default() }
}

/// Append text, merging into the previous chunk when the style matches, so
/// a long answer is a few chunks rather than one per word.
fn push_text(out: &mut Vec<Chunk>, text: &str, style: Style) {
    if text.is_empty() {
        return;
    }
    if let Some(Chunk::Text(prev, prev_style)) = out.last_mut()
        && *prev_style == style
    {
        prev.push_str(text);
        return;
    }
    out.push(Chunk::Text(text.to_string(), style));
}

/// "```" or "~~~" (three or more), optionally followed by a language.
fn fence_open(trimmed: &str) -> Option<String> {
    for ch in ['`', '~'] {
        let run = trimmed.chars().take_while(|c| *c == ch).count();
        if run >= 3 {
            let marker: String = std::iter::repeat_n(ch, run).collect();
            // A backtick fence's info string may not contain a backtick.
            if ch == '`' && trimmed[run..].contains('`') {
                return None;
            }
            return Some(marker);
        }
    }
    None
}

fn is_rule(trimmed: &str) -> bool {
    let t: String = trimmed.chars().filter(|c| !c.is_whitespace()).collect();
    t.len() >= 3
        && ['-', '*', '_'].iter().any(|m| t.chars().all(|c| c == *m))
}

/// `# ` .. `###### `: levels past three share the third weight, as the
/// BBCode renderer has exactly three.
fn heading(trimmed: &str) -> Option<(u8, &str)> {
    let hashes = trimmed.chars().take_while(|c| *c == '#').count();
    if !(1..=6).contains(&hashes) {
        return None;
    }
    let rest = &trimmed[hashes..];
    if !(rest.is_empty() || rest.starts_with(' ')) {
        return None;
    }
    let text = rest.trim().trim_end_matches('#').trim_end();
    Some(((hashes as u8).min(3), text))
}

/// `- x`, `* x`, `+ x` → "• "; `3. x` / `3) x` keep their number.
fn list_item(trimmed: &str) -> Option<(String, &str)> {
    for bullet in ["- ", "* ", "+ "] {
        if let Some(rest) = trimmed.strip_prefix(bullet) {
            // "- [ ] task" and "- [x] task": a checkbox, kept as text.
            return Some(("• ".into(), rest));
        }
    }
    let digits = trimmed.chars().take_while(|c| c.is_ascii_digit()).count();
    if (1..=9).contains(&digits) {
        let rest = &trimmed[digits..];
        for delim in [". ", ") "] {
            if let Some(text) = rest.strip_prefix(delim) {
                return Some((format!("{}. ", &trimmed[..digits]), text));
            }
        }
    }
    None
}

/// The inline forms, on one line. State never crosses a line, so an unclosed
/// marker can only restyle the rest of its own line.
fn inline(text: &str, base: Style, out: &mut Vec<Chunk>) {
    let mut style = base.clone();
    let mut plain = String::new();
    let mut i = 0;
    let bytes = text.as_bytes();
    let flush = |plain: &mut String, style: &Style, out: &mut Vec<Chunk>| {
        push_text(out, plain, style.clone());
        plain.clear();
    };
    while i < text.len() {
        let rest = &text[i..];
        let prev = text[..i].chars().next_back();

        if rest.starts_with('`') {
            let run = rest.chars().take_while(|c| *c == '`').count();
            let marker = &rest[..run];
            if let Some(end) = rest[run..].find(marker) {
                flush(&mut plain, &style, out);
                let body = &rest[run..run + end];
                let body = body.strip_prefix(' ').and_then(|b| b.strip_suffix(' ')).unwrap_or(body);
                push_text(out, body, Style { code: true, ..style.clone() });
                i += run + end + run;
                continue;
            }
        }
        if rest.starts_with("**") || rest.starts_with("__") {
            let marker = &rest[..2];
            // `__` only at a word edge: snake_case_names are not emphasis.
            let at_edge = marker == "**"
                || (!prev.is_some_and(|c| c.is_alphanumeric())
                    || !rest[2..].chars().next().is_some_and(|c| c.is_alphanumeric()));
            if at_edge && (style.bold != base.bold || rest[2..].contains(marker)) {
                flush(&mut plain, &style, out);
                style.bold = !style.bold;
                i += 2;
                continue;
            }
        }
        if rest.starts_with("~~") && (style.strikethrough != base.strikethrough || rest[2..].contains("~~")) {
            flush(&mut plain, &style, out);
            style.strikethrough = !style.strikethrough;
            i += 2;
            continue;
        }
        if bytes[i] == b'*' {
            let next = rest[1..].chars().next();
            let opens = !style.italic
                && next.is_some_and(|c| !c.is_whitespace() && c != '*')
                && closes_later(&rest[1..]);
            let closes = style.italic && prev.is_some_and(|c| !c.is_whitespace());
            if opens || closes {
                flush(&mut plain, &style, out);
                style.italic = !style.italic;
                i += 1;
                continue;
            }
        }
        if bytes[i] == b'['
            && let Some((label, url, used)) = link(rest)
        {
            flush(&mut plain, &style, out);
            let label = strip_inline_markers(label);
            let label = if label.trim().is_empty() { url.to_string() } else { label };
            out.push(Chunk::Link(label, url.to_string(), style.clone()));
            i += used;
            continue;
        }
        if (rest.starts_with("https://") || rest.starts_with("http://"))
            && !prev.is_some_and(|c| c.is_alphanumeric() || c == '/' || c == '(' || c == '<')
        {
            let url = bare_url(rest);
            if is_http_url(url) {
                flush(&mut plain, &style, out);
                out.push(Chunk::Link(url.to_string(), url.to_string(), style.clone()));
                i += url.len();
                continue;
            }
        }
        if rest.starts_with("<http")
            && let Some(end) = rest.find('>')
            && is_http_url(&rest[1..end])
        {
            flush(&mut plain, &style, out);
            let url = &rest[1..end];
            out.push(Chunk::Link(url.to_string(), url.to_string(), style.clone()));
            i += end + 1;
            continue;
        }
        let ch = rest.chars().next().expect("non-empty rest");
        plain.push(ch);
        i += ch.len_utf8();
    }
    flush(&mut plain, &style, out);
}

/// Whether a `*` in `rest` could close an italic run: one that follows a
/// non-space and is not part of `**`.
fn closes_later(rest: &str) -> bool {
    let mut prev: Option<char> = None;
    let mut it = rest.char_indices().peekable();
    while let Some((_, c)) = it.next() {
        if c == '*' {
            let doubled = it.peek().is_some_and(|(_, n)| *n == '*');
            if doubled {
                it.next();
            } else if prev.is_some_and(|p| !p.is_whitespace()) {
                return true;
            }
        }
        prev = Some(c);
    }
    false
}

/// `[label](url)` with an http(s) target; returns the bytes consumed. The
/// label may itself contain brackets (a KB title does), balanced.
fn link(rest: &str) -> Option<(&str, &str, usize)> {
    let mut depth = 0usize;
    let mut close = None;
    for (i, c) in rest.char_indices() {
        match c {
            '[' => depth += 1,
            ']' => {
                depth -= 1;
                if depth == 0 {
                    close = Some(i);
                    break;
                }
            }
            _ => {}
        }
    }
    let close = close?;
    let after = &rest[close + 1..];
    let inner = after.strip_prefix('(')?;
    let mut depth = 1usize;
    let mut close_paren = None;
    // `[x](url "title")`: the URL stops at the first space.
    let mut url_end = None;
    for (i, c) in inner.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    close_paren = Some(i);
                    break;
                }
            }
            ' ' if depth == 1 => {
                url_end.get_or_insert(i);
            }
            _ => {}
        }
    }
    let close_paren = close_paren?;
    let url_end = url_end.unwrap_or(close_paren);
    let target = inner[..url_end].trim().trim_start_matches('<').trim_end_matches('>');
    if !is_http_url(target) {
        return None;
    }
    Some((&rest[1..close], target, close + 2 + close_paren + 1))
}

/// A bare URL runs to whitespace, minus trailing sentence punctuation and an
/// unbalanced closing parenthesis.
fn bare_url(rest: &str) -> &str {
    let end = rest.find(|c: char| c.is_whitespace() || c == '<' || c == '"').unwrap_or(rest.len());
    let mut url = &rest[..end];
    loop {
        let trimmed = url.trim_end_matches(['.', ',', ';', ':', '!', '?', '*', '_', '\'']);
        let trimmed = if trimmed.ends_with(')') && trimmed.matches('(').count() < trimmed.matches(')').count() {
            &trimmed[..trimmed.len() - 1]
        } else {
            trimmed
        };
        if trimmed.len() == url.len() {
            return url;
        }
        url = trimmed;
    }
}

fn strip_inline_markers(label: &str) -> String {
    label.replace("**", "").replace('`', "")
}

/// Plain text of an answer, for the clipboard: Markdown is already readable,
/// so it is copied as written.
pub fn to_plain(src: &str) -> String {
    src.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(chunks: &[Chunk]) -> String {
        chunks
            .iter()
            .map(|c| match c {
                Chunk::Text(t, _) => t.clone(),
                Chunk::Link(l, u, _) => format!("<{l}|{u}>"),
                Chunk::Rule(_) => "---".into(),
                _ => String::new(),
            })
            .collect()
    }

    fn styled<'a>(chunks: &'a [Chunk], needle: &str) -> &'a Style {
        chunks
            .iter()
            .find_map(|c| match c {
                Chunk::Text(t, s) if t.contains(needle) => Some(s),
                Chunk::Link(l, _, s) if l.contains(needle) => Some(s),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no chunk with {needle:?} in {chunks:?}"))
    }

    #[test]
    fn the_captured_answer_renders_its_emphasis_and_its_link() {
        let answer: String = crate::ai::tests_support::captured_answer();
        let chunks = render(&answer);
        assert!(styled(&chunks, "October 8, 2024 cumulative").bold);
        assert!(!styled(&chunks, " is the ").bold);
        let links: Vec<_> = chunks
            .iter()
            .filter_map(|c| match c {
                Chunk::Link(l, u, _) => Some((l.as_str(), u.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(
            links,
            vec![(
                "KB5044284 for Windows 11: Security Fixes and Improvements",
                "https://windowsforum.com/news/kb5044284-for-windows-11-security-fixes-and-improvements.343509/"
            )]
        );
        assert!(!texts(&chunks).contains("**"), "{}", texts(&chunks));
    }

    #[test]
    fn blocks_headings_lists_quotes_rules_and_fences() {
        let md = "# Title\n## Sub ##\n#### Deep\n- one\n  - nested\n3) three\n> quoted *here*\n---\n```powershell\nGet-Item  C:\\*\n```\nafter";
        let chunks = render(md);
        assert_eq!(styled(&chunks, "Title").heading, Some(1));
        assert_eq!(styled(&chunks, "Sub").heading, Some(2));
        assert_eq!(styled(&chunks, "Deep").heading, Some(3));
        assert_eq!(styled(&chunks, "nested").list_depth, 2);
        let all = texts(&chunks);
        assert!(all.contains("• one\n") && all.contains("  • nested\n") && all.contains("3. three\n"), "{all}");
        assert_eq!(styled(&chunks, "quoted").quote_depth, 1);
        assert!(styled(&chunks, "here").italic);
        assert!(all.contains("---"), "{all}");
        let code = styled(&chunks, "Get-Item");
        assert!(code.code);
        assert!(all.contains("Get-Item  C:\\*\n"), "the fence body is verbatim: {all}");
        assert!(!all.contains("```") && !all.contains("Sub ##"), "{all}");
        assert!(all.ends_with("after"), "{all:?}");
    }

    #[test]
    fn an_unclosed_fence_or_marker_degrades_while_streaming() {
        let chunks = render("Run:\n```\nsfc /scannow");
        assert!(styled(&chunks, "sfc").code);
        let chunks = render("This is **half");
        assert_eq!(texts(&chunks), "This is **half");
        let chunks = render("5 * 3 = 15 and 2*x");
        assert_eq!(texts(&chunks), "5 * 3 = 15 and 2*x");
        assert!(!styled(&chunks, "15").italic);
    }

    #[test]
    fn inline_code_strike_and_word_internal_underscores() {
        let chunks = render("Use `DISM /Online` then ~~reboot~~ check my_var_name and __bold__");
        assert!(styled(&chunks, "DISM /Online").code);
        assert!(styled(&chunks, "reboot").strikethrough);
        assert!(!styled(&chunks, "my_var_name").bold);
        assert!(styled(&chunks, "bold").bold);
        assert_eq!(texts(&chunks), "Use DISM /Online then reboot check my_var_name and bold");
    }

    #[test]
    fn links_only_for_http_targets_and_bare_urls_lose_trailing_punctuation() {
        let chunks = render("See [docs](https://learn.microsoft.com/a_(b)) and [x](javascript:alert(1)). Or https://windowsforum.com/threads/1/.");
        assert_eq!(
            texts(&chunks),
            "See <docs|https://learn.microsoft.com/a_(b)> and [x](javascript:alert(1)). Or <https://windowsforum.com/threads/1/|https://windowsforum.com/threads/1/>."
        );
        let chunks = render("([KB [x] title](https://example.com/p \"T\"))");
        assert_eq!(texts(&chunks), "(<KB [x] title|https://example.com/p>)");
    }
}
