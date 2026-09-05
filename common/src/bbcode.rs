//! BBCode → styled chunk renderer.
//!
//! Deliberately independent of any UI toolkit: output is a flat sequence of
//! `Chunk`s (styled text or hyperlinks) that the TUI maps onto ratatui spans
//! (with OSC-8 for links where the terminal supports them).
//!
//! House rule for unknown input: never silently drop content. Unknown tags
//! and malformed brackets pass through as literal text.

use std::borrow::Cow;

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
    /// `[IMG]url[/IMG]` — an inline image reference. Its own variant rather
    /// than a `Link` labelled `[image]` so a consumer can tell a picture
    /// apart from a link whose label merely reads that way: the compose
    /// preview turns these into a caption plus a real inline image, and
    /// guessing from a label would put a picture under `[URL=x][image][/URL]`.
    Image(String, Style),
    /// `[ATTACH]id[/ATTACH]` (and the `=full` / `type="full"` spellings) — an
    /// attachment referenced by id. Only whoever holds the post's (or the
    /// draft's) attachment list can turn the id into a URL, so the parser
    /// carries the id through untouched.
    Attach(String, Style),
}

/// An image a piece of BBCode points at, in source order — what
/// [`Chunk::image_ref`] and [`image_refs`] hand back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageRef {
    /// `[IMG]https://…[/IMG]`. Always http/https: any other scheme
    /// (`data:`, `javascript:`, `file:`) is dropped here rather than at the
    /// fetch, so no consumer can be talked into loading one.
    Url(String),
    /// `[ATTACH]id[/ATTACH]`, resolved against an attachment list by the
    /// caller. Non-numeric ids are dropped — XenForo attachment ids are
    /// integers.
    Attachment(u32),
}

impl Chunk {
    /// The image this chunk references, if it is one. `None` for text, links
    /// and for image references that cannot be fetched (a non-http `[IMG]`
    /// target, an `[ATTACH]` whose id is not a number).
    pub fn image_ref(&self) -> Option<ImageRef> {
        match self {
            Chunk::Image(url, _) if is_http_url(url) => Some(ImageRef::Url(url.clone())),
            Chunk::Attach(id, _) => id.trim().parse::<u32>().ok().map(ImageRef::Attachment),
            _ => None,
        }
    }
}

/// Every image reference in `src`, in source order. Duplicates are kept: they
/// are separate references, and de-duplication is the caller's policy.
///
/// Tolerant in exactly the way [`render`] is, because it *is* `render`: tags
/// are case-insensitive, an unclosed `[IMG]` stays literal text, and an
/// `[IMG]` inside `[CODE]`/`[PLAIN]` is source, not a picture.
pub fn image_refs(src: &str) -> Vec<ImageRef> {
    render(src).iter().filter_map(Chunk::image_ref).collect()
}

/// True for an absolute http/https URL, scheme compared case-insensitively.
pub fn is_http_url(s: &str) -> bool {
    let lower = s.trim().to_ascii_lowercase();
    lower.starts_with("http://") || lower.starts_with("https://")
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
                Frame::List(_) => s.list_depth += 1,
                Frame::Spoiler => s.spoiler = true,
                Frame::InlineCode => s.code = true,
                Frame::Heading(_) | Frame::TableHeader => {
                    s.bold = true;
                }
                Frame::Color(_)
                | Frame::Size(_)
                | Frame::Font(_)
                | Frame::Align(_)
                | Frame::Link(_)
                | Frame::Table
                | Frame::TableRow
                | Frame::TableCell => {}
            }
        }
        s
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListKind {
    Bullet,
    Numbered(usize),
    Alpha(usize),
}

impl ListKind {
    fn from_attr(attr: Option<&str>) -> Self {
        match attr {
            Some("1") => ListKind::Numbered(1),
            Some("a") | Some("A") => ListKind::Alpha(1),
            _ => ListKind::Bullet,
        }
    }

