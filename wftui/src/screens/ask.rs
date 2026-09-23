//! Ask the AI — WindowsForum's assistant (`g k`, the palette's "Ask the AI").
//!
//! A transcript over a one-line question field, like the site's own
//! `/pages/ai/`. The answer streams in as the model writes it, rendered from
//! its Markdown through the same chunk pipeline posts use, so its links carry
//! `[n]` markers and the digit keys open them.
//!
//! Two modes, as Search has: typing (the default — the field owns the
//! keyboard, Up/Down still scroll) and reading (`Esc` from the field; `j/k`,
//! the digits, `y`). Esc while reading stops an answer that is still coming,
//! and otherwise leaves.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use super::browse::{chunk_lines_aligned, wrap_spans, wrap_spans_aligned, RuleWidth};
use super::{Action, AskAiState, AskTurnState, HitMap};
use crate::chrome::{self, Hints};
use crate::glyph::Glyphs;
use crate::hit::{Hit, HitPane};
use crate::theme::Theme;

const PROMPT: &str = "> ";
/// Rows PgUp/PgDn move.
const PAGE: usize = 10;

/// Lay the transcript out for `width` cells. Cheap enough to redo per delta
/// (an answer is a few KB of Markdown), and done only when something changed.
pub fn rebuild_lines(s: &mut AskAiState, theme: &Theme, g: &Glyphs) {
    let width = (s.width as usize).max(8);
    s.lines.clear();
    s.links.clear();
    s.dirty = false;

    if s.turns.is_empty() {
        let intro = [
            "Ask anything about Windows, PCs, networking or WindowsForum itself.",
            "",
            "Answers come from WindowsForum's assistant, which searches the forum and the web as it goes. It can be wrong \u{2014} check what it cites before you act on it.",
        ];
        for para in intro {
            if para.is_empty() {
                s.lines.push(Line::default());
                continue;
            }
            for row in wrap_spans(&[Span::styled(para.to_string(), theme.dim())], width) {
                s.lines.push(Line::from(row));
            }
        }
        return;
    }

    let you = Style::new().fg(theme.accent).add_modifier(Modifier::BOLD);
    let bot = theme.title();
    for (i, turn) in s.turns.iter().enumerate() {
        if i > 0 {
            s.lines.push(Line::default());
        }
        s.lines.push(Line::from(Span::styled("You", you)));
        for row in wrap_spans(&[Span::styled(turn.question.clone(), theme.base())], width) {
            s.lines.push(Line::from(row));
        }
        s.lines.push(Line::default());
        s.lines.push(Line::from(Span::styled("Assistant", bot)));

        if !turn.answer.is_empty() {
            let chunks = common::markdown::render(&turn.answer);
            let rule = RuleWidth::of(width.min(40), g);
            for (line, align) in chunk_lines_aligned(&chunks, &mut s.links, theme, false, rule) {
                for row in wrap_spans_aligned(&line, width, align) {
                    s.lines.push(Line::from(row));
                }
            }
        }
        // Sources the model cited as annotations rather than inline links.
        let fresh: Vec<_> = turn
            .citations
            .iter()
            .filter(|(url, _)| !s.links.contains(url))
            .cloned()
            .collect();
        if !fresh.is_empty() {
            s.lines.push(Line::from(Span::styled("Sources", theme.dim())));
            for (url, title) in fresh {
                s.links.push(url.clone());
                let label = if title.is_empty() { url.clone() } else { format!("{title} \u{2014} {url}") };
                let spans = [
                    Span::styled(format!("[{}] ", s.links.len()), theme.link()),
                    Span::styled(label, theme.dim()),
                ];
                for row in wrap_spans(&spans, width) {
                    s.lines.push(Line::from(row));
                }
            }
        }
        match &turn.state {
            AskTurnState::Streaming if turn.answer.is_empty() => {
                s.lines.push(Line::from(Span::styled("\u{2026}", theme.dim())));
            }
            AskTurnState::Failed(msg) => {
                let text = format!("{} {msg}", if g.ascii { "!" } else { "\u{26a0}" });
                for row in wrap_spans(&[Span::styled(text, Style::new().fg(theme.error))], width) {
                    s.lines.push(Line::from(row));
                }
            }
            AskTurnState::Cancelled => {
                s.lines.push(Line::from(Span::styled("(stopped)", theme.faint())));
            }
            AskTurnState::Done if turn.answer.trim().is_empty() => {
                s.lines.push(Line::from(Span::styled("(no answer)", theme.faint())));
            }
            _ => {}
        }
    }
}

