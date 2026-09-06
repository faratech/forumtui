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
    Image {
        url: String,
        /// The anchor target when the image sits inside `[URL=…]` — XF
        /// renders `<a href=full><img src=thumb></a>`, so `o`/the marker
        /// opens the anchor, not the picture (#620).
        link: Option<String>,
        style: Style,
    },
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
            Chunk::Image { url, .. } if is_http_url(url) => Some(ImageRef::Url(url.clone())),
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
    /// Rebuild the style from per-frame-kind open counts. O(1) per call —
    /// the replacement for the O(depth) `from_stack` walk, which is what
    /// let the frame-depth cap go entirely (#622).
    fn from_counts(c: &FrameCounts) -> Style {
        Style {
            bold: c.bold > 0,
            italic: c.italic > 0,
            underline: c.underline > 0,
            strikethrough: c.strike > 0,
            code: c.code > 0,
            spoiler: c.spoiler > 0,
            quote_depth: c.quote.min(u8::MAX as u32) as u8,
            list_depth: c.list.min(u8::MAX as u32) as u8,
        }
    }
}

/// Per-frame-kind open counts — the incremental mirror of the frame stack.
/// Push/pop are O(1), so emission sites read `Style::from_counts` without
/// walking the stack, and nesting is limited by nothing but input size
/// (#622): XF declares `$maxDepth` but never enforces it, and real posts
/// nest 33 deep.
#[derive(Debug, Default, Clone, Copy)]
struct FrameCounts {
    bold: u32,
    italic: u32,
    underline: u32,
    strike: u32,
    spoiler: u32,
    code: u32,
    quote: u32,
    list: u32,
}

/// Apply (or, with `on = false`, reverse) `frame`'s style contribution.
fn apply_frame(frame: &Frame, c: &mut FrameCounts, on: bool) {
    macro_rules! bump {
        ($field:ident) => {{
            if on {
                c.$field = c.$field.saturating_add(1);
            } else {
                c.$field = c.$field.saturating_sub(1);
            }
        }};
    }
    match frame {
        Frame::Bold => bump!(bold),
        Frame::Italic => bump!(italic),
        Frame::Underline => bump!(underline),
        Frame::Strike => bump!(strike),
        Frame::Spoiler => bump!(spoiler),
        Frame::InlineCode => bump!(code),
        Frame::Quote { .. } => bump!(quote),
        Frame::List(_) => bump!(list),
        // A heading always carries bold; popping it drops the contribution.
        Frame::Heading(_) | Frame::TableHeader => bump!(bold),
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

/// Push a frame and keep the counters and the mirrored `Style` in step.
fn push_frame(stack: &mut Vec<Frame>, c: &mut FrameCounts, style: &mut Style, frame: Frame) {
    apply_frame(&frame, c, true);
    *style = Style::from_counts(c);
    stack.push(frame);
}

/// Pop the innermost frame matching the closing tag name; false if none.
/// Frames above the match are unclosed inner tags and stay, exactly as
/// before — only the matched frame's contribution is reversed (#622).
fn pop_matching(
    stack: &mut Vec<Frame>,
    c: &mut FrameCounts,
    style: &mut Style,
    links: &mut Vec<String>,
    name: &str,
) -> bool {
    match stack.iter().rposition(|f| is_closer(name, f)) {
        Some(pos) => {
            let frame = stack.remove(pos);
            if matches!(frame, Frame::Link(_)) {
                links.pop();
            }
            apply_frame(&frame, c, false);
            *style = Style::from_counts(c);
            true
        }
        None => false,
    }
}

#[allow(clippy::match_like_matches_macro)] // the old nested fn, moved as-is
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
        Chunk::Image { .. } | Chunk::Attach(..) => false,
    })
}

fn emit_list_marker(out: &mut Vec<Chunk>, stack: &mut [Frame], style: Style) {
    let marker = if let Some(Frame::List(kind)) =
        stack.iter_mut().rev().find(|f| matches!(f, Frame::List(_)))
    {
        kind.next_marker()
    } else {
        "• ".to_string()
    };
    let mut st = style;
    st.bold = true;
    out.push(Chunk::Text(marker, st));
}

