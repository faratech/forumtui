//! Ask the AI — WindowsForum's assistant (`/pages/ai/` on the site), reached
//! through the TuiLink relay `POST /api/wf-tui-ai`.
//!
//! The relay authenticates the bearer and forwards to the site's `chat.php`
//! over loopback, then passes chat.php's own Server-Sent Events through
//! untouched. So the frames here are exactly the ones the site's web client
//! reads (`chatpage/static/js`), and [`SseParser`] maps them the way it does:
//! the answer arrives as `response.output_text.delta`, progress as
//! `response.output_item.added` / `response.<tool>_call.<phase>`, citations
//! as `url_citation` annotations, and the turn is over only at
//! `chat.stream.completed` (or `[DONE]`). A stream that ends before that is a
//! truncated answer, not a finished one.
//!
//! The fixture `testdata/ask_ai_stream.sse` is one whole captured turn.

use std::time::Duration;

use serde::Deserialize;

/// The web client's ceiling on one streamed answer; past it the stream is
/// abandoned rather than buffered without bound.
pub const MAX_STREAM_BYTES: usize = 2 * 1024 * 1024;

/// chat.php's `MAX_MESSAGE_LENGTH` (bytes, not characters).
pub const MAX_QUESTION_BYTES: usize = 4096;

/// chat.php keeps at most this many history items when it has to rebuild a
/// conversation (`CHATPAGE_HISTORY_SEED_LIMIT`), each capped at
/// `CHATPAGE_MAX_HISTORY_ITEM_CHARS`.
pub const MAX_HISTORY_ITEMS: usize = 20;
pub const MAX_HISTORY_ITEM_BYTES: usize = 4000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
}

impl Role {
    fn as_str(self) -> &'static str {
        match self {
            Role::User => "user",
            Role::Assistant => "assistant",
        }
    }
}

/// One question to the assistant.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AskRequest {
    pub message: String,
    /// The conversation this turn belongs to (`[A-Za-z0-9_-]{1,128}`).
    /// chat.php maps it to the provider conversation that carries context
    /// from turn to turn.
    pub conversation_id: String,
    /// Opaque per-turn id (`X-WF-Turn-Id`); chat.php uses it to replay a
    /// turn that already completed instead of paying for it twice.
    pub turn_id: String,
    /// Start the conversation over (the web client's "new chat").
    pub reset: bool,
    /// Sent only in answer to `history_required`, which is chat.php saying it
    /// lost a conversation this client still remembers.
    pub history: Vec<(Role, String)>,
    /// True when earlier turns exist, so chat.php can ask for `history`
    /// instead of silently answering with no context.
    pub has_local_history: bool,
}

impl AskRequest {
    /// The relay's form body. Form-encoded like every other TuiLink call, so
    /// XF's own input filter reads it.
    pub fn form(&self) -> Vec<(String, String)> {
        let mut form = vec![
            ("message".to_string(), self.message.clone()),
            ("conversation_id".to_string(), self.conversation_id.clone()),
        ];
        if self.reset {
            form.push(("reset".into(), "1".into()));
        }
        let skip = self.history.len().saturating_sub(MAX_HISTORY_ITEMS);
        for (i, (role, content)) in self.history.iter().skip(skip).enumerate() {
            form.push((format!("history[{i}][role]"), role.as_str().into()));
            form.push((
                format!("history[{i}][content]"),
                truncate_bytes(content, MAX_HISTORY_ITEM_BYTES).to_string(),
            ));
        }
        if self.history.is_empty() && self.has_local_history {
            form.push(("has_local_history".into(), "1".into()));
        }
        form
    }
}

/// The longest prefix of `s` within `max` bytes that ends on a char boundary.
pub fn truncate_bytes(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// A fresh id in chat.php's alphabet (`[A-Za-z0-9_-]`), for a conversation or
/// a turn. Not a secret — the conversation is bound to the signed-in member
/// server-side — only unique.
pub fn new_id(prefix: &str) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let mut bytes = [0u8; 22];
    crate::oauth::fill_from_os(&mut bytes);
    // `% 62` leans very slightly toward the first letters; an id only has
    // to be unique, not uniform.
    let tail: String = bytes
        .iter()
        .map(|b| ALPHABET[*b as usize % ALPHABET.len()] as char)
        .collect();
    format!("{prefix}{tail}")
}

