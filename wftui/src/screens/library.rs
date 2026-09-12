//! The library screens (issues #680, #697): XFMG's Media Gallery, XFRM's
//! Resource Manager, the resource page, and the full-size image viewer.
//!
//! The gallery mirrors the site's own shape — categories on the left, that
//! category's media on the right, thumbnails drawn on any graphics tier —
//! and both catalogs open their items *in the client* rather than handing
//! the reader a browser link: Enter on a media item shows the picture, Enter
//! on a resource renders its page from the same BBCode the site renders.
//! `o` is what opens the website.

use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState, Paragraph};
use ratatui::Frame;

use super::browse::{chunk_lines_aligned, truncate, wrap_spans, wrap_spans_aligned, RuleWidth};
use super::{Action, HitMap, MediaItem, Resource};
use crate::hit::{Hit, HitPane};
use crate::chrome::{self, Hints};
use crate::glyph::Glyphs;
use crate::images::{self, Request, Slot};
use crate::theme::{fmt_age, Theme};
use common::bbcode;
use common::models::MediaCategory;

/// Cells reserved for a gallery thumbnail. Small enough that four or five
/// items still fit a short pane, big enough to tell screenshots apart.
const THUMB_COLS: u16 = 12;
/// Rows one media row occupies with pictures on, and without them.
const ROW_H_IMAGE: u16 = 5;
const ROW_H_TEXT: u16 = 3;
/// Width of the category pane in the dual layout.
const CATEGORY_COLS: u16 = 28;
/// Below this the gallery drops to a single pane, like Home does.
const DUAL_COLS: u16 = 90;

/// `1–20 of 431` for a panel's bottom cap. The API's pagination carries no
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
    // The metadata is untrusted too (for example, a long resource author or
    // title).  Truncate it before reserving the title column; otherwise a
    // narrow panel can overflow even though the left side is cell-aware.
    let right = Span::styled(truncate(right_text, w), theme.dim());
    let right_w = right.width();
    let title_w = w.saturating_sub(right_w + 1);
    let left = Span::styled(truncate(title, title_w), theme.base());
    let pad = w.saturating_sub(left.width() + right_w);
    Line::from(vec![left, Span::raw(" ".repeat(pad)), right])
}

/// The panel a catalog draws into, plus the "nothing to draw yet" states
/// (loading spinner / error / empty). `Some(inner)` means: go on and draw
/// rows into this rect.
#[allow(clippy::too_many_arguments)]
fn catalog_panel(
    f: &mut Frame,
    area: Rect,
    theme: &Theme,
    g: &Glyphs,
    focused: bool,
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
    let block = chrome::panel(theme, g, title, focused, Some(&right), bottom.as_deref());
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

// ================= Media Gallery =================

/// Rows one media entry occupies. Scaled to the pane, not fixed: a short
/// pane packs compact rows so more of the page is reachable without
/// scrolling, a tall one gives each picture its full height, and the text
/// tier never reserves rows for a picture it cannot draw (#697).
fn media_row_height(policy: images::Policy, pane_rows: u16) -> u16 {
    if !policy.inline() {
        return ROW_H_TEXT;
    }
    // Four rows of thumbnail need a pane that can show at least a few of
    // them; below that the compact row wins over a bigger picture.
    if pane_rows >= ROW_H_IMAGE * 3 {
        ROW_H_IMAGE
    } else {
        ROW_H_TEXT + 1
    }
}

/// The category pane: "All media" plus every category the site lists, nested
/// under its parent the way the gallery nests them.
fn render_categories(
    s: &mut super::MediaListState,
    f: &mut Frame,
    area: Rect,
    theme: &Theme,
    g: &Glyphs,
    focused: bool,
    hits: &mut HitMap,
) {
    let block = chrome::panel(theme, g, "Categories", focused, None, None);
    let inner = block.inner(area);
    f.render_widget(block, area);
    s.cat_rect = area;
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    hits.push(inner, Hit::Pane(HitPane::Tree));

    let w = inner.width as usize;
    let mut items: Vec<ListItem> = Vec::with_capacity(s.categories.len() + 1);
    items.push(ListItem::new(Line::from(vec![
        Span::styled("All media", theme.base()),
    ])));
    for c in &s.categories {
        // One level of indent per depth, the way the site's own category
        // list steps children in under their parent.
        let depth = category_depth(&s.categories, c);
        let indent = " ".repeat((depth as usize).min(4));
        let count = format!(" {}", c.media_count);
        let title_w = w.saturating_sub(indent.len() + count.len());
        items.push(ListItem::new(Line::from(vec![
            Span::raw(indent),
            Span::styled(truncate(&c.title, title_w), theme.base()),
            Span::styled(count, theme.dim()),
        ])));
    }
    let mut state = ListState::default().with_selected(Some(s.cat_sel.min(items.len() - 1)));
    let list = List::new(items);
    let list = if focused {
        list.highlight_style(theme.selected())
    } else {
        list
    };
    let count = s.categories.len() + 1;
    f.render_stateful_widget(list, inner, &mut state);
    hits.rows(inner, state.offset(), count, Hit::Row);
}

/// How deep a category sits, by walking `parent_category_id` up. The list is
/// small (tens of rows) and this runs once per visible row.
fn category_depth(all: &[MediaCategory], c: &MediaCategory) -> u16 {
    let mut depth = 0u16;
    let mut parent = c.parent_category_id;
    while parent != 0 && depth < 8 {
        match all.iter().find(|p| p.category_id == parent) {
            Some(p) => {
                depth += 1;
                parent = p.parent_category_id;
            }
            None => break,
        }
    }
    depth
}

/// The media pane: one block per item — thumbnail on the left, title and
/// meta beside it. Drawn by hand rather than with `List` because the picture
/// slots need exact row coordinates, the same contract the thread view keeps.
fn render_media_items(
    s: &mut super::MediaListState,
    f: &mut Frame,
    area: Rect,
    theme: &Theme,
    g: &Glyphs,
    focused: bool,
    hits: &mut HitMap,
) {
    s.items_rect = area;
    let Some(inner) = catalog_panel(
        f,
        area,
        theme,
        g,
        focused,
        &s.category_title,
        "media",
        s.page,
        s.last_page,
        s.total,
        s.items.len(),
        s.loading,
        s.error.as_ref(),
    ) else {
        s.visible = 1;
        return;
    };
    hits.push(inner, Hit::Pane(HitPane::List));

    let row_h = media_row_height(s.images, inner.height);
    let visible = (inner.height / row_h).max(1) as usize;
    s.visible = visible;
    // Keep the selection on screen; the scroll is in items, not rows, so a
    // row never straddles the pane edge and no picture is half-drawn.
    if s.sel < s.scroll {
        s.scroll = s.sel;
    } else if s.sel >= s.scroll + visible {
        s.scroll = s.sel + 1 - visible;
    }
    s.scroll = s.scroll.min(s.items.len().saturating_sub(1));

    let text_x = if s.images.inline() { THUMB_COLS + 2 } else { 2 };
    let text_w = inner.width.saturating_sub(text_x) as usize;
    let mut slots: Vec<Slot> = Vec::new();

    for (row, item) in s.items.iter().enumerate().skip(s.scroll).take(visible) {
        let top = inner.y + ((row - s.scroll) as u16 * row_h);
        if top + row_h > inner.bottom() {
            break;
        }
        let selected = row == s.sel;
        let marker_style = if selected {
            Style::new().fg(theme.accent).add_modifier(Modifier::BOLD)
        } else {
            theme.faint()
        };
        let title_style = if selected {
            theme.selected()
        } else {
            theme.base()
        };
        // The whole block is the click target for this item.
        hits.push(
            Rect::new(inner.x, top, inner.width, row_h.min(inner.bottom() - top)),
            Hit::Row(row),
        );

        let marker = if selected { g.gutter } else { " " };
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(marker.to_string(), marker_style))),
            Rect::new(inner.x, top, 1, 1),
        );

        let x = inner.x + text_x;
        let mut lines: Vec<Line<'static>> = vec![Line::from(Span::styled(
            truncate(&item.title, text_w),
            title_style,
        ))];
        let mut meta = item.username.clone();
        if item.media_date > 0 {
            if !meta.is_empty() {
                meta.push_str(" \u{b7} ");
            }
            meta.push_str(&fmt_age(item.media_date));
        }
        lines.push(Line::from(Span::styled(truncate(&meta, text_w), theme.dim())));
        let mut stats = String::new();
        if item.view_count > 0 {
            stats.push_str(&format!("{} views", item.view_count));
        }
        if item.comment_count > 0 {
            if !stats.is_empty() {
                stats.push_str(" \u{b7} ");
            }
            stats.push_str(&format!("{} comments", item.comment_count));
        }
        if let Some((w, h)) = item.px() {
            if !stats.is_empty() {
                stats.push_str(" \u{b7} ");
            }
            stats.push_str(&format!("{w}\u{d7}{h}"));
        }
        if !stats.is_empty() {
            lines.push(Line::from(Span::styled(truncate(&stats, text_w), theme.faint())));
        }
        let text_rows = (row_h as usize).min(lines.len());
        f.render_widget(
            Paragraph::new(lines[..text_rows].to_vec()),
            Rect::new(x, top, inner.width.saturating_sub(text_x), text_rows as u16),
        );

        // The thumbnail, sized into its own box so every row lines up.
        if s.images.inline()
            && let Some(url) = item.thumbnail_url.as_deref()
        {
            let (cols, rows) = images::fit_within(
                THUMB_COLS,
                row_h.saturating_sub(1).max(1),
                item.px().unwrap_or((16, 9)),
                s.images.font,
            );
            slots.push(Slot {
                line: 0,
                x: 0,
                cols,
                rows,
                key: url.to_string(),
            });
            // Slots here are already absolute (this pane paints itself), so
            // the request is built directly rather than translated later.
            if let Some(slot) = slots.last() {
                s.image_requests.push(Request {
                    key: slot.key.clone(),
                    rect: Rect::new(inner.x + 1, top, cols, rows),
                full: false,
                });
            }
        }
    }
}