pub fn render(src: &str) -> Vec<Chunk> {
    let mut out: Vec<Chunk> = Vec::new();
    let mut stack: Vec<Frame> = Vec::new();
    // Incremental style mirror of `stack` (#622): O(1) push/pop instead of
    // an O(depth) walk per emitted chunk, which is what let the frame-depth
    // cap go.
    let mut counts = FrameCounts::default();
    let mut style = Style::default();
    // The open [URL] href, innermost last — the O(1) sibling of the frame
    // stack, so emit_text doesn't walk the stack per text run (#622).
    let mut links: Vec<String> = Vec::new();
    // `(buffered label, href)` for the open [URL] frame (#620).
    let mut link_label: Option<(String, String, usize)> = None;
    // Text runs buffered for the open [URL] frame, flushed as ONE
    // Chunk::Link when it closes (#620): XF renders one anchor around the
    // whole body, and the old per-run chunks gave every run its own [n]
    // marker. `None` = no link frame open.
    let mut misses = CloseMisses::default();
    let mut brackets = CloseBracket::default();
    let mut rest = src;

    'outer: while !rest.is_empty() {
        let bracket = match rest.find('[') {
            Some(i) => i,
            None => {
                emit_text(&mut out, rest, style.clone(), &mut link_label);
                break;
            }
        };
        if bracket > 0 {
            emit_text(&mut out, &rest[..bracket], style.clone(), &mut link_label);
            rest = &rest[bracket..];
        }

        // Try to parse a tag at position 0.
        match parse_tag(src, rest, &mut brackets) {
            Some(TagEvent::Open { name, value, len }) => {
                let raw_tag = &rest[..len];
                rest = &rest[len..];
                let tag_lower = name.to_ascii_lowercase();
                match tag_lower.as_str() {
                    "b" => push_frame(&mut stack, &mut counts, &mut style, Frame::Bold),
                    "i" => push_frame(&mut stack, &mut counts, &mut style, Frame::Italic),
                    "u" => push_frame(&mut stack, &mut counts, &mut style, Frame::Underline),
                    "s" | "strike" => push_frame(&mut stack, &mut counts, &mut style, Frame::Strike),
                    "sub" | "sup" => push_frame(&mut stack, &mut counts, &mut style, Frame::Italic),
                    "highlight" => push_frame(&mut stack, &mut counts, &mut style, Frame::Bold),
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
                            let mut st = style.clone();
                            st.code = true;
                            flush_before_chunk(&mut out, &mut link_label, &style);
                            out.push(Chunk::Text(decode_html_entities(inner), st));
                            rest = &rest[close_len..];
                        } else {
                            push_frame(&mut stack, &mut counts, &mut style, Frame::InlineCode);
                        }
                    }
                    "code" | "php" | "html" => {
                        let (inner, close_len) = take_until_close(rest, &tag_lower);
                        let mut st = style.clone();
                        st.code = true;
                        if let Some(v) = &value {
                            let clean_v = strip_quotes(v);
                            if !clean_v.is_empty() {
                                let mut hst = style.clone();
                                hst.bold = true;
                                flush_before_chunk(&mut out, &mut link_label, &style);
                                out.push(Chunk::Text(format!("[{clean_v} code]\n"), hst));
                            }
                        }
                        let decoded = decode_html_entities(inner.trim_matches('\n'));
                        flush_link_label(&mut out, &mut link_label, &style);
                        out.push(Chunk::Text(decoded, st.clone()));
                        flush_link_label(&mut out, &mut link_label, &style);
                        out.push(Chunk::Text("\n".into(), st));
                        rest = &rest[close_len..];
                    }
                    "plain" => {
                        let (inner, close_len) = take_until_close(rest, "plain");
                        emit_text(&mut out, inner, style.clone(), &mut link_label);
                        rest = &rest[close_len..];
                    }
                    "quote" => {
                        let byline = value.filter(|v| !v.trim().is_empty());
                        if let Some(by) = &byline {
                            let (author, _) = parse_quote_byline(by);
                            let mut st = style.clone();
                            st.italic = true;
                            flush_before_chunk(&mut out, &mut link_label, &style);
                            out.push(Chunk::Text(format!("{author} wrote:\n"), st));
                        }
                        push_frame(&mut stack, &mut counts, &mut style, Frame::Quote { byline });
                    }
                    "list" => {
                        let kind = ListKind::from_attr(value.as_deref());
                        push_frame(&mut stack, &mut counts, &mut style, Frame::List(kind));
                    }
                    "spoiler" | "ispoiler" => {
                        if let Some(title) = &value {
                            let clean = strip_quotes(title);
                            if !clean.is_empty() {
                                let mut st = style.clone();
                                st.bold = true;
                                flush_before_chunk(&mut out, &mut link_label, &style);
                                out.push(Chunk::Text(format!("[Spoiler: {clean}]\n"), st));
                            }
                        }
                        push_frame(&mut stack, &mut counts, &mut style, Frame::Spoiler);
                    }
                    "color" => push_frame(&mut stack, &mut counts, &mut style, Frame::Color(value.unwrap_or_default())),
                    "size" => push_frame(&mut stack, &mut counts, &mut style, Frame::Size(value.unwrap_or_default())),
                    "font" => push_frame(&mut stack, &mut counts, &mut style, Frame::Font(value.unwrap_or_default())),
                    "left" | "center" | "right" | "justify" => {
                        stack.push(Frame::Align(tag_lower))
                    }
                    "indent" => push_frame(&mut stack, &mut counts, &mut style, Frame::Align("indent".into())),
                    "heading" => {
                        let level = value
                            .as_deref()
                            .and_then(|v| strip_quotes(v).parse::<u8>().ok())
                            .unwrap_or(1);
                        if !out.is_empty() && !ends_with_newline(&out) {
                            flush_before_chunk(&mut out, &mut link_label, &style);
                            out.push(Chunk::Text("\n".into(), style.clone()));
                        }
                        push_frame(&mut stack, &mut counts, &mut style, Frame::Heading(level));
                    }
                    "h1" => {
                        if !out.is_empty() && !ends_with_newline(&out) {
                            flush_before_chunk(&mut out, &mut link_label, &style);
                            out.push(Chunk::Text("\n".into(), style.clone()));
                        }
                        push_frame(&mut stack, &mut counts, &mut style, Frame::Heading(1));
                    }
                    "h2" => {
                        if !out.is_empty() && !ends_with_newline(&out) {
                            flush_before_chunk(&mut out, &mut link_label, &style);
                            out.push(Chunk::Text("\n".into(), style.clone()));
                        }
                        push_frame(&mut stack, &mut counts, &mut style, Frame::Heading(2));
                    }
                    "h3" => {
                        if !out.is_empty() && !ends_with_newline(&out) {
                            flush_before_chunk(&mut out, &mut link_label, &style);
                            out.push(Chunk::Text("\n".into(), style.clone()));
                        }
                        push_frame(&mut stack, &mut counts, &mut style, Frame::Heading(3));
                    }
                    "hr" => {
                        if !out.is_empty() && !ends_with_newline(&out) {
                            flush_before_chunk(&mut out, &mut link_label, &style);
                            out.push(Chunk::Text("\n".into(), style.clone()));
                        }
                        flush_link_label(&mut out, &mut link_label, &style);
                        out.push(Chunk::Text("───\n".into(), style.clone()));
                    }
                    "table" => {
                        if !out.is_empty() && !ends_with_newline(&out) {
                            flush_before_chunk(&mut out, &mut link_label, &style);
                            out.push(Chunk::Text("\n".into(), style.clone()));
                        }
                        push_frame(&mut stack, &mut counts, &mut style, Frame::Table);
                    }
                    "tr" => {
                        if !out.is_empty() && !ends_with_newline(&out) {
                            flush_before_chunk(&mut out, &mut link_label, &style);
                            out.push(Chunk::Text("\n".into(), style.clone()));
                        }
                        push_frame(&mut stack, &mut counts, &mut style, Frame::TableRow);
                    }
                    "th" => push_frame(&mut stack, &mut counts, &mut style, Frame::TableHeader),
                    "td" => push_frame(&mut stack, &mut counts, &mut style, Frame::TableCell),
                    "url" => match value.filter(|v| !v.trim().is_empty()) {
                        // XF stores entities in tag values exactly as in text
                        // (`&amp;` inside an href), so the target decodes like
                        // everything else (#650) — a literal `&amp;` in the
                        // href was a wrong link.
                        Some(href) => {
                            flush_link_label(&mut out, &mut link_label, &style);
                            let href = decode_html_entities(strip_quotes(&href));
                            flush_link_label(&mut out, &mut link_label, &style);
                            flush_link_label(&mut out, &mut link_label, &style);
                        link_label = Some((String::new(), href.clone(), out.len()));
                            links.push(href.clone());
                            push_frame(&mut stack, &mut counts, &mut style, Frame::Link(href));
                        }
                        None => {
                            flush_link_label(&mut out, &mut link_label, &style);
                            flush_link_label(&mut out, &mut link_label, &style);
                        link_label = Some((String::new(), String::new(), out.len()));
                            links.push(String::new());
                            push_frame(&mut stack, &mut counts, &mut style, Frame::Link(String::new()));
                        }
                    },
                    "email" => match value.filter(|v| !v.trim().is_empty()) {
                        Some(target) => {
                            let clean = decode_html_entities(strip_quotes(&target));
                            let href = if clean.starts_with("mailto:") {
                                clean.to_string()
                            } else {
                                format!("mailto:{clean}")
                            };
                            flush_link_label(&mut out, &mut link_label, &style);
                            flush_link_label(&mut out, &mut link_label, &style);
                        link_label = Some((String::new(), href.clone(), out.len()));
                            links.push(href.clone());
                            push_frame(&mut stack, &mut counts, &mut style, Frame::Link(href));
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
                                let st = style.clone();
                                let addr = decode_html_entities(inner.trim());
                                flush_before_chunk(&mut out, &mut link_label, &style);
                                out.push(Chunk::Link(addr.clone(), format!("mailto:{addr}"), st));
                                rest = &rest[close_len..];
                            } else {
                                links.push(String::new());
                                push_frame(&mut stack, &mut counts, &mut style, Frame::Link(String::new()));
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
                        flush_link_label(&mut out, &mut link_label, &style);
                        link_label = Some((String::new(), url.clone(), out.len()));
                        links.push(url.clone());
                        push_frame(&mut stack, &mut counts, &mut style, Frame::Link(url));
                    }
                    "thread" => {
                        let id = value.as_deref().map(strip_quotes).unwrap_or("");
                        let url = if !id.is_empty() {
                            format!("https://windowsforum.com/threads/{id}/")
                        } else {
                            String::new()
                        };
                        flush_link_label(&mut out, &mut link_label, &style);
                        link_label = Some((String::new(), url.clone(), out.len()));
                        links.push(url.clone());
                        push_frame(&mut stack, &mut counts, &mut style, Frame::Link(url));
                    }
                    "img" => {
                        if let Some((inner, close_len)) = misses.split(src, rest, "img") {
                            let st = style.clone();
                            // Entity-decoded like the [URL=] target (#650): a
                            // literal `&amp;` in the src was a dead image.
                            let trimmed = decode_html_entities(strip_quotes(inner.trim()));
                            if trimmed.is_empty() {
                                flush_before_chunk(&mut out, &mut link_label, &style);
                                out.push(Chunk::Text("[image]".into(), st));
                            } else {
                                let link = links.last().filter(|h| **h != trimmed).cloned();
                                flush_before_chunk(&mut out, &mut link_label, &style);
                                out.push(Chunk::Image { url: trimmed, link, style: st });
                            }
                            rest = &rest[close_len..];
                        } else if let Some(v) = &value {
                            let st = style.clone();
                            let url = decode_html_entities(strip_quotes(v));
                            let link = links.last().filter(|h| **h != url).cloned();
                            flush_before_chunk(&mut out, &mut link_label, &style);
                            out.push(Chunk::Image { url, link, style: st });
                        } else {
                            emit_text(&mut out, raw_tag, style.clone(), &mut link_label);
                        }
                    }
                    "media" => {
                        if let Some((inner, close_len)) = misses.split(src, rest, "media") {
                            let st = style.clone();
                            let site = value.as_deref().map(strip_quotes).unwrap_or("media");
                            let (label, url) = resolve_media(site, inner);
                            flush_before_chunk(&mut out, &mut link_label, &style);
                            out.push(Chunk::Link(label, url, st));
                            rest = &rest[close_len..];
                        } else {
                            emit_text(&mut out, raw_tag, style.clone(), &mut link_label);
                        }
                    }
                    "attach" => {
                        if let Some((inner, close_len)) = misses.split(src, rest, "attach") {
                            let st = style.clone();
                            let id = if inner.trim().is_empty() {
                                value.as_deref().map(strip_quotes).unwrap_or("").trim()
                            } else {
                                inner.trim()
                            };
                            flush_before_chunk(&mut out, &mut link_label, &style);
                            out.push(Chunk::Attach(id.to_string(), st));
                            rest = &rest[close_len..];
                        } else if let Some(v) = &value {
                            let st = style.clone();
                            let clean = strip_quotes(v);
                            flush_before_chunk(&mut out, &mut link_label, &style);
                            out.push(Chunk::Attach(clean.to_string(), st));
                        } else {
                            emit_text(&mut out, raw_tag, style.clone(), &mut link_label);
                        }
                    }
                    "user" => {
                        if let Some((inner, close_len)) = misses.split(src, rest, "user") {
                            let st = style.clone();
                            let name = inner.trim();
                            let name = name.strip_prefix('@').unwrap_or(name);
                            flush_before_chunk(&mut out, &mut link_label, &style);
                            out.push(Chunk::Text(format!("@{name}"), st));
                            rest = &rest[close_len..];
                        } else if let Some(v) = &value {
                            let st = style.clone();
                            let clean = strip_quotes(v);
                            let clean = clean.strip_prefix('@').unwrap_or(clean);
                            flush_before_chunk(&mut out, &mut link_label, &style);
                            out.push(Chunk::Text(format!("@{clean}"), st));
                        } else {
                            emit_text(&mut out, raw_tag, style.clone(), &mut link_label);
                        }
                    }
                    "*" => {
                        emit_list_marker(&mut out, &mut stack, style.clone());
                    }
                    _ => {
                        // Unknown tag: literal passthrough, no frame pushed.
                        emit_text(&mut out, raw_tag, style.clone(), &mut link_label);
                    }
                }
            }
            Some(TagEvent::Close { name, len }) => {
                let raw_tag = &rest[..len];
                rest = &rest[len..];
                let tag_lower = name.to_ascii_lowercase();
                match tag_lower.as_str() {
                    "heading" | "h1" | "h2" | "h3" => {
                        if pop_matching(&mut stack, &mut counts, &mut style, &mut links, &tag_lower)
                            && !out.is_empty()
                            && !ends_with_newline(&out)
                        {
                            flush_before_chunk(&mut out, &mut link_label, &style);
                            out.push(Chunk::Text("\n".into(), style.clone()));
                        }
                    }
                    "td" | "th" => {
                        if pop_matching(&mut stack, &mut counts, &mut style, &mut links, &tag_lower) {
                            flush_before_chunk(&mut out, &mut link_label, &style);
                            out.push(Chunk::Text(" | ".into(), style.clone()));
                        }
                    }
                    "tr" => {
                        if pop_matching(&mut stack, &mut counts, &mut style, &mut links, "tr")
                            && !out.is_empty()
                            && !ends_with_newline(&out)
                        {
                            flush_before_chunk(&mut out, &mut link_label, &style);
                            out.push(Chunk::Text("\n".into(), style.clone()));
                        }
                    }
                    "table" => {
                        if pop_matching(&mut stack, &mut counts, &mut style, &mut links, "table")
                            && !out.is_empty()
                            && !ends_with_newline(&out)
                        {
                            flush_before_chunk(&mut out, &mut link_label, &style);
                            out.push(Chunk::Text("\n".into(), style.clone()));
                        }
                    }
                    // XF's hr is self-contained: the news template writes
                    // `[HR][/HR]` around every rule, and the close tag fell
                    // through to the unopened-closer arm and was emitted
                    // verbatim as a stray `[/HR]` line under the rule (#610).
                    "hr" => {}
                    _ => {
                        let links_before = links.len();
                        if pop_matching(&mut stack, &mut counts, &mut style, &mut links, &tag_lower) {
                            // A [URL] frame just closed: flush its buffered
                            // label as the one Link chunk (#620).
                            if links.len() < links_before {
                                flush_link_label(&mut out, &mut link_label, &style);
                            }
                        } else {
                            // Closing tag for an unknown/unopened tag: keep it visible.
                            emit_text(&mut out, raw_tag, style.clone(), &mut link_label);
                        }
                    }
                }
            }
            Some(TagEvent::Star(len)) => {
                rest = &rest[len..];
                emit_list_marker(&mut out, &mut stack, style.clone());
            }
            None => {
                // Stray '[': literal.
                emit_text(&mut out, &rest[..1], style.clone(), &mut link_label);
                rest = &rest[1..];
                continue 'outer;
            }
        }
    }

    // An unclosed [URL=x] still owes its buffered label (#620).
    flush_link_label(&mut out, &mut link_label, &style);
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
            Chunk::Image { url, .. } => {
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
fn tag_close(src: &str, s: &str, brackets: &mut CloseBracket) -> Option<usize> {
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
            // The tag name ends here. A `[` can never be part of the name
            // part that follows (only `=value` and ` attr` forms are legal),
            // and parse_tag's name check rejects any inner holding one — so
            // the bracket is literal no matter where a later `]` sits.
            // Refusing without looking for it is what keeps `[x[x[x…`
            // linear: the scan below otherwise walks from every bracket to
            // the next `]` (or end of input), quadratic on the UI thread
            // (the #607 class).
            if b == b'[' {
                return None;
            }
            if b.is_ascii_whitespace() {
                // Attribute form (`[NAME attr="v" …]`): the tag ends at the
                // first `]` OUTSIDE a quoted value — XF's attribute regex
                // allows `]` inside `"`/`'` quotes, and the news bot writes
                // alt text containing `[ICODE]…[/ICODE]`. The plain first-`]`
                // find ended such tags inside the alt text (#615). The memo
                // pre-check keeps bracket-dense posts with no `]` at all
                // from paying the scan per bracket.
                if brackets.find(src, s).is_some()
                    && let Some(pos) = scan_attr_close(bytes, i, QUOTED_VALUE_SCAN)
                {
                    return Some(pos);
                }
            }
            break;
        }
        i += 1;
    }
    // The memo speaks in src-absolute offsets (so a hit survives the render
    // loop's forward motion); tag_close's contract is relative to `s`.
    brackets
        .find(src, s)
        .map(|abs| abs - (src.len() - s.len()))
}