/// What one stream says, in order.
#[derive(Debug, Clone, PartialEq)]
pub enum AiEvent {
    /// More answer text (Markdown), to append.
    Delta(String),
    /// A step the assistant is taking ("Searching WindowsForum"). `id` pairs
    /// the start with its `done`.
    Progress { id: String, label: String, done: bool },
    /// A source the answer cited.
    Citation { url: String, title: String },
    /// The turn finished normally.
    Completed,
    /// The turn failed. Any text already delivered is kept by the caller: a
    /// partial answer is still worth reading.
    Failed {
        code: String,
        message: String,
        retryable: bool,
        retry_after: Option<Duration>,
    },
}

/// Incremental parser for the relay's `text/event-stream`: feed it the body
/// as it arrives, in any chunking, and it returns the events each chunk
/// completed. Frames split on a blank line; comments (`: keepalive`) are
/// skipped.
#[derive(Debug, Default)]
pub struct SseParser {
    buf: Vec<u8>,
    /// How much of `buf` is known to hold no frame boundary, so a frame that
    /// arrives a byte at a time is not rescanned from its start each time.
    scanned: usize,
    received: usize,
    /// Terminal event seen (completed or failed): anything after is ignored.
    done: bool,
    /// Whether any answer text arrived, for the `*.done` events that carry
    /// the whole text only as a fallback.
    has_text: bool,
}

impl SseParser {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_done(&self) -> bool {
        self.done
    }

    pub fn push(&mut self, chunk: &[u8]) -> Vec<AiEvent> {
        let mut out = Vec::new();
        if self.done {
            return out;
        }
        self.received += chunk.len();
        if self.received > MAX_STREAM_BYTES {
            self.done = true;
            out.push(failed(
                "stream_too_large",
                "The answer exceeded the safe size limit.",
                false,
            ));
            return out;
        }
        self.buf.extend_from_slice(chunk);
        while let Some((end, sep)) = frame_end(&self.buf, self.scanned) {
            let frame: Vec<u8> = self.buf.drain(..end + sep).take(end).collect();
            self.scanned = 0;
            self.frame(&String::from_utf8_lossy(&frame), &mut out);
            if self.done {
                self.buf.clear();
                break;
            }
        }
        // A separator is at most 3 bytes, so its start may sit just before
        // the end of what has been seen so far.
        self.scanned = self.buf.len().saturating_sub(2);
        out
    }

    /// The body ended. A final frame without its blank line still counts;
    /// ending before a terminal event is a truncated answer.
    pub fn finish(&mut self) -> Vec<AiEvent> {
        let mut out = Vec::new();
        if !self.done && !self.buf.is_empty() {
            let rest = std::mem::take(&mut self.buf);
            self.frame(&String::from_utf8_lossy(&rest), &mut out);
        }
        if !self.done {
            self.done = true;
            out.push(if self.has_text {
                failed("stream_truncated", "The answer was cut off before it finished.", true)
            } else {
                failed("stream_no_terminal", "No answer came back from the assistant.", true)
            });
        }
        out
    }