pub fn render(
    s: &mut AskAiState,
    f: &mut Frame,
    area: Rect,
    theme: &Theme,
    g: &Glyphs,
    hits: &mut HitMap,
) {
    let block = super::solo_panel(theme, g, "Ask the AI", s.usage.as_deref(), None);
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.width == 0 || inner.height < 3 {
        return;
    }
    let [body, status, input] = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(inner);

    if s.width != body.width || s.dirty {
        s.width = body.width;
        rebuild_lines(s, theme, g);
    }
    s.view_height = body.height;
    let max_scroll = s.lines.len().saturating_sub(body.height as usize);
    if s.follow {
        s.scroll = max_scroll;
    }
    s.scroll = s.scroll.min(max_scroll);
    f.render_widget(
        Paragraph::new(crate::editor::visible_window(&s.lines, s.scroll, body.height)),
        body,
    );
    hits.push(body, Hit::Pane(HitPane::List));

    // The status row: what the assistant is doing, or what the keys are for.
    let status_line = if s.busy {
        let step = s
            .turns
            .last()
            .and_then(|t| t.steps.iter().rev().find(|(_, _, done)| !done))
            .map(|(_, label, _)| label.as_str())
            .unwrap_or(if s.turns.last().is_some_and(|t| !t.answer.is_empty()) {
                "Writing"
            } else {
                "Thinking"
            });
        Line::from(Span::styled(
            chrome::take_cells(
                &format!("{} {step}\u{2026}", chrome::spinner(g, chrome::spinner_tick())),
                status.width as usize,
            ),
            theme.dim(),
        ))
    } else if s.scroll < max_scroll {
        Line::from(Span::styled(
            chrome::take_cells("more below \u{2014} G for the end", status.width as usize),
            theme.faint(),
        ))
    } else {
        Line::default()
    };
    f.render_widget(Paragraph::new(status_line), status);

    // The question field.
    let prompt_w = chrome::cell_width(PROMPT);
    let room = (input.width as usize).saturating_sub(prompt_w);
    let mut spans = vec![Span::styled(
        PROMPT,
        Style::new().fg(theme.accent).add_modifier(Modifier::BOLD),
    )];
    let mut caret = 0u16;
    if s.input.is_empty() && !s.input_mode {
        spans.push(Span::styled(chrome::take_cells("press i to ask a question", room), theme.dim()));
    } else if s.input.is_empty() {
        spans.push(Span::styled(
            chrome::take_cells(if s.busy { "answering\u{2026}" } else { "Ask a question" }, room),
            theme.faint(),
        ));
    } else {
        let (visible, at) = chrome::field_window(&s.input, s.cursor, room);
        caret = at;
        spans.push(Span::styled(visible, theme.base()));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), input);
    s.input_rect = input;
    hits.push(input, Hit::Field(0));
    if s.input_mode {
        let x = (input.x + prompt_w as u16 + caret).min(input.x + input.width.saturating_sub(1));
        f.set_cursor_position((x, input.y));
    }
}

/// A click in the question field: type there, with the caret under the
/// pointer.
pub fn click_field(s: &mut AskAiState, col: u16) {
    s.input_mode = true;
    let prompt = chrome::cell_width(PROMPT);
    let room = (s.input_rect.width as usize).saturating_sub(prompt);
    let x = col.saturating_sub(s.input_rect.x).saturating_sub(prompt as u16) as usize;
    s.cursor = crate::editor::field_caret_at(&s.input, s.cursor, room, x);
}

fn scroll_up(s: &mut AskAiState, n: usize) {
    s.follow = false;
    s.scroll = s.scroll.saturating_sub(n);
}

fn scroll_down(s: &mut AskAiState, n: usize) {
    let max = s.lines.len().saturating_sub(s.view_height as usize);
    s.scroll = s.scroll.saturating_add(n).min(max);
    // Back at the bottom: keep following the answer as it streams.
    s.follow = s.scroll >= max;
}