/// First `]` at or after `from` that sits outside a quoted attribute value,
/// the walk capped like the value-form scan (#615). A quote that never
/// closes exhausts the cap and the caller falls back to the plain find.
fn scan_attr_close(bytes: &[u8], from: usize, cap: usize) -> Option<usize> {
    let limit = bytes.len().min(from + cap);
    let mut quote: Option<u8> = None;
    let mut i = from;
    while i < limit {
        let b = bytes[i];
        match quote {
            // Byte scanning is safe: quotes and `]` are ASCII and never
            // appear inside a multi-byte UTF-8 sequence.
            Some(q) if b == q => quote = None,
            Some(_) => {}
            None if b == b'"' || b == b'\'' => quote = Some(b),
            None if b == b']' => return Some(i),
            None => {}
        }
        i += 1;
    }
    None
}

/// Per-render memo of the last `first-']'-at-or-after` scan.
///
/// `tag_close` ends in `find(']')` for every `[` whose tag does not parse
/// outright (`[url="` with the closing `']` truncated off a paste: the quote
/// scan caps at 4096 bytes and then still needs the fallback find). Each
/// such find walks to the next `]` or end of input, and the render loop
/// probes once per bracket, so a bracket-dense post re-paid that walk per
/// bracket — quadratic on the UI thread, the class the #607 close-tag miss
/// memo closed for `[/name]` scans. Offsets only ever move forward within
/// one render, so a single `(queried, found)` pair answers every later
/// query whose cached `]` still lies at or after it.
#[derive(Default)]
struct CloseBracket {
    /// Offset the scan was asked from, and the `]` it found (absolute in
    /// `src`; `None` = none exists at or after `at`).
    entry: Option<(usize, Option<usize>)>,
}

