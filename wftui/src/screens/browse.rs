//! Browsing screens: forum tree, thread list, thread view.

use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};

use common::bbcode::{self, Chunk};
use common::models::Node;

use super::{link_style, style_from, Action, ThreadListState, ThreadViewState};
use crate::app::footer_line;
use crate::theme::{fmt_time, Theme};

const NEWS_NODE: u32 = 4;
const SECURITY_NODE: u32 = 84;
const TUTORIALS_NODE: u32 = 305;

// ================= forum tree =================

pub fn forum_tree_key(s: &mut super::ForumTreeState, key: KeyEvent) -> Action {
    match key.code {
        KeyCode::Up | KeyCode::Char('k') => {
            if s.sel > 0 {
                s.sel -= 1;
            }
            Action::None
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if s.sel + 1 < s.nodes.len() {
                s.sel += 1;
            }
            Action::None
        }
        KeyCode::Left | KeyCode::Char('h') => {
            // Path up to parent node in tree hierarchy
            if let Some(node) = s.nodes.get(s.sel)
                && node.parent_node_id > 0
                && let Some(parent_idx) =
                    s.nodes.iter().position(|n| n.node_id == node.parent_node_id)
            {
                s.sel = parent_idx;
            }
            Action::None
        }
        KeyCode::Right => {
            // Path into category: jump to first child node
            if let Some(node) = s.nodes.get(s.sel)
                && node.node_type == "Category"
                && let Some((child_idx, _)) = s
                    .nodes
                    .iter()
                    .enumerate()
                    .skip(s.sel + 1)
                    .find(|(_, c)| c.parent_node_id == node.node_id || c.depth > node.depth)
            {
                s.sel = child_idx;
            }
            Action::None
        }
        KeyCode::Enter | KeyCode::Char('l') => match s.nodes.get(s.sel) {
            Some(node) => match node.node_type.as_str() {
                "Category" => {
                    // Categories do not have threads (/forums/{category_id} 404s).
                    // Path down into the first child forum of this category.
                    if let Some((child_idx, _)) = s.nodes.iter().enumerate().skip(s.sel + 1).find(
                        |(_, c)| {
                            (c.parent_node_id == node.node_id || c.depth > node.depth)
                                && c.node_type == "Forum"
                        },
                    ) {
                        s.sel = child_idx;
                    } else if let Some((child_idx, _)) = s
                        .nodes
                        .iter()
                        .enumerate()
                        .skip(s.sel + 1)
                        .find(|(_, c)| c.parent_node_id == node.node_id || c.depth > node.depth)
                    {
                        s.sel = child_idx;
                    }
                    Action::None
                }
                "LinkForum" | "Page" => {
                    if let Some(url) = &node.view_url {
                        Action::OpenUrl(url.clone())
                    } else {
                        Action::None
                    }
                }
                _ => Action::OpenThreadList(node.node_id, node.title.clone()),
            },
            None => Action::None,
        },
        KeyCode::Char('1') => open_node_action(&s.nodes, NEWS_NODE, "Windows News"),
        KeyCode::Char('2') => open_node_action(&s.nodes, SECURITY_NODE, "Security Alerts"),
        KeyCode::Char('3') => open_node_action(&s.nodes, TUTORIALS_NODE, "Windows Tutorials"),
        KeyCode::Char('L') => Action::OpenLatestThreads,
        KeyCode::Char('r') => {
            s.loading = true;
            Action::LoadNodes
        }
        KeyCode::Char('N') => match s.nodes.get(s.sel) {
            Some(node) if node.node_type == "Forum" => Action::StartNewThread(node.node_id),
            Some(node) if node.node_type == "Category" => {
                if let Some(child) = s.nodes.iter().skip(s.sel + 1).find(|c| {
                    (c.parent_node_id == node.node_id || c.depth > node.depth)
                        && c.node_type == "Forum"
                }) {
                    Action::StartNewThread(child.node_id)
                } else {
                    Action::None
                }
            }
            _ => Action::None,
        },
        KeyCode::Char('q') => Action::Quit,
        _ => Action::None,
    }
}

