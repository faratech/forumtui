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

#[cfg(test)]
#[derive(Clone, Copy, Default, Debug)]
struct RenderWork {
    index_steps: usize,
    inline_visits: usize,
    indexed_queries: usize,
}

#[cfg(test)]
std::thread_local! {
    static RENDER_WORK: std::cell::Cell<RenderWork> = const {
        std::cell::Cell::new(RenderWork { index_steps: 0, inline_visits: 0, indexed_queries: 0 })
    };
}

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
    // Answered once per line instead of by scanning the remaining text at
    // every position: the contains/closer questions below are all "does X
    // occur at or after byte k", and re-scanning made alternating `*`/`~`
    // runs quadratic — on the UI thread, once per streamed frame (#45).
    let scan = LineScan::of(text);
    #[cfg(test)]
    let mut inline_visits = 0;
    let flush = |plain: &mut String, style: &Style, out: &mut Vec<Chunk>| {
        push_text(out, plain, style.clone());
        plain.clear();
    };
    while i < text.len() {
        #[cfg(test)]
        { inline_visits += 1; }
        let rest = &text[i..];
        let prev = text[..i].chars().next_back();

        if rest.starts_with('`') {
            let run = scan.backtick_end[i] - i;
            let close = scan.backtick_close[i];
            if close != usize::MAX {
                flush(&mut plain, &style, out);
                let body = &text[i + run..close];
                let body = body.strip_prefix(' ').and_then(|b| b.strip_suffix(' ')).unwrap_or(body);
                push_text(out, body, Style { code: true, ..style.clone() });
                i = close + run;
                continue;
            }
        }
        if rest.starts_with("**") || rest.starts_with("__") {
            let marker = &rest[..2];
            // `__` only at a word edge: snake_case_names are not emphasis.
            let at_edge = marker == "**"
                || (!prev.is_some_and(|c| c.is_alphanumeric())
                    || !rest[2..].chars().next().is_some_and(|c| c.is_alphanumeric()));
            if at_edge && (style.bold != base.bold || scan.marker_from(i + 2, marker)) {
                flush(&mut plain, &style, out);
                style.bold = !style.bold;
                i += 2;
                continue;
            }
        }
        if rest.starts_with("~~")
            && (style.strikethrough != base.strikethrough || scan.marker_from(i + 2, "~~"))
        {
            flush(&mut plain, &style, out);
            style.strikethrough = !style.strikethrough;
            i += 2;
            continue;
        }
        if bytes[i] == b'*' {
            let next = rest[1..].chars().next();
            let opens = !style.italic
                && next.is_some_and(|c| !c.is_whitespace() && c != '*')
                && scan.closer_from(i + 1);
            let closes = style.italic && prev.is_some_and(|c| !c.is_whitespace());
            if opens || closes {
                flush(&mut plain, &style, out);
                style.italic = !style.italic;
                i += 1;
                continue;
            }
        }
        if bytes[i] == b'['
            && let Some((label, url, used)) = scan.link(text, i)
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
            && scan.bracket_close[i] != usize::MAX
            && has_http_scheme(&text[i + 1..scan.bracket_close[i]])
        {
            flush(&mut plain, &style, out);
            let end = scan.bracket_close[i];
            let url = &text[i + 1..end];
            out.push(Chunk::Link(url.to_string(), url.to_string(), style.clone()));
            i = end + 1;
            continue;
        }
        let ch = rest.chars().next().expect("non-empty rest");
        plain.push(ch);
        i += ch.len_utf8();
    }
    flush(&mut plain, &style, out);
    #[cfg(test)]
    RENDER_WORK.with(|counter| {
        let mut work = counter.get();
        work.index_steps += scan.index_steps;
        work.inline_visits += inline_visits;
        work.indexed_queries += scan.indexed_queries.get();
        counter.set(work);
    });
}

/// The per-line suffix tables behind `inline`'s "does a closer/marker exist
/// at or after byte k" questions (#45). `closes_later` stays the readable
/// reference the closer table is tested against.
struct LineScan {
    closer_from: Vec<bool>,
    bold_from: Vec<bool>,
    underline_from: Vec<bool>,
    strike_from: Vec<bool>,
    /// Matching ] / ) by opener offset, independent of the other delimiter.
    bracket_close: Vec<usize>,
    /// First space at a parenthesis's own depth (a link's optional title).
    paren_space: Vec<usize>,
    backtick_end: Vec<usize>,
    backtick_close: Vec<usize>,
    #[cfg(test)]
    index_steps: usize,
    #[cfg(test)]
    indexed_queries: std::cell::Cell<usize>,
}