/// The Media Gallery: categories beside their media, like the site (#697).
pub fn render_media_gallery(
    s: &mut super::MediaListState,
    f: &mut Frame,
    area: Rect,
    theme: &Theme,
    g: &Glyphs,
    hits: &mut HitMap,
) {
    s.image_requests.clear();
    s.dual = area.width >= DUAL_COLS;
    if !s.dual {
        // Narrow: the categories are reachable with Tab, one pane at a time.
        match s.focus {
            super::MediaPane::Categories => {
                render_categories(s, f, area, theme, g, true, hits)
            }
            super::MediaPane::Items => render_media_items(s, f, area, theme, g, true, hits),
        }
        return;
    }
    let [left, right] =
        Layout::horizontal([Constraint::Length(CATEGORY_COLS), Constraint::Min(20)]).areas(area);
    let cats_focused = s.focus == super::MediaPane::Categories;
    render_categories(s, f, left, theme, g, cats_focused, hits);
    render_media_items(s, f, right, theme, g, !cats_focused, hits);
}

// ================= Resource Manager =================

/// The Resource Manager list: `title` with a dim
/// `author · ↓ downloads · ★ rating · age` run, plus the tag line underneath.
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
        true,
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
        .map(|r: &Resource| {
            let mut title = r.title.clone();
            if !r.version.is_empty() {
                title.push_str(&format!(" {}", r.version));
            }
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
            if let Some(rating) = r.rating_avg {
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
            let mut lines = vec![catalog_line(theme, &title, &meta, w)];
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

// ================= the resource page =================

/// Lay the resource out once per width: the header block, then the BBCode
/// body through the very same chunk-to-line path the thread view uses, so a
/// resource reads here exactly as it reads on the site (#697).
pub fn rebuild_resource_lines(s: &mut super::ResourceViewState, theme: &Theme, g: &Glyphs) {
    let width = if s.width == 0 { 80 } else { s.width } as usize;
    s.width = width as u16;
    s.lines.clear();
    s.links.clear();
    s.image_slots.clear();
    let Some(r) = s.resource.clone() else { return };

    let icon_cols = if s.images.inline() && r.icon_url.is_some() {
        images::ICON_COLS
    } else {
        0
    };
    let text_x = if icon_cols > 0 { icon_cols + 2 } else { 0 };
    let body_w = width.saturating_sub(text_x as usize).max(8);
    let indent = " ".repeat(text_x as usize);

    let mut head = r.title.clone();
    if !r.version.is_empty() {
        head.push_str(&format!("  {}", r.version));
    }
    s.lines.push(Line::from(vec![
        Span::raw(indent.clone()),
        Span::styled(truncate(&head, body_w), theme.title()),
    ]));
    if !r.tag_line.is_empty() {
        s.lines.push(Line::from(vec![
            Span::raw(indent.clone()),
            Span::styled(truncate(&r.tag_line, body_w), theme.dim()),
        ]));
    }

    let star = if g.ascii { "*" } else { "\u{2605}" };
    let arrow = if g.ascii { "v" } else { "\u{2193}" };
    let mut meta: Vec<String> = Vec::new();
    if !r.username.is_empty() {
        meta.push(format!("by {}", r.username));
    }
    if let Some(cat) = r.category.as_ref().filter(|c| !c.title.is_empty()) {
        meta.push(cat.title.clone());
    }
    if r.last_update > 0 {
        meta.push(format!("updated {}", fmt_age(r.last_update)));
    } else if r.resource_date > 0 {
        meta.push(fmt_age(r.resource_date));
    }
    if r.download_count > 0 {
        meta.push(format!("{arrow} {}", r.download_count));
    }
    if let Some(rating) = r.rating_avg {
        meta.push(match r.rating_count {
            0 => format!("{star} {rating:.1}"),
            n => format!("{star} {rating:.1} ({n})"),
        });
    }
    if r.review_count > 0 {
        meta.push(format!("{} reviews", r.review_count));
    }
    if r.view_count > 0 {
        meta.push(format!("{} views", r.view_count));
    }
    if !meta.is_empty() {
        s.lines.push(Line::from(vec![
            Span::raw(indent.clone()),
            Span::styled(truncate(&meta.join(" \u{b7} "), body_w), theme.faint()),
        ]));
    }

    // The icon sits over the header rows, like the thread view's avatar.
    if icon_cols > 0
        && let Some(url) = r.icon_url.as_deref()
    {
        s.image_slots.push(Slot {
            line: 0,
            x: 0,
            cols: icon_cols,
            rows: images::ICON_ROWS,
            key: url.to_string(),
        });
    }
    while s.lines.len() < images::ICON_ROWS as usize && icon_cols > 0 {
        s.lines.push(Line::from(Span::raw("")));
    }

    s.lines.push(Line::from(Span::raw("")));
    let rule = if g.ascii { "-" } else { "\u{2500}" };
    s.lines.push(Line::from(Span::styled(
        rule.repeat(width),
        theme.faint(),
    )));
    s.lines.push(Line::from(Span::raw("")));

    if r.description.is_empty() {
        s.lines.push(Line::from(Span::styled(
            "This resource has no description.",
            theme.dim(),
        )));
    } else {
        let chunks = bbcode::render(&r.description);
        for (logical, align) in
            chunk_lines_aligned(&chunks, &mut s.links, theme, true, RuleWidth::of(width, g))
        {
            for wrapped in wrap_spans_aligned(&logical, width, align) {
                s.lines.push(Line::from(wrapped));
            }
        }
    }

    if !r.tags.is_empty() {
        let mut tags = r.tags.clone();
        tags.sort();
        s.lines.push(Line::from(Span::raw("")));
        s.lines.push(Line::from(Span::styled(
            truncate(&format!("tags: {}", tags.join(", ")), width),
            theme.dim(),
        )));
    }
    if !s.links.is_empty() {
        s.lines.push(Line::from(Span::raw("")));
        s.link_lines.clear();
        for (i, url) in s.links.iter().enumerate() {
            // Remember which row is which link so a click on it opens that
            // link — the same contract the thread view keeps (#701).
            s.link_lines.push((s.lines.len(), i));
            s.lines.push(Line::from(Span::styled(
                truncate(&format!("[{}] {url}", i + 1), width),
                theme.dim(),
            )));
        }
    }
}

/// The resource page.
pub fn render_resource_view(
    s: &mut super::ResourceViewState,
    f: &mut Frame,
    area: Rect,
    theme: &Theme,
    g: &Glyphs,
    hits: &mut HitMap,
) {
    s.image_requests.clear();
    let title = s
        .resource
        .as_ref()
        .map(|r| r.title.clone())
        .unwrap_or_else(|| "Resource".to_string());
    let block = chrome::panel(theme, g, &title, true, None, None);
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    if s.loading && s.resource.is_none() {
        f.render_widget(
            Paragraph::new(format!(
                "{} Loading resource\u{2026}",
                chrome::spinner(g, chrome::spinner_tick())
            ))
            .style(theme.dim()),
            inner,
        );
        return;
    }
    if let Some(err) = &s.error {
        f.render_widget(
            Paragraph::new(format!("Error: {err}\n\nPress R to retry."))
                .style(Style::new().fg(theme.error)),
            inner,
        );
        return;
    }
    if s.width != inner.width {
        s.width = inner.width;
        rebuild_resource_lines(s, theme, g);
    }
    let max_scroll = s.lines.len().saturating_sub(inner.height as usize);
    s.scroll = s.scroll.min(max_scroll);
    f.render_widget(
        Paragraph::new(crate::editor::visible_window(
            &s.lines,
            s.scroll,
            inner.height,
        )),
        inner,
    );
    hits.push(inner, Hit::Pane(HitPane::List));
    // Link rows, wherever the scroll has put them.
    for &(line, idx) in &s.link_lines {
        if line < s.scroll || line >= s.scroll + inner.height as usize {
            continue;
        }
        if let Some(url) = s.links.get(idx) {
            let y = inner.y + (line - s.scroll) as u16;
            hits.push(
                Rect::new(inner.x, y, inner.width, 1),
                Hit::Link(url.clone()),
            );
        }
    }

    if s.images.inline() {
        for slot in &s.image_slots {
            if slot.line < s.scroll {
                continue;
            }
            let y = inner.y as usize + (slot.line - s.scroll);
            if y + slot.rows as usize > inner.bottom() as usize {
                continue;
            }
            s.image_requests.push(Request {
                key: slot.key.clone(),
                rect: Rect::new(inner.x + slot.x, y as u16, slot.cols, slot.rows),
                full: false,
            });
        }
    }
}

// ================= the image viewer =================

/// One picture, as large as the pane allows, with its caption underneath —
/// the "open it here, not in a browser" screen (#697, and CLAUDE.md's
/// long-standing "Enter-to-expand an image is not implemented" gap).
pub fn render_image_view(
    s: &mut super::ImageViewState,
    f: &mut Frame,
    area: Rect,
    theme: &Theme,
    g: &Glyphs,
    hits: &mut HitMap,
) {
    s.image_requests.clear();
    let block = chrome::panel(theme, g, &s.title, true, None, None);
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    hits.push(inner, Hit::Pane(HitPane::List));

    if s.loading && s.key.is_none() {
        f.render_widget(
            Paragraph::new(format!(
                "{} Loading\u{2026}",
                chrome::spinner(g, chrome::spinner_tick())
            ))
            .style(theme.dim()),
            inner,
        );
        return;
    }
    if let Some(err) = &s.error {
        f.render_widget(
            Paragraph::new(format!("Error: {err}\n\nPress R to retry."))
                .style(Style::new().fg(theme.error)),
            inner,
        );
        return;
    }

    // Caption first: it is the part that must never be pushed off screen.
    let mut caption: Vec<Line<'static>> = Vec::new();
    if !s.meta.is_empty() {
        caption.push(Line::from(Span::styled(
            truncate(&s.meta, inner.width as usize),
            theme.dim(),
        )));
    }
    if !s.description.is_empty() {
        for wrapped in wrap_spans(
            &[Span::styled(s.description.clone(), theme.base())],
            inner.width as usize,
        )
        .into_iter()
        .take(3)
        {
            caption.push(Line::from(wrapped));
        }
    }
    let cap_rows = (caption.len() as u16).min(inner.height.saturating_sub(1));
    let pic_rows = inner.height.saturating_sub(cap_rows);

    if !s.images.inline() {
        let mut lines = vec![Line::from(Span::styled(
            "This terminal has no inline graphics \u{2014} press o to open it in a browser.",
            theme.dim(),
        ))];
        lines.extend(caption);
        f.render_widget(Paragraph::new(lines), inner);
        return;
    }

    if let Some(key) = s.key.clone()
        && pic_rows > 0
    {
        let (cols, rows) = images::fit_within(
            inner.width,
            pic_rows,
            s.px.unwrap_or((16, 9)),
            s.images.font,
        );
        // Centred horizontally: a picture pinned left in a wide pane reads
        // as a mistake rather than a viewer.
        let x = inner.x + (inner.width.saturating_sub(cols)) / 2;
        s.image_requests.push(Request {
            key,
            rect: Rect::new(x, inner.y, cols, rows),
            // The viewer's picture, not a thumbnail: the larger byte budget.
            full: true,
        });
    }
    if cap_rows > 0 {
        f.render_widget(
            Paragraph::new(caption),
            Rect::new(inner.x, inner.bottom() - cap_rows, inner.width, cap_rows),
        );
    }
}

// ================= keys =================

/// Shared paging/refresh arms (#657 discipline: the firing keys refuse out
/// loud while a load is in flight). `$load(page)` builds the fetch for this
/// screen.
macro_rules! paging_keys {
    ($s:expr, $key:expr, $load:expr) => {
        match $key.code {
            KeyCode::Char('R') | KeyCode::F(5) => {
                if $s.loading {
                    return Action::Notice("Already loading \u{2014} one moment.".into());
                }
                $s.loading = true;
                return $load($s.page.max(1));
            }
            KeyCode::Char('[') | KeyCode::PageUp => {
                if $s.loading {
                    return Action::Notice("Already loading \u{2014} one moment.".into());
                }
                if $s.page > 1 {
                    $s.loading = true;
                    return $load($s.page - 1);
                }
                return Action::None;
            }
            KeyCode::Char(']') | KeyCode::PageDown => {
                if $s.loading {
                    return Action::Notice("Already loading \u{2014} one moment.".into());
                }
                if $s.page < $s.last_page {
                    $s.loading = true;
                    return $load($s.page + 1);
                }
                return Action::None;
            }
            _ => {}
        }
    };
}

/// What Enter on a media item opens: the picture, in the client.
pub(crate) fn image_open_for(item: &MediaItem) -> Option<Box<super::ImageOpen>> {
    let key = item
        .media_url
        .clone()
        .or_else(|| item.thumbnail_url.clone())?;
    let mut meta = item.username.clone();
    if item.media_date > 0 {
        if !meta.is_empty() {
            meta.push_str(" \u{b7} ");
        }
        meta.push_str(&fmt_age(item.media_date));
    }
    if let Some((w, h)) = item.px() {
        meta.push_str(&format!(" \u{b7} {w}\u{d7}{h}"));
    }
    if item.view_count > 0 {
        meta.push_str(&format!(" \u{b7} {} views", item.view_count));
    }
    Some(Box::new(super::ImageOpen {
        title: item.title.clone(),
        meta,
        description: item.description.clone(),
        key,
        px: item.px(),
        web_url: item.view_url.clone(),
    }))
}

pub fn media_list_key(s: &mut super::MediaListState, key: KeyEvent) -> Action {
    if matches!(key.code, KeyCode::Tab | KeyCode::BackTab) {
        s.focus = match s.focus {
            super::MediaPane::Categories => super::MediaPane::Items,
            super::MediaPane::Items => super::MediaPane::Categories,
        };
        return Action::None;
    }
    if s.focus == super::MediaPane::Categories {
        return match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                s.cat_sel = s.cat_sel.saturating_sub(1);
                Action::None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if s.cat_sel < s.categories.len() {
                    s.cat_sel += 1;
                }
                Action::None
            }
            KeyCode::Enter | KeyCode::Char('l') | KeyCode::Right => {
                let (id, title) = match s.cat_sel.checked_sub(1).and_then(|i| s.categories.get(i)) {
                    Some(c) => (Some(c.category_id), c.title.clone()),
                    None => (None, "All media".to_string()),
                };
                s.category = id;
                s.category_title = title;
                s.focus = super::MediaPane::Items;
                s.loading = true;
                s.sel = 0;
                s.scroll = 0;
                Action::LoadMedia { category: id, page: 1 }
            }
            KeyCode::Char('R') | KeyCode::F(5) => Action::LoadMediaCategories,
            KeyCode::Char('q') => Action::PopScreen,
            _ => Action::None,
        };
    }

    paging_keys!(s, key, |page| Action::LoadMedia {
        category: s.category,
        page
    });
    match key.code {
        KeyCode::Char('q') => Action::PopScreen,
        KeyCode::Char('h') | KeyCode::Left => {
            s.focus = super::MediaPane::Categories;
            Action::None
        }
        KeyCode::Up | KeyCode::Char('k') => {
            if s.sel > 0 {
                s.sel -= 1;
            }
            Action::None
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if s.sel + 1 < s.items.len() {
                s.sel += 1;
            }
            Action::None
        }
        KeyCode::Enter | KeyCode::Char('l') => match s.items.get(s.sel) {
            Some(item) if item.is_image() => match image_open_for(item) {
                Some(open) => Action::OpenImage(open),
                None => Action::Notice("That item carries no image.".into()),
            },
            Some(item) => match item.view_url.clone() {
                // Video, audio and embeds are not ours to play: the site is.
                Some(url) => Action::OpenUrl(url),
                None => Action::Notice("Nothing to open.".into()),
            },
            None => Action::Notice("Nothing selected.".into()),
        },
        KeyCode::Char('o') => match s.items.get(s.sel).and_then(|i| i.view_url.clone()) {
            Some(url) => Action::OpenUrl(url),
            None => Action::Notice("Nothing selected.".into()),
        },
        _ => Action::None,
    }
}

pub fn resources_list_key(s: &mut super::ResourceListState, key: KeyEvent) -> Action {
    paging_keys!(s, key, Action::LoadResources);
    match key.code {
        KeyCode::Char('q') | KeyCode::Char('h') | KeyCode::Left => Action::PopScreen,
        KeyCode::Up | KeyCode::Char('k') => {
            if s.sel > 0 {
                s.sel -= 1;
            }
            Action::None
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if s.sel + 1 < s.items.len() {
                s.sel += 1;
            }
            Action::None
        }
        // Enter opens the resource *here* (#697); `o` is what opens the site.
        KeyCode::Enter | KeyCode::Char('l') => match s.items.get(s.sel) {
            Some(r) => Action::OpenResource(r.resource_id),
            None => Action::Notice("Nothing selected.".into()),
        },
        KeyCode::Char('o') => match s.items.get(s.sel).and_then(|r| r.view_url.clone()) {
            Some(url) => Action::OpenUrl(url),
            None => Action::Notice("Nothing selected.".into()),
        },
        _ => Action::None,
    }
}

pub fn resource_view_key(s: &mut super::ResourceViewState, key: KeyEvent) -> Action {
    match key.code {
        KeyCode::Char('q') | KeyCode::Char('h') | KeyCode::Left => Action::PopScreen,
        KeyCode::Up | KeyCode::Char('k') => {
            s.scroll = s.scroll.saturating_sub(1);
            Action::None
        }
        KeyCode::Down | KeyCode::Char('j') => {
            s.scroll = s.scroll.saturating_add(1).min(s.lines.len().saturating_sub(1));
            Action::None
        }
        KeyCode::PageUp => {
            s.scroll = s.scroll.saturating_sub(10);
            Action::None
        }
        KeyCode::PageDown => {
            s.scroll = s.scroll.saturating_add(10).min(s.lines.len().saturating_sub(1));
            Action::None
        }
        KeyCode::Char('R') | KeyCode::F(5) => {
            if s.loading {
                Action::Notice("Already loading \u{2014} one moment.".into())
            } else {
                s.loading = true;
                s.error = None;
                Action::LoadResource(s.id)
            }
        }
        // The download is a browser job — this client does not save files.
        KeyCode::Char('d') => match s.resource.as_ref().and_then(|r| {
            r.current_download_url
                .clone()
                .or_else(|| r.external_url.clone())
        }) {
            Some(url) => Action::OpenUrl(url),
            None => Action::Notice("This resource has no download link.".into()),
        },
        KeyCode::Char('o') => match s.resource.as_ref().and_then(|r| r.view_url.clone()) {
            Some(url) => Action::OpenUrl(url),
            None => Action::Notice("No web address for this resource.".into()),
        },
        _ => Action::None,
    }
}

pub fn image_view_key(s: &mut super::ImageViewState, key: KeyEvent) -> Action {
    match key.code {
        KeyCode::Char('q') | KeyCode::Char('h') | KeyCode::Left => Action::PopScreen,
        KeyCode::Char('o') => match s.web_url.clone() {
            Some(url) => Action::OpenUrl(url),
            None => Action::Notice("No web address for this image.".into()),
        },
        _ => Action::None,
    }
}

// ================= hints =================

fn catalog_hints(empty: bool, last_page: u32, extra: &[(&'static str, &'static str)]) -> Hints {
    let mut keys: Vec<(&str, &str)> = vec![("j/k", "move")];
    if !empty {
        keys.push(("Enter", "open"));
        keys.extend_from_slice(extra);
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
    if s.focus == super::MediaPane::Categories {
        return Hints::new(
            &[
                ("j/k", "move"),
                ("Enter", "browse"),
                ("Tab", "pane"),
                ("R", "refresh"),
                ("Esc", "back"),
            ],
            1,
        );
    }
    let mut keys: Vec<(&str, &str)> = vec![("j/k", "move")];
    if !s.items.is_empty() {
        keys.push(("Enter", "view"));
        keys.push(("o", "open web"));
    }
    if s.last_page > 1 {
        keys.push(("[ ]", "page"));
    }
    keys.push(("Tab", "pane"));
    keys.push(("R", "refresh"));
    keys.push(("Esc", "back"));
    // DESIGN.md: below 90 columns a screen supplies its own short set rather
    // than letting the bar clip mid-word.
    let mut short: Vec<(&str, &str)> = vec![("j/k", "")];
    if !s.items.is_empty() {
        short.push(("Enter", "view"));
    }
    short.push(("Tab", "pane"));
    short.push(("Esc", "back"));
    Hints::with_short(&keys, &short, if s.items.is_empty() { 0 } else { 1 })
}

pub fn resources_hints(s: &super::ResourceListState) -> Hints {
    catalog_hints(s.items.is_empty(), s.last_page, &[])
}

pub fn resource_view_hints(s: &super::ResourceViewState) -> Hints {
    let mut keys: Vec<(&str, &str)> = vec![("j/k", "scroll")];
    if s.resource.is_some() {
        if s.resource
            .as_ref()
            .is_some_and(|r| r.current_download_url.is_some() || r.external_url.is_some())
        {
            keys.push(("d", "download"));
        }
        keys.push(("o", "open web"));
    }
    keys.push(("R", "refresh"));
    keys.push(("Esc", "back"));
    Hints::new(&keys, 0)
}

pub fn image_view_hints(s: &super::ImageViewState) -> Hints {
    let mut keys: Vec<(&str, &str)> = Vec::new();
    if s.web_url.is_some() {
        keys.push(("o", "open web"));
    }
    keys.push(("Esc", "back"));
    Hints::new(&keys, 0)
}



// ================= the drafts list (#716) =================

/// Every unsent draft, in one place.
///
/// Before this a draft could only be seen by reopening the exact composer that
/// made it, so a new-thread draft in a forum you were not looking at was
/// effectively invisible and "what am I part-way through?" had no answer.
pub fn render_drafts(
    s: &mut super::DraftsState,
    f: &mut Frame,
    area: Rect,
    theme: &Theme,
    g: &Glyphs,
    hits: &mut HitMap,
) {
    let bottom = match s.rows.len() {
        0 => String::new(),
        1 => "1 draft".to_string(),
        n => format!("{n} drafts"),
    };
    let block = chrome::panel(
        theme,
        g,
        "Drafts",
        true,
        None,
        if bottom.is_empty() { None } else { Some(bottom.as_str()) },
    );
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    if s.rows.is_empty() {
        f.render_widget(
            Paragraph::new(vec![
                Line::from(Span::styled("No unsent drafts.", theme.dim())),
                Line::from(""),
                Line::from(Span::styled(
                    "Press Esc in a composer and whatever you had written is kept here.",
                    theme.dim(),
                )),
            ])
            .wrap(ratatui::widgets::Wrap { trim: false }),
            inner,
        );
        return;
    }

    let w = inner.width as usize;
    let items: Vec<ListItem> = s
        .rows
        .iter()
        .map(|r| {
            let label = if r.label.is_empty() {
                "(untitled)".to_string()
            } else {
                r.label.clone()
            };
            let mut meta = fmt_age(r.saved_at);
            if !r.shared {
                // An edit has no XF draft to sync to, so say why this one is
                // not on the website rather than let it look like a failure.
                meta.push_str(" \u{b7} this device only");
            }
            let mut lines = vec![catalog_line(theme, &label, &meta, w)];
            if !r.preview.is_empty() {
                lines.push(Line::from(Span::styled(
                    format!("   {}", truncate(&r.preview, w.saturating_sub(3))),
                    theme.dim(),
                )));
            }
            ListItem::new(lines)
        })
        .collect();
    let mut state = ListState::default().with_selected(Some(s.sel.min(s.rows.len() - 1)));
    let heights: Vec<u16> = s
        .rows
        .iter()
        .map(|r| if r.preview.is_empty() { 1 } else { 2 })
        .collect();
    f.render_stateful_widget(
        List::new(items).highlight_style(theme.selected()),
        inner,
        &mut state,
    );
    hits.list(inner, state.offset(), &heights, Hit::Row);
}

pub fn drafts_hints(s: &super::DraftsState) -> Hints {
    if s.rows.is_empty() {
        return Hints::new(&[("Esc", "back")], 0);
    }
    Hints::new(
        &[
            ("Enter", "resume"),
            ("D", "delete"),
            ("j/k", "move"),
            ("Esc", "back"),
        ],
        0,
    )
}

pub fn drafts_key(s: &mut super::DraftsState, key: KeyEvent) -> Action {
    if s.rows.is_empty() {
        return Action::None;
    }
    match key.code {
        KeyCode::Char('j') | KeyCode::Down => {
            s.sel = (s.sel + 1).min(s.rows.len().saturating_sub(1));
            Action::None
        }
        KeyCode::Char('k') | KeyCode::Up => {
            s.sel = s.sel.saturating_sub(1);
            Action::None
        }
        KeyCode::Enter => s
            .rows
            .get(s.sel)
            .map(|r| Action::ResumeDraft(r.key))
            .unwrap_or(Action::None),
        // Capital `D`, matching the thread view's delete: a lowercase key
        // next to `j`/`k` would throw away someone's writing on a mistype.
        KeyCode::Char('D') => s
            .rows
            .get(s.sel)
            .map(|r| Action::DropDraft(r.key))
            .unwrap_or(Action::None),
        _ => Action::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::glyph::UNICODE;
    use crate::images::{Policy, Tier};
    use crate::screens::{
        ImageViewState, MediaListState, MediaPane, ResourceListState, ResourceViewState,
    };
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn inline_policy() -> Policy {
        Policy { tier: Tier::Kitty, font: (10, 20) }
    }

    /// Render headless and return the rows as plain strings (styling
    /// dropped), for substring/column assertions.
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

    fn media(id: u32, title: &str) -> MediaItem {
        MediaItem {
            media_id: id,
            title: title.into(),
            username: "op".into(),
            media_date: 1_700_000_000,
            view_url: Some(format!("https://windowsforum.com/media/x.{id}/")),
            media_type: "image".into(),
            thumbnail_url: Some(format!("https://data.windowsforum.com/t/{id}.jpg")),
            media_url: Some(format!("https://windowsforum.com/api/media/{id}/data")),
            width: Some(1536),
            height: Some(1024),
            description: "A screenshot.".into(),
            view_count: 18,
            comment_count: 2,
            category_id: 11,
            ..Default::default()
        }
    }

    fn gallery() -> MediaListState {
        MediaListState {
            items: vec![media(1, "First shot"), media(2, "Second shot")],
            categories: vec![
                MediaCategory {
                    category_id: 11,
                    title: "Windows Forum".into(),
                    media_count: 18842,
                    ..Default::default()
                },
                MediaCategory {
                    category_id: 12,
                    title: "Screenshots".into(),
                    parent_category_id: 11,
                    media_count: 40,
                    ..Default::default()
                },
            ],
            category_title: "All media".into(),
            page: 1,
            last_page: 4,
            total: 80,
            focus: MediaPane::Items,
            ..Default::default()
        }
    }

    fn resource(id: u32) -> Resource {
        Resource {
            resource_id: id,
            title: "Handy tool".into(),
            tag_line: "does things".into(),
            username: "author".into(),
            version: "2.5.8".into(),
            resource_date: 1_700_000_000,
            last_update: 1_700_000_000,
            view_url: Some("https://windowsforum.com/resources/handy.1/".into()),
            current_download_url: Some("https://windowsforum.com/resources/handy.1/download".into()),
            download_count: 5678,
            view_count: 19017,
            rating_avg: Some(5.0),
            rating_count: 2,
            description: "A [B]useful[/B] tool.\nSecond paragraph.".into(),
            icon_url: Some("https://data.windowsforum.com/icons/1.jpg".into()),
            ..Default::default()
        }
    }

    /// #697: the gallery shows the site's own shape — categories beside the
    /// media — and each row carries its title, uploader and stats.
    #[test]
    fn the_gallery_draws_categories_beside_the_media() {
        let mut s = gallery();
        let rows = render_rows(120, 20, |f, a, t, g, h| {
            render_media_gallery(&mut s, f, a, t, g, h)
        });
        assert!(s.dual, "120 columns is a dual-pane gallery");
        let all = rows.join("\n");
        assert!(all.contains("Categories"), "{all}");
        assert!(all.contains("All media"), "{all}");
        assert!(all.contains("Windows Forum"), "{all}");
        assert!(all.contains("18842"), "category counts: {all}");
        assert!(all.contains("First shot") && all.contains("Second shot"), "{all}");
        assert!(all.contains("18 views"), "row stats: {all}");
        assert!(all.contains("1536\u{d7}1024"), "row dimensions: {all}");
        assert!(all.contains("page 1 of 4"), "pagination cap: {all}");
    }

    /// Narrow terminals drop to one pane, like Home does.
    #[test]
    fn a_narrow_gallery_shows_one_pane_at_a_time() {
        let mut s = gallery();
        let rows = render_rows(70, 16, |f, a, t, g, h| {
            render_media_gallery(&mut s, f, a, t, g, h)
        });
        assert!(!s.dual);
        let all = rows.join("\n");
        assert!(all.contains("First shot"), "{all}");
        assert!(!all.contains("Categories"), "the category pane must be hidden: {all}");
    }

    /// On a graphics tier every visible row reserves its thumbnail, inside
    /// the thumbnail column and inside the pane; the text tier reserves none.
    #[test]
    fn gallery_rows_reserve_thumbnails_only_on_a_graphics_tier() {
        let mut s = gallery();
        s.images = inline_policy();
        let _ = render_rows(120, 20, |f, a, t, g, h| {
            render_media_gallery(&mut s, f, a, t, g, h)
        });
        assert_eq!(s.image_requests.len(), 2, "one thumbnail per visible row");
        for req in &s.image_requests {
            assert!(req.rect.width <= THUMB_COLS, "{:?}", req.rect);
            assert!(req.rect.height <= ROW_H_IMAGE, "{:?}", req.rect);
            assert!(req.rect.right() <= 120 && req.rect.bottom() <= 20, "{:?}", req.rect);
            assert!(req.key.contains("/t/"), "the thumbnail, not the full image: {}", req.key);
        }

        let mut s = gallery();
        s.images = Policy::default();
        let _ = render_rows(120, 20, |f, a, t, g, h| {
            render_media_gallery(&mut s, f, a, t, g, h)
        });
        assert!(s.image_requests.is_empty(), "the text tier paints nothing");
    }

    /// Enter on a media item opens it *here* — the viewer, with the meta the
    /// caption needs — rather than handing the reader a browser link (#697).
    #[test]
    fn enter_on_a_media_item_opens_the_viewer_and_o_opens_the_site() {
        let mut s = gallery();
        let act = media_list_key(&mut s, KeyEvent::from(KeyCode::Enter));
        let Action::OpenImage(open) = act else {
            panic!("Enter must open the image viewer");
        };
        assert_eq!(open.title, "First shot");
        assert!(open.key.ends_with("/data"), "the full image: {}", open.key);
        assert!(open.meta.contains("op") && open.meta.contains("1536\u{d7}1024"), "{}", open.meta);
        assert_eq!(open.px, Some((1536, 1024)));

        let mut s = gallery();
        let act = media_list_key(&mut s, KeyEvent::from(KeyCode::Char('o')));
        assert!(
            matches!(act, Action::OpenUrl(url) if url.contains("/media/x.1/")),
            "o still opens the website"
        );
    }

    /// A non-image item (video, audio, an embed) is the site's job.
    #[test]
    fn a_non_image_item_falls_back_to_the_website() {
        let mut s = gallery();
        s.items[0].media_type = "video".into();
        let act = media_list_key(&mut s, KeyEvent::from(KeyCode::Enter));
        assert!(matches!(act, Action::OpenUrl(_)), "video opens on the site");
    }

    /// Choosing a category browses it, and Tab moves between the panes.
    #[test]
    fn choosing_a_category_loads_that_category() {
        let mut s = gallery();
        media_list_key(&mut s, KeyEvent::from(KeyCode::Tab));
        assert_eq!(s.focus, MediaPane::Categories);
        // Row 0 is "All media"; row 1 is the first real category.
        media_list_key(&mut s, KeyEvent::from(KeyCode::Down));
        let act = media_list_key(&mut s, KeyEvent::from(KeyCode::Enter));
        assert!(
            matches!(act, Action::LoadMedia { category: Some(11), page: 1 }),
            "Enter must browse the selected category"
        );
        assert_eq!(s.category_title, "Windows Forum");
        assert_eq!(s.focus, MediaPane::Items, "and hand the keyboard to the items");

        // Back up to "All media" and the scope clears again.
        media_list_key(&mut s, KeyEvent::from(KeyCode::Tab));
        media_list_key(&mut s, KeyEvent::from(KeyCode::Up));
        let act = media_list_key(&mut s, KeyEvent::from(KeyCode::Enter));
        assert!(matches!(act, Action::LoadMedia { category: None, page: 1 }));
    }

    /// #697: Enter on a resource opens its page in the client; `o` is what
    /// opens the website.
    #[test]
    fn enter_on_a_resource_opens_the_page_here() {
        let mut s = ResourceListState {
            items: vec![resource(150883)],
            page: 1,
            last_page: 1,
            ..Default::default()
        };
        let act = resources_list_key(&mut s, KeyEvent::from(KeyCode::Enter));
        assert!(matches!(act, Action::OpenResource(150883)), "Enter opens it here");
        let act = resources_list_key(&mut s, KeyEvent::from(KeyCode::Char('o')));
        assert!(matches!(act, Action::OpenUrl(_)), "o opens the website");
    }

    /// The resource page carries what the site's page carries: title with
    /// version, tag line, the meta run, and the BBCode body rendered — not
    /// its source.
    #[test]
    fn the_resource_page_renders_its_body_and_meta() {
        let mut s = ResourceViewState {
            id: 1,
            resource: Some(resource(1)),
            ..Default::default()
        };
        let rows = render_rows(90, 24, |f, a, t, g, h| {
            render_resource_view(&mut s, f, a, t, g, h)
        });
        let all = rows.join("\n");
        assert!(all.contains("Handy tool"), "{all}");
        assert!(all.contains("2.5.8"), "version: {all}");
        assert!(all.contains("does things"), "tag line: {all}");
        assert!(all.contains("by author"), "author: {all}");
        assert!(all.contains("5678"), "downloads: {all}");
        assert!(all.contains("\u{2605} 5.0 (2)"), "rating: {all}");
        assert!(all.contains("A useful tool."), "the body renders: {all}");
        assert!(!all.contains("[B]"), "BBCode source must not leak: {all}");
    }

    /// `d` follows the download, `o` the page — and each refuses out loud
    /// when the resource has no such link.
    #[test]
    fn the_resource_page_downloads_and_opens() {
        let mut s = ResourceViewState {
            id: 1,
            resource: Some(resource(1)),
            ..Default::default()
        };
        let act = resource_view_key(&mut s, KeyEvent::from(KeyCode::Char('d')));
        assert!(matches!(act, Action::OpenUrl(url) if url.ends_with("/download")));

        s.resource.as_mut().unwrap().current_download_url = None;
        let act = resource_view_key(&mut s, KeyEvent::from(KeyCode::Char('d')));
        assert!(matches!(act, Action::Notice(_)), "no link must say so");
    }

    /// The viewer fills the pane with the picture and keeps the caption on
    /// screen underneath it.
    #[test]
    fn the_image_viewer_fills_the_pane_and_keeps_its_caption() {
        let mut s = ImageViewState {
            title: "First shot".into(),
            meta: "op \u{b7} 3d \u{b7} 1536\u{d7}1024".into(),
            description: "A screenshot.".into(),
            key: Some("https://data.windowsforum.com/x.jpg".into()),
            px: Some((1536, 1024)),
            web_url: Some("https://windowsforum.com/media/x.1/".into()),
            images: inline_policy(),
            ..Default::default()
        };
        let rows = render_rows(100, 30, |f, a, t, g, h| {
            render_image_view(&mut s, f, a, t, g, h)
        });
        assert_eq!(s.image_requests.len(), 1, "one picture");
        let r = s.image_requests[0].rect;
        assert!(r.width > 40, "the viewer is not a thumbnail: {r:?}");
        assert!(r.right() <= 100 && r.bottom() <= 30, "inside the pane: {r:?}");
        let all = rows.join("\n");
        assert!(all.contains("First shot"), "title: {all}");
        assert!(all.contains("1536\u{d7}1024"), "caption: {all}");

        // Text tier: no picture, but the caption and the way out.
        let mut s = ImageViewState {
            title: "First shot".into(),
            key: Some("https://data.windowsforum.com/x.jpg".into()),
            ..Default::default()
        };
        let rows = render_rows(100, 30, |f, a, t, g, h| {
            render_image_view(&mut s, f, a, t, g, h)
        });
        assert!(s.image_requests.is_empty());
        assert!(rows.join("\n").contains("press o"), "{rows:?}");
    }

    /// #701: the resource page's link rows are click targets, wherever the
    /// scroll has put them — a URL you can see but not click is a URL you
    /// have to retype.
    #[test]
    fn resource_page_link_rows_are_clickable() {
        let theme = Theme::truecolor();
        let mut hits = HitMap::default();
        let mut s = ResourceViewState {
            id: 1,
            resource: Some(Resource {
                description: "See [URL=https://example.com/docs]the docs[/URL] first.".into(),
                ..resource(1)
            }),
            ..Default::default()
        };
        let mut term = Terminal::new(TestBackend::new(90, 24)).expect("terminal");
        term.draw(|f| {
            let area = f.area();
            render_resource_view(&mut s, f, area, &theme, &UNICODE, &mut hits);
        })
        .expect("draw");

        assert!(!s.links.is_empty(), "the body carried a link");
        let found = (0..24u16).any(|y| {
            (0..90u16).any(|x| {
                matches!(hits.at(x, y), Some(Hit::Link(u)) if u == "https://example.com/docs")
            })
        });
        assert!(found, "the [1] url row must be clickable");
    }

    /// Paging is bounded by `last_page` and refuses out loud while a fetch
    /// is in flight — a second `]` must not burn another `api_gate` slot.
    #[test]
    fn paging_is_bounded_and_refuses_while_loading() {
        let mut s = gallery();
        let act = media_list_key(&mut s, KeyEvent::from(KeyCode::Char(']')));
        assert!(matches!(act, Action::LoadMedia { page: 2, .. }), "] fetches page 2");
        assert!(s.loading);
        let act = media_list_key(&mut s, KeyEvent::from(KeyCode::Char(']')));
        assert!(matches!(act, Action::Notice(_)), "a second ] refuses out loud");

        let mut s = gallery();
        let act = media_list_key(&mut s, KeyEvent::from(KeyCode::Char('[')));
        assert!(matches!(act, Action::None), "page 1 has no previous page");
    }

    /// The title never runs into the meta run and the row is exactly the
    /// panel's inner width, measured in cells — a CJK title is the case that
    /// broke every `chars()`-based row builder in this app.
    #[test]
    fn a_cjk_title_row_keeps_exact_width() {
        let theme = Theme::truecolor();
        for w in [20usize, 40, 60] {
            let line = catalog_line(&theme, &"\u{6f22}\u{5b57}".repeat(30), "kemical \u{b7} 3d", w);
            let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            assert_eq!(chrome::cell_width(&text), w, "width {w}");
        }
    }

    #[test]
    fn an_overlong_catalog_meta_run_cannot_overflow_the_panel() {
        let theme = Theme::truecolor();
        for w in [1usize, 4, 10, 20] {
            let line = catalog_line(
                &theme,
                "short",
                &"\u{4f5c}\u{8005}".repeat(30),
                w,
            );
            let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            assert_eq!(chrome::cell_width(&text), w, "width {w}");
        }
    }
}