fn open_node_action(nodes: &[Node], id: u32, fallback_title: &str) -> Action {
    if let Some(node) = nodes.iter().find(|n| n.node_id == id) {
        if node.node_type == "Category" {
            if let Some(child) = nodes.iter().find(|c| {
                (c.parent_node_id == node.node_id || c.depth > node.depth)
                    && c.node_type == "Forum"
            }) {
                return Action::OpenThreadList(child.node_id, child.title.clone());
            }
        } else if matches!(node.node_type.as_str(), "LinkForum" | "Page")
            && let Some(url) = &node.view_url
        {
            return Action::OpenUrl(url.clone());
        }
        Action::OpenThreadList(node.node_id, node.title.clone())
    } else {
        Action::OpenThreadList(id, fallback_title.to_string())
    }
}

pub fn render_forum_tree(
    s: &mut super::ForumTreeState,
    f: &mut ratatui::Frame,
    area: ratatui::layout::Rect,
    theme: &Theme,
) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.dim())
        .title(Span::styled(" Forums ", theme.title()));
    let inner = block.inner(area);
    f.render_widget(block, area);

    if s.loading {
        f.render_widget(Paragraph::new("Loading forums…"), inner);
        return;
    }
    if let Some(err) = &s.error {
        f.render_widget(
            Paragraph::new(format!("Error: {err}\n\nPress r to retry."))
                .style(Style::new().fg(theme.error)),
            inner,
        );
        return;
    }
    let items: Vec<ListItem> = s
        .nodes
        .iter()
        .map(|n| {
            let indent = "  ".repeat(n.depth.min(6) as usize);
            let (icon, tag_style, tag_label) = match n.node_type.as_str() {
                "Category" => ("📁 ", Style::new().fg(theme.accent), " [Category]"),
                "LinkForum" => ("🔗 ", theme.dim(), " [Link]"),
                "Page" => ("📄 ", theme.dim(), " [Page]"),
                _ => ("💬 ", theme.dim(), ""),
            };
            let mut spans = vec![
                Span::styled(indent, theme.dim()),
                Span::styled(icon, tag_style),
            ];
            if n.node_type == "Category" {
                spans.push(Span::styled(
                    n.title.clone(),
                    theme.title().add_modifier(Modifier::BOLD),
                ));
            } else {
                spans.push(Span::raw(n.title.clone()));
            }
            if !tag_label.is_empty() {
                spans.push(Span::styled(tag_label, tag_style));
            } else {
                spans.push(Span::styled(format!("  #{}", n.node_id), theme.dim()));
            }
            ListItem::new(Line::from(spans))
        })
        .collect();
    let mut state = ListState::default().with_selected(Some(s.sel));
    f.render_stateful_widget(
        List::new(items).highlight_style(theme.selected()).block(Block::default()),
        inner,
        &mut state,
    );

    let hints = footer_line(
        theme,
        &[
            ("↑↓", "move"),
            ("Enter", "open"),
            ("←→", "tree in/out"),
            ("1/2/3", "news/security/tutorials"),
            ("N", "new thread"),
            ("r", "refresh"),
            ("c", "DMs"),
            ("a", "alerts"),
            ("s", "search"),
            ("q", "quit"),
        ],
    );
    let hint_area = ratatui::layout::Rect::new(
        inner.x,
        inner.y + inner.height.saturating_sub(1),
        inner.width,
        1,
    );
    f.render_widget(Paragraph::new(hints), hint_area);
}

// ================= thread list =================

