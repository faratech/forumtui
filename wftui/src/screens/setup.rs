//! First-run Setup (the "Terminal for XenForo" edition): three fields —
//! forum address, OAuth client id, forum name — validated by
//! `site::SiteConfig::validate`, written to `config.json` by the app, and
//! then the sign-in screen. The built-in-site edition never shows it.
//!
//! The fields own the keyboard while the screen is up (`input_capture`), so
//! letters are text; Esc quits — there is nothing behind this screen yet.

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::{Action, solo_panel};
use crate::chrome::{self, Hints};
use crate::glyph::Glyphs;
use crate::hit::HitMap;
use crate::theme::Theme;

/// The three fields, in Tab order.
pub const FIELDS: usize = 3;

#[derive(Default)]
pub struct SetupState {
    pub origin: String,
    pub client_id: String,
    pub name: String,
    pub cursors: [usize; FIELDS],
    /// Which field has the caret.
    pub field: usize,
    /// What the last submit was refused for, drawn under the fields.
    pub errors: Vec<String>,
    /// Which panel line carries the active field, stamped by the renderer
    /// so the caret lands on it.
    pub field_lines: [Option<usize>; FIELDS],
}

impl SetupState {
    pub fn text_mut(&mut self, field: usize) -> (&mut String, &mut usize) {
        let cursor = &mut self.cursors[field.min(FIELDS - 1)];
        let text = match field {
            0 => &mut self.origin,
            1 => &mut self.client_id,
            _ => &mut self.name,
        };
        (text, cursor)
    }

    fn text(&self, field: usize) -> &str {
        match field {
            0 => &self.origin,
            1 => &self.client_id,
            _ => &self.name,
        }
    }

    /// A forum name guessed from the address, offered when the name field
    /// is still empty on the way past it: `forum.example.com` → `Example`.
    fn suggested_name(&self) -> String {
        let host = self
            .origin
            .trim()
            .split_once("://")
            .map(|(_, r)| r)
            .unwrap_or(self.origin.trim())
            .split('/')
            .next()
            .unwrap_or("")
            .split(':')
            .next()
            .unwrap_or("");
        let labels: Vec<&str> = host.split('.').filter(|l| !l.is_empty()).collect();
        // The registrable label: the one before the public suffix, unless
        // the host is a bare name — or an IP address, which has no name.
        let numeric = !labels.is_empty() && labels.iter().all(|l| l.bytes().all(|b| b.is_ascii_digit()));
        let pick = match labels.len() {
            0 => "",
            _ if numeric => host,
            1 => labels[0],
            n => labels[n - 2],
        };
        let mut chars = pick.chars();
        match chars.next() {
            Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
            None => String::new(),
        }
    }
}

const LABELS: [&str; FIELDS] = ["Forum address", "OAuth client ID", "Forum name"];
const PANEL_WIDTH: u16 = 72;
const FIELD_COL: usize = 3;
const FIELD_WIDTH: usize = PANEL_WIDTH as usize - 2 - FIELD_COL - 1;

pub fn setup_key(s: &mut SetupState, key: KeyEvent) -> Action {
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        return match key.code {
            KeyCode::Char('y') | KeyCode::Char('v') => Action::PasteClipboard,
            KeyCode::Char('s') => submit(s),
            KeyCode::Char('c') => Action::Quit,
            _ => Action::None,
        };
    }
    match key.code {
        KeyCode::Esc => Action::Quit,
        KeyCode::Tab | KeyCode::Down => {
            advance(s, 1);
            Action::None
        }
        KeyCode::BackTab | KeyCode::Up => {
            advance(s, FIELDS - 1);
            Action::None
        }
        KeyCode::Enter => {
            if s.field + 1 < FIELDS {
                advance(s, 1);
                Action::None
            } else {
                submit(s)
            }
        }
        KeyCode::Backspace => {
            let (text, cursor) = s.text_mut(s.field);
            crate::editor::delete_back(text, cursor);
            Action::None
        }
        KeyCode::Delete => {
            let (text, cursor) = s.text_mut(s.field);
            crate::editor::delete_forward(text, cursor);
            Action::None
        }
        KeyCode::Left => {
            let (text, cursor) = s.text_mut(s.field);
            crate::editor::move_left(text, cursor);
            Action::None
        }
        KeyCode::Right => {
            let (text, cursor) = s.text_mut(s.field);
            crate::editor::move_right(text, cursor);
            Action::None
        }
        KeyCode::Home => {
            let (text, cursor) = s.text_mut(s.field);
            crate::editor::move_home(text, cursor);
            Action::None
        }
        KeyCode::End => {
            let (text, cursor) = s.text_mut(s.field);
            crate::editor::move_end(text, cursor);
            Action::None
        }
        KeyCode::Char(c) => {
            let (text, cursor) = s.text_mut(s.field);
            crate::editor::insert_str(text, cursor, &c.to_string());
            Action::None
        }
        _ => Action::None,
    }
}

