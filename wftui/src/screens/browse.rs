//! Browsing screens: Home (Forums + thread list), the standalone forum tree
//! and thread list, and the thread view.
//!
//! The column arithmetic in this file is the DESIGN.md "Row grammar" and is
//! pinned by unit tests against the reference artboards. Changing a width here
//! changes what the reference renders promise, so change the tests with it.

use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState, Paragraph};

use common::bbcode::{self, Chunk};
use common::models::{Node, Post, Thread};

use super::{
    Action, ForumTreeState, HomeState, Pane, ThreadListState, ThreadViewState, link_style,
    style_from,
};
use crate::chrome::{self, Hints};
use crate::glyph::Glyphs;
use crate::images::{self, Slot};
use crate::theme::{Theme, fmt_age};

pub(crate) const NEWS_NODE: u32 = 4;
pub(crate) const SECURITY_NODE: u32 = 84;
pub(crate) const TUTORIALS_NODE: u32 = 305;

/// The site bot (`project_bot_rename_windowsforum_ai_2026_07`): its posts get
/// an `AI` chip so a member never mistakes generated text for a human answer.
const AI_USER_ID: u32 = 125_694;

/// Both panes fit side by side from here up (DESIGN.md). Below it Home renders
/// whichever half has focus, and Enter pushes a screen as it always did.
const DUAL_PANE_MIN_COLS: u16 = 110;
/// Width of the Forums pane in the two-pane layout.
const TREE_PANE_COLS: u16 = 37;

// ================= row grammar =================

/// The two thread-row layouts of DESIGN.md. `Wide` is the ≥ 90-column form
/// with a column-header row; `Narrow` drops the header and squeezes the author
/// and reply columns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Grammar {
    Wide,
    Narrow,
}

impl Grammar {
    /// The unread mark, the type glyph and the spaces around them: `" ● ? "`.
    const LEAD: usize = 5;
    /// The `Active` column is 6 in both forms — `fmt_age` is capped to match.
    const AGE: usize = 6;

    /// DESIGN.md keys this off the *terminal* width, not the panel's: the
    /// two-pane Home at 120 columns uses the wide grammar in an 81-cell panel
    /// (see the Main artboard), which a panel-width rule would get wrong.
    pub(crate) fn for_width(width: u16) -> Grammar {
        if width >= 90 {
            Grammar::Wide
        } else {
            Grammar::Narrow
        }
    }

    const fn author(self) -> usize {
        match self {
            Grammar::Wide => 12,
            Grammar::Narrow => 9,
        }
    }

    const fn replies(self) -> usize {
        match self {
            Grammar::Wide => 7,
            Grammar::Narrow => 4,
        }
    }

    /// Gap between the three right-hand columns. The gap *before* the author
    /// is always 2, which is what separates the ragged title from the block.
    const fn gap(self) -> usize {
        match self {
            Grammar::Wide => 2,
            Grammar::Narrow => 1,
        }
    }

    /// Everything a row spends outside the title field.
    pub(crate) const fn fixed(self) -> usize {
        Self::LEAD + 2 + self.author() + self.gap() + self.replies() + self.gap() + Self::AGE + 1
    }

    /// Cells left for the title (including its inline prefix chip).
    pub(crate) fn title_width(self, inner: usize) -> usize {
        inner.saturating_sub(self.fixed())
    }
}

/// Right-align `s` in `w` cells, clipping from the left if it cannot fit.
fn rj(s: &str, w: usize) -> String {
    let n = s.chars().count();
    if n >= w {
        return s.chars().skip(n - w).collect();
    }
    format!("{}{s}", " ".repeat(w - n))
}

/// Left-align `s` in `w` cells, clipping with `…`.
fn lj(s: &str, w: usize) -> String {
    let t = truncate(s, w);
    let n = t.chars().count();
    format!("{t}{}", " ".repeat(w.saturating_sub(n)))
}

/// The dim column-header row above a wide thread list.
pub(crate) fn thread_list_header(theme: &Theme, gram: Grammar, inner: usize) -> Line<'static> {
    let tw = gram.title_width(inner);
    let text = format!(
        "{}{}  {}{}{}{}{} ",
        " ".repeat(Grammar::LEAD),
        lj("Thread", tw),
        lj("Started by", gram.author()),
        " ".repeat(gram.gap()),
        rj("Replies", gram.replies()),
        " ".repeat(gram.gap()),
        rj("Active", Grammar::AGE),
    );
    Line::from(Span::styled(text, theme.dim()))
}

/// One thread row, exactly `inner` cells wide so the selection band covers the
/// whole panel.
pub(crate) fn thread_row(
    t: &Thread,
    theme: &Theme,
    g: &Glyphs,
    gram: Grammar,
    inner: usize,
) -> Line<'static> {
    let (type_glyph, type_style) = type_mark(t, theme, g);
    let title_style = if t.is_unread {
        theme.base().add_modifier(Modifier::BOLD)
    } else {
        theme.base()
    };

    let tw = gram.title_width(inner);
    if tw < 4 {
        // No room for the metadata block: the title is the only thing worth
        // showing, and a row must never be wider than its panel.
        return Line::from(clip_to(
            vec![Span::styled(format!(" {}", t.title.clone()), title_style)],
            inner,
        ));
    }

    let mut spans = vec![
        Span::raw(" "),
        if t.is_unread {
            Span::styled(g.unread.to_string(), Style::new().fg(theme.accent))
        } else {
            Span::raw(" ")
        },
        Span::raw(" "),
        Span::styled(type_glyph.to_string(), type_style),
        Span::raw(" "),
    ];

    // Title field: the prefix chip is *inside* it, so a long prefix costs
    // title cells rather than shifting the right-hand columns.
    let mut used = 0usize;
    if let Some(prefix) = &t.prefix
        && !prefix.trim().is_empty()
    {
        let chip = chrome::chip(theme, &chrome::short_prefix(prefix));
        let cw = chip.width();
        if cw + 4 <= tw {
            spans.push(chip);
            spans.push(Span::raw(" "));
            used = cw + 1;
        }
    }
    let title = truncate(&t.title, tw - used);
    let tlen = title.chars().count();
    spans.push(Span::styled(title, title_style));
    spans.push(Span::raw(" ".repeat(tw - used - tlen)));

    spans.push(Span::raw("  "));
    spans.push(Span::styled(lj(&t.username, gram.author()), theme.dim()));
    spans.push(Span::raw(" ".repeat(gram.gap())));
    spans.push(Span::styled(
        rj(&t.reply_count.to_string(), gram.replies()),
        theme.base(),
    ));
    spans.push(Span::raw(" ".repeat(gram.gap())));
    spans.push(Span::styled(
        rj(&fmt_age(t.last_post_date.max(t.post_date)), Grammar::AGE),
        theme.dim(),
    ));
    spans.push(Span::raw(" "));
    Line::from(spans)
}

/// The single-cell type column: `»` sticky, `✓` solved, `?` open question,
/// `▪` article, `▌` suggestion, `▲` poll, `⊘` closed, blank for a plain
/// open discussion.
fn type_mark(t: &Thread, theme: &Theme, g: &Glyphs) -> (&'static str, Style) {
    if t.sticky {
        (g.sticky, Style::new().fg(theme.accent))
    } else if t.has_solution() {
        (g.solved, Style::new().fg(theme.ok))
    } else if t.is_question() {
        (g.question, Style::new().fg(theme.warn))
    } else if t.is_article() {
        (g.article, Style::new().fg(theme.chrome_bg))
    } else if t.is_suggestion() {
        (g.heading, Style::new().fg(theme.accent))
    } else if t.is_poll() {
        (g.vote, Style::new().fg(theme.accent))
    } else if !t.discussion_open {
        (g.locked, theme.dim())
    } else {
        (" ", theme.dim())
    }
}

// ================= Forums panel =================

/// A rendered Forums-panel row, and which `nodes` entry (if any) it stands for.
struct TreeRow {
    line: Line<'static>,
    node: Option<usize>,
}

/// The QUICK block: the four keys that skip the tree entirely.
const QUICK: [(&str, &str); 4] = [
    ("L", "Latest posts"),
    ("1", "Windows News"),
    ("2", "Security Alerts"),
    ("3", "Windows Tutorials"),
];

fn forum_rows(
    s: &ForumTreeState,
    current: u32,
    theme: &Theme,
    g: &Glyphs,
    width: usize,
) -> Vec<TreeRow> {
    let mut rows: Vec<TreeRow> = Vec::with_capacity(s.nodes.len() + QUICK.len() + 2);
    let plain = |line: Line<'static>| TreeRow { line, node: None };

    rows.push(plain(Line::from(Span::styled(
        " QUICK".to_string(),
        theme.dim().add_modifier(Modifier::BOLD),
    ))));
    for (key, label) in QUICK {
        rows.push(plain(Line::from(vec![
            Span::raw(" "),
            Span::styled(
                key.to_string(),
                Style::new().fg(theme.accent).add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("  {label}"), theme.base()),
        ])));
    }
    rows.push(plain(Line::from(Span::raw(""))));

    for (i, n) in s.nodes.iter().enumerate() {
        let is_current = n.node_id == current && n.node_type != "Category";
        let line = if n.node_type == "Category" {
            // Categories are the section rules of this panel: CAPS, bold-dim,
            // flush with the panel's own left margin.
            let indent = 2 * n.depth.min(6) as usize;
            Line::from(vec![
                Span::raw(format!(" {}", " ".repeat(indent))),
                Span::styled(
                    truncate(
                        &n.title.to_uppercase(),
                        width.saturating_sub(indent + 1),
                    ),
                    theme.dim().add_modifier(Modifier::BOLD),
                ),
            ])
        } else {
            // Forums sit two cells in from their category; the `›` marker
            // occupies the cell that indent would otherwise waste.
            let indent = 2 * n.depth.clamp(1, 6) as usize;
            let mut spans = vec![Span::raw(format!(" {}", " ".repeat(indent - 2)))];
            if is_current {
                spans.push(Span::styled(
                    g.crumb.to_string(),
                    Style::new().fg(theme.accent),
                ));
            } else {
                spans.push(Span::raw(" "));
            }
            spans.push(Span::raw(" "));
            let style = if is_current {
                theme.base().add_modifier(Modifier::BOLD)
            } else {
                theme.base()
            };
            let external = matches!(n.node_type.as_str(), "LinkForum" | "Page");
            let room = width
                .saturating_sub(indent + 1)
                .saturating_sub(if external { 2 } else { 0 });
            spans.push(Span::styled(truncate(&n.title, room), style));
            if external {
                spans.push(Span::styled(format!(" {}", g.external), theme.dim()));
            }
            Line::from(spans)
        };
        rows.push(TreeRow {
            line,
            node: Some(i),
        });
    }
    rows
}