pub fn thread_list_key(s: &mut ThreadListState, key: KeyEvent) -> Action {
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('h') | KeyCode::Left => {
            Action::PopScreen
        }
        KeyCode::Up | KeyCode::Char('k') => {
            if s.sel > 0 {
                s.sel -= 1;
            }
            Action::None
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if s.sel + 1 < s.threads.len() {
                s.sel += 1;
            }
            Action::None
        }
        KeyCode::Char('[') | KeyCode::PageUp => {
            if s.page > 1 {
                s.loading = true;
                Action::LoadForum(s.node_id, s.page - 1)
            } else {
                Action::None
            }
        }
        KeyCode::Char(']') | KeyCode::PageDown => {
            if s.page < s.last_page {
                s.loading = true;
                Action::LoadForum(s.node_id, s.page + 1)
            } else {
                Action::None
            }
        }
        KeyCode::Enter | KeyCode::Char('l') => match s.threads.get(s.sel) {
            Some(t) => Action::OpenThread(t.clone()),
            None => Action::None,
        },
        KeyCode::Char('m') => Action::MarkForumRead(s.node_id),
        KeyCode::Char('N') => Action::StartNewThread(s.node_id),
        KeyCode::Char('r') => {
            s.loading = true;
            Action::LoadForum(s.node_id, s.page)
        }
        _ => Action::None,
    }
}

pub fn render_thread_list(
    s: &mut ThreadListState,
    f: &mut ratatui::Frame,
    area: ratatui::layout::Rect,
    theme: &Theme,
) {
    let title = format!(
        " {} — page {}/{} ",
        s.title,
        s.page,
        s.last_page
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.dim())
        .title(Span::styled(title, theme.title()));
    let inner = block.inner(area);
    f.render_widget(block, area);

    if s.loading && s.threads.is_empty() {
        f.render_widget(Paragraph::new("Loading threads…"), inner);
        return;
    }
    if let Some(err) = &s.error {
        f.render_widget(
            Paragraph::new(format!("Error: {err}\n\nPress r to retry."))
                .style(Style::new().fg(theme.error)),
            inner,
        );
        return;
    }
    let items: Vec<ListItem> = s
        .threads
        .iter()
        .map(|t| {
            let mut spans = vec![];
            if t.sticky {
                spans.push(Span::styled("📌 ", theme.warn));
            }
            if t.has_solution() {
                spans.push(Span::styled(
                    "✓ [Solved] ",
                    Style::new()
                        .fg(ratatui::style::Color::Green)
                        .add_modifier(Modifier::BOLD),
                ));
            } else if t.is_question() {
                spans.push(Span::styled(
                    "❓ [Question] ",
                    Style::new().fg(ratatui::style::Color::Yellow),
                ));
            } else if t.is_article() {
                spans.push(Span::styled(
                    "📰 [Article] ",
                    Style::new().fg(ratatui::style::Color::Cyan),
                ));
            } else if t.is_suggestion() {
                spans.push(Span::styled(
                    "💡 [Suggestion] ",
                    Style::new().fg(ratatui::style::Color::Magenta),
                ));
            } else if t.is_poll() {
                spans.push(Span::styled(
                    "📊 [Poll] ",
                    Style::new().fg(ratatui::style::Color::Blue),
                ));
            }
            if let Some(prefix) = &t.prefix {
                spans.push(Span::styled(format!("[{prefix}] "), theme.accent));
            }
            spans.push(Span::styled(t.title.clone(), theme.base()));
            let mut meta = format!("  {} by {}", fmt_time(t.post_date), t.username);
            if t.vote_score != 0 {
                meta.push_str(&format!(" · ▲ {}", t.vote_score));
            }
            meta.push_str(&format!(
                " · {} replies · last {}",
                t.reply_count, t.last_post_username
            ));
            spans.push(Span::styled(meta, theme.dim()));
            ListItem::new(Line::from(spans))
        })
        .collect();
    let mut state = ListState::default().with_selected(Some(s.sel));
    f.render_stateful_widget(
        List::new(items).highlight_style(theme.selected()),
        inner,
        &mut state,
    );

    let hints = footer_line(
        theme,
        &[
            ("Enter", "read"),
            ("[/]", "page"),
            ("m", "mark read"),
            ("N", "new thread"),
            ("r", "refresh"),
        ],
    );
    let hint_area = ratatui::layout::Rect::new(
        inner.x,
        inner.y + inner.height.saturating_sub(1),
        inner.width,
        1,
    );
    f.render_widget(Paragraph::new(hints), hint_area);
}