    fn frame(&mut self, frame: &str, out: &mut Vec<AiEvent>) {
        let data: Vec<&str> = frame
            .lines()
            .filter_map(|l| l.strip_prefix("data:"))
            .map(|l| l.strip_prefix(' ').unwrap_or(l))
            .collect();
        if data.is_empty() {
            return;
        }
        let data = data.join("\n");
        let data = data.trim();
        if data.is_empty() {
            return;
        }
        if data == "[DONE]" {
            self.done = true;
            out.push(AiEvent::Completed);
            return;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(data) else {
            self.done = true;
            out.push(failed("malformed_stream", "The assistant sent a malformed reply.", true));
            return;
        };
        self.event(&v, out);
    }

    fn event(&mut self, v: &serde_json::Value, out: &mut Vec<AiEvent>) {
        let ty = str_at(v, "type");
        let text_of = |key: &str| v.get(key).and_then(|x| x.as_str()).unwrap_or_default().to_string();
        match ty {
            "error" | "function_call_error" => {
                let message = [str_at(v, "detail"), error_message(v), str_at(v, "message")]
                    .into_iter()
                    .find(|s| !s.is_empty())
                    .unwrap_or("The assistant ran into an error.")
                    .to_string();
                let status = v.get("status_code").and_then(|x| x.as_u64());
                let code = match str_at(v, "code") {
                    "" => ty,
                    c => c,
                };
                let retryable = v
                    .get("retryable")
                    .and_then(|x| x.as_bool())
                    .unwrap_or_else(|| status.is_none_or(|s| s == 408 || s == 429 || s >= 500));
                self.done = true;
                out.push(AiEvent::Failed {
                    code: code.to_string(),
                    message,
                    retryable,
                    retry_after: v
                        .get("retry_after")
                        .and_then(|x| x.as_f64())
                        .filter(|s| s.is_finite() && *s > 0.0)
                        .map(|s| Duration::from_secs_f64(s.min(3600.0))),
                });
            }
            "response.failed" => {
                let message = v
                    .pointer("/response/error/message")
                    .and_then(|x| x.as_str())
                    .unwrap_or("The assistant could not finish this answer.");
                self.done = true;
                out.push(failed("response_failed", message, true));
            }
            "response.incomplete" => {
                let reason = v
                    .pointer("/response/incomplete_details/reason")
                    .and_then(|x| x.as_str())
                    .unwrap_or("The answer was incomplete.");
                self.done = true;
                out.push(failed("response_incomplete", reason, true));
            }
            "response.output_text.delta" | "response.refusal.delta" => {
                let delta = text_of("delta");
                if !delta.is_empty() {
                    self.has_text = true;
                    out.push(AiEvent::Delta(delta));
                }
            }
            "response.output_text.done" | "response.refusal.done" => {
                let key = if ty == "response.output_text.done" { "text" } else { "refusal" };
                let text = text_of(key);
                if !self.has_text && !text.is_empty() {
                    self.has_text = true;
                    out.push(AiEvent::Delta(text));
                }
            }
            "response.output_text.annotation.added" => {
                if let Some(c) = v.get("annotation").and_then(citation) {
                    out.push(c);
                }
            }
            "response.content_part.done" => {
                if let Some(list) = v.pointer("/part/annotations").and_then(|x| x.as_array()) {
                    out.extend(list.iter().filter_map(citation));
                }
            }
            "chat.stream.completed" => {
                self.done = true;
                out.push(AiEvent::Completed);
            }
            "response.output_item.added" => {
                let item = v.get("item");
                let name = item.map(|i| str_at(i, "name")).unwrap_or_default();
                let kind = item.map(|i| str_at(i, "type")).unwrap_or_default();
                if let Some(label) = progress_label(name).or_else(|| progress_label(kind)) {
                    let id = match item.map(|i| str_at(i, "id")).unwrap_or_default() {
                        "" => format!("item_{}", v.get("output_index").and_then(|x| x.as_u64()).unwrap_or(0)),
                        id => id.to_string(),
                    };
                    out.push(AiEvent::Progress { id, label: label.into(), done: false });
                }
            }
            "response.output_item.done" => {
                let item = v.get("item");
                if item.map(|i| str_at(i, "type")) == Some("function_call") {
                    return;
                }
                if let Some(id) = item.map(|i| str_at(i, "id")).filter(|id| !id.is_empty()) {
                    out.push(AiEvent::Progress { id: id.into(), label: String::new(), done: true });
                }
            }
            _ => {
                // `response.<tool>_call.<in_progress|searching|completed>`
                let Some(rest) = ty.strip_prefix("response.") else { return };
                let Some((call, phase)) = rest.rsplit_once('.') else { return };
                if !call.ends_with("_call") || !matches!(phase, "in_progress" | "searching" | "completed") {
                    return;
                }
                let Some(label) = progress_label(call) else { return };
                let id = match str_at(v, "item_id") {
                    "" => format!("call_{call}"),
                    id => id.to_string(),
                };
                let label = if call == "web_search_call" && phase == "searching" {
                    "Searching the web"
                } else {
                    label
                };
                out.push(AiEvent::Progress { id, label: label.into(), done: phase == "completed" });
            }
        }
    }
}

fn failed(code: &str, message: &str, retryable: bool) -> AiEvent {
    AiEvent::Failed {
        code: code.into(),
        message: message.into(),
        retryable,
        retry_after: None,
    }
}

/// The first blank line in `buf` at or after `from`: (frame length,
/// separator length).
fn frame_end(buf: &[u8], from: usize) -> Option<(usize, usize)> {
    let mut i = from;
    while i < buf.len() {
        if buf[i] == b'\n' {
            if buf.get(i + 1) == Some(&b'\n') {
                return Some((i, 2));
            }
            if buf.get(i + 1) == Some(&b'\r') && buf.get(i + 2) == Some(&b'\n') {
                return Some((i, 3));
            }
        }
        i += 1;
    }
    None
}

fn str_at<'a>(v: &'a serde_json::Value, key: &str) -> &'a str {
    v.get(key).and_then(|x| x.as_str()).unwrap_or_default()
}