pub fn key(s: &mut AskAiState, key: KeyEvent) -> Action {
    // Both modes: scrolling (the mouse wheel arrives as Up/Down) and a new
    // conversation.
    match key.code {
        KeyCode::Up => {
            scroll_up(s, 1);
            return Action::None;
        }
        KeyCode::Down => {
            scroll_down(s, 1);
            return Action::None;
        }
        KeyCode::PageUp => {
            scroll_up(s, PAGE);
            return Action::None;
        }
        KeyCode::PageDown => {
            scroll_down(s, PAGE);
            return Action::None;
        }
        KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            return Action::AskAiNewChat;
        }
        _ => {}
    }
    if s.input_mode {
        input_key(s, key)
    } else {
        browse_key(s, key)
    }
}

fn input_key(s: &mut AskAiState, key: KeyEvent) -> Action {
    use crate::editor;
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('v') => return Action::PasteClipboard,
            KeyCode::Char('a') => editor::move_home(&s.input, &mut s.cursor),
            KeyCode::Char('e') => editor::move_end(&s.input, &mut s.cursor),
            KeyCode::Char('w') => editor::delete_word_back(&mut s.input, &mut s.cursor),
            KeyCode::Char('u') => editor::kill_to_start(&mut s.input, &mut s.cursor),
            KeyCode::Char('k') => editor::kill_to_end(&mut s.input, &mut s.cursor),
            _ => {}
        }
        return Action::None;
    }
    match key.code {
        // Reading mode: the transcript keys, and Esc there stops or leaves.
        KeyCode::Esc => s.input_mode = false,
        KeyCode::Enter => return submit(s),
        KeyCode::Backspace => editor::delete_back(&mut s.input, &mut s.cursor),
        KeyCode::Delete => editor::delete_forward(&mut s.input, &mut s.cursor),
        KeyCode::Left => editor::move_left(&s.input, &mut s.cursor),
        KeyCode::Right => editor::move_right(&s.input, &mut s.cursor),
        KeyCode::Home => editor::move_home(&s.input, &mut s.cursor),
        KeyCode::End => editor::move_end(&s.input, &mut s.cursor),
        KeyCode::Char(c) => editor::insert_char(&mut s.input, &mut s.cursor, c),
        _ => {}
    }
    Action::None
}

fn submit(s: &mut AskAiState) -> Action {
    let question = s.input.trim().to_string();
    if question.is_empty() {
        return Action::Notice("Type a question first.".into());
    }
    if s.busy {
        return Action::Notice("Still answering \u{2014} wait for it, or Esc twice to stop it.".into());
    }
    if question.len() > common::ai::MAX_QUESTION_BYTES {
        return Action::Notice(format!(
            "That question is too long for the assistant ({} of at most {} bytes).",
            question.len(),
            common::ai::MAX_QUESTION_BYTES
        ));
    }
    s.input.clear();
    s.cursor = 0;
    Action::AskAi(question)
}

fn browse_key(s: &mut AskAiState, key: KeyEvent) -> Action {
    match key.code {
        KeyCode::Char('i') | KeyCode::Enter => {
            s.input_mode = true;
            Action::None
        }
        KeyCode::Char('j') => {
            scroll_down(s, 1);
            Action::None
        }
        KeyCode::Char('k') => {
            scroll_up(s, 1);
            Action::None
        }
        KeyCode::Char('n') => Action::AskAiNewChat,
        KeyCode::Char('q') => Action::PopScreen,
        // Esc reaches here only while an answer is streaming (`esc_intent`).
        KeyCode::Esc => Action::AskAiCancel,
        KeyCode::Char('y') => match s.turns.iter().rev().find(|t| !t.answer.trim().is_empty()) {
            Some(turn) => Action::OscCopy(common::markdown::to_plain(&turn.answer)),
            None => Action::Notice("No answer to copy yet.".into()),
        },
        KeyCode::Char('o') => Action::OpenUrl(format!("{}/pages/ai/", s.site.origin)),
        KeyCode::Char(c @ '1'..='9') => {
            let n = c as usize - '0' as usize;
            match s.links.get(n - 1) {
                Some(url) => Action::OpenUrl(url.clone()),
                None => Action::Notice(format!("There is no source [{n}] here.")),
            }
        }
        _ => Action::None,
    }
}

