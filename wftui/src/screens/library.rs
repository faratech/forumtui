//! The Media Gallery (XFMG) and Resource Manager (XFRM) listing screens
//! (issue #680): paged, read-only catalogs. Enter/o opens the selected item
//! on the site; `[`/`]` page; `R` refreshes. Both follow the house paged-list
//! pattern: one fetch in flight per screen (`loading` guards on the firing
//! keys), replies are page-stamped and adopt the topmost screen.

use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState, Paragraph};
use ratatui::Frame;

use super::browse::truncate;
use super::{solo_panel, Action, HitMap, MediaListItem, ResourceListItem};
use crate::chrome::{self, Hints};
use crate::glyph::Glyphs;
use crate::hit::Hit;
use crate::theme::{fmt_age, Theme};

/// `1–20 of 431` for the panel's bottom cap. The API's pagination carries no
/// `per_page`, so the last page's range is derived backwards from the total —
/// the same arithmetic `browse::list_range` and `misc::search_range` use.
fn page_range(page: u32, last_page: u32, total: u64, len: usize) -> Option<String> {
    if total == 0 || len == 0 {
        return None;
    }
    let len = len as u64;
    let (start, end) = if page >= last_page.max(1) && total >= len {
        (total - len + 1, total)
    } else {
        let start = (page.max(1) as u64 - 1) * len + 1;
        (start, start + len - 1)
    };
    Some(format!("{start}\u{2013}{end} of {total}"))
}

/// One catalog row: `title` on the left, a dim meta run right-aligned, and
/// the whole line exactly `w` cells wide (cells, never chars — CJK titles
/// broke the naive form, issue #511/#595).
fn catalog_line(theme: &Theme, title: &str, right_text: &str, w: usize) -> Line<'static> {
    let right = Span::styled(right_text.to_string(), theme.dim());
    let right_w = right.width();
    // One space of gutter so the title never touches the meta run.
    let title_w = w.saturating_sub(right_w + 1);
    let left = Span::styled(truncate(title, title_w), theme.base());
    let pad = w.saturating_sub(left.width() + right_w);
    Line::from(vec![left, Span::raw(" ".repeat(pad)), right])
}

/// The panel every catalog draws into, plus the "nothing to draw yet" states
/// (loading spinner / error / empty). `Some(inner)` means: go on and draw
/// rows into this rect.
#[allow(clippy::too_many_arguments)]
fn catalog_panel(
    f: &mut Frame,
    area: Rect,
    theme: &Theme,
    g: &Glyphs,
    title: &str,
    what: &str,
    page: u32,
    last_page: u32,
    total: u64,
    len: usize,
    loading: bool,
    error: Option<&String>,
) -> Option<Rect> {
    let right = format!("page {} of {}", page.max(1), last_page.max(1));
    let bottom = page_range(page, last_page, total, len);
    let block = solo_panel(theme, g, title, Some(&right), bottom.as_deref());
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return None;
    }
    if loading && len == 0 {
        f.render_widget(
            Paragraph::new(format!(
                "{} Loading {what}\u{2026}",
                chrome::spinner(g, chrome::spinner_tick())
            ))
            .style(theme.dim()),
            inner,
        );
        return None;
    }
    if let Some(err) = error {
        f.render_widget(
            Paragraph::new(format!("Error: {err}\n\nPress R to retry."))
                .style(Style::new().fg(theme.error)),
            inner,
        );
        return None;
    }
    if len == 0 {
        f.render_widget(
            Paragraph::new(format!("No {what} here yet.")).style(theme.dim()),
            inner,
        );
        return None;
    }
    Some(inner)
}

/// The Media Gallery: one line per item — `title` with a dim
/// `uploader \u{00B7} age` run right-aligned.
pub fn render_media_gallery(
    s: &mut super::MediaListState,
    f: &mut Frame,
    area: Rect,
    theme: &Theme,
    g: &Glyphs,
    hits: &mut HitMap,
) {
    let Some(inner) = catalog_panel(
        f,
        area,
        theme,
        g,
        "Media Gallery",
        "media",
        s.page,
        s.last_page,
        s.total,
        s.items.len(),
        s.loading,
        s.error.as_ref(),
    ) else {
        return;
    };

    let w = inner.width as usize;
    let items: Vec<ListItem> = s
        .items
        .iter()
        .map(|m: &MediaListItem| {
            let mut meta = m.username.clone();
            if m.media_date > 0 {
                if !meta.is_empty() {
                    meta.push_str(" \u{b7} ");
                }
                meta.push_str(&fmt_age(m.media_date));
            }
            ListItem::new(catalog_line(theme, &m.title, &meta, w))
        })
        .collect();
    let mut state = ListState::default().with_selected(Some(s.sel.min(s.items.len() - 1)));
    f.render_stateful_widget(
        List::new(items).highlight_style(theme.selected()),
        inner,
        &mut state,
    );
    // `offset` is read back AFTER the render, when the widget has scrolled to
    // keep the selection visible — a stale offset points at the wrong rows.
    hits.rows(inner, state.offset(), s.items.len(), Hit::Row);
}