impl LineScan {
    fn of(text: &str) -> Self {
        let bytes = text.as_bytes();
        let n = bytes.len();
        let has_delimiters = bytes.iter().any(|b| matches!(b, b'[' | b'(' | b'<'));
        let has_ticks = bytes.contains(&b'`');
        let mut tables = LineScan {
            closer_from: vec![false; n + 1],
            bold_from: vec![false; n + 1],
            underline_from: vec![false; n + 1],
            strike_from: vec![false; n + 1],
            bracket_close: vec![usize::MAX; if has_delimiters { n + 1 } else { 0 }],
            paren_space: vec![usize::MAX; if has_delimiters { n + 1 } else { 0 }],
            backtick_end: vec![usize::MAX; if has_ticks { n + 1 } else { 0 }],
            backtick_close: vec![usize::MAX; if has_ticks { n + 1 } else { 0 }],
            #[cfg(test)]
            // Charge the full input for each bounded intrinsic flags scan;
            // count every variable parser/index loop below at its visits.
            index_steps: 2 * n,
            #[cfg(test)]
            indexed_queries: std::cell::Cell::new(0),
        };
        // Every opener is visited/popped once. Failed links used to scan
        // the suffix again at every '['; all-unclosed brackets were O(n²).
        let mut brackets = Vec::new();
        let mut parens = Vec::new();
        let mut angles = Vec::new();
        let mut ticks = Vec::new();
        let mut k = 0;
        while (has_delimiters || has_ticks) && k < n {
            #[cfg(test)]
            { tables.index_steps += 1; }
            match bytes[k] {
                b'[' => brackets.push(k),
                b']' => {
                    if let Some(open) = brackets.pop() {
                        tables.bracket_close[open] = k;
                    }
                }
                b'(' => parens.push(k),
                b'<' => angles.push(k),
                b'>' => {
                    // Angle autolinks end at the first '>', regardless of
                    // nested '<' bytes, just as the old suffix find did.
                    for open in angles.drain(..) {
                        tables.bracket_close[open] = k;
                        #[cfg(test)]
                        { tables.index_steps += 1; }
                    }
                }
                b')' => {
                    if let Some(open) = parens.pop() {
                        tables.bracket_close[open] = k;
                    }
                }
                b' ' => {
                    if let Some(&open) = parens.last()
                        && tables.paren_space[open] == usize::MAX
                    {
                        tables.paren_space[open] = k;
                    }
                }
                b'`' => {
                    let start = k;
                    while k < n && bytes[k] == b'`' {
                        #[cfg(test)]
                        { tables.index_steps += 1; }
                        k += 1;
                    }
                    ticks.push((start, k));
                    continue;
                }
                _ => {}
            }
            k += 1;
        }
        // The old code accepted the first future run AT LEAST as long as
        // the opener, even a suffix opener reached after a failed longer
        // run. Index every such query in two passes over the tick bytes.
        let longest = ticks.iter().map(|(a, b)| b - a).max().unwrap_or(0);
        #[cfg(test)]
        { tables.index_steps += ticks.len(); }
        let mut next_run = vec![usize::MAX; longest + 1];
        for &(start, end) in ticks.iter().rev() {
            for at in start..end {
                tables.backtick_end[at] = end;
                tables.backtick_close[at] = next_run[end - at];
            }
            next_run[1..=end - start].fill(start);
            #[cfg(test)]
            { tables.index_steps += 2 * (end - start); }
        }
        for (table, needle) in [
            (&mut tables.bold_from, &b"**"[..]),
            (&mut tables.underline_from, &b"__"[..]),
            (&mut tables.strike_from, &b"~~"[..]),
        ] {
            let mut seen = false;
            for k in (0..n).rev() {
                #[cfg(test)]
                { tables.index_steps += 1; }
                seen = seen || (k + 2 <= n && &bytes[k..k + 2] == needle);
                table[k] = seen;
            }
        }
        // An italic closer is a star the pair-skipping scan would actually
        // reach: the LAST star of an odd-length run — the scanner examines
        // even offsets only, so even-length runs hold none — and it counts
        // when the character before it is not whitespace (for the run's
        // first star; a deeper one is preceded by a star, which counts).
        let mut candidate = vec![false; n];
        let mut k = 0usize;
        while k < n {
            #[cfg(test)]
            { tables.index_steps += 1; }
            if bytes[k] == b'*' {
                let run_start = k;
                while k < n && bytes[k] == b'*' {
                    #[cfg(test)]
                    { tables.index_steps += 1; }
                    k += 1;
                }
                let run_len = k - run_start;
                if run_len % 2 == 1 {
                    let ok = run_len > 1
                        || match text[..run_start].chars().next_back() {
                            Some(prev) => !prev.is_whitespace(),
                            None => false,
                        };
                    if ok {
                        candidate[k - 1] = true;
                    }
                }
            } else {
                k += 1;
            }
        }
        let mut seen = false;
        for k in (0..n).rev() {
            #[cfg(test)]
            { tables.index_steps += 1; }
            seen = seen || candidate[k];
            tables.closer_from[k] = seen;
        }
        tables
    }

