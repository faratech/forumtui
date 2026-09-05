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
    let mut misses = CloseMisses::default();
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
                // XF's parser caps nesting at 20 frames (Parser.php
                // `$maxDepth`); past that an open tag is just text. Without a
                // cap an unclosed `[B]` per 3 bytes kept the frame stack (and
                // so every `Style::from_stack` walk) growing for the whole
                // post — 312 ms for 80 KB, re-paid on every redraw of the
                // thread view (issue #607).
                if stack.len() >= MAX_FRAME_DEPTH {
                    emit_text(&mut out, raw_tag, &stack);
                    continue;
                }
                let tag_lower = name.to_ascii_lowercase();
                match tag_lower.as_str() {
                    "b" => stack.push(Frame::Bold),
                    "i" => stack.push(Frame::Italic),
                    "u" => stack.push(Frame::Underline),
                    "s" | "strike" => stack.push(Frame::Strike),
                    "sub" | "sup" => stack.push(Frame::Italic),
                    "highlight" => stack.push(Frame::Bold),
                    "icode" | "inlinecode" => {
                        // XF's `icode` rule is `['plain' => true]` — children
                        // are literal, like [CODE]/[PHP]/[HTML]. Pushing a
                        // stack frame instead let the render loop keep
                        // scanning `[` inside the body, so a bracket in the
                        // content (e.g. `con2fb_map[i]`) opened a REAL `[i]`
                        // frame that leaked past `[/ICODE]` and a `[url]`
                        // opened a dangling link that swallowed the rest of
                        // the post (issue #540). Verbatim when a close tag
                        // exists; fall back to the old frame behaviour only
                        // when there is none, so an unclosed `[ICODE]` still
                        // degrades the way it always has rather than
                        // swallowing the rest of the document like [CODE]'s
                        // unclosed fallback does.
                        if let Some((inner, close_len)) = misses.split(src, rest, &tag_lower) {
                            let mut st = Style::from_stack(&stack);
                            st.code = true;
                            out.push(Chunk::Text(decode_html_entities(inner), st));
                            rest = &rest[close_len..];
                        } else {
                            stack.push(Frame::InlineCode);
                        }
                    }
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
                        None => {
                            // XF's no-value form, `[email]addr[/email]`: the
                            // address is the tag BODY, not an attribute.
                            // Build the `mailto:` link the same way the
                            // value form does instead of leaving an empty
                            // href, which `emit_text` falls back to filling
                            // with the raw address as both label and target
                            // (issue #603).
                            if let Some((inner, close_len)) = misses.split(src, rest, "email") {
                                let st = Style::from_stack(&stack);
                                let addr = decode_html_entities(inner.trim());
                                out.push(Chunk::Link(addr.clone(), format!("mailto:{addr}"), st));
                                rest = &rest[close_len..];
                            } else {
                                stack.push(Frame::Link(String::new()));
                            }
                        }
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
                        if let Some((inner, close_len)) = misses.split(src, rest, "img") {
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
                        if let Some((inner, close_len)) = misses.split(src, rest, "media") {
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
                        if let Some((inner, close_len)) = misses.split(src, rest, "attach") {
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
                        if let Some((inner, close_len)) = misses.split(src, rest, "user") {
                            let st = Style::from_stack(&stack);
                            let name = inner.trim();
                            let name = name.strip_prefix('@').unwrap_or(name);
                            out.push(Chunk::Text(format!("@{name}"), st));
                            rest = &rest[close_len..];
                        } else if let Some(v) = &value {
                            let st = Style::from_stack(&stack);
                            let clean = strip_quotes(v);
                            let clean = clean.strip_prefix('@').unwrap_or(clean);
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

/// XF's `Parser::$maxDepth`. Deeper open tags render as literal text.
const MAX_FRAME_DEPTH: usize = 20;

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

/// Byte offset of the `]` that ends the tag starting at `s[0]`.
///
/// XF's rule for the value form is `[NAME="value"]` where the delimiter is
/// `"]` (Parser.php: `strpos($text, "$delim]", $startPos)`), so a `]` INSIDE a
/// quoted value is part of the value — `[url="…/computer[theverge.com]("]` and
/// `[QUOTE="[hun]tobias88, post: 1, member: 2"]` are both legal and render
/// correctly on the site. Ending the tag at the first `]` split them mid-value:
/// the href kept its opening quote (so `o` opened a site-relative URL) and the
/// rest of the value leaked into the body (issue #602).
///
/// Only the value form is special-cased; the attribute form (`[NAME attr="v"]`,
/// a space before the `=`), the close form and the bare form keep the first
/// `]` exactly as before. The scan for `"]` is capped so a hostile unterminated
/// quote cannot make the parse quadratic; past the cap it falls back to the
/// first `]`, which is what this function always used to return.
fn tag_close(s: &str) -> Option<usize> {
    /// Longest quoted option value we will look through for the closing `"]`.
    const QUOTED_VALUE_SCAN: usize = 4096;
    let bytes = s.as_bytes();
    let mut i = 1;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'=' {
            let Some(&delim) = bytes.get(i + 1) else { break };
            if delim != b'"' && delim != b'\'' {
                break;
            }
            let start = i + 2;
            let limit = bytes.len().min(start + QUOTED_VALUE_SCAN);
            // Byte scanning is safe: `"`, `'` and `]` are ASCII and never
            // appear inside a multi-byte UTF-8 sequence.
            if let Some(pos) = bytes
                .get(start..limit)
                .and_then(|hay| hay.windows(2).position(|w| w[0] == delim && w[1] == b']'))
            {
                return Some(start + pos + 1);
            }
            break;
        }
        if !(b.is_ascii_alphanumeric() || b == b'_') {
            break;
        }
        i += 1;
    }
    s.find(']')
}

fn parse_tag(s: &str) -> Option<TagEvent> {
    debug_assert!(s.starts_with('['));
    let close = tag_close(s)?;
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
    //
    // A space before the `=` means this is the ATTRIBUTE form —
    // `[NAME attr="v" ...]`, e.g. XF's auto-unfurl `[url unfurl="true"]` or
    // `[ATTACH type="full"]` — where `attr`'s value is NOT the tag's own
    // value. Returning it as `value` made every `[url unfurl="true"]…[/url]`
    // (XF's stored form for ~1,600 posts) resolve to the literal href
    // "true" instead of falling back to the link text (issue #539). `[NAME=
    // value]` (no whitespace before `=`) is the real value form and is
    // unaffected.
    let (name, value) = if let Some(eq_pos) = inner.find('=') {
        let before_eq = &inner[..eq_pos];
        if let Some(space_pos) = before_eq.find(char::is_whitespace) {
            let name_part = before_eq[..space_pos].trim();
            (name_part, None)
        } else {
            let name_part = before_eq.trim();
            let val = inner[eq_pos + 1..].trim();
            (name_part, Some(strip_quotes(val).to_string()))
        }
    } else if let Some((name_part, val)) = inner.split_once(char::is_whitespace) {
        // `split_once` locates the match with the Pattern API (char-boundary
        // aware) rather than a raw byte offset + `+ 1`, so a multi-byte
        // whitespace character (U+00A0 NBSP, U+3000 IDEOGRAPHIC SPACE, ...)
        // can't land the slice mid-codepoint and panic (issue #533).
        (name_part.trim(), Some(strip_quotes(val.trim()).to_string()))
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

/// Byte offset of the next `[/name]`, matched ASCII-case-insensitively in a
/// single forward pass.
///
/// It used to lowercase the *whole remaining document* per call (and the
/// callers below lowercased it a second time just to test for the close
/// tag), which made a post's parse cost quadratic in its tag count: 16 000
/// `[IMG]` tags took 425 ms, and every one of those parses happens on the UI
/// thread (issue #522). Scanning bytes is safe here because `[`, `/`, `]`
/// and ASCII letters never appear inside a multi-byte UTF-8 sequence.
fn find_close_tag(s: &str, name: &str) -> Option<usize> {
    let hay = s.as_bytes();
    let needle = name.as_bytes();
    let total = needle.len() + 3; // "[/" + name + "]"
    if hay.len() < total {
        return None;
    }
    let last = hay.len() - total;
    let mut i = 0usize;
    while i <= last {
        if hay[i] == b'['
            && hay[i + 1] == b'/'
            && hay[i + 2 + needle.len()] == b']'
            && hay[i + 2..i + 2 + needle.len()].eq_ignore_ascii_case(needle)
        {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// `(content, bytes to consume)` for a tag that has a close tag; `None` when
/// it has none — the callers that used to ask `lower.contains("[/img]")`
/// first now ask this once and reuse the answer.
fn split_at_close<'a>(s: &'a str, name: &str) -> Option<(&'a str, usize)> {
    find_close_tag(s, name).map(|pos| (&s[..pos], pos + name.len() + 3))
}

/// Per-render memo of close-tag MISSES, one entry per tag name.
///
/// `find_close_tag` walks to end-of-input when there is no `[/name]`, and an
/// unclosed `[IMG]`/`[MEDIA]`/`[ATTACH]`/`[USER]`/`[EMAIL]`/`[ICODE]` does not
/// consume anything, so `"[IMG]".repeat(n)` used to cost Σ(n−k) byte steps —
/// 573 ms for a 100 KB post, re-paid on every resize and every `n`/`N`/`gg`/`G`
/// because the parse runs on the UI thread (issue #607). A miss at offset `o`
/// proves there is no `[/name]` at or after `o`, so every later query for that
/// name (offsets only ever move forward within one render) is answered from
/// here without a scan.
#[derive(Default)]
struct CloseMisses {
    /// `(tag name, lowest offset from which no close tag exists)`.
    entries: Vec<(String, usize)>,
}

impl CloseMisses {
    /// `split_at_close(rest, name)` with the miss memo in front of it. `rest`
    /// must be a suffix of `src` — it always is: `render` only ever advances
    /// it forward inside the source it was handed.
    fn split<'a>(&mut self, src: &str, rest: &'a str, name: &str) -> Option<(&'a str, usize)> {
        let offset = src.len() - rest.len();
        if let Some((_, from)) = self.entries.iter().find(|(n, _)| n == name)
            && offset >= *from
        {
            return None;
        }
        let found = split_at_close(rest, name);
        if found.is_none() {
            match self.entries.iter_mut().find(|(n, _)| n == name) {
                Some((_, from)) => *from = (*from).min(offset),
                None => self.entries.push((name.to_string(), offset)),
            }
        }
        found
    }
}

/// Find the closing tag for `name` (case-insensitive) returning content end
/// offset and the closing tag length.
fn take_until_close<'a>(s: &'a str, name: &str) -> (&'a str, usize) {
    split_at_close(s, name).unwrap_or((s, s.len()))
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

/// Offset of the next bare `http://` / `https://`, in one forward pass.
///
/// The lowercase-the-whole-remainder version this replaces was called once
/// per URL from `emit_text`, so a post that is mostly links cost O(n^2) —
/// 4 000 links took 75 ms, on the UI thread, per frame for a search page
/// (issue #522). Each scan here covers only the text up to the URL it
/// returns, and `emit_text` advances past it, so the whole run is linear.
fn find_url(s: &str) -> Option<usize> {
    fn starts_ci(hay: &[u8], needle: &[u8]) -> bool {
        hay.len() >= needle.len() && hay[..needle.len()].eq_ignore_ascii_case(needle)
    }
    let b = s.as_bytes();
    for i in 0..b.len() {
        if (b[i] | 0x20) == b'h'
            && (starts_ci(&b[i..], b"http://") || starts_ci(&b[i..], b"https://"))
        {
            return Some(i);
        }
    }
    None
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
    /// Issue #522: `render` must stay linear in the number of tags/URLs.
    /// Both scans used to lowercase the whole remaining input per tag/URL,
    /// which is quadratic — and every one of these parses runs on the UI
    /// thread (a search page re-parsed twenty messages twenty times a
    /// second). The bounds are deliberately loose: this is a complexity
    /// assertion, not a benchmark. Measured unoptimized on the dev box:
    /// 1.0 s before / 4.6 ms after for the 4 000-URL input.
    #[test]
    fn render_is_linear_in_tag_and_url_count() {
        fn parse_ms(src: &str) -> f64 {
            let t = std::time::Instant::now();
            let out = render(src);
            assert!(!out.is_empty());
            t.elapsed().as_secs_f64() * 1000.0
        }

        let urls_4k = "see https://example.com/a here ".repeat(4000);
        let urls_16k = "see https://example.com/a here ".repeat(16000);
        let t4 = parse_ms(&urls_4k);
        assert!(t4 < 400.0, "4 000 URLs took {t4:.1} ms — the URL scan is not linear");
        let t16 = parse_ms(&urls_16k);
        assert!(
            t16 < 8.0 * t4.max(1.0),
            "4x the input cost {:.1}x the time ({t4:.1} ms -> {t16:.1} ms): quadratic",
            t16 / t4.max(0.001)
        );

        // Same for close-tag search, which has its own scan.
        let imgs = "[IMG]https://example.com/a.png[/IMG] x ".repeat(4000);
        let ti = parse_ms(&imgs);
        assert!(ti < 400.0, "4 000 [IMG] tags took {ti:.1} ms");
    }

    /// Issue #607: the same contract for UNCLOSED tags, which #522's fix did
    /// not cover. `[IMG]` with no `[/IMG]` consumes nothing and rescans the
    /// whole remainder (573 ms for 100 KB before the miss memo), and an
    /// unclosed `[B]` left a frame on the stack forever so every
    /// `Style::from_stack` walk grew with the post (312 ms for 80 KB before
    /// the depth cap; the cap bounds that walk at 20 frames, so the style
    /// itself needs no memo). Both re-run on every resize and every n/N/gg/G.
    /// Measured unoptimized on the dev box: 31 ms and 36 ms respectively.
    #[test]
    fn render_is_linear_in_unclosed_tags() {
        fn parse_ms(src: &str) -> f64 {
            let t = std::time::Instant::now();
            let out = render(src);
            assert!(!out.is_empty());
            t.elapsed().as_secs_f64() * 1000.0
        }

        let imgs = "[IMG]".repeat(20000); // 100 KB, accepted by XF verbatim
        let ti = parse_ms(&imgs);
        assert!(ti < 200.0, "20 000 unclosed [IMG] took {ti:.1} ms");

        let bolds = "[B]x".repeat(20000); // 80 KB
        let tb = parse_ms(&bolds);
        assert!(tb < 200.0, "20 000 unclosed [B] took {tb:.1} ms");

        // …and the growth is linear, not quadratic.
        let half = parse_ms(&"[IMG]".repeat(10000));
        assert!(
            ti < 8.0 * half.max(1.0),
            "2x the input cost {:.1}x the time ({half:.1} ms -> {ti:.1} ms): quadratic",
            ti / half.max(0.001)
        );
    }

    /// The depth cap must not change what a normally-nested post renders as.
    #[test]
    fn the_frame_depth_cap_leaves_closed_tags_alone() {
        let src = "[QUOTE=\"a\"][B]bold [I]both[/I][/B] plain[/QUOTE]";
        let all = texts(&render(src)).concat();
        assert!(all.contains("a wrote:"), "{all:?}");
        assert!(all.contains("bold"), "{all:?}");
        assert!(all.contains("both"), "{all:?}");
        assert!(all.contains("plain"), "{all:?}");
        // Past the cap an open tag is literal text, exactly like XF.
        let deep = "[B]".repeat(MAX_FRAME_DEPTH + 2) + "x";
        let deep_text = texts(&render(&deep)).concat();
        assert!(deep_text.contains("[B]"), "{deep_text:?}");
        assert!(deep_text.ends_with('x'), "{deep_text:?}");
    }

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

    /// Issue #603: XF's no-value form (`[email]addr[/email]`, 656 live
    /// posts) must build a `mailto:` href, same as `[EMAIL=addr]addr[/EMAIL]`
    /// — not leave an empty href that falls back to the raw address as both
    /// label and target.
    #[test]
    fn email_no_value_form_gets_a_mailto_href() {
        let chunks = render("[email]a@b.com[/email]");
        match &chunks[0] {
            Chunk::Link(label, url, _) => {
                assert_eq!(label, "a@b.com");
                assert_eq!(url, "mailto:a@b.com");
            }
            other => panic!("expected a link chunk, got {other:?}"),
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

    /// Issue #539: XF stores every pasted bare URL as
    /// `[url unfurl="true"]https://…[/url]` (~1,600 live posts). The space
    /// before `=` makes this the ATTRIBUTE form, not `[url=value]` — the
    /// link's href must fall back to its text, not become the literal
    /// string "true".
    #[test]
    fn url_with_unfurl_attribute_uses_the_link_text_as_href() {
        let chunks = render(r#"[url unfurl="true"]https://www.axios.com/x[/url]"#);
        match &chunks[0] {
            Chunk::Link(label, url, _) => {
                assert_eq!(label, "https://www.axios.com/x");
                assert_eq!(url, "https://www.axios.com/x");
            }
            other => panic!("expected link, got {other:?}"),
        }
        assert_eq!(
            to_plain(r#"see [url unfurl="true"]https://www.axios.com/x[/url] now"#),
            "see https://www.axios.com/x now"
        );
    }

    /// The real value form (`[URL='...']`, no space before `=`) must keep
    /// working exactly as before.
    #[test]
    fn url_with_quoted_value_form_is_unaffected() {
        let chunks = render("[URL='https://example.com']label[/URL]");
        match &chunks[0] {
            Chunk::Link(label, url, _) => {
                assert_eq!(label, "label");
                assert_eq!(url, "https://example.com");
            }
            other => panic!("expected link, got {other:?}"),
        }
    }

    /// Issue #602: XF ends a quoted option value at `"]`, so a `]` inside the
    /// quotes belongs to the value. Live post 954836 stores exactly this URL;
    /// the old first-`]` split kept the opening quote in the href (which made
    /// `o` open a site-relative URL) and pushed the rest into the label.
    #[test]
    fn quoted_value_may_contain_a_closing_bracket() {
        let chunks = render(
            "[url=\"https://nexphone.com/blog/the-tale-of-nexphone-one-phone-every-computer[theverge.com](\"]nexphone.com[/url]",
        );
        match &chunks[0] {
            Chunk::Link(label, url, _) => {
                assert_eq!(label, "nexphone.com");
                assert_eq!(
                    url,
                    "https://nexphone.com/blog/the-tale-of-nexphone-one-phone-every-computer[theverge.com]("
                );
            }
            other => panic!("expected link, got {other:?}"),
        }
        assert!(
            !texts(&chunks).concat().contains('"'),
            "no part of the quoted value may leak into the body"
        );
    }

    /// The same rule for a quote byline: four live usernames contain `]`.
    #[test]
    fn quoted_byline_may_contain_a_closing_bracket() {
        let chunks = render(r#"[QUOTE="[hun]tobias88, post: 1, member: 2"]hi[/QUOTE]"#);
        let all = texts(&chunks).concat();
        assert!(all.contains("[hun]tobias88 wrote:"), "{all:?}");
        assert!(all.contains("hi"), "{all:?}");
        assert!(!all.contains("member: 2"), "the byline must not leak: {all:?}");
    }

    /// An unterminated quote must still parse the way it always did — the
    /// first `]` ends the tag — rather than swallowing the rest of the post.
    #[test]
    fn an_unterminated_quoted_value_falls_back_to_the_first_bracket() {
        let chunks = render("[URL=\"https://example.com]click[/URL] after");
        let all = texts(&chunks).concat();
        assert!(all.contains("after"), "{all:?}");
    }

    /// `[ATTACH type="full"]` is also the attribute form — must still resolve
    /// the attachment id from the tag's inner content, not from `type`'s
    /// value ("full").
    #[test]
    fn attach_type_attribute_does_not_leak_into_the_attachment_id() {
        let chunks = render(r#"[ATTACH type="full" alt="x"]1[/ATTACH]"#);
        match &chunks[0] {
            Chunk::Attach(id, _) => assert_eq!(id, "1"),
            other => panic!("expected an attachment chunk, got {other:?}"),
        }
    }

    /// Issue #540: `[ICODE]` content used to be parsed for nested tags
    /// instead of being verbatim (XF's `icode` rule is `plain => true`, like
    /// `[CODE]`). A bracket in the body must not open a real tag that leaks
    /// past `[/ICODE]`.
    #[test]
    fn icode_body_is_verbatim_and_does_not_leak_style_past_the_close() {
        let chunks = render("[ICODE]con2fb_map[i][/ICODE] rest");
        let code = chunks
            .iter()
            .find(|c| matches!(c, Chunk::Text(t, _) if t.contains("con2fb_map")))
            .expect("code chunk");
        match code {
            Chunk::Text(t, s) => {
                assert_eq!(t, "con2fb_map[i]", "the literal [i] must survive, not open Italic");
                assert!(s.code);
            }
            other => panic!("expected text, got {other:?}"),
        }
        let after = chunks
            .iter()
            .find(|c| matches!(c, Chunk::Text(t, _) if t.contains("rest")))
            .expect("trailing text");
        if let Chunk::Text(_, s) = after {
            assert!(!s.italic, "italic leaked past [/ICODE]");
        }
    }

    /// A `[url]` opened inside `[ICODE]` must not push a real `Frame::Link`
    /// that swallows every following text run as a numbered self-link.
    #[test]
    fn icode_body_does_not_open_a_dangling_link_frame() {
        let chunks = render("[ICODE][url][/ICODE] see docs");
        let after = chunks
            .iter()
            .find(|c| matches!(c, Chunk::Text(t, _) if t.contains("see docs")));
        assert!(
            matches!(after, Some(Chunk::Text(_, _))),
            "expected a plain Text chunk after ICODE, got {chunks:?}"
        );
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

    #[test]
    fn user_tag_xf_at_form_is_not_doubled() {
        // XF's MentionFormatter (userMentionKeepAt=1) stores the '@' inside the
        // tag body: [USER=142771]@Pittzey[/USER]. The client must not prefix a
        // second '@' on top of it.
        let chunks = render("[USER=142771]@Pittzey[/USER]");
        assert_eq!(texts(&chunks).concat(), "@Pittzey");
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

    /// Issue #533: a multi-byte Unicode space inside an unrecognized `[...]`
    /// used to panic in `parse_tag`'s no-`=` branch (`&inner[space_pos + 1..]`
    /// assumed a one-byte space and sliced mid-codepoint). NBSP (U+00A0) is
    /// common in text pasted from the web/Word. The tag is unrecognized, so
    /// the whole bracket is passed through literally.
    #[test]
    fn multibyte_whitespace_in_a_tag_does_not_panic() {
        let nbsp = texts(&render("[Note\u{a0}here] x")).concat();
        assert!(nbsp.contains("[Note\u{a0}here]"), "got: {nbsp:?}");

        let ideographic = texts(&render("[x\u{3000}y] x")).concat();
        assert!(ideographic.contains("[x\u{3000}y]"), "got: {ideographic:?}");

        let thin = texts(&render("[a\u{2009}b] x")).concat();
        assert!(thin.contains("[a\u{2009}b]"), "got: {thin:?}");
    }

    /// Sweep every `char::is_whitespace` code point (not just the three
    /// spot-checked above) through the same no-`=` branch and assert `render`
    /// never panics, regardless of the space's UTF-8 width.
    #[test]
    fn every_whitespace_code_point_is_char_boundary_safe() {
        for cp in 0u32..0x11_0000 {
            let Some(c) = char::from_u32(cp) else { continue };
            if !c.is_whitespace() {
                continue;
            }
            let src = format!("[a{c}b] x");
            let _ = render(&src); // must not panic
        }
    }
}