fn error_message(v: &serde_json::Value) -> &str {
    match v.get("error") {
        Some(serde_json::Value::String(s)) => s,
        Some(e) => str_at(e, "message"),
        None => "",
    }
}

/// A `url_citation` annotation. File citations name files in the site's
/// private index, which a terminal has no way to open, so they are dropped.
fn citation(a: &serde_json::Value) -> Option<AiEvent> {
    if str_at(a, "type") != "url_citation" {
        return None;
    }
    let url = str_at(a, "url");
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return None;
    }
    Some(AiEvent::Citation {
        url: url.to_string(),
        title: str_at(a, "title").to_string(),
    })
}

/// The web client's labels for a step, by tool or item name. Anything it
/// does not name is not shown there either.
pub fn progress_label(name: &str) -> Option<&'static str> {
    Some(match name {
        "file_search_call" | "search" | "searchWindowsForum" => "Searching WindowsForum",
        "web_search_call" => "Searching the web",
        "code_interpreter_call" => "Running code",
        "image_generation_call" | "generateImage" => "Creating an image",
        "mcp_call" => "Using a tool",
        "reasoning" => "Thinking it through",
        "searchThreads" => "Searching threads",
        "fetch" | "extractWebpageContent" => "Reading a page",
        "fetchThreadPosts" => "Reading a thread",
        "fetchThreadInfo" => "Looking up a thread",
        "fetchPostInfo" => "Looking up a post",
        "fetchUserInfo" => "Looking up a member",
        "fetchForumUpdates" => "Checking recent activity",
        "getTime" => "Checking the time",
        "assistant_get_weather" => "Checking the weather",
        "analyzeImage" => "Looking at an image",
        "processAttachments" => "Reading attachments",
        "getYouTubeTranscript" => "Reading a video transcript",
        "windows_screenshot" => "Capturing a verified Windows screen",
        "windows_computer_task" => "Working in managed Windows",
        _ => return None,
    })
}

/// The member's assistant allowance today (`GET /api/wf-tui-ai`, chat.php's
/// `getUsage`). Every field is optional: the quota service may be down, in
/// which case chat.php answers `{"logged_in": true, "unavailable": true}`.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct AiUsage {
    pub tier: Option<String>,
    pub used: Option<u64>,
    pub limit: Option<u64>,
    pub remaining: Option<u64>,
    pub unlimited: bool,
    pub allowed: Option<bool>,
    pub reset_at: Option<String>,
    pub unavailable: bool,
}