/// Move the caret `by` fields on (mod `FIELDS`), suggesting a name when the
/// name field is reached empty.
fn advance(s: &mut SetupState, by: usize) {
    s.field = (s.field + by) % FIELDS;
    if s.field == 2 && s.name.trim().is_empty() {
        s.name = s.suggested_name();
        s.cursors[2] = s.name.chars().count();
    }
}

fn submit(s: &mut SetupState) -> Action {
    let name = if s.name.trim().is_empty() { s.suggested_name() } else { s.name.trim().to_string() };
    Action::SetupSite {
        origin: s.origin.trim().to_string(),
        client_id: s.client_id.trim().to_string(),
        name,
    }
}

pub fn setup_hints(s: &SetupState) -> Hints {
    let submit = if s.field + 1 == FIELDS { ("Enter", "finish") } else { ("^S", "finish") };
    Hints::new(&[submit, ("Tab", "next field"), ("^Y", "paste"), ("Esc", "quit")], 0)
}

pub fn render_setup(
    s: &mut SetupState,
    f: &mut Frame,
    area: Rect,
    theme: &Theme,
    g: &Glyphs,
    _hits: &mut HitMap,
) {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let step = |n: &str, body: &str| {
        Line::from(vec![
            Span::raw(" "),
            Span::styled(n.to_string(), Style::new().fg(theme.accent).add_modifier(Modifier::BOLD)),
            Span::raw("  "),
            Span::styled(body.to_string(), theme.base()),
        ])
    };
    let note = |body: &str| Line::from(vec![Span::raw("    "), Span::styled(body.to_string(), theme.dim())]);
    lines.push(Line::from(Span::styled(
        format!(" Connect {} to a XenForo 2.3 forum.", common::site::PRODUCT_NAME),
        theme.base(),
    )));
    lines.push(Line::from(Span::raw("")));
    let helps: [&[&str]; FIELDS] = [
        &["https://forum.example.com"],
        &[
            "A PUBLIC OAuth client the forum's admin creates under",
            "Setup \u{2192} Service providers \u{2192} OAuth clients, with the redirect",
            "URI http://127.0.0.1/callback. The ID is not a secret.",
        ],
        &["What the header calls it. Everything else can wait for config.json."],
    ];
    s.field_lines = [None; FIELDS];
    for (i, label) in LABELS.iter().enumerate() {
        lines.push(step(&(i + 1).to_string(), label));
        let active = i == s.field;
        let (start, _) = crate::editor::hwindow(s.text(i), s.cursors[i], FIELD_WIDTH);
        let tail: String = s.text(i).chars().skip(start).collect();
        let shown = chrome::take_cells(&tail, FIELD_WIDTH).to_string();
        let pad = FIELD_WIDTH.saturating_sub(chrome::cell_width(&shown));
        let style = if active { theme.base().add_modifier(Modifier::UNDERLINED) } else { theme.dim() };
        s.field_lines[i] = Some(lines.len());
        lines.push(Line::from(vec![
            Span::raw(" ".repeat(FIELD_COL)),
            Span::styled(shown, style),
            Span::styled(" ".repeat(pad), style),
        ]));
        for h in helps[i] {
            lines.push(note(h));
        }
        lines.push(Line::from(Span::raw("")));
    }
    for e in &s.errors {
        lines.push(Line::from(Span::styled(format!(" {e}"), Style::new().fg(theme.error))));
    }
    let height = (lines.len() as u16 + 2).min(area.height);
    let panel_area = super::centered_box(area, PANEL_WIDTH, height);
    let block = solo_panel(theme, g, &format!("Set up {}", common::site::PRODUCT_NAME), None, None);
    let inner = block.inner(panel_area);
    f.render_widget(block, panel_area);
    f.render_widget(Paragraph::new(lines), inner);
    if let Some(line) = s.field_lines[s.field]
        && (line as u16) < inner.height
    {
        let (_, caret) = crate::editor::hwindow(s.text(s.field), s.cursors[s.field], FIELD_WIDTH);
        let x = FIELD_COL + caret;
        f.set_cursor_position((
            inner.x + (x as u16).min(inner.width.saturating_sub(1)),
            inner.y + line as u16,
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    #[test]
    fn fields_capture_typing_tab_cycles_and_enter_on_the_last_submits() {
        let mut s = SetupState::default();
        for c in "https://forum.example.com".chars() {
            setup_key(&mut s, key(c));
        }
        assert!(matches!(setup_key(&mut s, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)), Action::None));
        assert_eq!(s.field, 1);
        for c in "abc123".chars() {
            setup_key(&mut s, key(c));
        }
        assert!(matches!(setup_key(&mut s, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)), Action::None), "Enter moves on until the last field");
        assert_eq!(s.field, 2);
        assert_eq!(s.name, "Example", "the name is suggested from the host");
        // `q` is a letter here, not quit; Esc is.
        setup_key(&mut s, key('q'));
        assert_eq!(s.name, "Exampleq");
        setup_key(&mut s, KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        let submitted = |a: Action| matches!(a, Action::SetupSite { origin, client_id, name }
            if origin == "https://forum.example.com" && client_id == "abc123" && name == "Example");
        assert!(submitted(setup_key(&mut s, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))));
        assert!(submitted(setup_key(&mut s, KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL))), "^S submits from anywhere");
        assert!(matches!(setup_key(&mut s, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)), Action::Quit));
        assert!(matches!(setup_key(&mut s, KeyEvent::new(KeyCode::Char('y'), KeyModifiers::CONTROL)), Action::PasteClipboard));
        assert!(matches!(setup_key(&mut s, KeyEvent::new(KeyCode::BackTab, KeyModifiers::NONE)), Action::None));
        assert_eq!(s.field, 1);
        let hints = setup_hints(&s);
        assert!(hints.keys.iter().any(|(k, _)| *k == "^S"));
    }

    #[test]
    fn suggested_name_takes_the_registrable_label() {
        let mut s = SetupState::default();
        for (origin, want) in [
            ("https://forum.example.com", "Example"),
            ("https://example.org/", "Example"),
            ("http://127.0.0.1:1", "127.0.0.1"),
            ("intranet", "Intranet"),
            ("", ""),
        ] {
            s.origin = origin.into();
            assert_eq!(s.suggested_name(), want, "{origin}");
        }
    }

    #[test]
    fn renders_inside_the_panel_with_the_caret_on_the_active_field() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut s = SetupState { origin: "https://x.example".into(), errors: vec!["\"oauth_client_id\" is required".into()], ..Default::default() };
        s.cursors[0] = s.origin.chars().count();
        let theme = Theme::truecolor();
        for (w, h) in [(120u16, 36u16), (80, 24)] {
            let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
            term.draw(|f| {
                let body = Rect::new(0, 1, w, h - 3);
                render_setup(&mut s, f, body, &theme, &crate::glyph::UNICODE, &mut HitMap::default());
            })
            .unwrap();
            let buf = term.backend().buffer().clone();
            let text: String = (0..h).map(|y| (0..w).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>() + "\n").collect();
            assert!(text.contains("Set up") && text.contains("Forum address") && text.contains("OAuth client ID"), "{text}");
            assert!(text.contains("is required"), "{text}");
            // The caret sits at the end of the typed address, on the field's
            // row, inside the panel.
            let pos = term.get_cursor_position().unwrap();
            let row: String = (0..w).map(|x| buf[(x, pos.y)].symbol().to_string()).collect();
            assert!(row.contains("https://x.example"), "caret row is the address field at {w}x{h}: {row:?}");
            let before: String = (0..pos.x).map(|x| buf[(x, pos.y)].symbol().to_string()).collect();
            assert!(before.trim_end().ends_with("https://x.example"), "{before:?}");
        }
    }
}