pub fn hints(s: &AskAiState) -> Hints {
    if s.input_mode {
        let mut keys: Vec<chrome::Hint> = Vec::new();
        if !s.busy {
            keys.push(("Enter", "ask"));
        }
        keys.push(("^N", "new chat"));
        keys.push(("Esc", if s.busy { "read / stop" } else { "read" }));
        return Hints::new(&keys, 0);
    }
    let mut keys: Vec<chrome::Hint> = vec![("i", "ask"), ("j/k", "scroll")];
    if !s.links.is_empty() {
        keys.push(("1-9", "open source"));
    }
    if s.turns.iter().any(|t| !t.answer.trim().is_empty()) {
        keys.push(("y", "copy answer"));
    }
    keys.push(("n", "new chat"));
    keys.push(("o", "open web"));
    keys.push(("Esc", if s.busy { "stop" } else { "back" }));
    let short: Vec<chrome::Hint> = vec![("i", "ask"), ("j/k", ""), ("n", "new"), ("Esc", if s.busy { "stop" } else { "back" })];
    Hints::with_short(&keys, &short, 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::screens::AskTurn;

    fn state() -> AskAiState {
        AskAiState { input_mode: true, follow: true, ..Default::default() }
    }

    fn press(s: &mut AskAiState, code: KeyCode) -> Action {
        key(s, KeyEvent::new(code, KeyModifiers::NONE))
    }

    #[test]
    fn typing_then_enter_asks_and_clears_the_field() {
        let mut s = state();
        for c in "why is my pc slow".chars() {
            assert!(matches!(press(&mut s, KeyCode::Char(c)), Action::None));
        }
        match press(&mut s, KeyCode::Enter) {
            Action::AskAi(q) => assert_eq!(q, "why is my pc slow"),
            _ => panic!("Enter should ask"),
        }
        assert!(s.input.is_empty() && s.cursor == 0);
        // `j`, `k`, `n`, `q` are text while typing, not commands.
        for c in ['j', 'k', 'n', 'q', 'y', '1'] {
            assert!(matches!(press(&mut s, KeyCode::Char(c)), Action::None));
        }
        assert_eq!(s.input, "jknqy1");
    }

    #[test]
    fn empty_busy_and_oversized_questions_are_refused_out_loud() {
        let mut s = state();
        assert!(matches!(press(&mut s, KeyCode::Enter), Action::Notice(_)));
        s.input = "again".into();
        s.busy = true;
        assert!(matches!(press(&mut s, KeyCode::Enter), Action::Notice(_)));
        assert_eq!(s.input, "again", "a refused question keeps its text");
        s.busy = false;
        s.input = "x".repeat(common::ai::MAX_QUESTION_BYTES + 1);
        assert!(matches!(press(&mut s, KeyCode::Enter), Action::Notice(_)));
    }

    #[test]
    fn reading_mode_keys_open_sources_copy_and_start_over() {
        let mut s = state();
        s.input_mode = false;
        s.links = vec!["https://windowsforum.com/threads/1/".into()];
        s.turns.push(AskTurn { question: "q".into(), answer: "**a**".into(), ..Default::default() });
        assert!(matches!(press(&mut s, KeyCode::Char('1')), Action::OpenUrl(u) if u.ends_with("/threads/1/")));
        assert!(matches!(press(&mut s, KeyCode::Char('2')), Action::Notice(_)));
        assert!(matches!(press(&mut s, KeyCode::Char('y')), Action::OscCopy(t) if t == "**a**"));
        assert!(matches!(press(&mut s, KeyCode::Char('n')), Action::AskAiNewChat));
        assert!(matches!(press(&mut s, KeyCode::Char('o')), Action::OpenUrl(u) if u == "https://windowsforum.com/pages/ai/"));
        assert!(matches!(press(&mut s, KeyCode::Char('i')), Action::None));
        assert!(s.input_mode);
        // Esc in the field goes back to reading, never out of the screen.
        assert!(matches!(press(&mut s, KeyCode::Esc), Action::None));
        assert!(!s.input_mode);
    }

    #[test]
    fn scrolling_up_stops_following_and_the_bottom_resumes_it() {
        let mut s = state();
        s.lines = vec![Line::raw("x"); 50];
        s.view_height = 10;
        s.scroll = 40;
        press(&mut s, KeyCode::Up);
        assert!(!s.follow && s.scroll == 39);
        press(&mut s, KeyCode::PageDown);
        assert!(s.follow && s.scroll == 40);
    }
}
