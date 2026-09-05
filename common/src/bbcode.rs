//! BBCode → styled chunk renderer.
//!
//! Deliberately independent of any UI toolkit: output is a flat sequence of
//! `Chunk`s (styled text or hyperlinks) that the TUI maps onto ratatui spans
//! (with OSC-8 for links where the terminal supports them).
//!
//! House rule for unknown input: never silently drop content. Unknown tags
//! and malformed brackets pass through as literal text.

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Style {
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strikethrough: bool,
    pub code: bool,
    pub spoiler: bool,
    /// Quote nesting depth — the UI dims/indents by depth.
    pub quote_depth: u8,
    /// List nesting depth — the UI hangs bullets by depth.
    pub list_depth: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Chunk {
    Text(String, Style),
    /// Visible label + target URL.
    Link(String, String, Style),
}

impl Style {
    fn from_stack(stack: &[Frame]) -> Style {
        let mut s = Style::default();
        for frame in stack {
            match frame {
                Frame::Bold => s.bold = true,
                Frame::Italic => s.italic = true,
                Frame::Underline => s.underline = true,
                Frame::Strike => s.strikethrough = true,
                Frame::Quote { .. } => s.quote_depth += 1,
                Frame::List => s.list_depth += 1,
                Frame::Spoiler => s.spoiler = true,
                Frame::Verbatim | Frame::Color(_) | Frame::Size(_) | Frame::Font(_)
                | Frame::Align(_) | Frame::Link(_) => {}
            }
        }
        s
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Frame {
    Bold,
    Italic,
    Underline,
    Strike,
    Quote { byline: Option<String> },
    List,
    Spoiler,
    /// CODE/PHP: scan raw until the exact closing tag.
    Verbatim,
    Color(String),
    Size(String),
    Font(String),
    Align(String),
    /// [URL=href] — content until [/URL] is the label.
    Link(String),
}

pub fn render(src: &str) -> Vec<Chunk> {
    let mut out: Vec<Chunk> = Vec::new();
    let mut stack: Vec<Frame> = Vec::new();
    let mut rest = src;

    'outer: while !rest.is_empty() {
        // Verbatim frames swallow everything up to their closing tag.
        if stack.iter().any(|f| matches!(f, Frame::Verbatim)) {
            let close = find_verbatim_close(rest);
            match close {
                Some((end_content, close_len)) => {
                    let content = &rest[..end_content];
                    emit_verbatim(&mut out, content, &stack);
                    rest = &rest[end_content + close_len..];
                }
                None => {
                    emit_verbatim(&mut out, rest, &stack);
                    rest = "";
                }
            }
            continue;
        }

        let bracket = match rest.find('[') {
            Some(i) => i,
            None => {
                emit_text(&mut out, rest, &stack);
                break;
            }
        };
        if bracket > 0 {
            emit_text(&mut out, &rest[..bracket], &stack);
            rest = &rest[bracket..];
        }

        // Try to parse a tag at position 0.
        match parse_tag(rest) {
            Some(TagEvent::Open { name, value, len }) => {
                let raw_tag = &rest[..len];
                rest = &rest[len..];
                match name.to_ascii_lowercase().as_str() {
                    "b" => stack.push(Frame::Bold),
                    "i" => stack.push(Frame::Italic),
                    "u" => stack.push(Frame::Underline),
                    "s" => stack.push(Frame::Strike),
                    "quote" => {
                        let byline = value.filter(|v| !v.trim().is_empty());
                        if let Some(by) = &byline {
                            let mut st = Style::from_stack(&stack);
                            st.italic = true;
                            out.push(Chunk::Text(format!("{by} wrote:\n"), st));
                        }
                        stack.push(Frame::Quote { byline });
                    }
                    "list" => stack.push(Frame::List),
                    "spoiler" | "ispoiler" => stack.push(Frame::Spoiler),
                    "code" | "php" => stack.push(Frame::Verbatim),
                    "color" => stack.push(Frame::Color(value.unwrap_or_default())),
                    "size" => stack.push(Frame::Size(value.unwrap_or_default())),
                    "font" => stack.push(Frame::Font(value.unwrap_or_default())),
                    "left" | "center" | "right" => {
                        stack.push(Frame::Align(name.to_ascii_lowercase()))
                    }
                    "indent" => stack.push(Frame::Align("indent".into())),
                    "url" => match value.filter(|v| !v.trim().is_empty()) {
                        Some(href) => stack.push(Frame::Link(href.to_string())),
                        None => stack.push(Frame::Link(String::new())), // label IS the url
                    },
                    "img" | "media" => {
                        let (inner, len) = take_until_close(rest, &name);
                        let st = Style::from_stack(&stack);
                        let trimmed = inner.trim();
                        if trimmed.is_empty() {
                            out.push(Chunk::Text("[image]".into(), st));
                        } else {
                            out.push(Chunk::Link("[image]".into(), trimmed.to_string(), st));
                        }
                        rest = &rest[len..];
                    }
                    "attach" => {
                        let (inner, len) = take_until_close(rest, &name);
                        let st = Style::from_stack(&stack);
                        out.push(Chunk::Text(
                            format!("[attachment {}]", inner.trim()),
                            st,
                        ));
                        rest = &rest[len..];
                    }
                    "user" => {
                        // [USER=123]name[/USER]
                        let (inner, len) = take_until_close(rest, &name);
                        let st = Style::from_stack(&stack);
                        out.push(Chunk::Text(format!("@{}", inner.trim()), st));
                        rest = &rest[len..];
                    }
                    "*" => {
                        // List item marker inside [LIST]; bullet outside a list too.
                        let mut st = Style::from_stack(&stack);
                        st.bold = true;
                        out.push(Chunk::Text("• ".into(), st));
                    }
                    _ => {
                        // Unknown tag: literal passthrough, no frame pushed.
                        emit_text(&mut out, raw_tag, &stack);
                    }
                }
            }
            Some(TagEvent::Close { name, len }) => {
                let raw_tag = &rest[..len];
                rest = &rest[len..];
                if !pop_matching(&mut stack, &name.to_ascii_lowercase()) {
                    // Closing tag for an unknown/unopened tag: keep it visible.
                    emit_text(&mut out, raw_tag, &stack);
                }
            }
            Some(TagEvent::Star(len)) => {
                rest = &rest[len..];
                let mut st = Style::from_stack(&stack);
                st.bold = true;
                out.push(Chunk::Text("• ".into(), st));
            }
            None => {
                // Stray '[': literal.
                emit_text(&mut out, &rest[..1], &stack);
                rest = &rest[1..];
                continue 'outer;
            }
        }
    }

    if out.is_empty() {
        out.push(Chunk::Text(String::new(), Style::default()));
    }
    out
}

/// Single-paragraph plain-text preview (thread list secondary line).
pub fn to_plain(src: &str) -> String {
    let mut s = String::new();
    for chunk in render(src) {
        match chunk {
            Chunk::Text(t, _) => s.push_str(&t),
            Chunk::Link(label, url, _) => {
                s.push_str(&label);
                if label != url {
                    s.push_str(&format!(" ({url})"));
                }
            }
        }
    }
    s.replace('\n', " ").split_whitespace().collect::<Vec<_>>().join(" ")
}

enum TagEvent {
    Open { name: String, value: Option<String>, len: usize },
    Close { name: String, len: usize },
    Star(usize),
}

fn parse_tag(s: &str) -> Option<TagEvent> {
    debug_assert!(s.starts_with('['));
    let close = s.find(']')?;
    let inner = &s[1..close];
    // Reject obviously malformed tag bodies (spaces in tag names, newlines).
    if inner.is_empty() || inner.contains('\n') || inner.contains('\r') {
        return None;
    }
    if let Some(name) = inner.strip_prefix('/') {
        let name = name.trim();
        if name.is_empty() || !is_tag_name(name) {
            return None;
        }
        return Some(TagEvent::Close { name: name.to_string(), len: close + 1 });
    }
    let (name, value) = match inner.split_once('=') {
        Some((n, v)) => (n, Some(v.to_string())),
        None => (inner, None),
    };
    let name = name.trim();
    if name == "*" {
        return Some(TagEvent::Star(close + 1));
    }
    if !is_tag_name(name) {
        return None;
    }
    Some(TagEvent::Open {
        name: name.to_string(),
        value,
        len: close + 1,
    })
}

fn is_tag_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 16
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Find the closing tag for `name` (case-insensitive) returning content end
/// offset and the closing tag length.
fn take_until_close<'a>(s: &'a str, name: &str) -> (&'a str, usize) {
    let lower = s.to_ascii_lowercase();
    let mut needle = String::with_capacity(name.len() + 3);
    needle.push_str("[/");
    needle.push_str(&name.to_ascii_lowercase());
    needle.push(']');
    match lower.find(&needle) {
        Some(pos) => (&s[..pos], pos + needle.len()),
        None => (s, s.len()),
    }
}

/// In verbatim mode find the first [/CODE] or [/PHP]; returns content end and
/// close-tag length.
fn find_verbatim_close(s: &str) -> Option<(usize, usize)> {
    let lower = s.to_ascii_lowercase();
    let code = lower.find("[/code]");
    let php = lower.find("[/php]");
    match (code, php) {
        (Some(a), Some(b)) => {
            if a < b {
                Some((a, 7))
            } else {
                Some((b, 6))
            }
        }
        (Some(a), None) => Some((a, 7)),
        (None, Some(b)) => Some((b, 6)),
        (None, None) => None,
    }
}

fn emit_verbatim(out: &mut Vec<Chunk>, content: &str, stack: &[Frame]) {
    let mut st = Style::from_stack(stack);
    st.code = true;
    out.push(Chunk::Text(content.trim_matches('\n').to_string(), st.clone()));
    out.push(Chunk::Text("\n".into(), st));
}

/// Emit plain text: inside a [URL] frame the whole run is a link; otherwise
/// bare http(s) URLs are auto-linked on the way through.
fn emit_text(out: &mut Vec<Chunk>, text: &str, stack: &[Frame]) {
    if text.is_empty() {
        return;
    }
    let st = Style::from_stack(stack);
    if let Some(href) = stack.iter().rev().find_map(|f| match f {
        Frame::Link(h) => Some(h.clone()),
        _ => None,
    }) {
        // [URL=href]label[/URL] — empty href means the label IS the url.
        let target = if href.is_empty() { text } else { &href };
        out.push(Chunk::Link(text.to_string(), target.to_string(), st));
        return;
    }
    let mut rest = text;
    while let Some(pos) = find_url(rest) {
        let (before, url, after) = split_url(rest, pos);
        if !before.is_empty() {
            out.push(Chunk::Text(before.to_string(), st.clone()));
        }
        out.push(Chunk::Link(url.to_string(), url.to_string(), st.clone()));
        rest = after;
    }
    if !rest.is_empty() {
        out.push(Chunk::Text(rest.to_string(), st));
    }
}

fn find_url(s: &str) -> Option<usize> {
    let lower = s.to_ascii_lowercase();
    let http = lower.find("http://");
    let https = lower.find("https://");
    match (http, https) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

/// Split `s` at the URL starting at `pos` into (before, url, after). The URL
/// ends at whitespace, a closing bracket/quote, or end-of-input (trailing
/// punctuation kept — imperfect, harmless).
fn split_url(s: &str, pos: usize) -> (&str, &str, &str) {
    let rest = &s[pos..];
    let end = rest
        .char_indices()
        .find(|&(i, c)| {
            i > 0 && matches!(c, ' ' | '\t' | '\n' | '\r' | ')' | ']' | '"' | '\'' | '<' | '>')
        })
        .map(|(i, _)| i)
        .unwrap_or(rest.len());
    (&s[..pos], &rest[..end], &rest[end..])
}

/// Pop the innermost frame matching the closing tag name; false if none.
fn pop_matching(stack: &mut Vec<Frame>, name: &str) -> bool {
    #[allow(clippy::match_like_matches_macro)] // paired (name, frame) arms read clearer as a match
    fn is_closer(name: &str, f: &Frame) -> bool {
    match (name, f) {
        ("b", Frame::Bold)
        | ("i", Frame::Italic)
        | ("u", Frame::Underline)
        | ("s", Frame::Strike)
        | ("list", Frame::List)
        | ("spoiler", Frame::Spoiler)
        | ("ispoiler", Frame::Spoiler)
        | ("code", Frame::Verbatim)
        | ("php", Frame::Verbatim)
        | ("color", Frame::Color(_))
        | ("size", Frame::Size(_))
        | ("font", Frame::Font(_))
        | ("left", Frame::Align(_))
        | ("center", Frame::Align(_))
        | ("right", Frame::Align(_))
        | ("indent", Frame::Align(_))
        | ("url", Frame::Link(_)) => true,
        ("quote", Frame::Quote { .. }) => true,
        _ => false,
    }
    }
    match stack.iter().rposition(|f| is_closer(name, f)) {
        Some(pos) => {
            stack.remove(pos);
            true
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(chunks: &[Chunk]) -> Vec<&str> {
        chunks
            .iter()
            .filter_map(|c| match c {
                Chunk::Text(t, _) => Some(t.as_str()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn basic_formatting_flags() {
        let chunks = render("[B]bold[/B] [I]it[/I] [U]u[/U] [S]s[/S]");
        let (_, s0) = match &chunks[0] {
            Chunk::Text(t, s) => (t.as_str(), s),
            _ => panic!(),
        };
        assert!(s0.bold && !s0.italic);
        let it = chunks.iter().find(|c| matches!(c, Chunk::Text(t, _) if t.contains("it"))).unwrap();
        if let Chunk::Text(_, s) = it {
            assert!(s.italic && !s.bold);
        }
    }

    #[test]
    fn url_with_label_is_link() {
        let chunks = render("[URL=https://example.com]click[/URL]");
        match &chunks[0] {
            Chunk::Link(label, url, _) => {
                assert_eq!(label, "click");
                assert_eq!(url, "https://example.com");
            }
            _ => panic!("expected link"),
        }
    }

    #[test]
    fn url_without_label_uses_url_as_label() {
        let chunks = render("[URL]https://example.com/x[/URL]");
        match &chunks[0] {
            Chunk::Link(label, url, _) => {
                assert_eq!(label, "https://example.com/x");
                assert_eq!(url, "https://example.com/x");
            }
            _ => panic!("expected link"),
        }
    }

    #[test]
    fn bare_urls_autolink() {
        let chunks = render("see https://example.com/a here");
        assert_eq!(
            chunks,
            vec![
                Chunk::Text("see ".into(), Style::default()),
                Chunk::Link(
                    "https://example.com/a".into(),
                    "https://example.com/a".into(),
                    Style::default()
                ),
                Chunk::Text(" here".into(), Style::default()),
            ]
        );
    }

    #[test]
    fn code_block_is_verbatim_and_flags_code() {
        let chunks = render("[CODE]not [B]bold[/B] and [I]not it[/I][/CODE] after");
        let code = &chunks[0];
        if let Chunk::Text(t, s) = code {
            assert!(s.code);
            assert!(t.contains("[B]bold[/B]"));
            assert!(!t.contains("after"));
        } else {
            panic!("expected verbatim text");
        }
        assert!(texts(&chunks).iter().any(|t| t.contains("after")));
    }

    #[test]
    fn php_block_without_close_takes_rest() {
        let chunks = render("[PHP]$x = 1;[/PHP]");
        if let Chunk::Text(t, s) = &chunks[0] {
            assert!(s.code && t.contains("$x = 1;"));
        } else {
            panic!("expected verbatim");
        }
    }

    #[test]
    fn quote_nesting_raises_depth() {
        let chunks = render("[QUOTE=alice]outer [QUOTE=bob]inner[/QUOTE] back[/QUOTE]");
        let depths: Vec<u8> = chunks
            .iter()
            .filter_map(|c| match c {
                Chunk::Text(_, s) => Some(s.quote_depth),
                _ => None,
            })
            .collect();
        assert!(depths.contains(&1));
        assert!(depths.contains(&2));
        assert!(depths.contains(&1)); // after inner close
    }

    #[test]
    fn quote_byline_renders() {
        let chunks = render("[QUOTE=alice, post: 123]hi[/QUOTE]");
        let all = texts(&chunks).concat();
        assert!(all.contains("alice"));
        assert!(all.contains("hi"));
    }

    #[test]
    fn list_items_get_bullets() {
        let chunks = render("[LIST]\n[*]one\n[*]two\n[/LIST]");
        let all = texts(&chunks).concat();
        assert!(all.contains("• one"));
        assert!(all.contains("• two"));
    }

    #[test]
    fn star_outside_list_still_bullets() {
        let chunks = render("[*] lone");
        assert!(texts(&chunks).concat().contains("• "));
    }

    #[test]
    fn unknown_tags_pass_through_as_text() {
        let src = "[FOOBAR=1]hi[/FOOBAR]";
        let chunks = render(src);
        assert_eq!(texts(&chunks).concat(), src);
    }

    #[test]
    fn unbalanced_tags_keep_content() {
        let chunks = render("[B]bold to the end");
        let all_bold = chunks
            .iter()
            .all(|c| matches!(c, Chunk::Text(_, s) if s.bold));
        assert!(all_bold);
    }

    #[test]
    fn stray_bracket_is_literal() {
        let chunks = render("a [ b [B]c[/B]");
        let all = texts(&chunks).concat();
        assert!(all.starts_with("a [ b "));
    }

    #[test]
    fn img_becomes_link() {
        let chunks = render("[IMG]https://example.com/pic.png[/IMG]");
        match &chunks[0] {
            Chunk::Link(label, url, _) => {
                assert_eq!(label, "[image]");
                assert_eq!(url, "https://example.com/pic.png");
            }
            _ => panic!("expected image link"),
        }
    }

    #[test]
    fn attach_and_user_markers() {
        let all = texts(&render("[ATTACH=full]1234[/ATTACH] by [USER=9]bob[/USER]")).concat();
        assert!(all.contains("[attachment 1234]"));
        assert!(all.contains("@bob"));
    }

    #[test]
    fn spoiler_flagged() {
        let chunks = render("[ISPOILER]secret[/ISPOILER]");
        if let Chunk::Text(_, s) = &chunks[0] {
            assert!(s.spoiler);
        } else {
            panic!("expected spoiler text");
        }
    }

    #[test]
    fn crlf_is_tolerated() {
        let all = texts(&render("[B]a[/B]\r\n[B]b[/B]")).concat();
        assert!(all.contains("a") && all.contains("b"));
    }

    #[test]
    fn plain_preview_is_single_spaced_line() {
        let p = to_plain("line1\n\nline2   with    spaces [B]bold[/B]");
        assert_eq!(p, "line1 line2 with spaces bold");
    }

    #[test]
    fn empty_input_yields_one_empty_chunk() {
        assert_eq!(render(""), vec![Chunk::Text(String::new(), Style::default())]);
    }
}