// ================= thread view =================

impl ThreadViewState {
    pub fn rebuild_lines(&mut self, theme: &Theme) {
        let mut lines: Vec<Line<'static>> = Vec::new();
        let mut links: Vec<String> = Vec::new();
        let mut offsets: Vec<usize> = Vec::new();

        if let Some(url) = &self.thread.view_url {
            links.push(url.clone());
            lines.push(Line::from(Span::styled(
                format!("[{}] thread on the web", links.len()),
                link_style(theme),
            )));
        }

        if self.thread.is_article() {
            lines.push(Line::from(Span::styled(
                format!("📰 ARTICLE: {}", self.thread.title),
                theme.title().add_modifier(Modifier::BOLD),
            )));
            lines.push(Line::from(Span::styled(
                format!(
                    "Published by {} · {}",
                    self.thread.username,
                    fmt_time(self.thread.post_date)
                ),
                theme.dim(),
            )));
            lines.push(Line::from(Span::styled(
                "────────────────────────────────────────────────────────────",
                theme.dim(),
            )));
            lines.push(Line::from(Span::raw("")));
        } else if self.thread.is_question() {
            if self.thread.has_solution() {
                lines.push(Line::from(Span::styled(
                    "✓ QUESTION THREAD — MARKED SOLUTION AVAILABLE",
                    Style::new()
                        .fg(ratatui::style::Color::Green)
                        .add_modifier(Modifier::BOLD),
                )));
            } else {
                lines.push(Line::from(Span::styled(
                    "❓ QUESTION THREAD — AWAITING SOLUTION",
                    Style::new()
                        .fg(ratatui::style::Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                )));
            }
            lines.push(Line::from(Span::raw("")));
        }

        let solution_id = self.thread.solution_post_id();

        for post in &self.posts {
            offsets.push(lines.len());
            let is_solution = solution_id == Some(post.post_id);
            if is_solution {
                lines.push(Line::from(Span::styled(
                    "┌── ✓ MARKED SOLUTION ──────────────────────────────────────────────",
                    Style::new()
                        .fg(ratatui::style::Color::Green)
                        .add_modifier(Modifier::BOLD),
                )));
            }

            let mut header_spans = vec![
                Span::styled("■ ", theme.accent),
                Span::styled(
                    post.username.clone(),
                    theme.base().add_modifier(Modifier::BOLD),
                ),
            ];
            if post.vote_score != 0 {
                let col = if post.vote_score > 0 {
                    ratatui::style::Color::Green
                } else {
                    ratatui::style::Color::Red
                };
                header_spans.push(Span::styled(
                    format!("  ▲ {} ", post.vote_score),
                    Style::new().fg(col),
                ));
            }
            if post.reaction_score > 0 {
                header_spans.push(Span::styled(
                    format!("  ❤️ {} ", post.reaction_score),
                    theme.dim(),
                ));
            }
            header_spans.push(Span::styled(
                format!("  {} · #{}", fmt_time(post.post_date), post.post_id),
                theme.dim(),
            ));
            lines.push(Line::from(header_spans));

            push_bbcode(&mut lines, &mut links, &post.message, theme);

            if is_solution {
                lines.push(Line::from(Span::styled(
                    "└── ✓ END OF MARKED SOLUTION ───────────────────────────────────────",
                    Style::new().fg(ratatui::style::Color::Green),
                )));
            }
            lines.push(Line::from(Span::raw("")));
        }
        self.lines = lines;
        self.links = links;
        self.post_line_offsets = offsets;
    }
}

/// Render BBCode source into static lines, collecting link targets.
pub(crate) fn push_bbcode(
    lines: &mut Vec<Line<'static>>,
    links: &mut Vec<String>,
    src: &str,
    theme: &Theme,
) {
    let mut current_spans: Vec<Span<'static>> = Vec::new();

    for chunk in bbcode::render(src) {
        match chunk {
            Chunk::Text(t, s) => {
                if t.is_empty() {
                    continue;
                }
                // Preserve line breaks inside the chunk.
                for (i, seg) in t.split('\n').enumerate() {
                    if i > 0 {
                        lines.push(Line::from(std::mem::take(&mut current_spans)));
                    }
                    if !seg.is_empty() {
                        current_spans
                            .push(Span::styled(seg.to_string(), style_from(theme, &s)));
                    }
                }
            }
            Chunk::Link(label, url, s) => {
                links.push(url.clone());
                // Plain styled text only — never embed OSC sequences in
                // spans (ratatui re-emits cells and the terminal eats the
                // surrounding text). Select-with-mouse or open via [o].
                for (i, seg) in label.split('\n').enumerate() {
                    if i > 0 {
                        lines.push(Line::from(std::mem::take(&mut current_spans)));
                    }
                    if !seg.is_empty() {
                        current_spans
                            .push(Span::styled(seg.to_string(), style_from(theme, &s)));
                    }
                }
                current_spans.push(Span::styled(
                    format!(" [{}]", links.len()),
                    link_style(theme),
                ));
            }
        }
    }
    if !current_spans.is_empty() {
        lines.push(Line::from(current_spans));
    }
}

pub fn thread_view_key(s: &mut ThreadViewState, key: KeyEvent) -> Action {
    if s.link_popup {
        return link_popup_key(s, key);
    }
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('h') | KeyCode::Left => {
            Action::PopScreen
        }
        KeyCode::Up | KeyCode::Char('k') => {
            s.scroll = s.scroll.saturating_sub(1);
            Action::None
        }
        KeyCode::Down | KeyCode::Char('j') => {
            s.scroll = s.scroll.saturating_add(1);
            Action::None
        }
        KeyCode::PageUp => {
            s.scroll = s.scroll.saturating_sub(20);
            Action::None
        }
        KeyCode::PageDown => {
            s.scroll = s.scroll.saturating_add(20);
            Action::None
        }
        KeyCode::Char('[') => {
            if s.page > 1 {
                s.loading = true;
                Action::LoadThread(s.thread.thread_id, s.page - 1)
            } else {
                Action::None
            }
        }
        KeyCode::Char(']') => {
            if s.page < s.last_page {
                s.loading = true;
                Action::LoadThread(s.thread.thread_id, s.page + 1)
            } else {
                Action::None
            }
        }
        KeyCode::Char('r') => Action::StartReply(s.thread.clone()),
        KeyCode::Char('p') => {
            if let Some(post) = s.posts.get(s.sel_post) {
                Action::OpenProfile(post.user_id, post.username.clone())
            } else {
                Action::OpenProfile(s.thread.user_id, s.thread.username.clone())
            }
        }
        KeyCode::Char('P') => Action::OpenProfile(s.thread.user_id, s.thread.username.clone()),
        KeyCode::Char('l') => {
            if let Some(post) = s.posts.get(s.sel_post) {
                Action::ReactPost(post.post_id)
            } else if let Some(first) = s.posts.first() {
                Action::ReactPost(first.post_id)
            } else {
                Action::None
            }
        }
        KeyCode::Char('v') => {
            if let Some(post) = s.posts.get(s.sel_post) {
                Action::VotePost(post.post_id, "up".into())
            } else {
                Action::None
            }
        }
        KeyCode::Char('V') => {
            if let Some(post) = s.posts.get(s.sel_post) {
                Action::VotePost(post.post_id, "down".into())
            } else {
                Action::None
            }
        }
        KeyCode::Char('n') => {
            if s.sel_post + 1 < s.posts.len() {
                s.sel_post += 1;
                if let Some(&line) = s.post_line_offsets.get(s.sel_post) {
                    s.scroll = line as u16;
                }
            }
            Action::None
        }
        KeyCode::Char('N') => {
            if s.sel_post > 0 {
                s.sel_post -= 1;
                if let Some(&line) = s.post_line_offsets.get(s.sel_post) {
                    s.scroll = line as u16;
                }
            }
            Action::None
        }
        KeyCode::Char('m') => Action::MarkThreadRead(s.thread.thread_id),
        KeyCode::Char('o') => {
            if !s.links.is_empty() {
                s.sel_local = s.sel_local.min(s.links.len().saturating_sub(1));
                s.link_popup = true;
            }
            Action::None
        }
        KeyCode::Char('u') => match &s.thread.view_url {
            Some(url) => Action::OpenUrl(url.clone()),
            None => Action::None,
        },
        _ => Action::None,
    }
}