/// The Resource Manager: `title` with a dim
/// `\u{2193} downloads \u{00B7} \u{2605} rating \u{00B7} age` run, plus the
/// tag line underneath when the resource has one.
pub fn render_resources(
    s: &mut super::ResourceListState,
    f: &mut Frame,
    area: Rect,
    theme: &Theme,
    g: &Glyphs,
    hits: &mut HitMap,
) {
    let Some(inner) = catalog_panel(
        f,
        area,
        theme,
        g,
        "Resources",
        "resources",
        s.page,
        s.last_page,
        s.total,
        s.items.len(),
        s.loading,
        s.error.as_ref(),
    ) else {
        return;
    };

    let w = inner.width as usize;
    let arrow = if g.ascii { "v" } else { "\u{2193}" };
    let star = if g.ascii { "*" } else { "\u{2605}" };
    let items: Vec<ListItem> = s
        .items
        .iter()
        .map(|r: &ResourceListItem| {
            let mut meta = String::new();
            if !r.username.is_empty() {
                meta.push_str(&r.username);
            }
            if r.download_count > 0 {
                if !meta.is_empty() {
                    meta.push_str(" \u{b7} ");
                }
                meta.push_str(&format!("{arrow} {}", r.download_count));
            }
            if let Some(rating) = r.rating_average {
                if !meta.is_empty() {
                    meta.push_str(" \u{b7} ");
                }
                meta.push_str(&format!("{star} {rating:.1}"));
            }
            if r.resource_date > 0 {
                if !meta.is_empty() {
                    meta.push_str(" \u{b7} ");
                }
                meta.push_str(&fmt_age(r.resource_date));
            }
            let mut lines = vec![catalog_line(theme, &r.title, &meta, w)];
            if !r.tag_line.is_empty() {
                lines.push(Line::from(Span::styled(
                    format!("   {}", truncate(&r.tag_line, w.saturating_sub(3))),
                    theme.dim(),
                )));
            }
            ListItem::new(lines)
        })
        .collect();
    let mut state = ListState::default().with_selected(Some(s.sel.min(s.items.len() - 1)));
    // A row is one line, or two once the resource has a tag line — the same
    // shape just built, so a click lands on the row the eye is on.
    let heights: Vec<u16> = s
        .items
        .iter()
        .map(|r| if r.tag_line.is_empty() { 1 } else { 2 })
        .collect();
    f.render_stateful_widget(
        List::new(items).highlight_style(theme.selected()),
        inner,
        &mut state,
    );
    hits.list(inner, state.offset(), &heights, Hit::Row);
}