impl CloseBracket {
    /// First `]` in `rest` (a suffix of `src`), absolute in `src`, memoized.
    fn find(&mut self, src: &str, rest: &str) -> Option<usize> {
        let offset = src.len() - rest.len();
        if let Some((at, found)) = self.entry
            && offset >= at
            && found.is_none_or(|pos| pos >= offset)
        {
            return found;
        }
        let found = rest.find(']').map(|rel| offset + rel);
        self.entry = Some((offset, found));
        found
    }
}

fn parse_tag(src: &str, s: &str, brackets: &mut CloseBracket) -> Option<TagEvent> {
    debug_assert!(s.starts_with('['));
    let close = tag_close(src, s, brackets)?;
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
            // A space before the `=` makes this the attribute form, which
            // only opens when a real `key=` option follows (#616):
            // `[QUOTE = Trouble; 235284]` is literal text.
            if !has_tag_option(&inner[space_pos..]) {
                return None;
            }
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
        //
        // XF only accepts this attribute form when at least one `key=`
        // option follows the space (Parser.php: `if ($_options && $endChar
        // == ']') openTag … else pushText`) — `[i removed it]` with no
        // option is literal text, not an italic frame that swallows the
        // rest of the post (#616).
        if !has_tag_option(val) {
            return None;
        }
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

/// Does this attribute-form remainder carry at least one `key=` option?
/// XF's parser only opens the tag when it does (#616): the option key is a
/// run of word characters immediately before the `=`.
fn has_tag_option(words: &str) -> bool {
    let b = words.as_bytes();
    for (i, &c) in b.iter().enumerate() {
        if c == b'='
            && i > 0
            && (b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'_')
        {
            return true;
        }
    }
    false
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
/// A non-text chunk (image/attach/icode/…) is about to be emitted inside a
/// `[URL]` frame (#663): flush any buffered label first so source order
/// holds, and otherwise mark the body non-empty so the close cannot add a
/// phantom href-as-label link on top of the chunk's own anchor.
fn flush_before_chunk(
    out: &mut Vec<Chunk>,
    pending: &mut Option<(String, String, usize)>,
    style: &Style,
) {
    match pending {
        Some((label, _, _)) if label.is_empty() => {
            pending.take();
        }
        Some(_) => flush_link_label(out, pending, style),
        None => {}
    }
}

/// Flush the buffered `[URL]` label as one `Chunk::Link` (#620). An empty
/// body means the href is the label (XF: `<a href="x"></a>` renders the
/// URL as the anchor text).
fn flush_link_label(
    out: &mut Vec<Chunk>,
    pending: &mut Option<(String, String, usize)>,
    style: &Style,
) {
    if let Some((label, href, out_at)) = pending.take() {
        // The href-as-label fallback applies only when the body produced NO
        // chunks at all (`[url=x][/url]`) — a body of non-text chunks (an
        // [IMG] inside the anchor) already carries its own anchor via
        // `Chunk::Image::link`, and a second self-link here was a phantom
        // marker plus a printed URL under every wrapped thumbnail (#663).
        let body_empty = out.len() == out_at && label.is_empty();
        if body_empty && href.is_empty() {
            return;
        }
        let (label, target) = if href.is_empty() {
            (label.clone(), label)
        } else if body_empty {
            (href.clone(), href)
        } else {
            (label, href)
        };
        out.push(Chunk::Link(label, target, style.clone()));
    }
}

/// Emit plain text: inside a `[URL]` frame the run is buffered into the
/// anchor's label — flushed as ONE `Chunk::Link` when the frame closes
/// (#620), because XF renders one anchor around the whole body and per-run
/// chunks gave every run its own [n] marker; bare URLs inside the anchor
/// stop being auto-linked, a nested anchor not being a thing in HTML
/// either. Outside a frame, bare http(s) URLs are auto-linked.
fn emit_text(
    out: &mut Vec<Chunk>,
    text: &str,
    style: Style,
    pending: &mut Option<(String, String, usize)>,
) {
    // Inside a [URL] frame everything is the anchor's label.
    if let Some((label, _, _)) = pending {
        label.push_str(&decode_html_entities(text));
        return;
    }
    if text.is_empty() {
        return;
    }
    let decoded_text = decode_html_entities(text);
    let mut rest = decoded_text.as_str();
    while let Some(pos) = find_url(rest) {
        let (before, url, after) = split_url(rest, pos);
        if !before.is_empty() {
            out.push(Chunk::Text(before.to_string(), style.clone()));
        }
        out.push(Chunk::Link(url.to_string(), url.to_string(), style.clone()));
        rest = after;
    }
    if !rest.is_empty() {
        out.push(Chunk::Text(rest.to_string(), style));
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

    /// A bracket-dense post whose tags never parse. Three shapes, each once
    /// quadratic on the UI thread (the #607 class, with `]` instead of
    /// `[/name]`):
    /// - `[x[x[x…` made the name scan stop at the next `[` and the fallback
    ///   `find(']')` walk to the document's last `]` for every bracket;
    /// - `[u p` repeated walks to end-of-input through the same fallback;
    /// - `[url="paste` repeated (a cut-off paste) walks through the
    ///   unterminated-quote fallback, capped at 4096 bytes per bracket by
    ///   the #602 hardening.
    ///
    /// The first two must be flat-out linear; the third keeps its flat
    /// per-bracket cap but must not also grow with the document.
    #[test]
    fn render_is_linear_in_bracket_dense_posts() {
        fn parse_ms(src: &str) -> f64 {
            let t = std::time::Instant::now();
            let out = render(src);
            assert!(!out.is_empty());
            t.elapsed().as_secs_f64() * 1000.0
        }

        // 80 KB: every `[x` fails to parse, one `]` at the very end.
        let dense = format!("{}]", "[x".repeat(40000));
        let td = parse_ms(&dense);
        assert!(td < 200.0, "40 000 `[x` and a late `]` took {td:.1} ms");

        // ~80 KB: whitespace after the name, no `]` anywhere.
        let spaced = "[u p".repeat(20000);
        let ts = parse_ms(&spaced);
        assert!(ts < 200.0, "20 000 `[u p` with no `]` took {ts:.1} ms");

        // ~24 KB: unterminated quoted values, no `]` anywhere.
        let truncated = "[url=\"paste".repeat(2000);
        let tt = parse_ms(&truncated);
        assert!(tt < 200.0, "2 000 truncated `[url=\"` took {tt:.1} ms");

        // …and the growth is linear, not quadratic.
        let dense_half = parse_ms(&format!("{}]", "[x".repeat(20000)));
        let spaced_half = parse_ms(&"[u p".repeat(10000));
        assert!(
            td < 8.0 * dense_half.max(1.0) && ts < 8.0 * spaced_half.max(1.0),
            "2x the input cost more than 8x the time (dense {dense_half:.1} -> {td:.1} ms, \
             spaced {spaced_half:.1} -> {ts:.1} ms): quadratic"
        );
    }

    /// The bracket fast-fail is a speed-up only: the soup itself stays
    /// visible verbatim, and a real tag after it still renders.
    #[test]
    fn bracket_soup_stays_literal_and_real_tags_still_render() {
        let src = "[x[x[x]hello [b]bold[/b]";
        let text = texts(&render(src)).concat();
        assert!(text.contains("[x[x[x]hello"), "{text:?}");
        assert!(text.contains("bold"), "{text:?}");
        assert!(!text.contains("[b]"), "{text:?}");
    }

    /// XF stores entities in tag values exactly as in text, and the target
    /// of a link or image must decode like the label does (#650): a literal
    /// `&amp;` in the href was a wrong link, in an [IMG] src a dead image.
    #[test]
    fn url_and_img_targets_are_entity_decoded() {
        let url = render("[url=\"https://x.test/?a=1&amp;b=2\"]link[/url]");
        match &url[0] {
            Chunk::Link(label, href, _) => {
                assert_eq!(label.as_str(), "link");
                assert_eq!(href, "https://x.test/?a=1&b=2", "the href must decode");
            }
            other => panic!("expected a link, got {other:?}"),
        }
        let img = render("[IMG]https://x.test/a&amp;b.png[/IMG]");
        let Chunk::Image { url, .. } = &img[0] else {
            panic!("expected an image, got {:?}", img[0])
        };
        assert_eq!(url, "https://x.test/a&b.png");
    }

    /// #616: XF only opens the attribute form when a `key=` option follows
    /// the space — `[i removed it]` and `[QUOTE = Trouble; 235284]` are
    /// literal text, not frames that swallow the rest of the post.
    #[test]
    fn attribute_form_without_an_option_is_literal_text() {
        let chunks = render("before [i removed it] after");
        let after = chunks
            .iter()
            .find(|c| matches!(c, Chunk::Text(t, _) if t.contains("after")))
            .unwrap();
        if let Chunk::Text(_, s) = after {
            assert!(!s.italic, "no option, no italic frame: {s:?}");
            assert!(!s.bold, "no option, no stray frame: {s:?}");
        }
        let all = texts(&chunks).concat();
        assert!(all.contains("[i removed it]"), "still visible verbatim: {all:?}");

        // `[QUOTE = Trouble; 235284]` (space before `=`, no `key=`): literal.
        let chunks = render("[QUOTE = Trouble; 235284] body");
        let all = texts(&chunks).concat();
        assert!(all.contains("[QUOTE = Trouble; 235284]"), "{all:?}");
        assert!(!chunks.iter().any(|c| matches!(c, Chunk::Text(_, s) if s.quote_depth > 0)));
    }

    /// #615: attribute-form values may contain `]` inside quotes (news alt
    /// text carries [ICODE]…[/ICODE]); the tag ends at the first `]`
    /// outside quotes, so the attachment id is the body, not the alt tail.
    #[test]
    fn attribute_form_close_skips_quoted_values() {
        let src = "[ATTACH alt=\"specs [ICODE]cmd[/ICODE] stats.\"]150883[/ATTACH]";
        match &render(src)[0] {
            Chunk::Attach(id, _) => assert_eq!(id, "150883", "src: {src}"),
            other => panic!("expected the attachment id, got {other:?}"),
        }
    }

    /// …and the attribute form with a real option still opens (#539's
    /// unfurl case, and `[ATTACH type=…]`).
    #[test]
    fn attribute_form_with_an_option_still_opens() {
        let chunks = render("[ATTACH type=\"full\"]1[/ATTACH]");
        assert!(matches!(&chunks[0], Chunk::Attach(id, _) if id == "1"));
        // Per #539, the attribute form has no tag *value*: the href falls
        // back to the link text itself.
        let chunks = render("[url unfurl=\"true\"]site[/url]");
        assert!(matches!(&chunks[0], Chunk::Link(l, h, _) if l == "site" && h == "site"), "{:?}", chunks[0]);
    }

    /// #620: a [URL] frame is ONE anchor around its whole body. An empty
    /// body renders the URL as its own label; runs inside the anchor merge
    /// into a single link (one [n] marker in the thread view), and an [IMG]
    /// inside the anchor carries the href for `o`/the marker while still
    /// painting from its own src.
    #[test]
    fn url_frame_wraps_its_whole_body() {
        let chunks = render("[url=https://example.com/x][/url] tail");
        assert!(
            matches!(&chunks[0], Chunk::Link(l, h, _)
                if l == "https://example.com/x" && h == "https://example.com/x"),
            "an empty body makes the URL its own label: {:?}",
            chunks[0]
        );

        let chunks = render("[URL=https://example.com][I]Jaws[/I] swims[/URL]");
        let links = chunks.iter().filter(|c| matches!(c, Chunk::Link(..))).count();
        assert_eq!(links, 1, "one anchor, not one per run: {chunks:?}");
        if let Chunk::Link(label, _, _) = &chunks[0] {
            assert_eq!(label, "Jaws swims");
        }

        let chunks = render("[URL=https://example.com/full][IMG]https://example.com/thumb[/IMG][/URL]");
        match &chunks[0] {
            Chunk::Image { url, link, .. } => {
                assert_eq!(url.as_str(), "https://example.com/thumb");
                assert_eq!(link.as_deref(), Some("https://example.com/full"));
            }
            other => panic!("expected the image, got {other:?}"),
        }
    }

    /// #610: the news template writes `[HR][/HR]` before every heading; the
    /// close tag fell through to the unopened-closer arm and rendered as a
    /// literal `[/HR]` line under every rule. XF's hr is self-contained —
    /// the close is consumed silently.
    #[test]
    fn hr_close_tag_is_consumed_not_rendered() {
        let text = texts(&render("[HR][/HR]\nText after")).concat();
        assert!(!text.contains("[/HR]"), "{text:?}");
        assert!(text.contains("───"), "{text:?}");
        assert!(text.contains("Text after"), "{text:?}");
        // Search snippets go through `to_plain` and must not carry it either
        // (the rule text itself survives, space-joined by to_plain).
        assert_eq!(to_plain("[HR][/HR]rule"), "─── rule");
    }

    /// Uncapped nesting must not change what a normally-nested post renders
    /// as, and a real depth-33 post renders like XF's (#622).
    #[test]
    fn uncapped_nesting_leaves_closed_tags_alone() {
        let src = "[QUOTE=\"a\"][B]bold [I]both[/I][/B] plain[/QUOTE]";
        let all = texts(&render(src)).concat();
        assert!(all.contains("a wrote:"), "{all:?}");
        assert!(all.contains("bold"), "{all:?}");
        assert!(all.contains("both"), "{all:?}");
        assert!(all.contains("plain"), "{all:?}");
        // #622: deep nesting renders like XF (which declares $maxDepth but
        // never enforces it) — no depth at which an open tag degrades to
        // literal text, and the closes still match.
        let deep = "[B]".repeat(40) + "x";
        let deep_text = texts(&render(&deep)).concat();
        assert!(!deep_text.contains("[B]"), "{deep_text:?}");
        assert_eq!(deep_text, "x", "{deep_text:?}");
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
            Chunk::Image { url, .. } => assert_eq!(url, "https://example.com/pic.png"),
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

