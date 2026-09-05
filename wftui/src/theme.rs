//! Theme: one palette, dark and light terminal friendly (CSS-var-like roles,
//! not raw "blue because blue"). `NO_COLOR` flattens everything.

use ratatui::style::{Color, Modifier, Style};

#[derive(Debug, Clone, Copy)]
pub struct Theme {
    pub text: Color,
    pub dim: Color,
    pub accent: Color,
    pub link: Color,
    pub warn: Color,
    pub error: Color,
    pub header: Color,
    pub selected_bg: Color,
}

impl Theme {
    pub const fn dark() -> Self {
        Theme {
            text: Color::White,
            dim: Color::DarkGray,
            accent: Color::Cyan,
            link: Color::Blue,
            warn: Color::Yellow,
            error: Color::Red,
            header: Color::LightBlue,
            selected_bg: Color::DarkGray,
        }
    }

    pub fn detect() -> Self {
        if std::env::var_os("NO_COLOR").is_some() {
            return Theme::no_color();
        }
        Theme::dark()
    }

    pub const fn no_color() -> Self {
        Theme {
            text: Color::Reset,
            dim: Color::Reset,
            accent: Color::Reset,
            link: Color::Reset,
            warn: Color::Reset,
            error: Color::Reset,
            header: Color::Reset,
            selected_bg: Color::Reset,
        }
    }

    pub fn base(&self) -> Style {
        Style::new().fg(self.text)
    }
    pub fn dim(&self) -> Style {
        Style::new().fg(self.dim)
    }
    pub fn title(&self) -> Style {
        Style::new().fg(self.header).add_modifier(Modifier::BOLD)
    }
    pub fn selected(&self) -> Style {
        Style::new()
            .bg(self.selected_bg)
            .add_modifier(Modifier::BOLD)
    }
}

/// Time formatting: relative for <24h, absolute after.
pub fn fmt_time(ts: i64) -> String {
    let Ok(t) = time::OffsetDateTime::from_unix_timestamp(ts) else {
        return String::new();
    };
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    let age = now - ts;
    if age < 0 {
        return t.date().to_string();
    }
    if age < 60 {
        "just now".into()
    } else if age < 3600 {
        format!("{}m ago", age / 60)
    } else if age < 86_400 {
        format!("{}h ago", age / 3600)
    } else {
        format!("{}", t.date())
    }
}