fn link_popup_key(s: &mut ThreadViewState, key: KeyEvent) -> Action {
    if s.links.is_empty() {
        s.link_popup = false;
        return Action::None;
    }
    s.sel_local = s.sel_local.min(s.links.len().saturating_sub(1));
    match key.code {
        KeyCode::Esc => {
            s.link_popup = false;
            Action::None
        }
        KeyCode::Up | KeyCode::Char('k') => {
            s.sel_local = s.sel_local.saturating_sub(1);
            Action::None
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if s.sel_local + 1 < s.links.len() {
                s.sel_local += 1;
            }
            Action::None
        }
        KeyCode::Enter => {
            let sel = s.sel_local.min(s.links.len().saturating_sub(1));
            s.link_popup = false;
            Action::OpenUrl(s.links[sel].clone())
        }
        _ => Action::None,
    }
}

pub fn render_thread_view(
    s: &mut ThreadViewState,
    f: &mut ratatui::Frame,
    area: ratatui::layout::Rect,
    theme: &Theme,
) {
    let title = format!(
        " {} — post {}/{} ",
        truncate(&s.thread.title, 60),
        s.page,
        s.last_page
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.dim())
        .title(Span::styled(title, theme.title()));
    let inner = block.inner(area);
    f.render_widget(block, area);

    if s.loading && s.lines.is_empty() {
        f.render_widget(Paragraph::new("Loading thread…"), inner);
        return;
    }
    if let Some(err) = &s.error {
        f.render_widget(
            Paragraph::new(format!("Error: {err}\n\n[/] to change page."))
                .style(Style::new().fg(theme.error)),
            inner,
        );
        return;
    }

    let body = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(inner);
    let view = body[0];
    let total = s.lines.len() as u16;
    let max_scroll = total.saturating_sub(view.height);
    if s.scroll > max_scroll {
        s.scroll = max_scroll;
    }

    f.render_widget(
        Paragraph::new(s.lines.clone())
            .scroll((s.scroll, 0))
            .wrap(Wrap { trim: false }),
        view,
    );

    let hints = footer_line(
        theme,
        &[
            ("↑↓/n/N", "scroll/post"),
            ("[/]", "post page"),
            ("r", "reply"),
            ("l", "like"),
            ("v/V", "vote up/dn"),
            ("p/P", "post/OP profile"),
            ("m", "mark read"),
            ("o", "links"),
            ("u", "web"),
        ],
    );
    f.render_widget(Paragraph::new(hints), body[1]);

    if s.link_popup {
        let popup_area = super::centered_box(area, area.width.min(70), (s.links.len() as u16 + 4).min(20));
        let popup = Block::default()
            .borders(Borders::ALL)
            .border_style(theme.accent)
            .title(Span::styled(" Links — Enter to open, Esc to close ", theme.title()));
        let p_inner = popup.inner(popup_area);
        f.render_widget(ratatui::widgets::Clear, popup_area);
        f.render_widget(popup, popup_area);
        let items: Vec<ListItem> = s
            .links
            .iter()
            .enumerate()
            .map(|(i, url)| {
                ListItem::new(Line::from(vec![
                    Span::styled(format!(" [{}] ", i + 1), theme.accent),
                    Span::styled(url.clone(), link_style(theme)),
                ]))
            })
            .collect();
        let sel = s.sel_local.min(s.links.len().saturating_sub(1));
        let mut state = ListState::default().with_selected(Some(sel));
        f.render_stateful_widget(
            List::new(items).highlight_style(theme.selected()),
            p_inner,
            &mut state,
        );
    }
}