    fn next_marker(&mut self) -> String {
        match self {
            ListKind::Bullet => "• ".into(),
            ListKind::Numbered(n) => {
                let cur = *n;
                *n += 1;
                format!("{cur}. ")
            }
            ListKind::Alpha(n) => {
                let cur = *n;
                *n += 1;
                let c = (b'a' + ((cur - 1) % 26) as u8) as char;
                format!("{c}. ")
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Frame {
    Bold,
    Italic,
    Underline,
    Strike,
    Quote { byline: Option<String> },
    List(ListKind),
    Spoiler,
    InlineCode,
    Heading(u8),
    Color(String),
    Size(String),
    Font(String),
    Align(String),
    /// [URL=href] — content until [/URL] is the label.
    Link(String),
    Table,
    TableRow,
    TableHeader,
    TableCell,
}

/// Strip surrounding quotation marks ("..." or '...') and trim whitespace.
pub fn strip_quotes(s: &str) -> &str {
    let trimmed = s.trim();
    if (trimmed.starts_with('"') && trimmed.ends_with('"') && trimmed.len() >= 2)
        || (trimmed.starts_with('\'') && trimmed.ends_with('\'') && trimmed.len() >= 2)
    {
        &trimmed[1..trimmed.len() - 1]
    } else {
        trimmed
    }
}

/// Decode standard HTML entities commonly found in XenForo BBCode / API output.
pub fn decode_html_entities(input: &str) -> String {
    if !input.contains('&') {
        return input.to_string();
    }
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let after_amp = &rest[amp + 1..];
        if let Some(semi) = after_amp.find(';') {
            let entity = &after_amp[..semi];
            let decoded: Option<Cow<'static, str>> = match entity {
                "amp" => Some("&".into()),
                "quot" => Some("\"".into()),
                "#039" | "apos" => Some("'".into()),
                "lt" => Some("<".into()),
                "gt" => Some(">".into()),
                "nbsp" | "#160" => Some(" ".into()),
                _ if entity.starts_with('#') => {
                    let num_str = &entity[1..];
                    let ch = if let Some(hex) = num_str.strip_prefix('x').or_else(|| num_str.strip_prefix('X')) {
                        u32::from_str_radix(hex, 16).ok().and_then(char::from_u32)
                    } else {
                        num_str.parse::<u32>().ok().and_then(char::from_u32)
                    };
                    ch.map(|c| Cow::Owned(c.to_string()))
                }
                _ => None,
            };
            if let Some(d) = decoded {
                out.push_str(&d);
                rest = &after_amp[semi + 1..];
                continue;
            }
        }
        out.push('&');
        rest = after_amp;
    }
    out.push_str(rest);
    out
}

/// Parse XenForo quote attributes: e.g. "Alice, post: 12345, member: 678" -> ("Alice", Some(12345))
fn parse_quote_byline(raw: &str) -> (String, Option<u32>) {
    let unquoted = strip_quotes(raw);
    if let Some((author, rest)) = unquoted.split_once(',') {
        let post_id = rest
            .split(',')
            .find_map(|part| {
                let part = part.trim();
                part.strip_prefix("post:").and_then(|p| p.trim().parse::<u32>().ok())
            });
        (author.trim().to_string(), post_id)
    } else {
        (unquoted.to_string(), None)
    }
}

/// Resolve XenForo [MEDIA=site]id[/MEDIA] tags to valid hyperlinks and readable labels.
fn resolve_media(site: &str, media_id: &str) -> (String, String) {
    let clean_id = media_id.trim();
    match site.to_ascii_lowercase().as_str() {
        "youtube" => (
            "[video: YouTube]".into(),
            format!("https://www.youtube.com/watch?v={clean_id}"),
        ),
        "vimeo" => (
            "[video: Vimeo]".into(),
            format!("https://vimeo.com/{clean_id}"),
        ),
        "twitter" => (
            "[media: Twitter/X]".into(),
            format!("https://twitter.com/x/status/{clean_id}"),
        ),
        "tiktok" => (
            "[video: TikTok]".into(),
            format!("https://www.tiktok.com/@user/video/{clean_id}"),
        ),
        "spotify" => (
            "[audio: Spotify]".into(),
            format!("https://open.spotify.com/track/{clean_id}"),
        ),
        other => {
            if clean_id.starts_with("http://") || clean_id.starts_with("https://") {
                (format!("[media: {other}]"), clean_id.to_string())
            } else {
                (format!("[media: {other}]"), format!("https://{other}.com/{clean_id}"))
            }
        }
    }
}

fn ends_with_newline(chunks: &[Chunk]) -> bool {
    chunks.last().is_some_and(|c| match c {
        Chunk::Text(t, _) => t.ends_with('\n'),
        Chunk::Link(l, _, _) => l.ends_with('\n'),
        // Both render as a one-line placeholder, never a paragraph break.
        Chunk::Image(..) | Chunk::Attach(..) => false,
    })
}

fn emit_list_marker(out: &mut Vec<Chunk>, stack: &mut [Frame]) {
    let marker = if let Some(Frame::List(kind)) =
        stack.iter_mut().rev().find(|f| matches!(f, Frame::List(_)))
    {
        kind.next_marker()
    } else {
        "• ".to_string()
    };
    let mut st = Style::from_stack(stack);
    st.bold = true;
    out.push(Chunk::Text(marker, st));
}

pub fn render(src: &str) -> Vec<Chunk> {
    let mut out: Vec<Chunk> = Vec::new();
    let mut stack: Vec<Frame> = Vec::new();
    let mut rest = src;

    'outer: while !rest.is_empty() {
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
                let tag_lower = name.to_ascii_lowercase();
                match tag_lower.as_str() {
                    "b" => stack.push(Frame::Bold),
                    "i" => stack.push(Frame::Italic),
                    "u" => stack.push(Frame::Underline),
                    "s" | "strike" => stack.push(Frame::Strike),
                    "sub" | "sup" => stack.push(Frame::Italic),
                    "highlight" => stack.push(Frame::Bold),
                    "icode" | "inlinecode" => stack.push(Frame::InlineCode),
                    "code" | "php" | "html" => {
                        let (inner, close_len) = take_until_close(rest, &tag_lower);
                        let mut st = Style::from_stack(&stack);
                        st.code = true;
                        if let Some(v) = &value {
                            let clean_v = strip_quotes(v);
                            if !clean_v.is_empty() {
                                let mut hst = Style::from_stack(&stack);
                                hst.bold = true;
                                out.push(Chunk::Text(format!("[{clean_v} code]\n"), hst));
                            }
                        }
                        let decoded = decode_html_entities(inner.trim_matches('\n'));
                        out.push(Chunk::Text(decoded, st.clone()));
                        out.push(Chunk::Text("\n".into(), st));
                        rest = &rest[close_len..];
                    }
                    "plain" => {
                        let (inner, close_len) = take_until_close(rest, "plain");
                        emit_text(&mut out, inner, &stack);
                        rest = &rest[close_len..];
                    }
                    "quote" => {
                        let byline = value.filter(|v| !v.trim().is_empty());
                        if let Some(by) = &byline {
                            let (author, _) = parse_quote_byline(by);
                            let mut st = Style::from_stack(&stack);
                            st.italic = true;
                            out.push(Chunk::Text(format!("{author} wrote:\n"), st));
                        }
                        stack.push(Frame::Quote { byline });
                    }
                    "list" => {
                        let kind = ListKind::from_attr(value.as_deref());
                        stack.push(Frame::List(kind));
                    }
                    "spoiler" | "ispoiler" => {
                        if let Some(title) = &value {
                            let clean = strip_quotes(title);
                            if !clean.is_empty() {
                                let mut st = Style::from_stack(&stack);
                                st.bold = true;
                                out.push(Chunk::Text(format!("[Spoiler: {clean}]\n"), st));
                            }
                        }
                        stack.push(Frame::Spoiler);
                    }
                    "color" => stack.push(Frame::Color(value.unwrap_or_default())),
                    "size" => stack.push(Frame::Size(value.unwrap_or_default())),
                    "font" => stack.push(Frame::Font(value.unwrap_or_default())),
                    "left" | "center" | "right" | "justify" => {
                        stack.push(Frame::Align(tag_lower))
                    }
                    "indent" => stack.push(Frame::Align("indent".into())),
                    "heading" => {
                        let level = value
                            .as_deref()
                            .and_then(|v| strip_quotes(v).parse::<u8>().ok())
                            .unwrap_or(1);
                        if !out.is_empty() && !ends_with_newline(&out) {
                            out.push(Chunk::Text("\n".into(), Style::from_stack(&stack)));
                        }
                        stack.push(Frame::Heading(level));
                    }
                    "h1" => {
                        if !out.is_empty() && !ends_with_newline(&out) {
                            out.push(Chunk::Text("\n".into(), Style::from_stack(&stack)));
                        }
                        stack.push(Frame::Heading(1));
                    }
                    "h2" => {
                        if !out.is_empty() && !ends_with_newline(&out) {
                            out.push(Chunk::Text("\n".into(), Style::from_stack(&stack)));
                        }
                        stack.push(Frame::Heading(2));
                    }
                    "h3" => {
                        if !out.is_empty() && !ends_with_newline(&out) {
                            out.push(Chunk::Text("\n".into(), Style::from_stack(&stack)));
                        }
                        stack.push(Frame::Heading(3));
                    }
                    "hr" => {
                        if !out.is_empty() && !ends_with_newline(&out) {
                            out.push(Chunk::Text("\n".into(), Style::from_stack(&stack)));
                        }
                        out.push(Chunk::Text("───\n".into(), Style::from_stack(&stack)));
                    }
                    "table" => {
                        if !out.is_empty() && !ends_with_newline(&out) {
                            out.push(Chunk::Text("\n".into(), Style::from_stack(&stack)));
                        }
                        stack.push(Frame::Table);
                    }
                    "tr" => {
                        if !out.is_empty() && !ends_with_newline(&out) {
                            out.push(Chunk::Text("\n".into(), Style::from_stack(&stack)));
                        }
                        stack.push(Frame::TableRow);
                    }
                    "th" => stack.push(Frame::TableHeader),
                    "td" => stack.push(Frame::TableCell),
                    "url" => match value.filter(|v| !v.trim().is_empty()) {
                        Some(href) => stack.push(Frame::Link(strip_quotes(&href).to_string())),
                        None => stack.push(Frame::Link(String::new())),
                    },
                    "email" => match value.filter(|v| !v.trim().is_empty()) {
                        Some(target) => {
                            let clean = strip_quotes(&target);
                            let href = if clean.starts_with("mailto:") {
                                clean.to_string()
                            } else {
                                format!("mailto:{clean}")
                            };
                            stack.push(Frame::Link(href));
                        }
                        None => stack.push(Frame::Link(String::new())),
                    },
                    "post" => {
                        let id = value.as_deref().map(strip_quotes).unwrap_or("");
                        let url = if !id.is_empty() {
                            format!("https://windowsforum.com/posts/{id}/")
                        } else {
                            String::new()
                        };
                        stack.push(Frame::Link(url));
                    }
                    "thread" => {
                        let id = value.as_deref().map(strip_quotes).unwrap_or("");
                        let url = if !id.is_empty() {
                            format!("https://windowsforum.com/threads/{id}/")
                        } else {
                            String::new()
                        };
                        stack.push(Frame::Link(url));
                    }
                    "img" => {
                        let lower = rest.to_ascii_lowercase();
                        if lower.contains("[/img]") {
                            let (inner, close_len) = take_until_close(rest, "img");
                            let st = Style::from_stack(&stack);
                            let trimmed = strip_quotes(inner.trim());
                            if trimmed.is_empty() {
                                out.push(Chunk::Text("[image]".into(), st));
                            } else {
                                out.push(Chunk::Image(trimmed.to_string(), st));
                            }
                            rest = &rest[close_len..];
                        } else if let Some(v) = &value {
                            let st = Style::from_stack(&stack);
                            out.push(Chunk::Image(strip_quotes(v).to_string(), st));
                        } else {
                            emit_text(&mut out, raw_tag, &stack);
                        }
                    }
                    "media" => {
                        let lower = rest.to_ascii_lowercase();
                        if lower.contains("[/media]") {
                            let (inner, close_len) = take_until_close(rest, "media");
                            let st = Style::from_stack(&stack);
                            let site = value.as_deref().map(strip_quotes).unwrap_or("media");
                            let (label, url) = resolve_media(site, inner);
                            out.push(Chunk::Link(label, url, st));
                            rest = &rest[close_len..];
                        } else {
                            emit_text(&mut out, raw_tag, &stack);
                        }
                    }
                    "attach" => {
                        let lower = rest.to_ascii_lowercase();
                        if lower.contains("[/attach]") {
                            let (inner, close_len) = take_until_close(rest, "attach");
                            let st = Style::from_stack(&stack);
                            let id = if inner.trim().is_empty() {
                                value.as_deref().map(strip_quotes).unwrap_or("").trim()
                            } else {
                                inner.trim()
                            };
                            out.push(Chunk::Attach(id.to_string(), st));
                            rest = &rest[close_len..];
                        } else if let Some(v) = &value {
                            let st = Style::from_stack(&stack);
                            let clean = strip_quotes(v);
                            out.push(Chunk::Attach(clean.to_string(), st));
                        } else {
                            emit_text(&mut out, raw_tag, &stack);
                        }
                    }
                    "user" => {
                        let lower = rest.to_ascii_lowercase();
                        if lower.contains("[/user]") {
                            let (inner, close_len) = take_until_close(rest, "user");
                            let st = Style::from_stack(&stack);
                            out.push(Chunk::Text(format!("@{}", inner.trim()), st));
                            rest = &rest[close_len..];
                        } else if let Some(v) = &value {
                            let st = Style::from_stack(&stack);
                            let clean = strip_quotes(v);
                            out.push(Chunk::Text(format!("@{clean}"), st));
                        } else {
                            emit_text(&mut out, raw_tag, &stack);
                        }
                    }
                    "*" => {
                        emit_list_marker(&mut out, &mut stack);
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
                let tag_lower = name.to_ascii_lowercase();
                match tag_lower.as_str() {
                    "heading" | "h1" | "h2" | "h3" => {
                        if pop_matching(&mut stack, &tag_lower)
                            && !out.is_empty()
                            && !ends_with_newline(&out)
                        {
                            out.push(Chunk::Text("\n".into(), Style::from_stack(&stack)));
                        }
                    }
                    "td" | "th" => {
                        if pop_matching(&mut stack, &tag_lower) {
                            out.push(Chunk::Text(" | ".into(), Style::from_stack(&stack)));
                        }
                    }
                    "tr" => {
                        if pop_matching(&mut stack, "tr")
                            && !out.is_empty()
                            && !ends_with_newline(&out)
                        {
                            out.push(Chunk::Text("\n".into(), Style::from_stack(&stack)));
                        }
                    }
                    "table" => {
                        if pop_matching(&mut stack, "table")
                            && !out.is_empty()
                            && !ends_with_newline(&out)
                        {
                            out.push(Chunk::Text("\n".into(), Style::from_stack(&stack)));
                        }
                    }
                    _ => {
                        if !pop_matching(&mut stack, &tag_lower) {
                            // Closing tag for an unknown/unopened tag: keep it visible.
                            emit_text(&mut out, raw_tag, &stack);
                        }
                    }
                }
            }
            Some(TagEvent::Star(len)) => {
                rest = &rest[len..];
                emit_list_marker(&mut out, &mut stack);
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
                if label != url && !url.is_empty() {
                    s.push_str(&format!(" ({url})"));
                }
            }
            Chunk::Image(url, _) => {
                s.push_str("[image]");
                if !url.is_empty() {
                    s.push_str(&format!(" ({url})"));
                }
            }
            Chunk::Attach(id, _) => s.push_str(&format!("[attachment {id}]")),
        }
    }
    s.replace('\n', " ").split_whitespace().collect::<Vec<_>>().join(" ")
}

enum TagEvent {
    Open {
        name: String,
        value: Option<String>,
        len: usize,
    },
    Close {
        name: String,
        len: usize,
    },
    Star(usize),
}

fn parse_tag(s: &str) -> Option<TagEvent> {
    debug_assert!(s.starts_with('['));
    let close = s.find(']')?;
    let inner = &s[1..close];
    if inner.is_empty() || inner.contains('\n') || inner.contains('\r') {
        return None;
    }
    if let Some(name) = inner.strip_prefix('/') {
        let name = name.trim();
        if name.is_empty() || !is_tag_name(name) {
            return None;
        }
        return Some(TagEvent::Close {
            name: name.to_string(),
            len: close + 1,
        });
    }

    if inner.trim() == "*" {
        return Some(TagEvent::Star(close + 1));
    }

    // Split on first '=' or whitespace. Notice: check if there is a space BEFORE '='
    // (e.g. [ATTACH type="full"] vs [QUOTE="Alice, post: 123"]).
    let (name, value) = if let Some(eq_pos) = inner.find('=') {
        let before_eq = &inner[..eq_pos];
        if let Some(space_pos) = before_eq.find(char::is_whitespace) {
            let name_part = before_eq[..space_pos].trim();
            let val = inner[eq_pos + 1..].trim();
            (name_part, Some(strip_quotes(val).to_string()))
        } else {
            let name_part = before_eq.trim();
            let val = inner[eq_pos + 1..].trim();
            (name_part, Some(strip_quotes(val).to_string()))
        }
    } else if let Some(space_pos) = inner.find(char::is_whitespace) {
        let name_part = inner[..space_pos].trim();
        let val = inner[space_pos + 1..].trim();
        (name_part, Some(strip_quotes(val).to_string()))
    } else {
        (inner.trim(), None)
    };

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
    let needle = format!("[/{}]", name.to_ascii_lowercase());
    match lower.find(&needle) {
        Some(pos) => (&s[..pos], pos + needle.len()),
        None => (s, s.len()),
    }
}

/// Emit plain text: inside a [URL] frame the whole run is a link; otherwise
/// bare http(s) URLs are auto-linked on the way through.
fn emit_text(out: &mut Vec<Chunk>, text: &str, stack: &[Frame]) {
    if text.is_empty() {
        return;
    }
    let decoded_text = decode_html_entities(text);
    let st = Style::from_stack(stack);
    if let Some(href) = stack.iter().rev().find_map(|f| match f {
        Frame::Link(h) => Some(h.clone()),
        _ => None,
    }) {
        // [URL=href]label[/URL] — empty href means the label IS the url.
        let target = if href.is_empty() {
            decoded_text.clone()
        } else {
            href
        };
        out.push(Chunk::Link(decoded_text, target, st));
        return;
    }
    let mut rest = decoded_text.as_str();
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
    #[allow(clippy::match_like_matches_macro)]
    fn is_closer(name: &str, f: &Frame) -> bool {
        match (name, f) {
            ("b", Frame::Bold)
            | ("i", Frame::Italic)
            | ("u", Frame::Underline)
            | ("s" | "strike", Frame::Strike)
            | ("sub" | "sup" | "highlight", Frame::Italic | Frame::Bold)
            | ("list", Frame::List(_))
            | ("spoiler" | "ispoiler", Frame::Spoiler)
            | ("icode" | "inlinecode", Frame::InlineCode)
            | ("heading" | "h1" | "h2" | "h3", Frame::Heading(_))
            | ("color", Frame::Color(_))
            | ("size", Frame::Size(_))
            | ("font", Frame::Font(_))
            | ("left" | "center" | "right" | "justify" | "indent", Frame::Align(_))
            | ("url" | "email" | "post" | "thread", Frame::Link(_))
            | ("table", Frame::Table)
            | ("tr", Frame::TableRow)
            | ("th", Frame::TableHeader)
            | ("td", Frame::TableCell) => true,
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
        let it = chunks
            .iter()
            .find(|c| matches!(c, Chunk::Text(t, _) if t.contains("it")))
            .unwrap();
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
    fn url_with_quotes_stripped() {
        let chunks = render(r#"[URL="https://example.com"]click[/URL]"#);
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
    fn code_block_is_verbatim_and_does_not_corrupt_subsequent_text() {
        let chunks = render("[CODE]not [B]bold[/B] and [I]not it[/I][/CODE] after code");
        let code = &chunks[0];
        if let Chunk::Text(t, s) = code {
            assert!(s.code);
            assert!(t.contains("[B]bold[/B]"));
            assert!(!t.contains("after code"));
        } else {
            panic!("expected verbatim text");
        }

        // CRITICAL BUG VERIFICATION: Text after [/CODE] must NOT have s.code = true!
        let after = chunks
            .iter()
            .find(|c| matches!(c, Chunk::Text(t, _) if t.contains("after code")))
            .expect("should find after-code chunk");
        if let Chunk::Text(_, s) = after {
            assert!(!s.code, "text after [/CODE] must not inherit code style!");
        }
    }

    #[test]
    fn code_block_with_language_tag() {
        let chunks = render("[CODE=rust]let x = 42;[/CODE]");
        let all = texts(&chunks).concat();
        assert!(all.contains("[rust code]"));
        assert!(all.contains("let x = 42;"));
    }

    #[test]
    fn inline_code_tag() {
        let chunks = render("call [ICODE]println![/ICODE] here");
        let icode = chunks
            .iter()
            .find(|c| matches!(c, Chunk::Text(t, _) if t == "println!"))
            .unwrap();
        if let Chunk::Text(_, s) = icode {
            assert!(s.code);
        } else {
            panic!("expected code chunk");
        }
        let after = chunks
            .iter()
            .find(|c| matches!(c, Chunk::Text(t, _) if t.contains("here")))
            .unwrap();
        if let Chunk::Text(_, s) = after {
            assert!(!s.code);
        }
    }

    #[test]
    fn plain_tag_escapes_bbcode() {
        let chunks = render("[PLAIN][B]not bold[/B] and [URL]none[/URL][/PLAIN] outside");
        let all = texts(&chunks).concat();
        assert!(all.contains("[B]not bold[/B]"));
        assert!(!chunks.iter().any(|c| match c {
            Chunk::Text(_, s) => s.bold,
            _ => false,
        }));
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
    fn quote_byline_renders_and_unquotes() {
        let chunks = render(r#"[QUOTE="Alice, post: 12345, member: 678"]hi there[/QUOTE]"#);
        let all = texts(&chunks).concat();
        assert!(all.contains("Alice wrote:"));
        assert!(!all.contains('"'));
        assert!(all.contains("hi there"));
    }

    #[test]
    fn list_items_get_bullets() {
        let chunks = render("[LIST]\n[*]one\n[*]two\n[/LIST]");
        let all = texts(&chunks).concat();
        assert!(all.contains("• one"));
        assert!(all.contains("• two"));
    }

    #[test]
    fn ordered_list_numbered_and_alpha() {
        let numbered = render("[LIST=1]\n[*]first\n[*]second\n[/LIST]");
        let all_num = texts(&numbered).concat();
        assert!(all_num.contains("1. first"));
        assert!(all_num.contains("2. second"));

        let alpha = render("[LIST=a]\n[*]alpha\n[*]beta\n[/LIST]");
        let all_alpha = texts(&alpha).concat();
        assert!(all_alpha.contains("a. alpha"));
        assert!(all_alpha.contains("b. beta"));
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
    fn img_becomes_an_image_chunk() {
        let chunks = render("[IMG]https://example.com/pic.png[/IMG]");
        match &chunks[0] {
            Chunk::Image(url, _) => assert_eq!(url, "https://example.com/pic.png"),
            other => panic!("expected an image chunk, got {other:?}"),
        }
        assert_eq!(to_plain("[IMG]https://example.com/pic.png[/IMG]"), "[image] (https://example.com/pic.png)");
    }

    #[test]
    fn attach_and_user_markers_with_attributes() {
        let chunks = render(r#"[ATTACH type="full"]1234[/ATTACH] by [USER=9]bob[/USER]"#);
        match &chunks[0] {
            Chunk::Attach(id, _) => assert_eq!(id, "1234"),
            other => panic!("expected an attachment chunk, got {other:?}"),
        }
        assert!(texts(&chunks).concat().contains("@bob"));
        assert!(to_plain(r#"[ATTACH type="full"]1234[/ATTACH]"#).contains("[attachment 1234]"));
    }

    // ---------- image reference extraction ----------

    #[test]
    fn image_refs_collects_img_and_attach_in_source_order() {
        let refs = image_refs(
            "intro\n[IMG]https://a.example/1.png[/IMG]\n[ATTACH]12[/ATTACH]\n             [ATTACH=full]34[/ATTACH] [ATTACH type=\"full\"]56[/ATTACH]\n             [img]HTTP://B.example/2.PNG[/img]",
        );
        assert_eq!(
            refs,
            vec![
                ImageRef::Url("https://a.example/1.png".into()),
                ImageRef::Attachment(12),
                ImageRef::Attachment(34),
                ImageRef::Attachment(56),
                ImageRef::Url("HTTP://B.example/2.PNG".into()),
            ]
        );
    }

    #[test]
    fn image_refs_survive_nesting_and_reject_everything_unfetchable() {
        // Nested inside other tags: still an image, still in order.
        assert_eq!(
            image_refs("[QUOTE=\"bob\"][B][IMG]https://a.example/in-quote.png[/IMG][/B][/QUOTE]"),
            vec![ImageRef::Url("https://a.example/in-quote.png".into())]
        );
        // Duplicates are separate references; de-duplication is the caller's.
        assert_eq!(image_refs("[IMG]https://a/x.png[/IMG][IMG]https://a/x.png[/IMG]").len(), 2);

        // Non-http schemes never become a fetchable reference.
        for src in [
            "[IMG]data:image/png;base64,AAAA[/IMG]",
            "[IMG]javascript:alert(1)[/IMG]",
            "[IMG]file:///etc/passwd[/IMG]",
            "[IMG]/relative/path.png[/IMG]",
            "[IMG][/IMG]",
        ] {
            assert!(image_refs(src).is_empty(), "{src} produced a reference");
        }
        // …but the content is still shown, never silently dropped.
        assert!(to_plain("[IMG]data:image/png;base64,AAAA[/IMG]").contains("data:image/png"));

        // A missing close tag stays literal text.
        assert!(image_refs("[IMG]https://a.example/1.png").is_empty());
        assert!(to_plain("[IMG]https://a.example/1.png").contains("[IMG]"));

        // An unclosed IMG after a closed one: the closed one still resolves,
        // the unclosed one stays literal.
        assert_eq!(
            image_refs("[IMG]https://a/1.png[/IMG] then [IMG]https://a/2.png"),
            vec![ImageRef::Url("https://a/1.png".into())]
        );

        // Source, not pictures.
        assert!(image_refs("[CODE][IMG]https://a.example/1.png[/IMG][/CODE]").is_empty());
        assert!(image_refs("[PLAIN][IMG]https://a.example/1.png[/IMG][/PLAIN]").is_empty());

        // A non-numeric attachment id is not an id.
        assert!(image_refs("[ATTACH]screenshot[/ATTACH]").is_empty());
    }

    #[test]
    fn media_youtube_resolves() {
        let chunks = render("[MEDIA=youtube]dQw4w9WgXcQ[/MEDIA]");
        match &chunks[0] {
            Chunk::Link(label, url, _) => {
                assert_eq!(label, "[video: YouTube]");
                assert_eq!(url, "https://www.youtube.com/watch?v=dQw4w9WgXcQ");
            }
            _ => panic!("expected youtube link"),
        }
    }

    #[test]
    fn table_and_heading_rendering() {
        let chunks = render("[HEADING=2]Section[/HEADING][TABLE][TR][TH]Header[/TH][/TR][TR][TD]Data[/TD][/TR][/TABLE]");
        let all = texts(&chunks).concat();
        assert!(all.contains("Section"));
        assert!(all.contains("Header |"));
        assert!(all.contains("Data |"));
    }

    #[test]
    fn html_entity_decoding() {
        let chunks = render("Tom &amp; Jerry said &quot;hello&#039;s &lt;world&gt;&quot;");
        let all = texts(&chunks).concat();
        assert_eq!(all, "Tom & Jerry said \"hello's <world>\"");
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
        assert_eq!(
            render(""),
            vec![Chunk::Text(String::new(), Style::default())]
        );
    }
}