impl AiUsage {
    /// A short chip for the panel ("12 of 25 left today"), or `None` when
    /// there is nothing worth saying.
    pub fn summary(&self) -> Option<String> {
        if self.unavailable {
            return None;
        }
        if self.allowed == Some(false) {
            return Some("daily limit reached".into());
        }
        if self.unlimited {
            return None;
        }
        match (self.remaining, self.limit) {
            (Some(r), Some(l)) => Some(format!("{r} of {l} left today")),
            (Some(r), None) => Some(format!("{r} left today")),
            _ => None,
        }
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct UsageReply {
    #[serde(default)]
    pub usage: AiUsage,
}

/// The captured turn's answer text, for other modules' tests.
#[cfg(test)]
pub(crate) mod tests_support {
    pub fn captured_answer() -> String {
        let mut p = super::SseParser::new();
        let mut events = p.push(include_str!("testdata/ask_ai_stream.sse").as_bytes());
        events.extend(p.finish());
        events
            .into_iter()
            .filter_map(|e| match e {
                super::AiEvent::Delta(d) => Some(d),
                _ => None,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("testdata/ask_ai_stream.sse");

    fn parse_in_chunks(body: &[u8], size: usize) -> Vec<AiEvent> {
        let mut p = SseParser::new();
        let mut out = Vec::new();
        for chunk in body.chunks(size) {
            out.extend(p.push(chunk));
        }
        out.extend(p.finish());
        out
    }

    fn answer(events: &[AiEvent]) -> String {
        events
            .iter()
            .filter_map(|e| match e {
                AiEvent::Delta(d) => Some(d.as_str()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn the_captured_turn_parses_to_its_answer_and_completes() {
        let events = parse_in_chunks(FIXTURE.as_bytes(), 8192);
        let text = answer(&events);
        assert!(text.starts_with("KB5044284 is the **October 8, 2024"), "{text}");
        assert!(text.contains("](https://windowsforum.com/news/kb5044284-"), "{text}");
        assert_eq!(events.last(), Some(&AiEvent::Completed));
        assert_eq!(
            events.iter().filter(|e| matches!(e, AiEvent::Completed)).count(),
            1,
            "response.completed is not the chat's terminal event"
        );
        assert!(!events.iter().any(|e| matches!(e, AiEvent::Failed { .. })), "{events:?}");
        // Both tool calls (get_kb_article, search) announce themselves by
        // their item type, as the web client labels them.
        let steps: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                AiEvent::Progress { label, done: false, .. } => Some(label.as_str()),
                _ => None,
            })
            .collect();
        assert!(steps.contains(&"Using a tool"), "{steps:?}");
    }

    #[test]
    fn chunking_never_changes_the_result_even_mid_utf8() {
        let whole = parse_in_chunks(FIXTURE.as_bytes(), FIXTURE.len());
        for size in [1, 2, 3, 7, 64, 1000] {
            assert_eq!(parse_in_chunks(FIXTURE.as_bytes(), size), whole, "chunk size {size}");
        }
    }

    #[test]
    fn keepalives_crlf_and_done_are_understood() {
        let body = b": keepalive\r\n\r\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"Hi\"}\r\n\r\ndata: [DONE]\n\n";
        let events = parse_in_chunks(body, 5);
        assert_eq!(events, vec![AiEvent::Delta("Hi".into()), AiEvent::Completed]);
    }

    #[test]
    fn a_stream_that_stops_early_is_truncated_not_finished() {
        let body = b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"Half an\"}\n\n";
        let events = parse_in_chunks(body, 16);
        assert!(matches!(&events[1], AiEvent::Failed { code, .. } if code == "stream_truncated"), "{events:?}");
        let events = parse_in_chunks(b"", 1);
        assert!(matches!(&events[0], AiEvent::Failed { code, .. } if code == "stream_no_terminal"));
    }

    #[test]
    fn error_frames_carry_code_retryability_and_retry_after() {
        let body = b"data: {\"type\":\"error\",\"code\":\"rate_limited\",\"detail\":\"Slow down\",\"retryable\":true,\"status_code\":429,\"retry_after\":7}\n\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"ignored\"}\n\n";
        let events = parse_in_chunks(body, 9);
        assert_eq!(
            events,
            vec![AiEvent::Failed {
                code: "rate_limited".into(),
                message: "Slow down".into(),
                retryable: true,
                retry_after: Some(Duration::from_secs(7)),
            }]
        );
        let body = b"data: {\"type\":\"error\",\"error\":{\"message\":\"Bad\"},\"status_code\":400}\n\n";
        assert!(matches!(
            &parse_in_chunks(body, 64)[0],
            AiEvent::Failed { code, message, retryable: false, .. } if code == "error" && message == "Bad"
        ));
    }

    #[test]
    fn citations_come_from_annotations_and_only_url_ones_count() {
        let body = concat!(
            "data: {\"type\":\"response.output_text.annotation.added\",\"annotation\":{\"type\":\"url_citation\",\"url\":\"https://windowsforum.com/threads/1/\",\"title\":\"T\"}}\n\n",
            "data: {\"type\":\"response.content_part.done\",\"part\":{\"annotations\":[{\"type\":\"file_citation\",\"file_id\":\"f\"},{\"type\":\"url_citation\",\"url\":\"javascript:alert(1)\"}]}}\n\n",
            "data: {\"type\":\"chat.stream.completed\"}\n\n"
        );
        let events = parse_in_chunks(body.as_bytes(), 13);
        assert_eq!(
            events,
            vec![
                AiEvent::Citation { url: "https://windowsforum.com/threads/1/".into(), title: "T".into() },
                AiEvent::Completed
            ]
        );
    }

    #[test]
    fn done_text_is_a_fallback_only() {
        let body = b"data: {\"type\":\"response.output_text.done\",\"text\":\"All at once\"}\n\ndata: {\"type\":\"chat.stream.completed\"}\n\n";
        assert_eq!(answer(&parse_in_chunks(body, 64)), "All at once");
        let body = b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"A\"}\n\ndata: {\"type\":\"response.output_text.done\",\"text\":\"A\"}\n\ndata: {\"type\":\"chat.stream.completed\"}\n\n";
        assert_eq!(answer(&parse_in_chunks(body, 64)), "A");
    }

    #[test]
    fn an_oversized_stream_is_abandoned() {
        let mut p = SseParser::new();
        let big = vec![b' '; MAX_STREAM_BYTES + 1];
        let events = p.push(&big);
        assert!(matches!(&events[0], AiEvent::Failed { code, .. } if code == "stream_too_large"));
        assert!(p.push(b"data: [DONE]\n\n").is_empty());
        assert!(p.finish().is_empty());
    }

    #[test]
    fn the_form_carries_history_only_when_asked_and_bounded() {
        let mut req = AskRequest {
            message: "q".into(),
            conversation_id: "c1".into(),
            has_local_history: true,
            ..Default::default()
        };
        let form = req.form();
        assert!(form.contains(&("has_local_history".into(), "1".into())));
        req.history = (0..25)
            .map(|i| (if i % 2 == 0 { Role::User } else { Role::Assistant }, "é".repeat(3000)))
            .collect();
        let form = req.form();
        assert!(!form.iter().any(|(k, _)| k == "has_local_history"));
        assert_eq!(form.iter().filter(|(k, _)| k.ends_with("[role]")).count(), MAX_HISTORY_ITEMS);
        assert!(form.iter().all(|(k, v)| !k.ends_with("[content]") || v.len() <= MAX_HISTORY_ITEM_BYTES));
        assert_eq!(form[2], ("history[0][role]".to_string(), "assistant".to_string()));
    }

    #[test]
    fn ids_are_in_chat_phps_alphabet() {
        let id = new_id("tui");
        assert!(id.starts_with("tui") && id.len() == 25, "{id}");
        assert!(id.bytes().all(|b| b.is_ascii_alphanumeric()));
        assert_ne!(new_id("tui"), id);
    }

    #[test]
    fn the_captured_usage_body_decodes() {
        let reply: UsageReply =
            serde_json::from_str(include_str!("testdata/ask_ai_usage.json")).expect("usage");
        assert!(reply.usage.unlimited);
        assert_eq!(reply.usage.summary(), None);
        let capped = AiUsage { remaining: Some(3), limit: Some(25), ..Default::default() };
        assert_eq!(capped.summary().as_deref(), Some("3 of 25 left today"));
        let spent = AiUsage { allowed: Some(false), ..Default::default() };
        assert_eq!(spent.summary().as_deref(), Some("daily limit reached"));
        let down: UsageReply = serde_json::from_str(r#"{"usage":{"logged_in":true,"unavailable":true}}"#).expect("down");
        assert_eq!(down.usage.summary(), None);
    }
}