/// Shared movement/paging/open/refresh handling (#657 discipline: the
/// firing keys refuse out loud while a load is in flight). `$load(page)`
/// builds the fetch `Action` for this screen.
macro_rules! list_keys {
    ($s:expr, $key:expr, $open:expr, $load:expr) => {{
        match $key.code {
            KeyCode::Char('q') | KeyCode::Char('h') | KeyCode::Left => Action::PopScreen,
            KeyCode::Char('R') | KeyCode::F(5) => {
                if $s.loading {
                    Action::Notice("Already loading \u{2014} one moment.".into())
                } else {
                    $s.loading = true;
                    $load($s.page.max(1))
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if $s.sel > 0 {
                    $s.sel -= 1;
                }
                Action::None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if $s.sel + 1 < $s.items.len() {
                    $s.sel += 1;
                }
                Action::None
            }
            KeyCode::Char('[') | KeyCode::PageUp => {
                if $s.loading {
                    return Action::Notice("Already loading \u{2014} one moment.".into());
                }
                if $s.page > 1 {
                    $s.loading = true;
                    $load($s.page - 1)
                } else {
                    Action::None
                }
            }
            KeyCode::Char(']') | KeyCode::PageDown => {
                if $s.loading {
                    return Action::Notice("Already loading \u{2014} one moment.".into());
                }
                if $s.page < $s.last_page {
                    $s.loading = true;
                    $load($s.page + 1)
                } else {
                    Action::None
                }
            }
            KeyCode::Enter | KeyCode::Char('l') | KeyCode::Char('o') => match $open {
                Some(url) => Action::OpenUrl(url),
                None => Action::Notice("Nothing selected.".into()),
            },
            _ => Action::None,
        }
    }};
}

/// Media Gallery keys (`Action::LoadMedia` fetches).
pub fn media_list_key(s: &mut super::MediaListState, key: KeyEvent) -> Action {
    let open = s.items.get(s.sel).and_then(|m| m.view_url.clone());
    list_keys!(s, key, open, Action::LoadMedia)
}

/// Resource Manager keys (`Action::LoadResources` fetches).
pub fn resources_list_key(s: &mut super::ResourceListState, key: KeyEvent) -> Action {
    let open = s.items.get(s.sel).and_then(|r| r.view_url.clone());
    list_keys!(s, key, open, Action::LoadResources)
}

/// The caps both catalogs advertise. State-aware: with nothing loaded "open"
/// is inert, and `[ ]` only pages once a second page is known — hide them
/// rather than advertise a dead key (#658/#604).
fn catalog_hints(empty: bool, last_page: u32) -> Hints {
    let mut keys: Vec<(&str, &str)> = vec![("j/k", "move")];
    if !empty {
        keys.push(("Enter", "open"));
        keys.push(("o", "open web"));
    }
    if last_page > 1 {
        keys.push(("[ ]", "page"));
    }
    keys.push(("R", "refresh"));
    keys.push(("Esc", "back"));
    Hints::new(&keys, if empty { 0 } else { 1 })
}

pub fn media_hints(s: &super::MediaListState) -> Hints {
    catalog_hints(s.items.is_empty(), s.last_page)
}

pub fn resources_hints(s: &super::ResourceListState) -> Hints {
    catalog_hints(s.items.is_empty(), s.last_page)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::glyph::UNICODE;
    use crate::screens::{MediaListState, ResourceListState};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    /// Render a catalog headless and return its rows as plain strings
    /// (styling dropped), for substring/column assertions.
    fn render_rows(
        w: u16,
        h: u16,
        draw: impl FnOnce(&mut ratatui::Frame, Rect, &Theme, &Glyphs, &mut HitMap),
    ) -> Vec<String> {
        let theme = Theme::truecolor();
        let mut hits = HitMap::default();
        let mut term = Terminal::new(TestBackend::new(w, h)).expect("test terminal");
        term.draw(|f| {
            let area = f.area();
            draw(f, area, &theme, &UNICODE, &mut hits);
        })
        .expect("draw");
        let buf = term.backend().buffer().clone();
        (0..h)
            .map(|y| {
                (0..w)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect()
    }

    fn media_state() -> MediaListState {
        MediaListState {
            items: vec![MediaListItem {
                media_id: 33005,
                title: "Registry Explained".into(),
                username: "op".into(),
                media_date: 1_700_000_000,
                view_url: Some("https://windowsforum.com/media/registry.33005/".into()),
            }],
            page: 1,
            last_page: 4,
            total: 80,
            ..Default::default()
        }
    }

    fn resources_state() -> ResourceListState {
        ResourceListState {
            items: vec![ResourceListItem {
                resource_id: 150883,
                title: "Handy tool".into(),
                tag_line: "does things".into(),
                username: "author".into(),
                resource_date: 1_700_000_000,
                view_url: Some("https://windowsforum.com/resources/handy.150883/".into()),
                download_count: 42,
                rating_average: Some(4.5),
            }],
            page: 2,
            last_page: 3,
            total: 60,
            ..Default::default()
        }
    }

    /// #680: both catalogs wear the house panel — a titled border, the
    /// `page N of M` right cap and the derived range along the bottom. They
    /// shipped as bare lists first, which left the user no way to see that
    /// pages 2..N existed even though `[`/`]` worked.
    #[test]
    fn the_catalogs_wear_the_house_panel_with_page_caps() {
        let mut s = media_state();
        let rows = render_rows(60, 8, |f, a, t, g, h| {
            render_media_gallery(&mut s, f, a, t, g, h)
        });
        assert!(rows[0].contains("Media Gallery"), "title: {:?}", rows[0]);
        assert!(rows[0].contains("page 1 of 4"), "right cap: {:?}", rows[0]);
        assert!(
            rows.last().expect("a bottom row").contains("1\u{2013}1 of 80"),
            "bottom cap: {:?}",
            rows.last()
        );
        assert!(
            rows[1].contains("Registry Explained") && rows[1].contains("op"),
            "row: {:?}",
            rows[1]
        );

        let mut r = resources_state();
        let rows = render_rows(60, 8, |f, a, t, g, h| render_resources(&mut r, f, a, t, g, h));
        assert!(rows[0].contains("Resources"), "title: {:?}", rows[0]);
        assert!(rows[0].contains("page 2 of 3"), "right cap: {:?}", rows[0]);
        assert!(rows[1].contains("Handy tool"), "row: {:?}", rows[1]);
        assert!(rows[1].contains("\u{2193} 42"), "downloads: {:?}", rows[1]);
        assert!(rows[1].contains("\u{2605} 4.5"), "rating: {:?}", rows[1]);
        assert!(rows[2].contains("does things"), "tag line: {:?}", rows[2]);
    }

    /// A first load draws the shared spinner, not a bare word — and an error
    /// names the retry key the bar advertises.
    #[test]
    fn the_first_load_spins_and_an_error_names_its_retry_key() {
        let mut s = MediaListState {
            loading: true,
            ..Default::default()
        };
        let rows = render_rows(60, 6, |f, a, t, g, h| {
            render_media_gallery(&mut s, f, a, t, g, h)
        });
        assert!(rows[1].contains("Loading media"), "{:?}", rows[1]);

        let mut s = MediaListState {
            error: Some("gateway timeout".into()),
            ..Default::default()
        };
        let rows = render_rows(60, 6, |f, a, t, g, h| {
            render_media_gallery(&mut s, f, a, t, g, h)
        });
        assert!(rows[1].contains("gateway timeout"), "{:?}", rows[1]);
        assert!(
            rows.iter().any(|r| r.contains("Press R to retry")),
            "{rows:?}"
        );
    }

    /// The title never runs into the meta run and the row is exactly the
    /// panel's inner width, measured in cells — a CJK title is the case that
    /// broke every `chars()`-based row builder in this app.
    #[test]
    fn a_cjk_title_row_keeps_exact_width() {
        let theme = Theme::truecolor();
        for w in [20usize, 40, 60] {
            let line = catalog_line(
                &theme,
                &"\u{6f22}\u{5b57}".repeat(30),
                "kemical \u{b7} 3d",
                w,
            );
            assert_eq!(
                chrome::cell_width(&line.spans.iter().map(|s| s.content.as_ref()).collect::<String>()),
                w,
                "width {w}"
            );
        }
    }

    /// #604's shape: `[ ]` is only advertised once a second page is known,
    /// and "open" only once there is something to open.
    #[test]
    fn the_caps_follow_the_state() {
        let empty = MediaListState::default();
        let caps: Vec<&str> = media_hints(&empty).keys.iter().map(|(k, _)| *k).collect();
        assert!(!caps.contains(&"Enter"), "{caps:?}");
        assert!(!caps.contains(&"[ ]"), "{caps:?}");

        let loaded = media_state();
        let caps: Vec<&str> = media_hints(&loaded).keys.iter().map(|(k, _)| *k).collect();
        assert!(caps.contains(&"Enter") && caps.contains(&"[ ]"), "{caps:?}");
    }

    /// Paging is bounded by `last_page` and refuses out loud while a fetch is
    /// in flight — a second `]` must not burn another `api_gate` slot.
    #[test]
    fn paging_is_bounded_and_refuses_while_loading() {
        let mut s = media_state();
        let act = media_list_key(&mut s, KeyEvent::from(KeyCode::Char(']')));
        assert!(matches!(act, Action::LoadMedia(2)), "] must fetch page 2");
        assert!(s.loading);
        let act = media_list_key(&mut s, KeyEvent::from(KeyCode::Char(']')));
        assert!(matches!(act, Action::Notice(_)), "a second ] must refuse out loud");

        let mut s = media_state();
        let act = media_list_key(&mut s, KeyEvent::from(KeyCode::Char('[')));
        assert!(matches!(act, Action::None), "page 1 has no previous page");
        assert!(!s.loading);

        let mut r = resources_state();
        r.page = r.last_page;
        let act = resources_list_key(&mut r, KeyEvent::from(KeyCode::Char(']')));
        assert!(matches!(act, Action::None), "the last page has no next page");
    }
}