/// Scroll the Forums panel so `sel_row` stays visible, reserving the last row
/// for the `▾ N more` indicator whenever anything is left below.
fn tree_window(total: usize, height: usize, sel_row: usize, scroll: &mut usize) -> (usize, usize) {
    if height == 0 || total <= height {
        *scroll = 0;
        return (0, total);
    }
    // One row is spent on the indicator unless we are parked at the bottom.
    let vis = height - 1;
    if *scroll > sel_row {
        *scroll = sel_row;
    }
    if sel_row >= *scroll + vis {
        *scroll = sel_row + 1 - vis;
    }
    if *scroll + height >= total {
        *scroll = total - height;
        return (*scroll, total);
    }
    (*scroll, *scroll + vis)
}

pub(crate) fn render_forum_panel(
    s: &mut ForumTreeState,
    current: u32,
    f: &mut ratatui::Frame,
    area: Rect,
    theme: &Theme,
    g: &Glyphs,
    focused: bool,
) {
    let block = chrome::panel(theme, g, "Forums", focused, None, None);
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    if s.loading && s.nodes.is_empty() {
        f.render_widget(
            Paragraph::new(format!(
                "{} Loading forums…",
                chrome::spinner(g, chrome::spinner_tick())
            ))
            .style(theme.dim()),
            inner,
        );
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

    let rows = forum_rows(s, current, theme, g, inner.width as usize);
    let sel_row = rows
        .iter()
        .position(|r| r.node == Some(s.sel))
        .unwrap_or(0);
    let height = inner.height as usize;
    let (start, end) = tree_window(rows.len(), height, sel_row, &mut s.scroll);

    let mut items: Vec<ListItem> = rows[start..end]
        .iter()
        .map(|r| ListItem::new(r.line.clone()))
        .collect();
    if end < rows.len() {
        items.push(ListItem::new(Line::from(Span::styled(
            format!("   {} {} more", g.more, rows.len() - end),
            theme.dim(),
        ))));
    }

    // Only a focused pane shows the band; unfocused, the `›` marker alone says
    // where the list pane came from.
    let sel_in_window = (start..end).contains(&sel_row);
    let mut state = ListState::default();
    if focused && sel_in_window {
        state.select(Some(sel_row - start));
    }
    f.render_stateful_widget(
        List::new(items).highlight_style(theme.selected()),
        inner,
        &mut state,
    );
}

// ================= thread list panel =================

/// `1–20 of 431`. The API's pagination has no `per_page`, so the last page's
/// range is derived backwards from the total instead of guessed.
fn list_range(s: &ThreadListState) -> Option<String> {
    if s.total == 0 || s.threads.is_empty() {
        return None;
    }
    let len = s.threads.len() as u64;
    let (start, end) = if s.page >= s.last_page.max(1) && s.total >= len {
        (s.total - len + 1, s.total)
    } else {
        let start = (s.page.max(1) as u64 - 1) * len + 1;
        (start, start + len - 1)
    };
    Some(format!("{start}\u{2013}{end} of {}", s.total))
}

pub(crate) fn render_thread_panel(
    s: &mut ThreadListState,
    f: &mut ratatui::Frame,
    area: Rect,
    theme: &Theme,
    g: &Glyphs,
    focused: bool,
    gram: Grammar,
) {
    let title = if s.title.is_empty() {
        "Threads".to_string()
    } else {
        s.title.clone()
    };
    let right = format!("page {} of {}", s.page.max(1), s.last_page.max(1));
    let bottom = list_range(s);
    let block = chrome::panel(
        theme,
        g,
        &title,
        focused,
        Some(&right),
        bottom.as_deref(),
    );
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    if s.loading && s.threads.is_empty() {
        f.render_widget(
            Paragraph::new(format!(
                "{} Loading threads…",
                chrome::spinner(g, chrome::spinner_tick())
            ))
            .style(theme.dim()),
            inner,
        );
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
    if s.threads.is_empty() {
        f.render_widget(
            Paragraph::new("No threads here yet.").style(theme.dim()),
            inner,
        );
        return;
    }

    let iw = inner.width as usize;
    let (head, list_area) = if gram == Grammar::Wide && inner.height > 1 {
        let [h, rest] =
            Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(inner);
        (Some(h), rest)
    } else {
        (None, inner)
    };
    if let Some(h) = head {
        f.render_widget(Paragraph::new(thread_list_header(theme, gram, iw)), h);
    }

    let items: Vec<ListItem> = s
        .threads
        .iter()
        .map(|t| ListItem::new(thread_row(t, theme, g, gram, iw)))
        .collect();
    let mut state = ListState::default().with_selected(Some(s.sel.min(s.threads.len() - 1)));
    let list = List::new(items);
    let list = if focused {
        list.highlight_style(theme.selected())
    } else {
        list
    };
    f.render_stateful_widget(list, list_area, &mut state);
}

// ================= Home =================

pub fn home_hints() -> Hints {
    Hints::with_short(
        &[
            ("Enter", "open"),
            ("j/k", "move"),
            ("Tab", "pane"),
            ("N", "new thread"),
            ("m", "mark read"),
            ("/", "search"),
            ("g", "go to\u{2026}"),
            ("?", "help"),
            ("q", "quit"),
        ],
        // Narrow.dc.html row 22, verbatim: the caps all survive, the words
        // that can be guessed from the cap do not.
        &[
            ("Enter", "open"),
            ("j/k", ""),
            ("N", "new"),
            ("m", "read"),
            ("/", ""),
            ("g", ""),
            ("?", "more"),
        ],
        0,
    )
}

pub fn home_key(s: &mut HomeState, key: KeyEvent) -> Action {
    if matches!(key.code, KeyCode::Tab | KeyCode::BackTab) {
        s.focus = match s.focus {
            Pane::Tree => Pane::List,
            Pane::List => Pane::Tree,
        };
        return Action::None;
    }
    match s.focus {
        Pane::Tree => {
            // The Home key bar advertises `m mark read`, and in the Forums
            // pane that means the forum under the cursor.
            if key.code == KeyCode::Char('m') {
                return match s.tree.nodes.get(s.tree.sel) {
                    Some(n) if n.node_type == "Forum" => Action::MarkForumRead(n.node_id),
                    _ => Action::None,
                };
            }
            let act = forum_tree_key(&mut s.tree, key);
            // A load lands in the right-hand pane, so the keyboard follows it.
            if s.dual
                && matches!(
                    act,
                    Action::OpenThreadList(..) | Action::OpenLatestThreads
                )
            {
                s.focus = Pane::List;
            }
            act
        }
        Pane::List => match key.code {
            // `q` quits from Home; only the standalone list screen pops.
            KeyCode::Char('q') => Action::Quit,
            KeyCode::Esc | KeyCode::Char('h') | KeyCode::Left => {
                s.focus = Pane::Tree;
                Action::None
            }
            KeyCode::Char('1') => open_node_action(&s.tree.nodes, NEWS_NODE, "Windows News"),
            KeyCode::Char('2') => open_node_action(&s.tree.nodes, SECURITY_NODE, "Security Alerts"),
            KeyCode::Char('3') => {
                open_node_action(&s.tree.nodes, TUTORIALS_NODE, "Windows Tutorials")
            }
            KeyCode::Char('L') => Action::OpenLatestThreads,
            _ => thread_list_key(&mut s.list, key),
        },
    }
}

pub fn render_home(
    s: &mut HomeState,
    f: &mut ratatui::Frame,
    area: Rect,
    theme: &Theme,
    g: &Glyphs,
) {
    let dual = area.width >= DUAL_PANE_MIN_COLS;
    s.dual = dual;
    if !dual {
        // One panel, showing whichever half has the keyboard.
        match s.focus {
            Pane::Tree => {
                render_forum_panel(&mut s.tree, s.list.node_id, f, area, theme, g, true)
            }
            Pane::List => render_thread_panel(
                &mut s.list,
                f,
                area,
                theme,
                g,
                true,
                Grammar::for_width(area.width),
            ),
        }
        return;
    }
    let [left, right] =
        Layout::horizontal([Constraint::Length(TREE_PANE_COLS), Constraint::Min(0)]).areas(area);
    render_forum_panel(
        &mut s.tree,
        s.list.node_id,
        f,
        left,
        theme,
        g,
        s.focus == Pane::Tree,
    );
    render_thread_panel(
        &mut s.list,
        f,
        right,
        theme,
        g,
        s.focus == Pane::List,
        Grammar::for_width(area.width),
    );
}

// ================= forum tree (standalone) =================

pub fn forum_tree_key(s: &mut ForumTreeState, key: KeyEvent) -> Action {
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

pub(crate) fn open_node_action(nodes: &[Node], id: u32, fallback_title: &str) -> Action {
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

pub fn forum_tree_hints() -> Hints {
    Hints::with_short(
        &[
            ("Enter", "open"),
            ("j/k", "move"),
            ("h/l", "tree in/out"),
            ("1/2/3", "news/security/tutorials"),
            ("N", "new thread"),
            ("r", "refresh"),
            ("c", "DMs"),
            ("a", "alerts"),
            ("s", "search"),
            ("q", "quit"),
        ],
        &[
            ("Enter", "open"),
            ("j/k", ""),
            ("h/l", ""),
            ("N", "new"),
            ("s", "search"),
            ("q", "quit"),
        ],
        0,
    )
}

pub fn render_forum_tree(
    s: &mut ForumTreeState,
    f: &mut ratatui::Frame,
    area: Rect,
    theme: &Theme,
    g: &Glyphs,
) {
    render_forum_panel(s, 0, f, area, theme, g, true);
}

// ================= thread list (standalone) =================

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
        KeyCode::Home => {
            s.sel = 0;
            Action::None
        }
        KeyCode::End => {
            s.sel = s.threads.len().saturating_sub(1);
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

pub fn thread_list_hints() -> Hints {
    Hints::with_short(
        &[
            ("Enter", "read"),
            ("j/k", "move"),
            ("[/]", "page"),
            ("N", "new thread"),
            ("m", "mark read"),
            ("/", "search"),
            ("r", "refresh"),
            ("?", "help"),
            ("Esc", "back"),
        ],
        &[
            ("Enter", "read"),
            ("j/k", ""),
            ("[/]", "page"),
            ("N", "new"),
            ("m", "read"),
            ("/", ""),
            ("Esc", "back"),
        ],
        0,
    )
}

pub fn thread_list_crumb(s: &ThreadListState) -> String {
    s.title.clone()
}

pub fn render_thread_list(
    s: &mut ThreadListState,
    f: &mut ratatui::Frame,
    area: Rect,
    theme: &Theme,
    g: &Glyphs,
) {
    render_thread_panel(s, f, area, theme, g, true, Grammar::for_width(area.width));
}

// ================= thread view =================

/// `30 Aug 2026`.
fn fmt_date(ts: i64) -> String {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let Ok(t) = time::OffsetDateTime::from_unix_timestamp(ts) else {
        return String::new();
    };
    format!(
        "{} {} {}",
        t.day(),
        MONTHS[t.month() as usize - 1],
        t.year()
    )
}

/// `30 Aug 2026, 19:42`.
fn fmt_stamp(ts: i64) -> String {
    let Ok(t) = time::OffsetDateTime::from_unix_timestamp(ts) else {
        return String::new();
    };
    format!("{}, {:02}:{:02}", fmt_date(ts), t.hour(), t.minute())
}

fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Split a string into alternating whitespace / non-whitespace runs, so a
/// greedy wrapper can keep styles attached to the text they came with.
fn tokens(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut prev: Option<bool> = None;
    for (i, c) in s.char_indices() {
        let ws = c.is_whitespace();
        match prev {
            Some(p) if p == ws => {}
            Some(_) => {
                out.push(&s[start..i]);
                start = i;
            }
            None => start = i,
        }
        prev = Some(ws);
    }
    if start < s.len() {
        out.push(&s[start..]);
    }
    out
}

/// Greedy word wrap over styled spans.
///
/// Display width in terminal cells. `Span::width` is ratatui's own
/// unicode-width measure — the one the buffer bills a run of cells at — which
/// is why the rest of the chrome measures with it too.
fn cell_width(s: &str) -> usize {
    Span::raw(s).width()
}

/// The thread view pre-wraps instead of using `Paragraph::wrap` because every
/// body line carries a gutter glyph and every header line is right-aligned —
/// widget-level wrapping would strip the gutter from continuation lines and
/// fold the right-aligned block onto the next row.
///
/// `width` is terminal *cells*, so every measurement here goes through
/// `cell_width` — a CJK ideograph or an emoji pasted into a post is two cells
/// wide, and billing it one would build lines up to twice the panel width.
/// The body is drawn with `Paragraph::new(..).scroll(..)` and no `Wrap`, so
/// anything over-long is silently clipped at the panel edge rather than
/// folded.
pub(crate) fn wrap_spans(spans: &[Span<'static>], width: usize) -> Vec<Vec<Span<'static>>> {
    let width = width.max(1);
    let mut out: Vec<Vec<Span<'static>>> = Vec::new();
    let mut line: Vec<Span<'static>> = Vec::new();
    let mut used = 0usize;
    for sp in spans {
        let style = sp.style;
        for tok in tokens(sp.content.as_ref()) {
            let is_space = tok.starts_with(char::is_whitespace);
            let mut rest = tok;
            loop {
                let len = cell_width(rest);
                if is_space {
                    if used == 0 {
                        break; // a wrapped line never starts with the old space
                    }
                    if used + len > width {
                        out.push(std::mem::take(&mut line));
                        used = 0;
                    } else {
                        line.push(Span::styled(rest.to_string(), style));
                        used += len;
                    }
                    break;
                }
                if used + len <= width {
                    line.push(Span::styled(rest.to_string(), style));
                    used += len;
                    break;
                }
                if len <= width && used > 0 {
                    out.push(std::mem::take(&mut line));
                    used = 0;
                    continue;
                }
                // Longer than a whole line (a bare URL, say): hard-split it.
                let room = width - used;
                if room == 0 {
                    out.push(std::mem::take(&mut line));
                    used = 0;
                    continue;
                }
                // Take characters while they still fit in `room` *cells*, so
                // a double-width character never straddles the last column.
                let mut head = String::new();
                let mut head_w = 0usize;
                for ch in rest.chars() {
                    let mut buf = [0u8; 4];
                    let cw = cell_width(ch.encode_utf8(&mut buf));
                    if head_w + cw > room {
                        break;
                    }
                    head.push(ch);
                    head_w += cw;
                }
                if head.is_empty() {
                    // `room` is narrower than the next character. Flush and
                    // retry on a full-width line; if the line was already
                    // empty the character is wider than the whole panel, so
                    // take it anyway — the loop must make progress.
                    if used > 0 {
                        out.push(std::mem::take(&mut line));
                        used = 0;
                        continue;
                    }
                    match rest.chars().next() {
                        Some(ch) => head.push(ch),
                        None => break,
                    }
                }
                let bytes = head.len();
                line.push(Span::styled(head, style));
                out.push(std::mem::take(&mut line));
                used = 0;
                rest = &rest[bytes..];
            }
        }
    }
    out.push(line);
    out
}

/// Render BBCode into logical (unwrapped) lines, collecting link targets.
fn bbcode_lines(
    src: &str,
    links: &mut Vec<String>,
    theme: &Theme,
) -> Vec<Vec<Span<'static>>> {
    chunk_lines(&bbcode::render(src), links, theme)
}

/// `bbcode_lines` over an already-parsed chunk stream. Split out so a screen
/// that treats some chunks specially — the compose preview lifts image
/// references out into a caption plus reserved image rows — can render the
/// runs between them without a second copy of this loop.
pub(crate) fn chunk_lines(
    chunks: &[Chunk],
    links: &mut Vec<String>,
    theme: &Theme,
) -> Vec<Vec<Span<'static>>> {
    let mut out: Vec<Vec<Span<'static>>> = Vec::new();
    let mut current: Vec<Span<'static>> = Vec::new();
    for chunk in chunks.iter().cloned() {
        match chunk {
            Chunk::Text(t, s) => {
                if t.is_empty() {
                    continue;
                }
                for (i, seg) in t.split('\n').enumerate() {
                    if i > 0 {
                        out.push(std::mem::take(&mut current));
                    }
                    if !seg.is_empty() {
                        current.push(Span::styled(seg.to_string(), style_from(theme, &s)));
                    }
                }
            }
            Chunk::Link(label, url, s) => {
                links.push(url.clone());
                // Plain styled text only — never embed OSC sequences in spans
                // (CLAUDE.md hard rule 1). Select with the mouse, or press o.
                for (i, seg) in label.split('\n').enumerate() {
                    if i > 0 {
                        out.push(std::mem::take(&mut current));
                    }
                    if !seg.is_empty() {
                        current.push(Span::styled(seg.to_string(), style_from(theme, &s)));
                    }
                }
                current.push(Span::styled(
                    format!(" [{}]", links.len()),
                    link_style(theme),
                ));
            }
            // The thread view has the real picture beside its own caption
            // (built from the API's attachment record), so here an image
            // reference stays what it has always been: the `[image]`
            // placeholder plus a numbered link for `o` / `1`-`9`.
            Chunk::Image(url, s) => {
                links.push(url);
                current.push(Span::styled("[image]".to_string(), style_from(theme, &s)));
                current.push(Span::styled(
                    format!(" [{}]", links.len()),
                    link_style(theme),
                ));
            }
            Chunk::Attach(id, s) => {
                current.push(Span::styled(
                    format!("[attachment {id}]"),
                    style_from(theme, &s),
                ));
            }
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

/// Back-compat wrapper kept for the golden tests and any caller that just
/// wants flat lines.
pub(crate) fn push_bbcode(
    lines: &mut Vec<Line<'static>>,
    links: &mut Vec<String>,
    src: &str,
    theme: &Theme,
) {
    for l in bbcode_lines(src, links, theme) {
        lines.push(Line::from(l));
    }
}

impl ThreadViewState {
    /// The post number of `posts[i]` within the whole thread. `pagination` has
    /// no `per_page`, so the last page is counted backwards from the total and
    /// earlier pages use this page's own length.
    pub fn post_number(&self, i: usize) -> u64 {
        let len = self.posts.len() as u64;
        let page = self.page.max(1) as u64;
        if self.page >= self.last_page.max(1) && self.total >= len {
            self.total - len + 1 + i as u64
        } else {
            (page - 1) * len + 1 + i as u64
        }
    }

    /// Total posts in the thread, best effort.
    pub fn post_total(&self) -> u64 {
        if self.total > 0 {
            self.total
        } else if self.thread.reply_count > 0 {
            self.thread.reply_count + 1
        } else {
            self.posts.len() as u64
        }
    }

    pub fn rebuild_lines(&mut self, theme: &Theme, g: &Glyphs) {
        let width = if self.width == 0 { 80 } else { self.width } as usize;
        self.width = width as u16;
        let policy = self.images;
        let mut lines: Vec<Line<'static>> = Vec::new();
        let mut links: Vec<String> = Vec::new();
        let mut offsets: Vec<usize> = Vec::new();
        let mut slots: Vec<Slot> = Vec::new();

        lines.push(thread_summary_line(self, theme, g, width));
        lines.push(Line::from(Span::raw("")));

        let solution_id = self.thread.solution_post_id();
        for (i, post) in self.posts.iter().enumerate() {
            offsets.push(lines.len());
            let selected = i == self.sel_post;
            let number = self.post_number(i);
            let header_line = lines.len();
            lines.push(post_header_line(post, number, theme, width));
            // Only when the payload actually carries author details; a blank
            // dim row under every header just loosens the card for nothing.
            let meta = post_meta_line(post, theme, width);
            let has_meta = meta.is_some();
            if let Some(meta) = meta {
                lines.push(meta);
            }
            // The avatar is 2 rows x 5 cells (DESIGN.md) over the header row
            // and the meta row beneath it — the initials chip's own slot plus
            // one column, so nothing reflows and the chip stays underneath as
            // the fallback. Without a meta row the second row would be body
            // text, so that post keeps the chip.
            if policy.inline()
                && has_meta
                && let Some(url) = images::avatar_url(post.user.as_ref())
            {
                slots.push(Slot {
                    line: header_line,
                    x: 1,
                    cols: images::AVATAR_COLS,
                    rows: images::AVATAR_ROWS,
                    key: url.to_string(),
                });
            }

            let gutter_style = if selected {
                Style::new().fg(theme.accent)
            } else {
                theme.faint()
            };
            let gutter = |extra: Vec<Span<'static>>| -> Line<'static> {
                let mut spans = vec![
                    Span::raw(" "),
                    Span::styled(g.gutter.to_string(), gutter_style),
                    Span::raw(" "),
                ];
                spans.extend(extra);
                Line::from(spans)
            };
            let body_w = width.saturating_sub(3).max(8);

            if solution_id == Some(post.post_id) {
                lines.push(gutter(vec![Span::styled(
                    format!("{} marked solution", g.solved),
                    Style::new().fg(theme.ok).add_modifier(Modifier::BOLD),
                )]));
            }

            let post_link_base = links.len();
            for logical in bbcode_lines(&post.message, &mut links, theme) {
                for wrapped in wrap_spans(&logical, body_w) {
                    lines.push(gutter(wrapped));
                }
            }

            // Attachments. Every image gets its caption (`\u{25A3} name \u{00B7} W\u{00D7}H \u{00B7} n of N`,
            // and `n` is the digit that opens it in a browser); on a graphics
            // tier the image itself is reserved directly under its caption, at
            // most 40 % of the panel wide and 12 rows tall. Non-image
            // attachments are captioned but carry no index: no digit opens
            // them and nothing can decode them.
            let image_count = post.attachments.iter().filter(|a| a.is_image()).count();
            let mut image_n = 0usize;
            for att in post.attachments.iter() {
                let dims = match (att.width, att.height) {
                    (Some(w), Some(h)) if w > 0 && h > 0 => format!(" \u{00B7} {w}\u{00D7}{h}"),
                    _ => String::new(),
                };
                let caption = if att.is_image() {
                    image_n += 1;
                    format!(
                        "{} {}{dims} \u{00B7} {image_n} of {image_count}",
                        g.image, att.filename
                    )
                } else {
                    format!("{} {}{dims}", g.image, att.filename)
                };
                lines.push(gutter(vec![Span::styled(
                    truncate(&caption, body_w),
                    theme.dim(),
                )]));

                if !policy.inline() {
                    continue;
                }
                let Some(url) = images::attachment_url(att) else {
                    continue;
                };
                let (cols, rows) = images::attachment_box(width as u16, att, policy.font);
                let cols = cols.min(body_w as u16).max(1);
                slots.push(Slot {
                    line: lines.len(),
                    x: 3,
                    cols,
                    rows,
                    key: url.to_string(),
                });
                // Blank gutter rows the app paints the image over. Reserved
                // here rather than at render time so scrolling, `n`/`N` post
                // jumps and the panel footer all agree about where the image
                // is.
                for _ in 0..rows {
                    lines.push(gutter(Vec::new()));
                }
            }
            // Tier 5 (`ThreadText.dc.html`): say what the digits do, and why
            // there is no picture.
            if image_count > 0 && !policy.inline() {
                let last = image_count.min(9);
                let which = if last == 1 {
                    "1 opens it".to_string()
                } else {
                    format!("1\u{2013}{last} open them")
                };
                lines.push(gutter(vec![Span::styled(
                    truncate(
                        &format!(
                            "{which} in your browser \u{00B7} inline images need kitty, sixel or iTerm2"
                        ),
                        body_w,
                    ),
                    theme.dim(),
                )]));
            }

            lines.push(gutter(vec![Span::styled(
                format!(
                    "{} {}   {} {}",
                    g.like, post.reaction_score, g.vote, post.vote_score
                ),
                theme.dim(),
            )]));

            for (n, url) in links.iter().enumerate().skip(post_link_base) {
                lines.push(gutter(vec![
                    Span::styled(format!("[{}] ", n + 1), Style::new().fg(theme.accent)),
                    Span::styled(truncate(url, body_w.saturating_sub(6)), link_style(theme)),
                ]));
            }

            lines.push(Line::from(Span::raw("")));
        }

        self.lines = lines;
        self.links = links;
        self.post_line_offsets = offsets;
        self.image_slots = slots;
    }
}

/// The card stack's first line: prefix chip · forum · started by · date ·
/// views · replies, with `★ watching` pushed to the right edge.
fn thread_summary_line(
    s: &ThreadViewState,
    theme: &Theme,
    g: &Glyphs,
    width: usize,
) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = vec![Span::raw(" ")];
    if let Some(prefix) = &s.thread.prefix
        && !prefix.trim().is_empty()
    {
        spans.push(chrome::chip(theme, &chrome::short_prefix(prefix)));
        spans.push(Span::raw(" "));
    }
    let mut meta = String::new();
    if !s.forum_title.is_empty() {
        meta.push_str(&s.forum_title);
        meta.push_str(" \u{00B7} ");
    }
    meta.push_str("started by ");
    spans.push(Span::styled(meta, theme.dim()));
    spans.push(Span::styled(
        s.thread.username.clone(),
        theme.base().add_modifier(Modifier::BOLD),
    ));
    spans.push(Span::styled(
        format!(
            " \u{00B7} {} \u{00B7} {} views \u{00B7} {} replies",
            fmt_date(s.thread.post_date),
            thousands(s.thread.view_count),
            thousands(s.thread.reply_count)
        ),
        theme.dim(),
    ));

    let right: Vec<Span<'static>> = if s.thread.is_watching {
        vec![
            Span::styled(g.watching.to_string(), Style::new().fg(theme.warn)),
            Span::styled(" watching ".to_string(), theme.dim()),
        ]
    } else {
        Vec::new()
    };
    justify(spans, right, width)
}

/// A post's header: the 4-cell avatar slot, the author, their chips, and the
/// timestamp + `#n` on the right.
fn post_header_line(post: &Post, number: u64, theme: &Theme, width: usize) -> Line<'static> {
    let mut spans = vec![
        Span::raw(" "),
        // Phase 3 paints an avatar image over exactly these four cells; the
        // initials chip is the same size so nothing reflows when it does.
        chrome::initials_chip(theme, &post.username),
        Span::raw("  "),
        Span::styled(
            post.username.clone(),
            theme.base().add_modifier(Modifier::BOLD),
        ),
    ];
    if post.user_id == AI_USER_ID {
        spans.push(Span::raw(" "));
        spans.push(chrome::chip_active(theme, "AI"));
    }
    if post.user.as_ref().is_some_and(|u| u.is_staff) {
        spans.push(Span::raw(" "));
        spans.push(chrome::chip(theme, "STAFF"));
    }
    let right = vec![
        Span::styled(fmt_stamp(post.post_date), theme.dim()),
        Span::styled(format!("   #{number} "), theme.dim()),
    ];
    justify(spans, right, width)
}

/// The dim line under a post header: the author's title, post count, join date.
fn post_meta_line(post: &Post, theme: &Theme, width: usize) -> Option<Line<'static>> {
    let mut parts: Vec<String> = Vec::new();
    if let Some(u) = &post.user {
        if !u.custom_title.trim().is_empty() {
            parts.push(u.custom_title.clone());
        }
        if u.message_count > 0 {
            parts.push(format!("{} posts", thousands(u.message_count)));
        }
        if u.register_date > 0 {
            parts.push(format!("joined {}", fmt_date(u.register_date)));
        }
    }
    if parts.is_empty() && post.attach_count > 0 {
        parts.push(format!("{} attachments", post.attach_count));
    }
    if parts.is_empty() {
        return None;
    }
    let text = truncate(&parts.join(" \u{00B7} "), width.saturating_sub(8));
    Some(Line::from(vec![
        // Aligned under the username, not the avatar slot.
        Span::raw("       "),
        Span::styled(text, theme.dim()),
    ]))
}

/// Left spans, then padding, then right spans — clipped to `width` with the
/// left side losing cells first (the right side is short and load-bearing).
fn justify(
    left: Vec<Span<'static>>,
    right: Vec<Span<'static>>,
    width: usize,
) -> Line<'static> {
    let lw: usize = left.iter().map(Span::width).sum();
    let rw: usize = right.iter().map(Span::width).sum();
    if rw == 0 {
        return Line::from(clip_to(left, width));
    }
    if lw + rw < width {
        let mut spans = left;
        spans.push(Span::raw(" ".repeat(width - lw - rw)));
        spans.extend(right);
        return Line::from(spans);
    }
    if rw + 2 <= width {
        let mut spans = clip_to(left, width - rw - 1);
        let used: usize = spans.iter().map(Span::width).sum();
        spans.push(Span::raw(" ".repeat(width - rw - used)));
        spans.extend(right);
        return Line::from(spans);
    }
    Line::from(clip_to(left, width))
}

fn clip_to(spans: Vec<Span<'static>>, max: usize) -> Vec<Span<'static>> {
    let mut out = Vec::with_capacity(spans.len());
    let mut used = 0usize;
    for s in spans {
        let w = s.width();
        if used + w <= max {
            used += w;
            out.push(s);
            continue;
        }
        let room = max - used;
        if room > 0 {
            let text: String = s.content.chars().take(room).collect();
            out.push(Span::styled(text, s.style));
        }
        break;
    }
    out
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
        // `w` (watch) stays advertised but inert: XenForo's REST API exposes
        // no thread-watch endpoint (see common/src/api.rs — there is no
        // `watch` method to call). It is a no-op rather than absent so the
        // key bar the design specifies is the key bar members learn.
        KeyCode::Char('w') => Action::None,
        // `1`–`9` open the nth IMAGE attachment of the selected post in a
        // browser — the same numbering the caption lines print, on every
        // tier. (Enter on an image line would need a second, larger encode
        // inside an overlay, and overlays deliberately suppress image
        // drawing; that is not the cheap toggle the phase brief allows for.)
        KeyCode::Char(c) if c.is_ascii_digit() && c != '0' => {
            let n = c.to_digit(10).unwrap_or(0) as usize;
            match s
                .posts
                .get(s.sel_post)
                .and_then(|p| p.attachments.iter().filter(|a| a.is_image()).nth(n - 1))
                .and_then(|a| a.open_url())
            {
                Some(url) => Action::OpenUrl(url.to_string()),
                None => Action::None,
            }
        }
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

pub fn thread_view_hints() -> Hints {
    Hints::with_short(
        &[
            ("r", "reply"),
            ("j/k", "scroll"),
            ("n/N", "post"),
            ("l", "like"),
            ("v", "vote"),
            ("o", "links"),
            ("1-9", "image"),
            ("w", "watch"),
            ("u", "open in web"),
            ("Esc", "back"),
        ],
        &[
            ("r", "reply"),
            ("j/k", ""),
            ("n/N", "post"),
            ("l", "like"),
            ("v", "vote"),
            ("o", "links"),
            ("Esc", "back"),
        ],
        0,
    )
}

pub fn thread_view_crumb(s: &ThreadViewState) -> String {
    s.thread.title.clone()
}

pub fn render_thread_view(
    s: &mut ThreadViewState,
    f: &mut ratatui::Frame,
    area: Rect,
    theme: &Theme,
    g: &Glyphs,
) {
    let total = s.post_total();
    let right = format!(
        "{total} posts \u{00B7} page {} of {}",
        s.page.max(1),
        s.last_page.max(1)
    );
    let bottom = if s.posts.is_empty() {
        None
    } else {
        Some(format!(
            "post {} of {total}",
            s.post_number(s.sel_post.min(s.posts.len() - 1))
        ))
    };
    let block = chrome::panel(
        theme,
        g,
        &truncate(&s.thread.title, 60),
        true,
        Some(&right),
        bottom.as_deref(),
    );
    let inner = block.inner(area);
    f.render_widget(block, area);
    // Cleared before every early return: a stale rect from the previous frame
    // would have the app paint an image over a spinner or an error.
    s.image_requests.clear();
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    if s.loading && s.posts.is_empty() {
        f.render_widget(
            Paragraph::new(format!(
                "{} Loading thread…",
                chrome::spinner(g, chrome::spinner_tick())
            ))
            .style(theme.dim()),
            inner,
        );
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

    // Lines are wrapped to a fixed width, so a resize has to rebuild them.
    if s.width != inner.width {
        s.width = inner.width;
        s.rebuild_lines(theme, g);
    }

    let max_scroll = (s.lines.len() as u16).saturating_sub(inner.height);
    if s.scroll > max_scroll {
        s.scroll = max_scroll;
    }
    f.render_widget(
        Paragraph::new(s.lines.clone()).scroll((s.scroll, 0)),
        inner,
    );

    // Translate the reserved slots into absolute screen rects for the app to
    // paint. A slot that is only partly on screen is dropped rather than
    // clipped: kitty and sixel paint pixels, not cells, and half an image
    // would spill over the panel border.
    if s.images.inline() {
        let scroll = s.scroll as usize;
        let mut reqs: Vec<images::Request> = Vec::with_capacity(s.image_slots.len());
        for slot in &s.image_slots {
            if slot.line < scroll {
                continue;
            }
            let y = inner.y as usize + (slot.line - scroll);
            if y + slot.rows as usize > inner.bottom() as usize {
                continue;
            }
            let x = inner.x as usize + slot.x as usize;
            if x + slot.cols as usize > inner.right() as usize {
                continue;
            }
            reqs.push(images::Request {
                key: slot.key.clone(),
                rect: Rect::new(x as u16, y as u16, slot.cols, slot.rows),
            });
        }
        s.image_requests = reqs;
    }

    if s.link_popup {
        let popup_area = super::centered_box(
            area,
            area.width.min(70),
            (s.links.len() as u16 + 4).min(20),
        );
        let popup = chrome::panel(theme, g, "Links", true, Some("Enter open \u{00B7} Esc close"), None);
        let p_inner = popup.inner(popup_area);
        f.render_widget(ratatui::widgets::Clear, popup_area);
        f.render_widget(popup, popup_area);
        let items: Vec<ListItem> = s
            .links
            .iter()
            .enumerate()
            .map(|(i, url)| {
                ListItem::new(Line::from(vec![
                    Span::styled(format!(" [{}] ", i + 1), Style::new().fg(theme.accent)),
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
    } else if max == 0 {
        String::new()
    } else {
        let cut: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{cut}\u{2026}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::glyph::UNICODE;
    use crate::screens::{Screen, home_state};
    use common::models::{Node, Post, Thread, User};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::{KeyEvent, KeyModifiers};

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    fn text(line: &Line<'static>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    /// Column (cell) index of `needle`, not the byte offset `str::find`
    /// returns — every glyph in this grammar is multi-byte and single-cell.
    fn col(s: &str, needle: &str) -> Option<usize> {
        s.find(needle).map(|b| s[..b].chars().count())
    }

    // ---------- row grammar ----------

    #[test]
    fn wide_grammar_matches_the_reference_artboard_columns() {
        // Main.dc.html: 120-column screen, 37-wide Forums pane, 83-wide list
        // pane -> 81 inner cells.
        let gram = Grammar::for_width(120);
        assert_eq!(gram, Grammar::Wide);
        assert_eq!(gram.fixed(), 37);
        assert_eq!(gram.title_width(81), 44);

        let theme = Theme::truecolor();
        let head = thread_list_header(&theme, gram, 81);
        let h = text(&head);
        assert_eq!(h.chars().count(), 81);
        assert_eq!(col(&h, "Thread"), Some(5));
        assert_eq!(col(&h, "Started by"), Some(51));
        assert_eq!(col(&h, "Replies"), Some(65));
        assert_eq!(col(&h, "Active"), Some(74));
        assert!(
            !h.contains('\u{2026}'),
            "the column header must never be clipped: {h}"
        );
    }

    #[test]
    fn narrow_grammar_matches_the_reference_artboard_columns() {
        // Narrow.dc.html: 80 columns, one pane -> 78 inner cells.
        let gram = Grammar::for_width(80);
        assert_eq!(gram, Grammar::Narrow);
        assert_eq!(gram.fixed(), 29);
        assert_eq!(gram.title_width(78), 49);

        let theme = Theme::truecolor();
        let t = Thread {
            title: "How do I control MS edge update schedule?".into(),
            username: "HItest".into(),
            prefix: Some("Windows 11".into()),
            reply_count: 27,
            is_unread: true,
            discussion_open: true,
            ..Default::default()
        };
        let row = text(&thread_row(&t, &theme, &UNICODE, gram, 78));
        assert_eq!(row.chars().count(), 78);
        assert_eq!(col(&row, "HItest"), Some(56));
        // Replies are right-aligned in a 4-cell column ending at 69.
        assert_eq!(col(&row, "27"), Some(68));
    }

    #[test]
    fn thread_row_puts_every_column_where_the_artboard_does() {
        let theme = Theme::truecolor();
        let t = Thread {
            title: "How do I control MS edge update schedule?".into(),
            username: "HItest".into(),
            prefix: Some("Windows 11".into()),
            reply_count: 27,
            is_unread: true,
            discussion_open: true,
            ..Default::default()
        };
        let line = thread_row(&t, &theme, &UNICODE, Grammar::Wide, 81);
        let row = text(&line);
        assert_eq!(line.width(), 81, "{row}");
        assert_eq!(&row[..1], " ");
        assert_eq!(row.chars().nth(1), Some('\u{25CF}'), "unread column: {row}");
        // The prefix chip is inline, inside the title field.
        assert_eq!(col(&row, " Win11 "), Some(5));
        assert_eq!(col(&row, "How do I"), Some(13));
        assert_eq!(col(&row, "HItest"), Some(51));
        assert_eq!(col(&row, "27"), Some(70));
    }

    #[test]
    fn thread_row_clips_the_title_and_keeps_the_row_width() {
        let theme = Theme::truecolor();
        let t = Thread {
            title: "Windows Incredibly Slow To Start; Constantly Locks Up; Phone Link Never Works"
                .into(),
            username: "Starwind Amada".into(),
            reply_count: 1,
            ..Default::default()
        };
        for inner in [40usize, 60, 71, 78, 81, 120] {
            for gram in [Grammar::Wide, Grammar::Narrow] {
                let line = thread_row(&t, &theme, &UNICODE, gram, inner);
                assert_eq!(line.width(), inner, "{gram:?} at inner {inner}");
            }
        }
        let row = text(&thread_row(&t, &theme, &UNICODE, Grammar::Wide, 81));
        assert!(row.contains('\u{2026}'), "long titles clip with an ellipsis");
        // The author column clips too rather than pushing the age column.
        assert_eq!(col(&row, "Starwind"), Some(51));
        assert_eq!(row.chars().count(), 81);
    }

    #[test]
    fn type_mark_covers_every_thread_kind_including_closed() {
        let theme = Theme::truecolor();
        let open = Thread { discussion_open: true, ..Default::default() };
        let mark = |t: &Thread| type_mark(t, &theme, &UNICODE).0;

        // A plain open discussion has no type glyph at all.
        assert_eq!(mark(&open), " ");
        // Closed threads are the last arm: everything else outranks them.
        let closed = Thread { discussion_open: false, ..Default::default() };
        assert_eq!(mark(&closed), UNICODE.locked);
        assert_eq!(
            mark(&Thread { sticky: true, ..closed.clone() }),
            UNICODE.sticky,
            "sticky outranks locked"
        );
        assert_eq!(
            mark(&Thread {
                discussion_type: "article".into(),
                ..closed.clone()
            }),
            UNICODE.article,
            "article outranks locked"
        );
        // ...and it reaches the rendered row, dim, in the type column.
        let row = text(&thread_row(&closed, &theme, &UNICODE, Grammar::Wide, 81));
        assert_eq!(row.chars().nth(3), Some('\u{2298}'), "type column: {row}");
    }

    #[test]
    fn thread_row_survives_panels_too_small_for_the_grammar() {
        let theme = Theme::truecolor();
        let t = Thread {
            title: "A thread".into(),
            username: "someone".into(),
            ..Default::default()
        };
        for inner in 0..40usize {
            let line = thread_row(&t, &theme, &UNICODE, Grammar::Wide, inner);
            assert!(line.width() <= inner, "overflowed at inner {inner}");
        }
    }

    #[test]
    fn list_range_derives_the_last_page_backwards_from_the_total() {
        let mut s = ThreadListState {
            threads: vec![Thread::default(); 20],
            page: 1,
            last_page: 22,
            total: 431,
            ..Default::default()
        };
        assert_eq!(list_range(&s).as_deref(), Some("1\u{2013}20 of 431"));
        s.page = 2;
        assert_eq!(list_range(&s).as_deref(), Some("21\u{2013}40 of 431"));
        // The last page is short; counting forwards would overshoot.
        s.page = 22;
        s.threads.truncate(11);
        assert_eq!(list_range(&s).as_deref(), Some("421\u{2013}431 of 431"));
        // No total from the server -> no footer at all.
        s.total = 0;
        assert!(list_range(&s).is_none());
    }

    // ---------- Forums panel ----------

    fn sample_nodes() -> Vec<Node> {
        vec![
            Node {
                node_id: 1,
                title: "Windows Forums".into(),
                node_type: "Category".into(),
                depth: 0,
                ..Default::default()
            },
            Node {
                node_id: 302,
                title: "Windows Help and Support".into(),
                node_type: "Forum".into(),
                parent_node_id: 1,
                depth: 1,
                ..Default::default()
            },
            Node {
                node_id: 303,
                title: "Windows Upgrade and Installation".into(),
                node_type: "Forum".into(),
                parent_node_id: 1,
                depth: 1,
                ..Default::default()
            },
            Node {
                node_id: 400,
                title: "BSOD AI Analyzer".into(),
                node_type: "LinkForum".into(),
                parent_node_id: 303,
                depth: 2,
                view_url: Some("https://example.com/bsod".into()),
                ..Default::default()
            },
        ]
    }

    #[test]
    fn forum_rows_follow_the_quick_block_then_caps_then_indented_forums() {
        let theme = Theme::truecolor();
        let s = ForumTreeState {
            nodes: sample_nodes(),
            ..Default::default()
        };
        let rows = forum_rows(&s, 302, &theme, &UNICODE, 35);
        let t: Vec<String> = rows.iter().map(|r| text(&r.line)).collect();
        assert_eq!(t[0], " QUICK");
        assert_eq!(t[1], " L  Latest posts");
        assert_eq!(t[2], " 1  Windows News");
        assert_eq!(t[3], " 2  Security Alerts");
        assert_eq!(t[4], " 3  Windows Tutorials");
        assert_eq!(t[5], "");
        // Categories: CAPS at column 1.
        assert_eq!(t[6], " WINDOWS FORUMS");
        // The current forum carries the `›` marker; siblings are indented two.
        assert_eq!(t[7], " \u{203A} Windows Help and Support");
        assert_eq!(t[8], "   Windows Upgrade and Installation");
        // A link forum is marked and indented one level deeper.
        assert_eq!(t[9], "     BSOD AI Analyzer \u{2197}");
        // Only the four node rows map back to `nodes`.
        assert_eq!(rows[6].node, Some(0));
        assert_eq!(rows[9].node, Some(3));
        assert!(rows[0].node.is_none());
    }

    #[test]
    fn tree_window_keeps_the_selection_visible_and_leaves_room_for_more() {
        let mut scroll = 0usize;
        // Everything fits: no window, no indicator.
        assert_eq!(tree_window(10, 20, 3, &mut scroll), (0, 10));
        // Selection below the fold pulls the window down, reserving one row.
        scroll = 0;
        assert_eq!(tree_window(40, 10, 12, &mut scroll), (4, 13));
        assert_eq!(scroll, 4);
        // Selection above the window pulls it back up.
        assert_eq!(tree_window(40, 10, 2, &mut scroll), (2, 11));
        // Parked at the bottom: the whole height is rows, no indicator row.
        scroll = 0;
        assert_eq!(tree_window(40, 10, 39, &mut scroll), (30, 40));
    }

    // ---------- Home ----------

    fn home_with_data() -> HomeState {
        HomeState {
            tree: ForumTreeState {
                nodes: sample_nodes(),
                sel: 1,
                ..Default::default()
            },
            list: ThreadListState {
                node_id: 302,
                title: "Windows Help and Support".into(),
                threads: vec![
                    Thread {
                        thread_id: 1,
                        title: "How do I control MS edge update schedule?".into(),
                        username: "HItest".into(),
                        prefix: Some("Windows 11".into()),
                        reply_count: 27,
                        is_unread: true,
                        ..Default::default()
                    },
                    Thread {
                        thread_id: 2,
                        title: "Windows 12 is coming..".into(),
                        username: "kemical".into(),
                        reply_count: 26,
                        sticky: true,
                        ..Default::default()
                    },
                ],
                page: 1,
                last_page: 22,
                total: 431,
                ..Default::default()
            },
            focus: Pane::List,
            dual: true,
        }
    }

    /// Render a screen headless and return the rows as strings.
    fn render_rows(screen: &mut Screen, w: u16, h: u16) -> Vec<String> {
        let theme = Theme::truecolor();
        let mut term = Terminal::new(TestBackend::new(w, h)).expect("terminal");
        term.draw(|f| {
            let area = f.area();
            screen.render(f, area, &theme, &UNICODE);
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

    #[test]
    fn home_renders_two_panes_at_120_columns() {
        let mut screen = Screen::Home(home_with_data());
        let rows = render_rows(&mut screen, 120, 36);
        for (i, r) in rows.iter().enumerate() {
            assert_eq!(r.chars().count(), 120, "row {i} is not the frame width");
        }
        // The Forums pane is 37 wide, so its right border and the list pane's
        // left border are adjacent at columns 36 and 37.
        assert_eq!(rows[0].chars().nth(36), Some('\u{256E}'), "{}", rows[0]);
        assert_eq!(rows[0].chars().nth(37), Some('\u{256D}'), "{}", rows[0]);
        assert!(rows[0].contains(" Forums "), "{}", rows[0]);
        assert!(rows[0].contains("page 1 of 22"), "{}", rows[0]);
        assert!(
            rows[35].contains("1\u{2013}2 of 431"),
            "bottom title: {}",
            rows[35]
        );
        // QUICK block, then the tree, beside the column header and the rows.
        assert!(rows[1].contains("QUICK"), "{}", rows[1]);
        assert!(
            rows[1].contains("Thread") && rows[1].contains("Started by"),
            "{}",
            rows[1]
        );
        assert!(!rows[1].contains('\u{2026}'), "header clipped: {}", rows[1]);
        assert!(rows[2].contains("Latest posts"), "{}", rows[2]);
        assert!(rows[2].contains("How do I control"), "{}", rows[2]);
    }

    #[test]
    fn home_selection_band_spans_the_list_pane_width() {
        let theme = Theme::truecolor();
        let mut screen = Screen::Home(home_with_data());
        let mut term = Terminal::new(TestBackend::new(120, 36)).expect("terminal");
        term.draw(|f| {
            let area = f.area();
            screen.render(f, area, &theme, &UNICODE);
        })
        .expect("draw");
        let buf = term.backend().buffer().clone();
        // Row 2 of the frame is the first thread row (0 = borders, 1 = header).
        let y = 2u16;
        for x in 38..119u16 {
            assert_eq!(
                buf[(x, y)].style().bg,
                Some(theme.selected_bg),
                "selection band missing at column {x}"
            );
        }
        // …and it stops at the panel border.
        assert_ne!(buf[(37, y)].style().bg, Some(theme.selected_bg));
        assert_ne!(buf[(119, y)].style().bg, Some(theme.selected_bg));
    }

    #[test]
    fn home_falls_back_to_one_pane_at_80_columns() {
        let mut home = home_with_data();
        home.focus = Pane::List;
        let mut screen = Screen::Home(home);
        let rows = render_rows(&mut screen, 80, 24);
        for (i, r) in rows.iter().enumerate() {
            assert_eq!(r.chars().count(), 80, "row {i} is not the frame width");
        }
        assert!(
            rows[0].contains("Windows Help and Support"),
            "{}",
            rows[0]
        );
        // Narrow grammar: no column-header row, so row 1 is a thread already.
        assert!(!rows[1].contains("Started by"), "{}", rows[1]);
        assert!(rows[1].contains("How do I control"), "{}", rows[1]);
        // The Forums pane is not on screen while the list has focus.
        assert!(!rows[1].contains("QUICK"), "{}", rows[1]);

        // Tab shows the other half in the same single panel.
        let Screen::Home(h) = &mut screen else {
            unreachable!()
        };
        h.focus = Pane::Tree;
        let rows = render_rows(&mut screen, 80, 24);
        assert!(rows[0].contains(" Forums "), "{}", rows[0]);
        assert!(rows[1].contains("QUICK"), "{}", rows[1]);
    }

    #[test]
    fn home_renders_at_absurd_sizes_without_panicking() {
        for (w, h) in [(120u16, 36u16), (110, 30), (109, 30), (80, 24), (40, 10), (20, 5), (4, 3)] {
            let mut screen = Screen::Home(home_with_data());
            let rows = render_rows(&mut screen, w, h);
            assert_eq!(rows.len(), h as usize);
        }
    }

    #[test]
    fn home_tab_moves_focus_and_enter_loads_into_the_pane() {
        let mut h = home_with_data();
        h.focus = Pane::Tree;
        // Tab moves focus, and does not leak an action to the app.
        let act = home_key(&mut h, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert!(matches!(act, Action::None));
        assert_eq!(h.focus, Pane::List);
        home_key(&mut h, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(h.focus, Pane::Tree);

        // Enter on a forum asks the app to load, and focus follows the load.
        let act = home_key(&mut h, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(act, Action::OpenThreadList(302, _)), "sel is the help forum");
        assert_eq!(h.focus, Pane::List);

        // h / Left return focus to the tree instead of popping the screen.
        let act = home_key(&mut h, key('h'));
        assert!(matches!(act, Action::None));
        assert_eq!(h.focus, Pane::Tree);
    }

    #[test]
    fn home_keeps_every_pre_redesign_key_in_both_panes() {
        for focus in [Pane::Tree, Pane::List] {
            let mut h = home_with_data();
            h.focus = focus;
            assert!(matches!(home_key(&mut h, key('1')), Action::OpenThreadList(4, _)));
            assert!(matches!(home_key(&mut h, key('2')), Action::OpenThreadList(84, _)));
            assert!(matches!(
                home_key(&mut h, key('3')),
                Action::OpenThreadList(305, _)
            ));
            let mut h = home_with_data();
            h.focus = focus;
            assert!(matches!(home_key(&mut h, key('L')), Action::OpenLatestThreads));
            assert!(matches!(home_key(&mut h, key('q')), Action::Quit));
        }
        // `m` marks read in either pane: the pane's forum, or the one under
        // the tree cursor.
        let mut h = home_with_data();
        h.focus = Pane::Tree;
        assert!(matches!(home_key(&mut h, key('m')), Action::MarkForumRead(302)));
        // The list pane keeps its own verbs.
        let mut h = home_with_data();
        h.focus = Pane::List;
        assert!(matches!(home_key(&mut h, key('m')), Action::MarkForumRead(302)));
        assert!(matches!(home_key(&mut h, key('N')), Action::StartNewThread(302)));
        assert!(matches!(home_key(&mut h, key('r')), Action::LoadForum(302, 1)));
        assert!(matches!(
            home_key(&mut h, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Action::OpenThread(t) if t.thread_id == 1
        ));
        // …and the tree pane keeps refresh + new thread.
        let mut h = home_with_data();
        h.focus = Pane::Tree;
        assert!(matches!(home_key(&mut h, key('r')), Action::LoadNodes));
        assert!(matches!(home_key(&mut h, key('N')), Action::StartNewThread(302)));
    }

    #[test]
    fn home_state_starts_on_the_tree_and_is_narrow_until_drawn() {
        let Screen::Home(h) = home_state(true) else {
            unreachable!()
        };
        assert_eq!(h.focus, Pane::Tree);
        assert!(!h.dual, "dual is a render-time fact, never a default");
        assert!(h.tree.loading);
    }

    // ---------- thread view ----------

    fn thread_view_fixture() -> ThreadViewState {
        let thread = Thread {
            thread_id: 101,
            title: "How do I control MS edge update schedule?".into(),
            username: "HItest".into(),
            user_id: 10,
            prefix: Some("Windows 11".into()),
            view_count: 686,
            reply_count: 27,
            is_watching: true,
            ..Default::default()
        };
        ThreadViewState {
            thread,
            forum_title: "Windows Help and Support".into(),
            posts: vec![
                Post {
                    post_id: 201,
                    user_id: 10,
                    username: "HItest".into(),
                    message: "Hi folks. I am using MS Edge as my default browser.".into(),
                    reaction_score: 0,
                    ..Default::default()
                },
                Post {
                    post_id: 202,
                    user_id: AI_USER_ID,
                    username: "WindowsForum AI".into(),
                    message: "Microsoft Edge supports a daily update-suppression window."
                        .into(),
                    reaction_score: 2,
                    vote_score: 1,
                    user: Some(User {
                        user_id: AI_USER_ID,
                        username: "WindowsForum AI".into(),
                        custom_title: "site assistant".into(),
                        message_count: 48_102,
                        is_staff: true,
                        ..Default::default()
                    }),
                    ..Default::default()
                },
            ],
            page: 1,
            last_page: 2,
            total: 28,
            width: 118,
            ..Default::default()
        }
    }

    #[test]
    fn thread_view_cards_carry_the_design_furniture() {
        let theme = Theme::truecolor();
        let mut s = thread_view_fixture();
        s.rebuild_lines(&theme, &UNICODE);
        let all: Vec<String> = s.lines.iter().map(text).collect();

        // First line: chip · forum · started by · date · views · replies, and
        // the watching star pinned right.
        assert!(all[0].contains(" Win11 "), "{}", all[0]);
        assert!(all[0].contains("Windows Help and Support"), "{}", all[0]);
        assert!(all[0].contains("started by HItest"), "{}", all[0]);
        assert!(all[0].contains("686 views"), "{}", all[0]);
        assert!(all[0].contains("27 replies"), "{}", all[0]);
        assert!(all[0].trim_end().ends_with("\u{2605} watching"), "{}", all[0]);
        assert_eq!(s.lines[0].width(), 118);

        // Post header: 4-cell avatar slot, bold name, AI + STAFF chips, #n.
        let header = &all[2];
        assert_eq!(&header[..1], " ");
        assert!(header.contains(" HI "), "initials chip: {header}");
        assert!(header.contains("HItest"), "{header}");
        assert!(header.trim_end().ends_with("#1"), "{header}");
        assert_eq!(s.lines[2].width(), 118);

        let ai_header = all
            .iter()
            .find(|l| l.contains("WindowsForum AI"))
            .expect("AI post header");
        assert!(ai_header.contains(" AI "), "{ai_header}");
        assert!(ai_header.contains(" STAFF "), "{ai_header}");
        assert!(ai_header.trim_end().ends_with("#2"), "{ai_header}");
        assert!(
            all.iter().any(|l| l.contains("site assistant") && l.contains("48,102 posts")),
            "meta line missing"
        );

        // Bodies sit behind a gutter; the footer is the reaction/vote pair.
        assert!(
            all.iter().any(|l| l.starts_with(" \u{2503} Hi folks.")),
            "gutter-prefixed body missing: {all:?}"
        );
        assert!(
            all.iter().any(|l| l.contains("\u{2661} 2   \u{25B2} 1")),
            "reaction/vote footer missing"
        );
    }

    #[test]
    fn thread_view_gutter_is_accent_on_the_selected_post_and_faint_elsewhere() {
        let theme = Theme::truecolor();
        let mut s = thread_view_fixture();
        s.sel_post = 1;
        s.rebuild_lines(&theme, &UNICODE);
        let gutters: Vec<(usize, Option<ratatui::style::Color>)> = s
            .lines
            .iter()
            .enumerate()
            .filter(|(_, l)| l.spans.get(1).is_some_and(|sp| sp.content == "\u{2503}"))
            .map(|(i, l)| (i, l.spans[1].style.fg))
            .collect();
        let first = s.post_line_offsets[0];
        let second = s.post_line_offsets[1];
        for (i, fg) in gutters {
            let want = if i >= second { theme.accent } else { theme.faint };
            assert_eq!(fg, Some(want), "gutter at line {i} (post starts {first}/{second})");
        }
    }

    // ---------- inline images (phase 3) ----------

    fn image_att(id: u32, name: &str, w: u32, h: u32) -> common::models::Attachment {
        common::models::Attachment {
            attachment_id: id,
            filename: name.into(),
            width: Some(w),
            height: Some(h),
            thumbnail_url: Some(format!("https://wf/thumb/{id}.png")),
            direct_url: Some(format!("https://wf/full/{id}.png")),
            ..Default::default()
        }
    }

    /// One post with two screenshots and a log file, and an author who has an
    /// avatar and enough profile for the meta row the avatar needs.
    fn thread_view_with_attachments() -> ThreadViewState {
        let mut s = thread_view_fixture();
        s.posts[0].attach_count = 3;
        s.posts[0].attachments = vec![
            image_att(1, "winver command returns this.webp", 1152, 720),
            image_att(2, "second shot.png", 800, 600),
            common::models::Attachment {
                attachment_id: 3,
                filename: "dump.txt".into(),
                direct_url: Some("https://wf/full/3.txt".into()),
                ..Default::default()
            },
        ];
        s.posts[0].user = Some(User {
            user_id: 10,
            username: "HItest".into(),
            custom_title: "Windows 11".into(),
            message_count: 14,
            avatar_urls: Some(common::models::AvatarUrls {
                s: Some("https://wf/avatar/s/10.jpg".into()),
                ..Default::default()
            }),
            ..Default::default()
        });
        s
    }

    #[test]
    fn text_tier_keeps_the_placeholder_line_and_says_which_digit_opens_it() {
        let theme = Theme::truecolor();
        let mut s = thread_view_with_attachments();
        assert_eq!(s.images.tier, crate::images::Tier::Text, "default is tier 5");
        s.rebuild_lines(&theme, &UNICODE);
        let all: Vec<String> = s.lines.iter().map(text).collect();

        // The placeholder is unchanged, and `n of N` counts images only,
        // because the digits open images only.
        assert!(
            all.iter().any(|l| l.contains("\u{25A3} winver command returns this.webp \u{00B7} 1152\u{00D7}720 \u{00B7} 1 of 2")),
            "{all:#?}"
        );
        assert!(all.iter().any(|l| l.contains("second shot.png \u{00B7} 800\u{00D7}600 \u{00B7} 2 of 2")), "{all:#?}");
        // The log file is captioned but carries no index: no digit opens it.
        let log = all.iter().find(|l| l.contains("dump.txt")).expect("log caption");
        assert!(!log.contains(" of "), "{log}");

        // The ThreadText.dc.html hint.
        let hint = all
            .iter()
            .find(|l| l.contains("open them in your browser"))
            .expect("digit hint");
        assert!(hint.contains("1\u{2013}2"), "{hint}");
        assert!(hint.contains("kitty"), "{hint}");

        // Nothing is reserved and nothing will be painted.
        assert!(s.image_slots.is_empty());

        // And it survives an actual frame at 120x36.
        let mut screen = Screen::ThreadView(s);
        let rows = render_rows(&mut screen, 120, 36);
        let joined = rows.join("\n");
        assert!(joined.contains("winver command returns this.webp"), "{joined}");
        assert!(joined.contains("open them in your browser"), "{joined}");
        let Screen::ThreadView(s) = &screen else { unreachable!() };
        assert!(s.image_requests.is_empty(), "the text tier paints nothing");
    }

    #[test]
    fn a_graphics_tier_reserves_the_image_box_and_the_avatar_slot() {
        let theme = Theme::truecolor();
        let mut s = thread_view_with_attachments();
        s.images = crate::images::Policy {
            tier: crate::images::Tier::Kitty,
            font: (10, 20),
        };
        s.rebuild_lines(&theme, &UNICODE);
        let all: Vec<String> = s.lines.iter().map(text).collect();

        // Captions stay; the hint does not (there is a picture instead).
        assert!(all.iter().any(|l| l.contains("winver command returns this.webp")));
        assert!(!all.iter().any(|l| l.contains("open them in your browser")), "{all:#?}");

        // Slots: the avatar (2x5 at column 1) then the two screenshots
        // (behind the gutter at column 3), never the log file.
        assert_eq!(s.image_slots.len(), 3, "{:#?}", s.image_slots);
        let avatar = &s.image_slots[0];
        assert_eq!(avatar.key, "https://wf/avatar/s/10.jpg");
        assert_eq!((avatar.x, avatar.cols, avatar.rows), (1, 5, 2));

        let shot = &s.image_slots[1];
        assert_eq!(shot.key, "https://wf/thumb/1.png", "the thumbnail, not the full file");
        assert_eq!(shot.x, 3, "images sit behind the post gutter");
        assert_eq!(
            (shot.cols, shot.rows),
            crate::images::fit(118, (1152, 720), (10, 20))
        );
        // Exactly `rows` blank gutter rows were reserved under the caption.
        for n in 0..shot.rows as usize {
            let row = &all[shot.line + n];
            assert_eq!(row.trim(), "\u{2503}", "reserved row {n}: {row:?}");
        }

        // A frame turns the visible slots into absolute rects.
        let mut screen = Screen::ThreadView(s);
        let _ = render_rows(&mut screen, 120, 36);
        let Screen::ThreadView(s) = &screen else { unreachable!() };
        assert!(!s.image_requests.is_empty());
        let inner_x = 1u16; // panel border
        assert_eq!(s.image_requests[0].rect.x, inner_x + 1, "avatar column");
        assert_eq!(s.image_requests[0].rect.width, 5);
        assert_eq!(s.image_requests[0].rect.height, 2);
        for req in &s.image_requests {
            assert!(
                req.rect.right() <= 119 && req.rect.bottom() <= 35,
                "an image escaped the panel: {req:?}"
            );
        }
    }

    #[test]
    fn an_image_that_does_not_fit_the_panel_is_dropped_rather_than_clipped() {
        let theme = Theme::truecolor();
        let mut s = thread_view_with_attachments();
        s.images = crate::images::Policy {
            tier: crate::images::Tier::Kitty,
            font: (10, 20),
        };
        s.rebuild_lines(&theme, &UNICODE);

        // At 20 rows the first screenshot still fits under the fold but the
        // second one's 12-row box runs past the panel's bottom border. Kitty
        // and sixel paint pixels, not cells, so half a box would spill over
        // the border instead of being clipped by it.
        let mut screen = Screen::ThreadView(s);
        let _ = render_rows(&mut screen, 120, 20);
        let Screen::ThreadView(s) = &screen else { unreachable!() };
        let keys: Vec<&str> = s.image_requests.iter().map(|r| r.key.as_str()).collect();
        assert!(keys.contains(&"https://wf/thumb/1.png"), "{keys:?}");
        assert!(
            !keys.contains(&"https://wf/thumb/2.png"),
            "a box running past the bottom edge must be dropped: {keys:?}"
        );
        for req in &s.image_requests {
            assert!(req.rect.y >= 1, "an image escaped the top border: {req:?}");
            assert!(req.rect.bottom() <= 19, "an image escaped the bottom: {req:?}");
        }
    }

    #[test]
    fn digits_open_the_nth_image_of_the_selected_post() {
        let mut s = thread_view_with_attachments();
        s.sel_post = 0;

        // `direct_url` is what a browser should open, not the thumbnail.
        match thread_view_key(&mut s, key('1')) {
            Action::OpenUrl(u) => assert_eq!(u, "https://wf/full/1.png"),
            _ => panic!("1 did not open the first image"),
        }
        match thread_view_key(&mut s, key('2')) {
            Action::OpenUrl(u) => assert_eq!(u, "https://wf/full/2.png"),
            _ => panic!("2 did not open the second image"),
        }
        // The log file is the third attachment but not the third image.
        assert!(matches!(thread_view_key(&mut s, key('3')), Action::None));
        assert!(matches!(thread_view_key(&mut s, key('9')), Action::None));

        // The post with no attachments answers nothing at all.
        s.sel_post = 1;
        assert!(matches!(thread_view_key(&mut s, key('1')), Action::None));
    }

    #[test]
    fn a_post_without_a_meta_row_keeps_its_initials_chip() {
        let theme = Theme::truecolor();
        let mut s = thread_view_fixture();
        // No custom title, no post count, no join date, no attachments: the
        // meta row is dropped, so the second avatar row would be body text.
        s.posts[0].user = Some(User {
            user_id: 10,
            username: "HItest".into(),
            avatar_urls: Some(common::models::AvatarUrls {
                s: Some("https://wf/avatar/s/10.jpg".into()),
                ..Default::default()
            }),
            ..Default::default()
        });
        s.images = crate::images::Policy {
            tier: crate::images::Tier::Kitty,
            font: (10, 20),
        };
        s.rebuild_lines(&theme, &UNICODE);
        assert!(
            !s.image_slots.iter().any(|slot| slot.key.contains("avatar")),
            "an avatar must not overwrite the first line of a post body"
        );
    }

    #[test]
    fn thread_view_numbers_posts_across_pages() {
        let mut s = thread_view_fixture();
        // Last page of 28 posts, 2 shown -> #27 and #28.
        s.page = 2;
        assert_eq!(s.post_number(0), 27);
        assert_eq!(s.post_number(1), 28);
        assert_eq!(s.post_total(), 28);
        // First page counts forwards from this page's length.
        s.page = 1;
        assert_eq!(s.post_number(0), 1);
        assert_eq!(s.post_number(1), 2);
        // Without a pagination total, the reply count stands in.
        s.total = 0;
        assert_eq!(s.post_total(), 28);
    }

    #[test]
    fn thread_view_wraps_long_bodies_behind_the_gutter() {
        let theme = Theme::truecolor();
        let mut s = thread_view_fixture();
        s.posts[0].message = "word ".repeat(80);
        s.width = 60;
        s.rebuild_lines(&theme, &UNICODE);
        let body: Vec<&Line<'static>> = s
            .lines
            .iter()
            .filter(|l| text(l).starts_with(" \u{2503} word"))
            .collect();
        assert!(body.len() > 5, "the body should wrap over several lines");
        for l in body {
            assert!(l.width() <= 60, "wrapped line overflows: {}", text(l));
            assert_eq!(l.spans[1].content.as_ref(), "\u{2503}");
        }
    }

    #[test]
    fn wrap_spans_hard_splits_a_token_longer_than_the_line() {
        let long = "x".repeat(25);
        let spans = vec![Span::raw(long)];
        let out = wrap_spans(&spans, 10);
        assert_eq!(out.len(), 3);
        for l in &out {
            let w: usize = l.iter().map(Span::width).sum();
            assert!(w <= 10);
        }
    }

    /// The wrap budget is terminal cells, not `char`s. A CJK run billed one
    /// cell per char builds lines twice the panel width, and the body is
    /// drawn with no `Wrap`, so the overflow is clipped away silently.
    #[test]
    fn wrap_spans_measures_double_width_text_in_cells() {
        // 25 ideographs = 50 cells, no break opportunity: the hard split.
        let cjk: String = "\u{5e73}".repeat(25);
        let out = wrap_spans(&[Span::raw(cjk)], 10);
        assert_eq!(out.len(), 5, "50 cells over a 10-cell line");
        for l in &out {
            let w: usize = l.iter().map(Span::width).sum();
            assert_eq!(w, 10, "a full line is exactly 10 cells");
        }

        // Word wrapping: five 4-cell words do not fit three to a 10-cell line.
        let words = "\u{6f22}\u{5b57} \u{6f22}\u{5b57} \u{6f22}\u{5b57} \u{6f22}\u{5b57} \u{6f22}\u{5b57}";
        let out = wrap_spans(&[Span::raw(words)], 10);
        for l in &out {
            let w: usize = l.iter().map(Span::width).sum();
            assert!(w <= 10, "wrapped line overflows: {w} cells");
        }
        assert!(out.len() >= 3, "{} lines", out.len());

        // An odd `room` must never split a double-width char across the edge.
        let out = wrap_spans(&[Span::raw("abc\u{5e73}\u{5e73}\u{5e73}\u{5e73}")], 8);
        for l in &out {
            let w: usize = l.iter().map(Span::width).sum();
            assert!(w <= 8, "wrapped line overflows: {w} cells");
        }

        // Degenerate: a panel narrower than one character still terminates,
        // and drops nothing.
        let out = wrap_spans(&[Span::raw("\u{5e73}\u{5e73}")], 1);
        assert!(out.len() >= 2, "{} lines", out.len());
        let joined: String = out.iter().flatten().map(|sp| sp.content.as_ref()).collect();
        assert_eq!(joined, "\u{5e73}\u{5e73}", "no character is dropped");
    }

    // ---------- retained behaviour ----------

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
        let mut state = ThreadViewState {
            links: vec!["https://example.com/1".into(), "https://example.com/2".into()],
            sel_local: 10,
            link_popup: false,
            ..Default::default()
        };

        let act = thread_view_key(&mut state, key('o'));
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

        // Enter on a Category steps into its first child forum, never opens it.
        let act = forum_tree_key(&mut state, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(act, Action::None));
        assert_eq!(state.sel, 1);

        let act = forum_tree_key(&mut state, KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        assert!(matches!(act, Action::None));
        assert_eq!(state.sel, 0);

        let act = forum_tree_key(&mut state, KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        assert!(matches!(act, Action::None));
        assert_eq!(state.sel, 1);

        let act = forum_tree_key(&mut state, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(act, Action::OpenThreadList(id, _) if id == 302));

        state.sel = 2;
        let act = forum_tree_key(&mut state, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(act, Action::OpenUrl(url) if url == "https://example.com/docs"));

        state.sel = 0;
        let act = forum_tree_key(&mut state, key('N'));
        assert!(matches!(act, Action::StartNewThread(id) if id == 302));

        let act = open_node_action(&nodes, 301, "Windows Forums");
        assert!(matches!(act, Action::OpenThreadList(id, _) if id == 302));

        let act = forum_tree_key(&mut state, key('L'));
        assert!(matches!(act, Action::OpenLatestThreads));
    }

    #[test]
    fn thread_view_shortcuts_and_type_rendering() {
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
                ..Default::default()
            },
        ];

        let theme = Theme::dark();
        let mut state = ThreadViewState {
            thread,
            posts,
            width: 100,
            ..Default::default()
        };
        state.rebuild_lines(&theme, &UNICODE);

        let rendered: String = state.lines.iter().map(text).collect();
        assert!(
            rendered.contains("marked solution"),
            "expected the solution callout"
        );
        assert!(rendered.contains("Bob_Guru"));

        let act = thread_view_key(&mut state, key('p'));
        assert!(matches!(act, Action::OpenProfile(uid, name) if uid == 10 && name == "Alice"));

        let act = thread_view_key(&mut state, key('n'));
        assert!(matches!(act, Action::None));
        assert_eq!(state.sel_post, 1);

        let act = thread_view_key(&mut state, key('p'));
        assert!(matches!(act, Action::OpenProfile(uid, name) if uid == 20 && name == "Bob_Guru"));

        let act = thread_view_key(
            &mut state,
            KeyEvent::new(KeyCode::Char('P'), KeyModifiers::SHIFT),
        );
        assert!(matches!(act, Action::OpenProfile(uid, name) if uid == 10 && name == "Alice"));

        let act = thread_view_key(&mut state, key('l'));
        assert!(matches!(act, Action::ReactPost(pid) if pid == 202));

        let act = thread_view_key(&mut state, key('v'));
        assert!(matches!(act, Action::VotePost(pid, vt) if pid == 202 && vt == "up"));

        let act = thread_view_key(
            &mut state,
            KeyEvent::new(KeyCode::Char('V'), KeyModifiers::SHIFT),
        );
        assert!(matches!(act, Action::VotePost(pid, vt) if pid == 202 && vt == "down"));

        let act = thread_view_key(
            &mut state,
            KeyEvent::new(KeyCode::Char('N'), KeyModifiers::SHIFT),
        );
        assert!(matches!(act, Action::None));
        assert_eq!(state.sel_post, 0);

        // Advertised-but-inert keys must stay inert, not fall through to a verb.
        assert!(matches!(thread_view_key(&mut state, key('w')), Action::None));
        assert!(matches!(thread_view_key(&mut state, key('1')), Action::None));
    }
}