    fn marker_from(&self, k: usize, marker: &str) -> bool {
        #[cfg(test)]
        self.indexed_queries.set(self.indexed_queries.get() + 1);
        let k = k.min(self.bold_from.len() - 1);
        match marker {
            "**" => self.bold_from[k],
            "__" => self.underline_from[k],
            _ => self.strike_from[k],
        }
    }

    fn closer_from(&self, k: usize) -> bool {
        #[cfg(test)]
        self.indexed_queries.set(self.indexed_queries.get() + 1);
        self.closer_from[k.min(self.closer_from.len() - 1)]
    }

    fn link<'a>(&self, text: &'a str, at: usize) -> Option<(&'a str, &'a str, usize)> {
        #[cfg(test)]
        self.indexed_queries.set(self.indexed_queries.get() + 1);
        let close = self.bracket_close[at];
        if close == usize::MAX || text.as_bytes().get(close + 1) != Some(&b'(') {
            return None;
        }
        let open_paren = close + 1;
        let end = self.bracket_close[open_paren];
        if end == usize::MAX {
            return None;
        }
        let url_end = self.paren_space[open_paren].min(end);
        let target = text[open_paren + 1..url_end].trim().trim_start_matches('<').trim_end_matches('>');
        // is_http_url lowercases the entire target; rejected nested links
        // may overlap, so only inspect the scheme here (the same result).
        if !has_http_scheme(target) {
            return None;
        }
        Some((&text[at + 1..close], target, end + 1 - at))
    }
}

fn has_http_scheme(target: &str) -> bool {
    let scheme = target.trim_start();
    scheme.get(..7).is_some_and(|p| p.eq_ignore_ascii_case("http://"))
        || scheme.get(..8).is_some_and(|p| p.eq_ignore_ascii_case("https://"))
}