pub(crate) fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{cut}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_bbcode_keeps_inline_formatting_on_same_line() {
        let theme = Theme::dark();
        let mut lines = Vec::new();
        let mut links = Vec::new();
        push_bbcode(
            &mut lines,
            &mut links,
            "Hello [B]world[/B] from [URL=https://example.com]Windows[/URL]!",
            &theme,
        );
        assert_eq!(lines.len(), 1, "inline styling should remain on a single line");
        assert_eq!(links.len(), 1);
        assert_eq!(links[0], "https://example.com");
    }

    #[test]
    fn push_bbcode_respects_newlines() {
        let theme = Theme::dark();
        let mut lines = Vec::new();
        let mut links = Vec::new();
        push_bbcode(
            &mut lines,
            &mut links,
            "Line 1 with [B]bold[/B]\nLine 2 with [I]italic[/I]\n\nLine 4",
            &theme,
        );
        assert_eq!(lines.len(), 4);
    }

    #[test]
    fn link_popup_clamps_out_of_bounds_selection() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let mut state = ThreadViewState {
            links: vec!["https://example.com/1".into(), "https://example.com/2".into()],
            sel_local: 10,
            link_popup: false,
            ..Default::default()
        };

        let act = thread_view_key(
            &mut state,
            KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE),
        );
        assert!(matches!(act, Action::None));
        assert!(state.link_popup);
        assert_eq!(state.sel_local, 1);

        let act = thread_view_key(
            &mut state,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        );
        assert!(matches!(act, Action::OpenUrl(url) if url == "https://example.com/2"));
        assert!(!state.link_popup);
    }

    #[test]
    fn forum_tree_category_navigation_and_actions() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        use common::models::Node;
        use crate::screens::ForumTreeState;

        let nodes = vec![
            Node {
                node_id: 301,
                title: "Windows Forums".into(),
                description: "Category root".into(),
                node_type: "Category".into(),
                parent_node_id: 0,
                depth: 0,
                view_url: None,
            },
            Node {
                node_id: 302,
                title: "Windows Help and Support".into(),
                description: "Help forum".into(),
                node_type: "Forum".into(),
                parent_node_id: 301,
                depth: 1,
                view_url: None,
            },
            Node {
                node_id: 400,
                title: "Documentation Link".into(),
                description: "External docs".into(),
                node_type: "LinkForum".into(),
                parent_node_id: 301,
                depth: 1,
                view_url: Some("https://example.com/docs".into()),
            },
        ];

        let mut state = ForumTreeState {
            nodes: nodes.clone(),
            sel: 0, // Points to Category 301
            ..Default::default()
        };

        // Pressing Enter on Category should NOT open thread list for category; it should step into child forum
        let act = forum_tree_key(&mut state, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(act, Action::None));
        assert_eq!(state.sel, 1, "Enter on Category should advance sel to child forum (idx 1)");

        // Pressing Left / h on child forum should step back to parent Category (idx 0)
        let act = forum_tree_key(&mut state, KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        assert!(matches!(act, Action::None));
        assert_eq!(state.sel, 0, "Left on child should step up to parent Category (idx 0)");

        // Pressing Right on Category should step into child forum
        let act = forum_tree_key(&mut state, KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        assert!(matches!(act, Action::None));
        assert_eq!(state.sel, 1, "Right on Category should advance sel to child forum");

        // Pressing Enter on Forum should open thread list
        let act = forum_tree_key(&mut state, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(act, Action::OpenThreadList(id, _) if id == 302));

        // Moving to LinkForum and pressing Enter should open URL
        state.sel = 2;
        let act = forum_tree_key(&mut state, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(act, Action::OpenUrl(url) if url == "https://example.com/docs"));

        // Pressing 'N' on Category should resolve child forum for thread creation
        state.sel = 0;
        let act = forum_tree_key(&mut state, KeyEvent::new(KeyCode::Char('N'), KeyModifiers::NONE));
        assert!(matches!(act, Action::StartNewThread(id) if id == 302));

        // open_node_action on Category should resolve to child forum
        let act = open_node_action(&nodes, 301, "Windows Forums");
        assert!(matches!(act, Action::OpenThreadList(id, _) if id == 302));

        // 'L' opens latest threads
        let act = forum_tree_key(&mut state, KeyEvent::new(KeyCode::Char('L'), KeyModifiers::NONE));
        assert!(matches!(act, Action::OpenLatestThreads));
    }

    #[test]
    fn thread_view_shortcuts_and_type_rendering() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        use common::models::{Post, Thread};

        let mut thread = Thread {
            thread_id: 101,
            title: "How do I fix BSOD on Windows 11?".into(),
            username: "Alice".into(),
            user_id: 10,
            discussion_type: "question".into(),
            vote_score: 5,
            highlighted_post_ids: vec![202],
            ..Default::default()
        };
        thread.type_data = serde_json::json!({ "solution_post_id": 202 });

        let posts = vec![
            Post {
                post_id: 201,
                thread_id: 101,
                user_id: 10,
                username: "Alice".into(),
                message: "Here is my crash dump...".into(),
                vote_score: 0,
                is_first_post: true,
                ..Default::default()
            },
            Post {
                post_id: 202,
                thread_id: 101,
                user_id: 20,
                username: "Bob_Guru".into(),
                message: "Update your GPU driver to v550+.".into(),
                vote_score: 12,
                is_first_post: false,
                ..Default::default()
            },
        ];

        let theme = Theme::dark();
        let mut state = ThreadViewState {
            thread,
            posts,
            ..Default::default()
        };
        state.rebuild_lines(&theme);

        // Verify solution callout box is rendered in the lines
        let rendered_text: String = state.lines.iter().flat_map(|l| l.spans.iter().map(|s| s.content.as_ref())).collect();
        assert!(rendered_text.contains("MARKED SOLUTION"), "Expected MARKED SOLUTION callout in question thread lines");
        assert!(rendered_text.contains("Bob_Guru"), "Expected solution author Bob_Guru");

        // Key 'p': open author profile of active post (initially post 0: Alice)
        let act = thread_view_key(&mut state, KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE));
        assert!(matches!(act, Action::OpenProfile(uid, name) if uid == 10 && name == "Alice"));

        // Key 'n': navigate to next post (post 1: Bob_Guru)
        let act = thread_view_key(&mut state, KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
        assert!(matches!(act, Action::None));
        assert_eq!(state.sel_post, 1);

        // Key 'p': open author profile of Bob_Guru
        let act = thread_view_key(&mut state, KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE));
        assert!(matches!(act, Action::OpenProfile(uid, name) if uid == 20 && name == "Bob_Guru"));

        // Key 'P': open thread OP profile (Alice)
        let act = thread_view_key(&mut state, KeyEvent::new(KeyCode::Char('P'), KeyModifiers::SHIFT));
        assert!(matches!(act, Action::OpenProfile(uid, name) if uid == 10 && name == "Alice"));

        // Key 'l': react to active post (post 202)
        let act = thread_view_key(&mut state, KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE));
        assert!(matches!(act, Action::ReactPost(pid) if pid == 202));

        // Key 'v': vote up on active post
        let act = thread_view_key(&mut state, KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE));
        assert!(matches!(act, Action::VotePost(pid, vt) if pid == 202 && vt == "up"));

        // Key 'V': vote down on active post
        let act = thread_view_key(&mut state, KeyEvent::new(KeyCode::Char('V'), KeyModifiers::SHIFT));
        assert!(matches!(act, Action::VotePost(pid, vt) if pid == 202 && vt == "down"));

        // Key 'N': navigate back to previous post (post 0)
        let act = thread_view_key(&mut state, KeyEvent::new(KeyCode::Char('N'), KeyModifiers::SHIFT));
        assert!(matches!(act, Action::None));
        assert_eq!(state.sel_post, 0);
    }
}