/// Whether a `*` in `rest` could close an italic run: one that follows a
/// non-space and is not part of `**`. The readable reference for what
/// [`LineScan`]'s table answers — production reads the table, and this
/// stays (test-only) as what the agreement test compares against, like the
/// editor's reference row model.
#[cfg(test)]
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
#[cfg(test)]
fn link_reference(rest: &str) -> Option<(&str, &str, usize)> {
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
    let opens = url.bytes().filter(|&b| b == b'(').count();
    let mut closes = url.bytes().filter(|&b| b == b')').count();
    loop {
        let trimmed = url.trim_end_matches(['.', ',', ';', ':', '!', '?', '*', '_', '\'']);
        let trimmed = if trimmed.ends_with(')') && opens < closes {
            closes -= 1;
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

    #[test]
    fn delimiter_indexes_agree_with_the_original_suffix_scans() {
        let alphabet: Vec<char> = "[]()`<>http:/x é".chars().collect();
        let mut seed = 0x53a8_u64;
        let mut sample = |n: usize| {
            (0..n).map(|_| {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                alphabet[(seed >> 32) as usize % alphabet.len()]
            }).collect::<String>()
        };
        let mut cases = vec![
            "[outer [x] title](https://example.test/a_(b) \"title\")".into(),
            "[a](< https://example.test>) [a](javascript:x)".into(),
            "x````before```middle``end`".into(),
            "[a](https://example.test/( nested (title )))".into(),
            "<http<http<https://example.test> <httpno-scheme>".into(),
        ];
        cases.extend((0..2000).map(|n| sample(1 + n % 80)));
        for text in cases {
            let scan = LineScan::of(&text);
            for (at, ch) in text.char_indices() {
                if ch == '[' {
                    assert_eq!(scan.link(&text, at), link_reference(&text[at..]), "link at {at} of {text:?}");
                } else if ch == '`' {
                    let rest = &text[at..];
                    let run = rest.bytes().take_while(|&c| c == b'`').count();
                    let close = rest[run..].find(&rest[..run]).map(|k| at + run + k);
                    assert_eq!(scan.backtick_end[at], at + run, "run at {at} of {text:?}");
                    assert_eq!(scan.backtick_close[at], close.unwrap_or(usize::MAX), "close at {at} of {text:?}");
                } else if ch == '<' {
                    assert_eq!(scan.bracket_close[at], text[at..].find('>').map_or(usize::MAX, |k| at + k), "angle at {at} of {text:?}");
                }
            }
        }
    }

    #[test]
    fn malformed_brackets_and_backticks_use_linear_index_work() {
        for n in [10_000, 40_000, 160_000] {
            for src in ["[".repeat(n), format!("x{}", "`".repeat(n)), "<http".repeat(n / 5)] {
                let scan = LineScan::of(&src);
                assert!(scan.index_steps <= 14 * src.len(), "{} full-index steps for {} bytes", scan.index_steps, src.len());
                assert_eq!(texts(&render(&src)), src, "unclosed input stays literal");
            }
            let suffix = ")".repeat(n);
            let chunks = render(&format!("https://example.test{suffix}"));
            assert_eq!(texts(&chunks), format!("<https://example.test|https://example.test>{suffix}"));
        }
    }

    /// #45: the precomputed tables must answer exactly what the readable
    /// reference scan answers, at every offset of every shape of line.
    #[test]
    fn the_line_scan_agrees_with_the_reference_closer_scan() {
        let alphabet = "*_~ab \n";
        let mut seed = 0xc0ffee_u64;
        let lcg = |s: &mut u64| {
            *s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (*s >> 33) as usize
        };
        for _ in 0..300 {
            let len = 1 + lcg(&mut seed) % 40;
            let chars: Vec<char> = alphabet.chars().collect();
            let line: String = (0..len)
                .map(|_| chars[lcg(&mut seed) % chars.len()])
                .collect();
            let scan = LineScan::of(&line);
            // Production only ever queries from just after an opener star,
            // whose next byte is not a star - i.e. from a byte outside any
            // star run, where the scan enters every run at its first star
            // and the global run analysis is exact. Query those points.
            let bytes = line.as_bytes();
            for k in 0..=line.len() {
                if k < line.len() && bytes[k] == b'*' {
                    continue;
                }
                assert_eq!(
                    scan.closer_from(k),
                    closes_later(&line[k..]),
                    "closer at {k} of {line:?}"
                );
                                        }
            let q = lcg(&mut seed) % (line.len() + 1);
            assert_eq!(
                scan.marker_from(q, "~~"),
                line[q.min(line.len())..].contains("~~"),
                "strike at {q} of {line:?}"
            );
            assert_eq!(
                scan.marker_from(q, "**"),
                line[q.min(line.len())..].contains("**"),
                "bold at {q} of {line:?}"
            );
            assert_eq!(
                scan.marker_from(q, "__"),
                line[q.min(line.len())..].contains("__"),
                "underline at {q} of {line:?}"
            );
        }
    }

    /// #45: a suffix scan per unmatched star was quadratic. Count the
    /// actual indexed render pipeline instead of timing scheduler slices.
    #[test]
    fn render_is_linear_in_emphasis_marker_count() {
        fn measured(src: &str) -> (Vec<Chunk>, RenderWork) {
            RENDER_WORK.with(|counter| counter.set(RenderWork::default()));
            let chunks = render(src);
            (chunks, RENDER_WORK.with(|counter| counter.get()))
        }

        let mut baseline = None;
        for scale in [1, 4, 16] {
            // Every space-preceded star asks whether a closer exists; none
            // does. A per-opener suffix scan must traverse the whole tail.
            let stars = 1000 * scale;
            let strikes = 300 * scale;
            let literal = " *a".repeat(stars);
            let src = literal.clone() + &" ~~c~~ ".repeat(strikes);
            let (chunks, work) = measured(&src);
            assert_eq!(texts(&chunks), literal + &" c ".repeat(strikes));
            let struck: String = chunks.iter().filter_map(|chunk| match chunk {
                Chunk::Text(text, style) if style.strikethrough => Some(text.as_str()),
                _ => None,
            }).collect();
            assert_eq!(struck, "c".repeat(strikes));
            assert!(chunks.iter().all(|chunk| match chunk {
                Chunk::Text(_, style) => !style.italic,
                _ => false,
            }), "unmatched stars stay literal, not italic");

            // Four suffix tables, one candidate pass plus its star runs,
            // and two bounded flags scans take at most eight byte visits.
            // Each emitted character/marker is visited once; each star and
            // opening strike pair performs exactly one indexed query.
            assert!(work.index_steps <= 8 * src.len(), "{work:?} for {} bytes", src.len());
            assert_eq!(work.inline_visits, 3 * stars + 5 * strikes);
            assert_eq!(work.indexed_queries, stars + strikes,
                "every opener must use the precomputed index");
            let total = work.index_steps + work.inline_visits + work.indexed_queries;
            assert!(total <= 10 * src.len(), "{work:?} for {} bytes", src.len());
            if let Some(first) = baseline {
                assert_eq!(total, first * scale, "operation count must scale with input");
            } else {
                baseline = Some(total);
            }
        }
    }
}
