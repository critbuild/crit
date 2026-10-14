use crate::{
    app::{
        App, COMMANDS, Confirm, IntentDialog, LayoutMode, Modal, Pane, Project, ProjectPicker,
        Scrollbar, SplitDivider, TabHit, Workspace, WorktreeHit, WorktreeHitAction, project_slots,
    },
    integration::AgentState,
    worktree::SplitTarget,
};
use anyhow::Result;
use ratatui::{
    Frame,
    buffer::Buffer,
    layout::{Rect, Size},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
};
use std::ops::Range;
use std::sync::atomic::Ordering;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

const FG: Color = Color::Rgb(224, 228, 232);
const BG: Color = Color::Reset;
const MUTED: Color = Color::Rgb(154, 165, 180);
const BORDER: Color = Color::Rgb(51, 62, 77);
const HEADER_DIVIDER: Color = Color::Rgb(112, 114, 116);
const SURFACE: Color = Color::Rgb(32, 44, 56);
const ACCENT: Color = Color::Rgb(144, 191, 255);
const GREEN: Color = Color::Rgb(165, 214, 179);
const AMBER: Color = Color::Rgb(232, 188, 128);

fn areas(app: &App, size: Size) -> ([Rect; 2], usize) {
    let mut content = size;
    content.height = content
        .height
        .saturating_sub(notice_height(app.notice.as_deref(), size));
    // The worktree pane takes columns from the window and the content alike, so a split
    // that no longer fits beside it collapses; the notice rows keep the full width.
    let pane = worktree_pane_width(app, size.width);
    content.width -= pane;
    pane_areas(
        app.project.layout,
        app.project.secondary.is_some(),
        app.project.split_ratio,
        Size::new(size.width - pane, size.height),
        content,
    )
}

/// Narrowest window that draws the worktree pane; it leaves the terminal at least 34 columns.
const WORKTREE_PANE_MIN_WINDOW: u16 = 60;
/// Columns of a worktree pane cell (🗑 or ×): the glyph and a space on each side.
const WORKTREE_CELL: u16 = 3;

/// Columns the worktree pane takes from a window `width` wide: none while it is closed or the
/// window is too narrow, where the terminal keeps the full width and the flag stays set.
fn worktree_pane_width(app: &App, width: u16) -> u16 {
    if app.worktree_pane.is_some() && width >= WORKTREE_PANE_MIN_WINDOW {
        (width / 3).clamp(26, 40)
    } else {
        0
    }
}

fn pane_areas(
    layout: LayoutMode,
    split: bool,
    split_ratio: u16,
    window: Size,
    size: Size,
) -> ([Rect; 2], usize) {
    // Only visible notices reserve bottom rows; areas() subtracts their height.
    let main = Rect::new(0, 1, size.width, size.height.saturating_sub(1));
    // The window decides whether both panes fit; a notice must not collapse a split.
    if !split || layout == LayoutMode::Full || window.width < 60 || window.height < 16 {
        return ([main, Rect::default()], 1);
    }
    match layout {
        LayoutMode::Vertical => {
            let width = SplitDivider::left_width(main.width, split_ratio);
            (
                [
                    Rect::new(main.x, main.y, width, main.height),
                    Rect::new(main.x + width, main.y, main.width - width, main.height),
                ],
                2,
            )
        }
        LayoutMode::Horizontal => {
            let height = main.height.saturating_sub(1) / 2;
            (
                [
                    Rect::new(main.x, main.y, main.width, height),
                    Rect::new(
                        main.x,
                        main.y + height + 1,
                        main.width,
                        main.height - height - 1,
                    ),
                ],
                2,
            )
        }
        LayoutMode::Full => ([main, Rect::default()], 1),
    }
}
/// Columns kept clear on each side of terminal content and the notice row.
/// Crit zeroes Kitty's own padding in its window, so this is the only gap
/// between text and the window edge; the tab bar still runs edge to edge.
const MARGIN: u16 = 1;
// One workspace header row, followed by a thin horizontal divider.
const HEADER_ROWS: u16 = 2;
fn body(area: Rect) -> Rect {
    Rect::new(
        area.x + MARGIN,
        area.y + area.height.min(HEADER_ROWS),
        area.width.saturating_sub(2 * MARGIN),
        area.height.saturating_sub(HEADER_ROWS),
    )
}

fn split_divider(layout: LayoutMode, panes: [Rect; 2], count: usize) -> Option<SplitDivider> {
    if layout != LayoutMode::Vertical || count != 2 {
        return None;
    }
    let content = body(panes[0]);
    if content.height == 0 {
        return None;
    }
    let track = Rect::new(panes[0].right(), content.y, 1, content.height);
    Some(SplitDivider {
        track,
        // Reuse adjacent pane margins instead of reserving a middle gutter.
        handle: Rect::new(track.x - MARGIN, track.y + track.height / 2, 2 * MARGIN, 1),
        available_width: panes[0].width + panes[1].width,
    })
}

fn draw_split_handle(buffer: &mut Buffer, divider: SplitDivider, dragging: bool) {
    let color = if dragging { ACCENT } else { HEADER_DIVIDER };
    buffer[(divider.handle.x, divider.handle.y)]
        .set_symbol("┤")
        .set_fg(color);
    buffer[(divider.handle.right() - 1, divider.handle.y)]
        .set_symbol("├")
        .set_fg(color);
}

fn pane_target(project: &Project, pane: usize, count: usize) -> (usize, usize, bool) {
    let secondary = if count == 1 {
        project.layout != LayoutMode::Full && project.secondary.is_some() && project.focus_secondary
    } else {
        pane == 1
    };
    let workspace = if secondary {
        project.secondary.unwrap_or(project.active)
    } else {
        project.active
    };
    (workspace, project.pane_session(secondary), secondary)
}

/// Sizes the sessions the next frame draws and lists them in `app.next_panes`, so that the frame
/// can be held for them before it has drawn and filled `app.panes`.
pub fn resize_sessions(app: &mut App, size: Size) -> Result<()> {
    app.next_panes.clear();
    // Nothing is drawn below the minimum; keep the last usable PTY size.
    if size.width < 30 || size.height < 10 {
        return Ok(());
    }
    let (areas, count) = areas(app, size);
    for (pane, area) in areas.iter().enumerate().take(count) {
        let (index, selected, _) = pane_target(&app.project, pane, count);
        let rect = body(*area);
        let workspace = &mut app.project.workspaces[index];
        if let Some(session) = workspace.sessions.get_mut(selected) {
            let keep = match &mut app.selection {
                Some(selection) if selection.session == session.id && selection.body != rect => {
                    // A notice only moves the bottom edge; the frozen rows keep their places.
                    let same = (selection.body.x, selection.body.y, selection.body.width)
                        == (rect.x, rect.y, rect.width);
                    if same {
                        selection.body = rect;
                    }
                    same
                }
                _ => true,
            };
            if !keep {
                app.selection = None;
            }
            session.resize(rect.height, rect.width)?;
            app.next_panes.push((index, selected));
        }
    }
    Ok(())
}

pub fn draw(frame: &mut Frame, app: &mut App) {
    app.split_divider = None;
    let bounds = frame.area();
    app.interaction_visible = bounds.width >= 30 && bounds.height >= 10;
    frame
        .buffer_mut()
        .set_style(bounds, Style::default().fg(FG).bg(BG));
    app.tab_hits.clear();
    app.worktree_hits.clear();
    app.worktree_area = None;
    app.panes.clear();
    if bounds.width < 30 || bounds.height < 10 {
        app.finish_split_drag();
        frame.render_widget(
            Paragraph::new("crit — enlarge terminal to at least 30×10. F12 toggles Kitty keys.")
                .wrap(Wrap { trim: false }),
            bounds,
        );
        return;
    }
    let width = draw_key_mode(frame, app.native_keys, Some(app.limiter.hint()));
    draw_tabs(frame, app, bounds.width.saturating_sub(width));
    let (regions, count) = areas(app, Size::new(bounds.width, bounds.height));
    let pane_width = worktree_pane_width(app, bounds.width);
    let content_width = bounds.width - pane_width;
    app.split_divider = split_divider(app.project.layout, regions, count);
    if app.split_divider.is_none() {
        app.finish_split_drag();
    }
    if count == 2 && app.project.layout == LayoutMode::Horizontal {
        line_horizontal(
            frame.buffer_mut(),
            0,
            regions[0].bottom(),
            content_width,
            BORDER,
        );
    }
    for (pane, area) in regions.iter().copied().enumerate().take(count) {
        let (index, selected, secondary) = pane_target(&app.project, pane, count);
        let focused = count == 1 || secondary == app.project.focus_secondary;
        let pane_body = body(area);
        let scrollbar = draw_workspace(
            frame,
            &app.project.workspaces[index],
            selected,
            area,
            pane_body,
            focused,
            app.modal.is_none(),
            app.selection.as_ref(),
        );
        app.panes.push(Pane {
            workspace: index,
            session: selected,
            secondary,
            body: pane_body,
            scrollbar,
        });
        if area.height >= HEADER_ROWS && (pane == 0 || app.project.layout == LayoutMode::Horizontal)
        {
            // Span both side-by-side panes, including their shared separator.
            line_horizontal(
                frame.buffer_mut(),
                bounds.x,
                area.y + 1,
                content_width,
                HEADER_DIVIDER,
            );
        }
    }
    if let Some(divider) = app.split_divider {
        // The right header's leading space belongs to the shared pane boundary.
        frame.buffer_mut()[(regions[1].x, regions[1].y)]
            .set_symbol("│")
            .set_fg(BORDER);
        for y in divider.track.y..divider.track.bottom() {
            // Keep the left pane's scroll thumb and track when history exists.
            if app.panes[0].scrollbar.is_none() {
                frame.buffer_mut()[(divider.handle.x, y)]
                    .set_symbol("│")
                    .set_fg(HEADER_DIVIDER);
            }
            frame.buffer_mut()[(divider.handle.right() - 1, y)]
                .set_symbol("│")
                .set_fg(HEADER_DIVIDER);
        }
        draw_split_handle(frame.buffer_mut(), divider, app.split_drag.is_some());
    }
    if pane_width > 0 {
        draw_worktree_pane(frame, app, pane_width, regions[count - 1].bottom());
    }
    if let Some(notice) = &app.notice {
        let height = notice_height(Some(notice), Size::new(bounds.width, bounds.height));
        draw_notice(
            frame,
            notice,
            Rect::new(
                MARGIN,
                bounds.height - height,
                bounds.width.saturating_sub(2 * MARGIN),
                height,
            ),
        );
    }
    if let Some(modal) = &app.modal {
        draw_modal(frame, app, modal);
    }
}

/// Every open project contributes a name cell and one cell per worktree, in
/// opening order, so other projects stay visible and clickable. Cells are
/// staged in `tab_hits` (capacity reused across frames) and trimmed to the
/// visible window around the focused tab.
fn draw_tabs(frame: &mut Frame, app: &mut App, width: u16) {
    let focused = app.focused_index();
    let App {
        project,
        saved_projects,
        tab_hits,
        ..
    } = app;
    let mut focused_cell = 0;
    for (slot, open) in project_slots(project, saved_projects) {
        let separator = u16::from(!tab_hits.is_empty());
        tab_hits.push(TabHit {
            area: Rect::new(0, 0, (open.name.width() as u16 + 2).min(22) + separator, 1),
            project: slot,
            workspace: None,
        });
        for (index, workspace) in open.workspaces.iter().enumerate() {
            if slot.is_none() && index == focused {
                focused_cell = tab_hits.len();
            }
            tab_hits.push(TabHit {
                area: Rect::new(0, 0, (workspace.label().width() as u16 + 8).min(34), 1),
                project: slot,
                workspace: Some(index),
            });
        }
    }
    let mut start = focused_cell;
    let mut used = tab_hits[start].area.width;
    while start > 0 && used + tab_hits[start - 1].area.width < width.saturating_sub(4) {
        start -= 1;
        used += tab_hits[start].area.width;
    }
    let mut x = 0;
    if start > 0 {
        frame.render_widget(
            Paragraph::new("‹").style(Style::default().fg(MUTED)),
            Rect::new(0, 0, 1, 1),
        );
        x = 2;
    }
    let mut end = start;
    for (cell, hit) in tab_hits.iter_mut().enumerate().skip(start) {
        let cell_width = hit.area.width.min(width.saturating_sub(x));
        // Keep a cramped first cell (the focused tab at minimum width) clickable.
        if cell > start && cell_width < hit.area.width.min(5) {
            break;
        }
        hit.area = Rect::new(x, 0, cell_width, 1);
        let current = hit.project.is_none();
        let open = hit.project.map_or(&*project, |slot| &saved_projects[slot]);
        let (line, style) = match hit.workspace {
            None => (
                Line::from(vec![
                    Span::styled(if cell > 0 { "│" } else { "" }, Style::default().fg(BORDER)),
                    Span::styled(
                        format!(" {} ", open.name),
                        Style::default().fg(if current { FG } else { MUTED }).bold(),
                    ),
                ]),
                Style::default().bg(BG),
            ),
            Some(index) => {
                let workspace = &open.workspaces[index];
                let (mark, color) = state_style(workspace.state());
                let selected = current && index == focused;
                (
                    Line::from(vec![
                        Span::raw(format!(" {} ", index + 1)),
                        Span::raw(workspace.label()),
                        Span::styled(format!(" {mark} "), Style::default().fg(color)),
                    ]),
                    Style::default()
                        .fg(if selected { ACCENT } else { MUTED })
                        .bg(BG),
                )
            }
        };
        frame.render_widget(Paragraph::new(line).style(style), hit.area);
        x += cell_width;
        end = cell + 1;
        if x >= width.saturating_sub(2) {
            break;
        }
    }
    tab_hits.truncate(end);
    tab_hits.drain(..start);
}

/// The longest prefix of `text`, in whole grapheme clusters, that fits `width` columns, and
/// whether text was cut. A cut prefix leaves its last column free for `…`.
fn clip_width(text: &str, width: usize) -> (&str, bool) {
    if text.width() <= width {
        return (text, false);
    }
    let mut used = 0;
    for (offset, grapheme) in text.grapheme_indices(true) {
        used += grapheme.width();
        if used >= width {
            return (&text[..offset], true);
        }
    }
    (text, false)
}

/// Draws `text` within `width` columns, ending in `…` where it was cut; returns the next column.
fn put_clipped(buffer: &mut Buffer, x: u16, y: u16, text: &str, width: u16, style: Style) -> u16 {
    let (shown, cut) = clip_width(text, usize::from(width));
    let (end, _) = buffer.set_stringn(x, y, shown, usize::from(width), style);
    if cut {
        let left = usize::from(width - (end - x));
        return buffer.set_stringn(end, y, "…", left, style).0;
    }
    end
}

/// Which of `total` worktree rows show in `capacity` lines when scrolled to `scroll`: the first
/// row, how many follow it, and whether `↑ n more` / `↓ n more` take a line at the top / bottom.
/// The offset is clamped so the last row can still be reached.
fn worktree_window(total: usize, capacity: usize, scroll: usize) -> (usize, usize, bool, bool) {
    if total <= capacity {
        return (0, total, false, false);
    }
    if capacity < 3 {
        // No room for an indicator line beside a row.
        return (scroll.min(total - capacity), capacity, false, false);
    }
    // Scrolled to the end, only the `↑` line shares the list with rows.
    let first = scroll.min(total - (capacity - 1));
    let above = first > 0;
    let room = capacity - usize::from(above);
    let below = first + room < total;
    (first, room - usize::from(below), above, below)
}

/// The F2 sidebar: `width` columns at the right edge from row 1 down to `bottom`. A header
/// names the project and holds the close cell; below it come one row per worktree of
/// `project.trees`, each ending in a 🗑 cell, or the reason a non-Git project has none. Click
/// cells are staged in `worktree_hits`, and the scroll offset is clamped to what fits.
fn draw_worktree_pane(frame: &mut Frame, app: &mut App, width: u16, bottom: u16) {
    let focused = app.focused_index();
    let App {
        project,
        worktree_pane,
        worktree_hits,
        worktree_area,
        ..
    } = app;
    let Some(pane) = worktree_pane else { return };
    let area = Rect::new(
        frame.area().width - width,
        1,
        width,
        bottom.saturating_sub(1),
    );
    *worktree_area = Some(area);
    let inner = Rect::new(area.x + 1, area.y, area.width - 1, area.height);
    let list = Rect::new(
        inner.x,
        inner.y + 1,
        inner.width,
        inner.height.saturating_sub(1),
    );
    let buffer = frame.buffer_mut();
    for y in area.y..area.bottom() {
        buffer[(area.x, y)].set_symbol("│").set_fg(BORDER);
    }
    if inner.is_empty() {
        return;
    }
    let muted = Style::default().fg(MUTED);
    let close = Rect::new(inner.right() - WORKTREE_CELL, inner.y, WORKTREE_CELL, 1);
    worktree_hits.push(WorktreeHit {
        area: close,
        action: WorktreeHitAction::Close,
    });
    let (title, _) = buffer.set_stringn(
        inner.x,
        inner.y,
        " Worktrees ",
        usize::from(close.x - inner.x),
        Style::default().fg(FG).bold(),
    );
    put_clipped(
        buffer,
        title,
        inner.y,
        &project.name,
        close.x - title,
        muted,
    );
    buffer.set_string(close.x, inner.y, " × ", muted);
    if project.repository.is_none() {
        frame.render_widget(
            Paragraph::new("Not a Git project.\nOpen a Git folder to manage worktrees.")
                .style(muted)
                .wrap(Wrap { trim: true }),
            Rect::new(list.x + 1, list.y, list.width - 1, list.height),
        );
        return;
    }
    let total = project.trees.len();
    let (first, count, above, below) =
        worktree_window(total, usize::from(list.height), pane.scroll);
    pane.scroll = first;
    let more = usize::from(list.width) - 1;
    let mut y = list.y;
    if above {
        buffer.set_stringn(list.x + 1, y, format!("↑ {first} more"), more, muted);
        y += 1;
    }
    for index in first..first + count {
        let tree = &project.trees[index];
        let open = project
            .workspaces
            .iter()
            .any(|workspace| workspace.tree.id == tree.id);
        // The label and markers share the room left of the trash cell, after a margin column.
        let text = Rect::new(list.x, y, list.width - WORKTREE_CELL, 1);
        let trash = Rect::new(text.right(), y, WORKTREE_CELL, 1);
        let markers = [
            (open, " ●", GREEN),
            (tree.is_main, " main", MUTED),
            (tree.locked, " locked", AMBER),
            (tree.prunable, " missing", AMBER),
        ];
        let markers_width: u16 = markers
            .iter()
            .filter(|marker| marker.0)
            .map(|marker| marker.1.width() as u16)
            .sum();
        let current = tree.id == project.workspaces[focused].tree.id;
        let mut x = put_clipped(
            buffer,
            text.x + 1,
            y,
            // A bare repository has neither branch nor commit, so `label` would say "terminal".
            if tree.bare {
                "bare repository"
            } else {
                tree.label()
            },
            (text.width - 1).saturating_sub(markers_width),
            Style::default().fg(if current { ACCENT } else { FG }),
        );
        for (_, marker, color) in markers.iter().filter(|marker| marker.0) {
            let style = Style::default().fg(*color);
            x = buffer
                .set_stringn(
                    x,
                    y,
                    marker,
                    usize::from(text.right().saturating_sub(x)),
                    style,
                )
                .0;
        }
        // Rows that cannot be deleted dim the cell; clicking it says why.
        let removable = !(tree.is_main || tree.locked || tree.bare);
        buffer.set_string(
            trash.x,
            y,
            " 🗑 ",
            Style::default().fg(if removable { MUTED } else { BORDER }),
        );
        worktree_hits.push(WorktreeHit {
            area: text,
            action: WorktreeHitAction::Open(index),
        });
        worktree_hits.push(WorktreeHit {
            area: trash,
            action: WorktreeHitAction::Remove(index),
        });
        y += 1;
    }
    if below {
        let hidden = total - first - count;
        buffer.set_stringn(list.x + 1, y, format!("↓ {hidden} more"), more, muted);
    }
}

// One pane's frame, header and body: each parameter is one thing the frame decided about it.
#[expect(clippy::too_many_arguments)]
fn draw_workspace(
    frame: &mut Frame,
    workspace: &Workspace,
    selected: usize,
    area: Rect,
    body: Rect,
    focused: bool,
    show_cursor: bool,
    selection: Option<&crate::selection::Selection>,
) -> Option<Scrollbar> {
    if area.height < 2 || area.width < 3 {
        return None;
    }
    let exited = workspace
        .sessions
        .get(selected)
        .filter(|session| session.state == AgentState::Exited);
    let mut header = Vec::new();
    let label = Span::styled(
        format!(" {}{} ", if focused { "› " } else { "" }, workspace.label()),
        Style::default()
            .fg(if focused { ACCENT } else { FG })
            .bold(),
    );
    for (index, session) in workspace.sessions.iter().enumerate() {
        let (_, color) = state_style(session.state);
        let active = index == selected;
        // An agent that has not drawn yet is starting, not in an unknown state.
        let name = if session.state == AgentState::Unknown && !session.has_output() {
            "starting"
        } else {
            state_name(session.state)
        };
        header.push(Span::styled(
            format!(
                " {}{} {} · {}{} ",
                if active { "[" } else { "" },
                session.kind,
                index + 1,
                name,
                if active { "]" } else { "" }
            ),
            Style::default().fg(if active { color } else { MUTED }),
        ));
    }
    let selected_width = header.get(selected).map_or(0, Span::width);
    let label_width = label
        .width()
        .min(usize::from(area.width).saturating_sub(selected_width)) as u16;
    let session_width = area.width - label_width;
    let mut start = selected.min(header.len());
    let mut visible_width = selected_width;
    while start > 0 && visible_width + header[start - 1].width() <= usize::from(session_width) {
        start -= 1;
        visible_width += header[start].width();
    }
    let scroll = header[..start]
        .iter()
        .map(Span::width)
        .sum::<usize>()
        .min(u16::MAX as usize) as u16;
    if let Some(session) = exited {
        header.push(Span::styled(
            format!(
                " Exited ({}) · F1 restart ",
                session.message.as_deref().unwrap_or("finished")
            ),
            Style::default().fg(AMBER),
        ));
    }
    frame.render_widget(
        Paragraph::new(Line::from(label)),
        Rect::new(area.x, area.y, label_width, 1),
    );
    frame.render_widget(
        Paragraph::new(Line::from(header)).scroll((0, scroll)),
        Rect::new(area.x + label_width, area.y, session_width, 1),
    );
    if let Some(session) = workspace.sessions.get(selected) {
        let mut parser = session.parser.lock();
        // Clear while holding the parser lock: output after this frame must wake us.
        session.dirty.store(false, Ordering::Release);
        let selection =
            selection.filter(|selection| selection.session == session.id && selection.body == body);
        let scrollbar = scrollbar(parser.screen_mut(), body);
        let screen = parser.screen();
        if !body.is_empty() {
            let buffer = frame.buffer_mut();
            let rows = body.y..body.bottom();
            match selection {
                Some(selection) => {
                    for ((cells, highlight), y) in selection.rows().zip(rows) {
                        copy_row(buffer, body, y, cells.iter(), highlight);
                    }
                }
                None => {
                    for (row, y) in screen.visible_rows().zip(rows) {
                        copy_row(buffer, body, y, row.cells(), 0..0);
                    }
                }
            }
        }
        // The copy loop blanked the body; until the program paints, say what it is waiting for.
        if !body.is_empty() && session.alive.load(Ordering::Acquire) && !session.has_output() {
            put_clipped(
                frame.buffer_mut(),
                body.x,
                body.y,
                &starting_line(&session.kind, session.resume.is_some()),
                body.width,
                Style::default().fg(MUTED),
            );
        }
        if focused
            && show_cursor
            && selection.is_none()
            && !screen.hide_cursor()
            && screen.scrollback() == 0
        {
            let (row, col) = screen.cursor_position();
            // Pending wrap parks the cursor at `cols`; show it on the last column.
            let col = col.min(screen.size().1.saturating_sub(1));
            if row < body.height && col < body.width {
                frame.set_cursor_position((body.x + col, body.y + row));
            }
        }
        if let Some(bar) = scrollbar {
            for y in bar.track.y..bar.track.bottom() {
                let thumb = y >= bar.thumb.y && y < bar.thumb.bottom();
                frame.buffer_mut()[(bar.track.x, y)]
                    .set_symbol(if thumb { "┃" } else { "│" })
                    .set_fg(if thumb { MUTED } else { BORDER });
            }
        }
        return scrollbar;
    } else {
        frame.render_widget(
            Paragraph::new("No session open. F1 → ask to start OMP, OpenCode, or a shell.")
                .style(Style::default().fg(MUTED))
                .wrap(Wrap { trim: false }),
            body,
        );
    }
    None
}

/// Copies one row of a pane's terminal cells into row `y` of `body`, as far as the body is
/// wide, giving the `highlight` columns the selection's colours.
///
/// The cells must still be as the frame starts them, blank and without modifiers: nothing
/// draws under a pane's body before this. So a blank is left alone, as setting a symbol is the
/// costliest step here and most cells of a screen are usually blank, and the colours and
/// modifiers are assigned outright rather than merged in from a `Style`.
fn copy_row<'a>(
    buffer: &mut Buffer,
    body: Rect,
    y: u16,
    cells: impl Iterator<Item = &'a vt100::Cell>,
    highlight: Range<u16>,
) {
    // One bounds check for the row instead of one per cell.
    let start = buffer.index_of(body.x, y);
    let end = buffer.index_of(body.right() - 1, y) + 1;
    let targets = &mut buffer.content[start..end];
    for ((col, cell), target) in (0..body.width).zip(cells).zip(targets) {
        if cell.is_wide_continuation() {
            target.set_diff_option(ratatui::buffer::CellDiffOption::Skip);
            continue;
        }
        let symbol = cell.contents();
        if symbol.len() > 3 && !cell.is_wide() && symbol.contains('\u{fe0f}') {
            set_symbol_without_vs16(target, symbol);
        } else if !symbol.is_empty() && symbol != " " {
            target.set_symbol(symbol);
        }
        let mut modifier = Modifier::empty();
        if cell.bold() {
            modifier |= Modifier::BOLD;
        }
        if cell.dim() {
            modifier |= Modifier::DIM;
        }
        if cell.italic() {
            modifier |= Modifier::ITALIC;
        }
        if cell.underline() || cell.hyperlink().is_some() {
            modifier |= Modifier::UNDERLINED;
        }
        if highlight.contains(&col) {
            target.fg = SURFACE;
            target.bg = ACCENT;
        } else {
            target.fg = vt_color(cell.fgcolor(), FG);
            target.bg = vt_color(cell.bgcolor(), BG);
            if cell.inverse() {
                modifier |= Modifier::REVERSED;
            }
        }
        target.modifier = modifier;
    }
}

/// vt100 gave this emoji presentation sequence one column; drop VS16 so the outer terminal
/// uses one column too. Out of `copy_row`, whose loop this rare case slowed down twice over.
#[cold]
fn set_symbol_without_vs16(target: &mut ratatui::buffer::Cell, symbol: &str) {
    // Cells hold at most 22 bytes.
    let mut text = [0_u8; 32];
    let mut len = 0;
    for ch in symbol.chars().filter(|&ch| ch != '\u{fe0f}') {
        len += ch.encode_utf8(&mut text[len..]).len();
    }
    target.set_symbol(std::str::from_utf8(&text[..len]).unwrap_or(" "));
}

fn scrollbar(screen: &mut vt100::Screen, body: Rect) -> Option<Scrollbar> {
    if body.is_empty() || screen.alternate_screen() {
        return None;
    }
    let offset = screen.scrollback();
    screen.set_scrollback(usize::MAX);
    let max_offset = screen.scrollback();
    screen.set_scrollback(offset);
    if max_offset == 0 {
        return None;
    }
    let visible = usize::from(body.height);
    let height = (visible * visible / (visible + max_offset)).max(1) as u16;
    let travel = body.height - height;
    let top = ((max_offset - offset) * usize::from(travel) / max_offset) as u16;
    Some(Scrollbar {
        track: Rect::new(body.right(), body.y, 1, body.height),
        thumb: Rect::new(body.right(), body.y + top, 1, height),
        max_offset,
    })
}

fn state_style(state: AgentState) -> (&'static str, Color) {
    match state {
        AgentState::Working => ("*", GREEN),
        AgentState::Blocked => ("!", AMBER),
        AgentState::Idle => ("·", MUTED),
        AgentState::Unknown => ("?", MUTED),
        AgentState::Exited => ("x", MUTED),
    }
}
fn state_name(state: AgentState) -> &'static str {
    match state {
        AgentState::Working => "working",
        AgentState::Blocked => "input",
        AgentState::Idle => "idle",
        AgentState::Unknown => "unknown",
        AgentState::Exited => "exited",
    }
}

/// What a pane shows until its program paints: the program it started, and for an agent
/// reopening a saved chat, that the chat has to load before anything is drawn.
fn starting_line(kind: &str, saved_chat: bool) -> String {
    match (kind, saved_chat) {
        ("omp", true) => "Starting omp — loading its saved chat…".into(),
        ("opencode", true) => "Starting opencode — opening its saved chat…".into(),
        _ => format!("Starting {kind}…"),
    }
}
fn vt_color(color: vt100::Color, default: Color) -> Color {
    match color {
        vt100::Color::Default => default,
        vt100::Color::Idx(index) => Color::Indexed(index),
        vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}
fn line_horizontal(buffer: &mut Buffer, x: u16, y: u16, width: u16, color: Color) {
    for column in x..x + width {
        buffer[(column, y)].set_symbol("─").set_fg(color);
    }
}

/// Draws the key hint at the right end of the top row and returns its width. `cpu` is the agent
/// CPU limit's state for F3 once a project is open, and `None` on the start screen, which only
/// takes F12. The full labels take at most half the row, so the tabs keep the rest; narrower
/// windows get shorter forms that leave the tabs at least 15 columns, so a focused tab fits
/// even at the 30-column minimum.
fn draw_key_mode(frame: &mut Frame, native_keys: bool, cpu: Option<&str>) -> u16 {
    let width = usize::from(frame.area().width);
    let hint_width =
        |(start, end): (&str, &str), cpu: &str| start.width() + cpu.width() + end.width();
    let (start, state, end) = match cpu {
        _ if native_keys => (" F12  KITTY KEYS ", "", ""),
        None => (" F12  CRIT KEYS ", "", ""),
        Some(cpu) if 2 * hint_width(FULL_KEYS, cpu) <= width => (FULL_KEYS.0, cpu, FULL_KEYS.1),
        Some(cpu) if hint_width(SHORT_KEYS, cpu) + 15 <= width => (SHORT_KEYS.0, cpu, SHORT_KEYS.1),
        Some(_) => (" F1·F2·F3·F12 ", "", ""),
    };
    let used = (start.width() + state.width() + end.width()) as u16;
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::raw(start),
            Span::raw(state),
            Span::raw(end),
        ]))
        .style(
            Style::default()
                .fg(if native_keys { AMBER } else { ACCENT })
                .bg(BG),
        ),
        Rect::new(frame.area().width.saturating_sub(used), 0, used, 1),
    );
    used
}

/// The key hint around the agent CPU limit's state, with every key named.
const FULL_KEYS: (&str, &str) = (" F1 ASK · F2 WORKTREES · F3 CPU ", " · F12 CRIT KEYS ");

/// The key hint around the agent CPU limit's state, for windows too narrow for `FULL_KEYS`.
const SHORT_KEYS: (&str, &str) = (" F1 · F2 · F3 CPU ", " · F12 ");

pub fn draw_project_chooser(
    frame: &mut Frame,
    picker: &ProjectPicker,
    notice: &str,
    native_keys: bool,
) {
    let bounds = frame.area();
    frame
        .buffer_mut()
        .set_style(bounds, Style::default().fg(FG).bg(BG));
    if bounds.width < 30 || bounds.height < 10 {
        frame.render_widget(
            Paragraph::new("crit — enlarge terminal to at least 30×10. Esc quits.")
                .wrap(Wrap { trim: false }),
            bounds,
        );
        return;
    }
    let mode_width = draw_key_mode(frame, native_keys, None);
    frame.render_widget(
        Paragraph::new(" crit · start").style(Style::default().fg(FG).bold()),
        Rect::new(0, 0, bounds.width.saturating_sub(mode_width), 1),
    );
    let height = notice_height(Some(notice), Size::new(bounds.width, bounds.height));
    let available = Rect::new(bounds.x, bounds.y, bounds.width, bounds.height - height);
    let inner = modal_area_in(
        frame,
        available,
        " new terminal / open project ",
        Size::new(76, 22),
    );
    project_folder(frame, inner, picker);
    draw_notice(
        frame,
        notice,
        Rect::new(
            MARGIN,
            bounds.height - height,
            bounds.width.saturating_sub(2 * MARGIN),
            height,
        ),
    );
}

fn project_folder(frame: &mut Frame, inner: Rect, picker: &ProjectPicker) {
    if inner.height < 4 || inner.width < 3 {
        frame.render_widget(
            Paragraph::new("Enlarge the terminal to choose a project."),
            inner,
        );
        return;
    }
    let compact = inner.height < 8;
    if !compact {
        frame.render_widget(
            Paragraph::new(if picker.terminal_path.is_some() {
                "New terminal, or filter projects / enter any folder:"
            } else {
                "Type to filter projects, or enter any folder path:"
            }),
            Rect::new(inner.x, inner.y, inner.width, 1),
        );
    }
    input_line(
        frame,
        Rect::new(
            inner.x,
            inner.y + if compact { 0 } else { 2 },
            inner.width,
            1,
        ),
        &picker.input,
        !picker.pending,
    );
    let status = if picker.pending {
        "Opening project..."
    } else if picker.searching {
        "Finding projects near this folder and under home..."
    } else if picker.shows_terminal() {
        "New terminal or detected Git projects"
    } else if picker.projects.is_empty() {
        "No nearby projects found. Enter a folder above."
    } else if picker.visible_projects().len() == 0 {
        "No matching projects. Edit the search or enter a folder path."
    } else {
        "Detected projects"
    };
    frame.render_widget(
        Paragraph::new(status).style(Style::default().fg(MUTED)),
        Rect::new(
            inner.x,
            inner.y + if compact { 1 } else { 4 },
            inner.width,
            1,
        ),
    );
    choice_list(
        frame,
        Rect::new(
            inner.x,
            inner.y + if compact { 2 } else { 5 },
            inner.width,
            inner.height - if compact { 3 } else { 8 },
        ),
        picker
            .terminal_path
            .as_ref()
            .filter(|_| picker.shows_terminal())
            .map(|path| format!("New terminal  {}", path.display()))
            .into_iter()
            .chain(picker.visible_projects().map(|path| {
                format!(
                    "{}  {}",
                    path.file_name().unwrap_or_default().to_string_lossy(),
                    path.display(),
                )
            })),
        picker.selected,
    );
    frame.render_widget(
        Paragraph::new(if compact {
            "↑↓ choose · Enter · Esc"
        } else {
            "↑/↓ select · Enter open · Esc cancel · Ctrl+U clear"
        })
        .style(Style::default().fg(MUTED)),
        Rect::new(
            inner.x,
            inner.bottom() - if compact { 1 } else { 2 },
            inner.width,
            1,
        ),
    );
    if !compact {
        frame.render_widget(
            Paragraph::new(format!(
                "Paths: absolute, ~/ or relative to {}",
                picker.base.display()
            ))
            .style(Style::default().fg(MUTED)),
            Rect::new(inner.x, inner.bottom() - 1, inner.width, 1),
        );
    }
}

fn intent_dialog(frame: &mut Frame, inner: Rect, dialog: &IntentDialog) {
    if inner.height < 14 || inner.width < 3 {
        frame.render_widget(
            Paragraph::new("Enlarge the terminal to ask Crit. F2: manual controls. Esc: close.")
                .wrap(Wrap { trim: false }),
            inner,
        );
        return;
    }
    frame.render_widget(
        Paragraph::new("Describe what you want. Crit handles the workspace."),
        Rect::new(inner.x, inner.y, inner.width, 2),
    );
    frame.render_widget(
        Paragraph::new("Haiku 5.5 · current project and focused workspace")
            .style(Style::default().fg(MUTED)),
        Rect::new(inner.x, inner.y + 2, inner.width, 2),
    );
    let prompt = dialog.question().unwrap_or(
        "Start a branch for login fixes in a separate worktree.\nOpen the API project with OpenCode.\nShow this workspace's logs.",
    );
    frame.render_widget(
        Paragraph::new(prompt)
            .style(Style::default().fg(if dialog.question().is_some() {
                ACCENT
            } else {
                MUTED
            }))
            .wrap(Wrap { trim: false }),
        Rect::new(inner.x, inner.y + 4, inner.width, 4),
    );
    input_line(
        frame,
        Rect::new(inner.x, inner.y + 9, inner.width, 1),
        &dialog.input,
        !dialog.pending,
    );
    let status_area = Rect::new(inner.x, inner.y + 11, inner.width, inner.height - 13);
    if let Some(error) = &dialog.error {
        draw_notice(frame, error, status_area);
    } else {
        let status = if dialog.loading {
            "Finding projects and worktrees..."
        } else if dialog.pending {
            "Interpreting with Haiku 5.5... Esc cancels."
        } else {
            "Enter to act · destructive actions still require confirmation."
        };
        frame.render_widget(
            Paragraph::new(status)
                .style(Style::default().fg(MUTED))
                .wrap(Wrap { trim: false }),
            status_area,
        );
    }
    frame.render_widget(
        Paragraph::new("Enter send · Esc cancel · Ctrl+U clear · F2 manual controls")
            .style(Style::default().fg(MUTED)),
        Rect::new(inner.x, inner.bottom() - 1, inner.width, 1),
    );
}

fn modal_area_in(frame: &mut Frame, bounds: Rect, title: &'static str, desired: Size) -> Rect {
    let width = bounds.width.saturating_sub(4).min(desired.width);
    let height = bounds.height.saturating_sub(2).min(desired.height);
    let area = Rect::new(
        bounds.x + (bounds.width - width) / 2,
        bounds.y + (bounds.height - height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .style(Style::default().fg(FG).bg(SURFACE))
        .border_style(Style::default().fg(ACCENT));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    inner
}

fn draw_split_picker(frame: &mut Frame, bounds: Rect, selected: usize) {
    let existing = if bounds.width < 32 {
        "Existing branch/tree"
    } else {
        "Existing branch/worktree"
    };
    let choices = ["New worktree", "Split terminal", existing];
    let controls = "↑/↓ select · Enter · Esc";
    let width = choices
        .iter()
        .map(|choice| choice.width() + 2)
        .max()
        .unwrap()
        .max(controls.width());
    let inner = modal_area_in(
        frame,
        bounds,
        " split ",
        Size::new(width as u16 + 2, choices.len() as u16 + 3),
    );
    choice_list(
        frame,
        Rect::new(
            inner.x,
            inner.y,
            inner.width,
            inner.height.saturating_sub(1),
        ),
        choices,
        selected,
    );
    frame.render_widget(
        Paragraph::new(controls).style(Style::default().fg(MUTED)),
        Rect::new(inner.x, inner.bottom().saturating_sub(1), inner.width, 1),
    );
}

fn draw_existing_split_picker(
    frame: &mut Frame,
    bounds: Rect,
    choices: &[SplitTarget],
    selected: usize,
    error: Option<&str>,
) {
    let mut error_rows = 0_u16;
    if let Some(error) = error {
        notice_chunks(error, bounds.width.saturating_sub(6).min(74), |_| {
            error_rows = error_rows.saturating_add(1);
        });
        error_rows = error_rows.clamp(1, 3);
    }
    let rows = if choices.is_empty() {
        2
    } else {
        choices.len().min(17) as u16
    };
    let inner = modal_area_in(
        frame,
        bounds,
        " existing branch / worktree ",
        Size::new(76, (rows + error_rows + 3).min(20)),
    );
    let error_rows = error_rows.min(inner.height.saturating_sub(2));
    let list = Rect::new(
        inner.x,
        inner.y,
        inner.width,
        inner.height.saturating_sub(error_rows + 1),
    );
    if choices.is_empty() {
        frame.render_widget(
            Paragraph::new("No other branches or\nworktrees to open.")
                .style(Style::default().fg(MUTED)),
            list,
        );
    } else {
        choice_list(
            frame,
            list,
            choices.iter().map(|choice| match choice {
                SplitTarget::Worktree(tree) => {
                    format!("{}  {}", tree.label(), tree.path.display())
                }
                SplitTarget::Branch(branch) => format!("{branch}  (create worktree)"),
            }),
            selected,
        );
    }
    if let Some(error) = error {
        draw_notice(
            frame,
            error,
            Rect::new(inner.x, list.bottom(), inner.width, error_rows),
        );
    }
    frame.render_widget(
        Paragraph::new("↑↓ choose · Enter · Esc").style(Style::default().fg(MUTED)),
        Rect::new(inner.x, inner.bottom().saturating_sub(1), inner.width, 1),
    );
}

fn draw_opening_split(frame: &mut Frame, bounds: Rect) {
    let message = "Opening split… Esc hides";
    let inner = modal_area_in(
        frame,
        bounds,
        " opening ",
        Size::new(message.width() as u16 + 2, 3),
    );
    frame.render_widget(Paragraph::new(message), inner);
}

fn draw_modal(frame: &mut Frame, app: &App, modal: &Modal) {
    let mut available = frame.area();
    if app.notice.is_some() {
        available.height = available.height.saturating_sub(notice_height(
            app.notice.as_deref(),
            Size::new(available.width, available.height),
        ));
    }
    let (title, desired_height) = match modal {
        Modal::Intent(_) => (" ask Crit ", 22),
        Modal::Commands { .. } => (" manual controls ", 24),
        Modal::NewTab { choices, .. } => (" new tab ", choices.len() as u16 + 4),
        Modal::OpenProject(_) => (" open project ", 22),
        Modal::Loading { .. } => (" opening ", 7),
        Modal::SwitchProject { .. } => (
            " switch project ",
            (app.saved_projects.len() as u16 + 4).min(20),
        ),
        Modal::NewWorktree { split: None, .. } => (" new worktree tab ", 12),
        Modal::NewWorktree { .. } => (" new worktree in split ", 12),
        Modal::NewSession { .. } => (" new session ", 8),
        Modal::OpenWorktree { trees, .. } => {
            (" open existing worktree ", (trees.len() as u16 + 4).min(20))
        }
        Modal::Split { selected, .. } => return draw_split_picker(frame, available, *selected),
        Modal::ExistingSplit {
            choices,
            selected,
            error,
            ..
        } => {
            return draw_existing_split_picker(
                frame,
                available,
                choices,
                *selected,
                error.as_deref(),
            );
        }
        Modal::OpeningSplit { .. } => return draw_opening_split(frame, available),
        Modal::Preview { urls, .. } => (" open endpoint ", (urls.len() as u16 + 4).min(20)),
        Modal::Confirm(_) => (" confirm ", 9),
        Modal::Removal { error, .. } => (
            if error.is_some() {
                " deletion blocked "
            } else {
                " deleting worktree "
            },
            0,
        ),
    };
    if let Modal::Confirm(Confirm::CloseWorktree { selected }) = modal {
        let prompt = "End this tab's sessions?";
        let controls = "↑/↓ select · Enter · Esc";
        let choices = ["Yes", "Yes, delete worktree", "No"];
        let width = prompt.width().max(controls.width()).max(
            choices
                .iter()
                .map(|choice| choice.width() + 2)
                .max()
                .unwrap(),
        );
        let inner = modal_area_in(
            frame,
            available,
            " close tab ",
            Size::new(width as u16 + 2, choices.len() as u16 + 4),
        );
        // Keep all three choices and the controls visible at the minimum size.
        let show_prompt = inner.height >= choices.len() as u16 + 2;
        if show_prompt {
            frame.render_widget(
                Paragraph::new(prompt),
                Rect::new(inner.x, inner.y, inner.width, 1),
            );
        }
        let top = inner.y + u16::from(show_prompt);
        choice_list(
            frame,
            Rect::new(
                inner.x,
                top,
                inner.width,
                inner.bottom().saturating_sub(top + 1),
            ),
            choices,
            *selected,
        );
        frame.render_widget(
            Paragraph::new(controls).style(Style::default().fg(MUTED)),
            Rect::new(inner.x, inner.bottom() - 1, inner.width, 1),
        );
        return;
    }
    // A checkout deleted from the worktree pane may have no tab to close or keep.
    let tabless = matches!(modal, Modal::Removal { tree, .. }
        if !app.project.workspaces.iter().any(|workspace| workspace.tree.id == *tree));
    let kept = if tabless {
        "Checkout kept."
    } else {
        "Tab and checkout kept."
    };
    let removal_message = match modal {
        Modal::Removal { error, .. } => Some(error.as_deref().unwrap_or(if tabless {
            "Checking and removing worktree…\nEsc hides this; removal continues."
        } else {
            "Checking and removing worktree…\nThe tab will close on success.\nEsc hides this; removal continues."
        })),
        _ => None,
    };
    let desired = if let Some(message) = removal_message {
        let failed = matches!(modal, Modal::Removal { error: Some(_), .. });
        let width = message
            .lines()
            .map(str::width)
            .max()
            .unwrap_or(0)
            .max(kept.width())
            .min(60) as u16;
        let mut rows = 0_u16;
        notice_chunks(
            message,
            width.min(available.width.saturating_sub(6)),
            |_| {
                rows = rows.saturating_add(1);
            },
        );
        Size::new(width + 2, rows.saturating_add(if failed { 4 } else { 2 }))
    } else {
        Size::new(76, desired_height)
    };
    let inner = modal_area_in(frame, available, title, desired);
    match modal {
        Modal::Intent(dialog) => intent_dialog(frame, inner, dialog),
        Modal::Removal { error, .. } => {
            let failed = error.is_some();
            let message_area = Rect::new(
                inner.x,
                inner.y,
                inner.width,
                inner.height.saturating_sub(if failed { 2 } else { 0 }),
            );
            draw_notice(frame, removal_message.unwrap(), message_area);
            if failed {
                frame.render_widget(
                    Paragraph::new(kept).style(Style::default().fg(AMBER)),
                    Rect::new(inner.x, inner.bottom() - 2, inner.width, 1),
                );
                frame.render_widget(
                    Paragraph::new("Enter back · Esc dismiss").style(Style::default().fg(MUTED)),
                    Rect::new(inner.x, inner.bottom() - 1, inner.width, 1),
                );
            }
        }
        Modal::Loading { message, .. } => {
            frame.render_widget(Paragraph::new(*message).wrap(Wrap { trim: false }), inner);
            frame.render_widget(
                Paragraph::new("Esc cancel").style(Style::default().fg(MUTED)),
                Rect::new(inner.x, inner.bottom().saturating_sub(1), inner.width, 1),
            );
        }
        Modal::Commands { selected } => {
            choice_list(
                frame,
                Rect::new(
                    inner.x,
                    inner.y,
                    inner.width,
                    inner.height.saturating_sub(2),
                ),
                COMMANDS.iter().map(|(label, _)| (*label).to_owned()),
                *selected,
            );
            frame.render_widget(
                Paragraph::new("↑/↓ choose · Enter run · F1 ask Crit · Esc close")
                    .style(Style::default().fg(MUTED)),
                Rect::new(inner.x, inner.bottom().saturating_sub(1), inner.width, 1),
            );
        }
        Modal::NewTab { choices, selected } => choice_list(
            frame,
            inner,
            choices.iter().map(|(label, _)| label.clone()),
            *selected,
        ),
        Modal::OpenProject(picker) => project_folder(frame, inner, picker),
        Modal::SwitchProject { selected } => choice_list(
            frame,
            inner,
            app.saved_projects.iter().map(|project| {
                format!(
                    "{}  {}  ({} sessions)",
                    project.name,
                    project.path().display(),
                    project
                        .workspaces
                        .iter()
                        .map(|w| w.sessions.len())
                        .sum::<usize>(),
                )
            }),
            *selected,
        ),
        Modal::NewWorktree { input, split } => {
            let outcome = match split {
                Some(LayoutMode::Vertical) => "opens it side by side with",
                Some(_) => "opens it below",
                None => "opens it in a new tab next to",
            };
            frame.render_widget(
                Paragraph::new("Branch name (existing or new):"),
                Rect::new(inner.x, inner.y, inner.width, 1),
            );
            if inner.height > 2 {
                input_line(
                    frame,
                    Rect::new(inner.x, inner.y + 2, inner.width, 1),
                    input,
                    true,
                );
            }
            if inner.height > 5 {
                frame.render_widget(
                    Paragraph::new(format!(
                        "Enter creates the checkout from {base}, starts its Compose stack, and {outcome} {base}.",
                        base = app.focused().label(),
                    )).wrap(Wrap { trim: false }),
                    Rect::new(inner.x, inner.y + 4, inner.width, inner.height - 5),
                );
            }
            frame.render_widget(
                Paragraph::new("Enter create · Esc cancel").style(Style::default().fg(MUTED)),
                Rect::new(inner.x, inner.bottom().saturating_sub(1), inner.width, 1),
            );
        }
        Modal::NewSession { selected } => choice_list(
            frame,
            inner,
            ["OMP", "OpenCode", "Shell"].into_iter().map(String::from),
            *selected,
        ),
        Modal::OpenWorktree { trees, selected } => choice_list(
            frame,
            inner,
            trees
                .iter()
                .map(|tree| format!("{}  {}", tree.label(), tree.path.display())),
            *selected,
        ),
        Modal::Split { .. } | Modal::ExistingSplit { .. } | Modal::OpeningSplit { .. } => {
            unreachable!("split dialogs are rendered above")
        }
        Modal::Preview { urls, selected } => choice_list(
            frame,
            inner,
            urls.iter().map(|(name, url)| format!("{name}  {url}")),
            *selected,
        ),
        Modal::Confirm(confirm) => {
            let named;
            let message = match confirm {
                Confirm::Quit => {
                    "Exit Crit; stop sessions?\nSaved chats reopen.\nDocker/checkouts stay."
                }
                Confirm::CloseWorktree { .. } => unreachable!("close tab is rendered above"),
                Confirm::CloseSession => {
                    "End the focused session?\nA tab left empty closes, like a Kitty window.\nOther sessions, checkouts and Docker remain."
                }
                Confirm::StopEnvironment => {
                    "Stop this worktree's Docker?\nOther worktrees unaffected.\nVolumes remain."
                }
                Confirm::RemoveWorktree { tree: None } => {
                    "Delete this checkout?\nIgnored files go with it.\nDirty/main/locked refused.\nStop Docker first.\nBranch remains."
                }
                Confirm::RemoveWorktree { tree: Some(tree) } => {
                    // The branch shares one line with the question, so the warnings keep their rows.
                    let room = usize::from(inner.width).saturating_sub("Delete worktree ?".width());
                    let (label, cut) = clip_width(tree.label(), room);
                    let ellipsis = if cut { "…" } else { "" };
                    named = format!(
                        "Delete worktree {label}{ellipsis}?\nIgnored files go with it.\nDirty/main/locked refused.\nStop Docker first.\nBranch remains."
                    );
                    named.as_str()
                }
            };
            draw_confirmation(frame, inner, message);
        }
    }
}

fn input_line(frame: &mut Frame, area: Rect, input: &str, show_cursor: bool) {
    if area.is_empty() {
        return;
    }
    let line = Line::from(vec![Span::raw("› "), Span::raw(input)]);
    let cursor = line.width().min(u16::MAX as usize) as u16;
    let minimum_scroll = cursor.saturating_sub(area.width - 1);
    let mut scroll = 0_u16;
    // Ratatui retains a whole wide grapheme when an offset cuts through it.
    // Align the viewport to its end so the cursor cannot cover the last glyph.
    for grapheme in line.styled_graphemes(Style::default()) {
        if scroll >= minimum_scroll {
            break;
        }
        scroll = scroll.saturating_add(grapheme.symbol.width() as u16);
    }
    frame.render_widget(Paragraph::new(line).scroll((0, scroll)), area);
    if show_cursor {
        frame.set_cursor_position((area.x + cursor - scroll, area.y));
    }
}

fn draw_confirmation(frame: &mut Frame, inner: Rect, message: &str) {
    if inner.is_empty() {
        return;
    }
    frame.render_widget(
        Paragraph::new(message).wrap(Wrap { trim: false }),
        Rect::new(
            inner.x,
            inner.y,
            inner.width,
            inner.height.saturating_sub(1),
        ),
    );
    frame.render_widget(
        Paragraph::new("y confirm · Esc/n cancel").style(Style::default().fg(AMBER)),
        Rect::new(inner.x, inner.bottom() - 1, inner.width, 1),
    );
}

// Character-wrap notices without dropping text or depending on paragraph's
// private word-wrap measurement. Slices borrow the existing message.
fn notice_chunks<'a>(text: &'a str, width: u16, mut emit: impl FnMut(&'a str)) {
    if width == 0 {
        return;
    }
    for line in text.trim_end_matches('\n').split('\n') {
        let mut start = 0;
        let mut columns = 0;
        let mut place = |offset: usize, next: u16| {
            if columns + next > width && offset > start {
                emit(&line[start..offset]);
                start = offset;
                columns = 0;
            }
            columns += next;
        };
        // Ratatui draws whole grapheme clusters and drops those holding control
        // characters, so measure the same units. ASCII lines skip the segmentation.
        if line.is_ascii() {
            for (offset, byte) in line.bytes().enumerate() {
                place(offset, u16::from(!byte.is_ascii_control()));
            }
        } else {
            for (offset, grapheme) in line.grapheme_indices(true) {
                let next = if grapheme.contains(char::is_control) {
                    0
                } else {
                    grapheme.width() as u16
                };
                place(offset, next);
            }
        }
        emit(&line[start..]);
    }
}

fn notice_height(notice: Option<&str>, size: Size) -> u16 {
    let Some(notice) = notice else { return 0 };
    let mut lines = 0usize;
    notice_chunks(notice, size.width.saturating_sub(2 * MARGIN), |_| {
        lines += 1
    });
    // Leave room for a usable modal and input even at the minimum size.
    (lines.min(u16::MAX as usize) as u16)
        .min((size.height / 3).min(size.height.saturating_sub(8)).max(1))
        .max(1)
}

fn draw_notice(frame: &mut Frame, notice: &str, area: Rect) {
    if area.is_empty() {
        return;
    }
    let mut lines = Vec::new();
    notice_chunks(notice, area.width, |line| lines.push(Line::raw(line)));
    let overflow = lines.len() > area.height as usize;
    if overflow && area.height == 1 {
        frame.render_widget(
            Paragraph::new(lines.pop().unwrap()).style(Style::default().fg(AMBER)),
            Rect::new(area.x + 1, area.y, area.width.saturating_sub(1), 1),
        );
        frame.buffer_mut()[(area.x, area.y)]
            .set_symbol("↑")
            .set_fg(AMBER);
        return;
    }
    let visible = area.height.saturating_sub(u16::from(overflow));
    let scroll = lines
        .len()
        .saturating_sub(visible as usize)
        .min(u16::MAX as usize) as u16;
    let content = Rect::new(area.x, area.y + u16::from(overflow), area.width, visible);
    frame.render_widget(
        Paragraph::new(lines)
            .scroll((scroll, 0))
            .style(Style::default().fg(AMBER).bg(BG)),
        content,
    );
    if overflow {
        frame.render_widget(
            Paragraph::new("↑ earlier notice text").style(Style::default().fg(AMBER)),
            Rect::new(area.x, area.y, area.width, 1),
        );
    }
}
fn choice_list<'a>(
    frame: &mut Frame,
    area: Rect,
    items: impl IntoIterator<Item = impl Into<ListItem<'a>>>,
    selected: usize,
) {
    let list = List::new(items)
        .highlight_style(Style::default().fg(ACCENT).bg(BG))
        .highlight_symbol("› ");
    let mut state = ListState::default().with_selected(Some(selected));
    frame.render_stateful_widget(list, area, &mut state);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    fn test_workspace() -> Workspace {
        Workspace {
            tree: crate::worktree::Worktree {
                id: "terminal:test".into(),
                path: ".".into(),
                branch: String::new(),
                head: String::new(),
                is_main: false,
                locked: false,
                bare: false,
                prunable: false,
            },
            source: None,
            sessions: Vec::new(),
            selected: 0,
            environment: None,
            pending: false,
        }
    }

    fn row_text(buffer: &Buffer, area: Rect, row: u16) -> String {
        (area.x..area.right())
            .map(|column| buffer[(column, row)].symbol())
            .collect()
    }

    #[test]
    fn pane_targets_keep_shared_workspace_sessions_independent_when_collapsed() {
        let mut project = Project {
            repository: None,
            name: "test".into(),
            workspaces: vec![test_workspace()],
            trees: Vec::new(),
            active: 0,
            secondary: Some(0),
            secondary_session: Some(1),
            focus_secondary: false,
            layout: LayoutMode::Vertical,
            split_ratio: crate::restore::default_split_ratio(),
            order: 0,
        };
        for layout in [LayoutMode::Vertical, LayoutMode::Horizontal] {
            project.layout = layout;
            for focus_secondary in [false, true] {
                project.focus_secondary = focus_secondary;
                for size in [Size::new(80, 24), Size::new(30, 10), Size::new(80, 15)] {
                    let (_, count) = pane_areas(layout, true, project.split_ratio, size, size);
                    if count == 2 {
                        assert_eq!(pane_target(&project, 0, count), (0, 0, false));
                        assert_eq!(pane_target(&project, 1, count), (0, 1, true));
                    } else {
                        assert_eq!(
                            pane_target(&project, 0, count),
                            (0, usize::from(focus_secondary), focus_secondary)
                        );
                    }
                }
            }
        }
        project.workspaces.push(test_workspace());
        project.workspaces[1].selected = 2;
        project.secondary = Some(1);
        project.secondary_session = None;
        assert_eq!(pane_target(&project, 0, 2), (0, 0, false));
        assert_eq!(pane_target(&project, 1, 2), (1, 2, true));
        assert_eq!(pane_target(&project, 0, 1), (1, 2, true));
        project.layout = LayoutMode::Full;
        assert_eq!(pane_target(&project, 0, 1), (0, 0, false));
    }

    #[test]
    fn shared_workspace_draws_each_sessions_header_output_cursor_and_selection() {
        let directory = tempfile::tempdir().unwrap();
        let mut workspace = test_workspace();
        workspace.tree.path = directory.path().to_owned();
        let (tx, _rx) = std::sync::mpsc::sync_channel(16);
        let panes = [Rect::new(0, 1, 100, 12), Rect::new(100, 1, 100, 12)];
        let bodies = panes.map(body);
        for id in 1..=2 {
            workspace.sessions.push(
                crate::terminal::Session::spawn(
                    id,
                    "shell",
                    None,
                    directory.path(),
                    &directory.path().join("unused.sock"),
                    tx.clone(),
                    bodies[0].height,
                    bodies[0].width,
                    Some(("/bin/cat".into(), Vec::new())),
                    None,
                )
                .unwrap(),
            );
        }
        // The transcripts below stand in for what the programs wrote.
        for session in &workspace.sessions {
            session.mark_output();
        }
        workspace.sessions[0]
            .parser
            .lock()
            .process(b"first transcript");
        workspace.sessions[1]
            .parser
            .lock()
            .process(b"second transcript\r\nsecond cursor");
        let mut terminal = Terminal::new(TestBackend::new(200, 13)).unwrap();
        for focused in 0..2 {
            terminal
                .draw(|frame| {
                    for (selected, area) in panes.iter().copied().enumerate() {
                        assert!(
                            draw_workspace(
                                frame,
                                &workspace,
                                selected,
                                area,
                                bodies[selected],
                                selected == focused,
                                true,
                                None,
                            )
                            .is_none()
                        );
                    }
                })
                .unwrap();
            let buffer = terminal.backend().buffer();
            for (selected, pane) in panes.iter().enumerate() {
                let header = row_text(buffer, *pane, pane.y);
                assert!(header.contains(&format!("[shell {} ", selected + 1)));
                assert_eq!(header.contains('›'), selected == focused);
                assert!(!workspace.sessions[selected].dirty.load(Ordering::Acquire));
            }
            assert!(row_text(buffer, bodies[0], bodies[0].y).starts_with("first transcript"));
            assert!(row_text(buffer, bodies[1], bodies[1].y).starts_with("second transcript"));
            let (row, column) = workspace.sessions[focused]
                .parser
                .lock()
                .screen()
                .cursor_position();
            let cursor = terminal.get_cursor_position().unwrap();
            assert_eq!(
                (cursor.x, cursor.y),
                (bodies[focused].x + column, bodies[focused].y + row)
            );
        }
        let mut selection = crate::selection::Selection::new(
            workspace.sessions[1].id,
            bodies[1],
            workspace.sessions[1].parser.lock().screen(),
            bodies[1].x,
            bodies[1].y,
        );
        selection.update(bodies[1].x + 5, bodies[1].y);
        workspace.sessions[1]
            .parser
            .lock()
            .process(b"\x1b[Hchanged");
        terminal
            .draw(|frame| {
                for (selected, area) in panes.iter().copied().enumerate() {
                    draw_workspace(
                        frame,
                        &workspace,
                        selected,
                        area,
                        bodies[selected],
                        selected == 0,
                        true,
                        Some(&selection),
                    );
                }
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        assert!(row_text(buffer, bodies[1], bodies[1].y).starts_with("second transcript"));
        assert_eq!(selection.text(), "second");
        assert_eq!(buffer[(bodies[1].x, bodies[1].y)].bg, ACCENT);
        assert_ne!(buffer[(bodies[0].x, bodies[0].y)].bg, ACCENT);
        let cursor = terminal.get_cursor_position().unwrap();
        assert_eq!((cursor.x, cursor.y), (bodies[0].x + 16, bodies[0].y));

        workspace.sessions[1].state = AgentState::Exited;
        workspace.sessions[1].message = Some("test exit".into());
        for row in 0..30 {
            workspace.sessions[1]
                .parser
                .lock()
                .process(format!("history {row}\r\n").as_bytes());
        }
        terminal
            .draw(|frame| {
                for (selected, area) in panes.iter().copied().enumerate() {
                    let bar = draw_workspace(
                        frame,
                        &workspace,
                        selected,
                        area,
                        bodies[selected],
                        selected == 1,
                        true,
                        None,
                    );
                    assert_eq!(bar.is_some(), selected == 1);
                    if let Some(bar) = bar {
                        assert_eq!(bar.track.x, bodies[selected].right());
                    }
                }
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        assert!(!row_text(buffer, panes[0], panes[0].y).contains("Exited ("));
        assert!(row_text(buffer, panes[1], panes[1].y).contains("Exited (test exit)"));
    }

    /// A workspace whose only session is an idle `cat` sized to `body`.
    fn cat_workspace(
        directory: &std::path::Path,
        body: Rect,
    ) -> (Workspace, std::sync::mpsc::Receiver<crate::event::Event>) {
        let mut workspace = test_workspace();
        workspace.tree.path = directory.to_owned();
        let (tx, rx) = std::sync::mpsc::sync_channel(16);
        workspace.sessions.push(
            crate::terminal::Session::spawn(
                1,
                "shell",
                None,
                directory,
                &directory.join("unused.sock"),
                tx,
                body.height,
                body.width,
                Some(("/bin/cat".into(), Vec::new())),
                None,
            )
            .unwrap(),
        );
        (workspace, rx)
    }

    #[test]
    fn vs16_emoji_keep_their_single_vt100_column() {
        let directory = tempfile::tempdir().unwrap();
        let area = Rect::new(0, 1, 40, 6);
        let content = body(area);
        let (workspace, _rx) = cat_workspace(directory.path(), content);
        workspace.sessions[0]
            .parser
            .lock()
            .process("⚠\u{fe0f}ab".as_bytes());
        workspace.sessions[0].mark_output();
        let mut terminal = Terminal::new(TestBackend::new(40, 7)).unwrap();
        terminal
            .draw(|frame| {
                draw_workspace(frame, &workspace, 0, area, content, true, true, None);
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        // vt100 keeps the sequence in one column, so the outer terminal must use one too.
        assert_eq!(buffer[(content.x, content.y)].symbol(), "⚠");
        assert_eq!(buffer[(content.x + 1, content.y)].symbol(), "a");
        // Copied text still comes from vt100's untouched cell.
        let parser = workspace.sessions[0].parser.lock();
        assert_eq!(parser.screen().cell(0, 0).unwrap().contents(), "⚠\u{fe0f}");
    }

    #[test]
    fn cursor_stays_visible_in_pending_wrap_column() {
        let directory = tempfile::tempdir().unwrap();
        let area = Rect::new(0, 1, 12, 6);
        let content = body(area);
        assert_eq!(content.width, 10);
        let (workspace, _rx) = cat_workspace(directory.path(), content);
        workspace.sessions[0].parser.lock().process(b"0123456789");
        // The last column is written, so vt100 parks the cursor one past it.
        let cursor = workspace.sessions[0]
            .parser
            .lock()
            .screen()
            .cursor_position();
        assert_eq!(cursor, (0, 10));
        let mut terminal = Terminal::new(TestBackend::new(12, 7)).unwrap();
        terminal
            .draw(|frame| {
                draw_workspace(frame, &workspace, 0, area, content, true, true, None);
            })
            .unwrap();
        let cursor = terminal.get_cursor_position().unwrap();
        assert_eq!((cursor.x, cursor.y), (content.x + 9, content.y));
    }

    #[test]
    fn fresh_sessions_say_they_are_starting() {
        use std::time::{Duration, Instant};
        let directory = tempfile::tempdir().unwrap();
        let area = Rect::new(0, 1, 100, 6);
        let content = body(area);
        let (tx, _rx) = std::sync::mpsc::sync_channel(16);
        let spawn = |kind: &str, chat: Option<&str>, program: &str, args: &[&str]| {
            let mut workspace = test_workspace();
            workspace.tree.path = directory.path().to_owned();
            workspace.sessions.push(
                crate::terminal::Session::spawn(
                    1,
                    kind,
                    chat.map(crate::integration::Resume::Chat),
                    directory.path(),
                    &directory.path().join("unused.sock"),
                    tx.clone(),
                    content.height,
                    content.width,
                    Some((program.into(), args.iter().map(|&arg| arg.into()).collect())),
                    None,
                )
                .unwrap(),
            );
            workspace
        };
        let draw = |workspace: &Workspace, area: Rect| {
            let mut terminal = Terminal::new(TestBackend::new(100, 7)).unwrap();
            terminal
                .draw(|frame| {
                    draw_workspace(frame, workspace, 0, area, body(area), true, true, None);
                })
                .unwrap();
            terminal.backend().buffer().clone()
        };
        // Silent programs. An agent whose state is not known yet is "starting"; a shell is
        // idle from its first moment, and its pane still says what it is waiting for.
        for (kind, chat, line, state) in [
            ("shell", None, "Starting shell…", "idle"),
            ("omp", None, "Starting omp…", "starting"),
            (
                "omp",
                Some("/chats/a.jsonl"),
                "Starting omp — loading its saved chat…",
                "starting",
            ),
            ("opencode", None, "Starting opencode…", "starting"),
            (
                "opencode",
                Some("ses_a"),
                "Starting opencode — opening its saved chat…",
                "starting",
            ),
        ] {
            let workspace = spawn(kind, chat, "/bin/sleep", &["5"]);
            assert!(!workspace.sessions[0].has_output());
            let buffer = draw(&workspace, area);
            // One muted line in the body's top-left corner, and nothing else in the body.
            assert_eq!(row_text(&buffer, content, content.y).trim_end(), line);
            assert_eq!(buffer[(content.x, content.y)].fg, MUTED);
            for row in content.y + 1..content.bottom() {
                assert!(row_text(&buffer, content, row).trim().is_empty());
            }
            let header = row_text(&buffer, area, area.y);
            assert!(
                header.contains(&format!("[{kind} 1 · {state}]")),
                "{header:?}"
            );
        }
        // A body too narrow for the line cuts it with `…` and never draws outside itself.
        let workspace = spawn("omp", None, "/bin/sleep", &["5"]);
        let narrow = Rect::new(0, 1, 12, 6);
        let buffer = draw(&workspace, narrow);
        assert_eq!(
            row_text(&buffer, body(narrow), body(narrow).y),
            "Starting …"
        );
        assert_eq!(buffer[(body(narrow).right(), body(narrow).y)].symbol(), " ");
        // Terminal setup and a screen of blanks show nothing, so an agent that has sent only
        // those is still starting. Once text shows, its output is drawn as it is.
        let script = "printf '\\033[?1049h\\033[?1000h%200s' ''; read line; \
                      printf '\\033[Hpainted'; sleep 5";
        let workspace = spawn("omp", None, "/bin/sh", &["-c", script]);
        let session = &workspace.sessions[0];
        let deadline = Instant::now() + Duration::from_secs(5);
        while session.parser.lock().screen().contents().is_empty() {
            assert!(Instant::now() < deadline, "the blanks never arrived");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!session.has_output());
        let buffer = draw(&workspace, area);
        assert_eq!(
            row_text(&buffer, content, content.y).trim_end(),
            "Starting omp…"
        );
        assert!(row_text(&buffer, area, area.y).contains("[omp 1 · starting]"));
        // The newline ends the program's `read`; its echo moves the cursor and paints nothing.
        session.paste("\n").unwrap();
        while !session.has_output() {
            assert!(Instant::now() < deadline, "the program never painted");
            std::thread::sleep(Duration::from_millis(10));
        }
        let buffer = draw(&workspace, area);
        assert_eq!(row_text(&buffer, content, content.y).trim_end(), "painted");
        assert!(row_text(&buffer, area, area.y).contains("[omp 1 · unknown]"));
        // A program that exited without painting is not starting any more.
        let workspace = spawn("omp", None, "/bin/true", &[]);
        let deadline = Instant::now() + Duration::from_secs(5);
        while workspace.sessions[0].alive.load(Ordering::Acquire) {
            assert!(Instant::now() < deadline, "the program never exited");
            std::thread::sleep(Duration::from_millis(10));
        }
        let buffer = draw(&workspace, area);
        assert!(row_text(&buffer, content, content.y).trim().is_empty());
    }

    #[test]
    fn split_picker_keeps_every_selection_and_controls_inside_available_space() {
        for size in [Size::new(30, 10), Size::new(80, 24)] {
            for notice in [None, Some("progress\nprogress\nprogress")] {
                let bounds = Rect::new(0, 0, size.width, size.height - notice_height(notice, size));
                let mut terminal =
                    Terminal::new(TestBackend::new(size.width, size.height)).unwrap();
                let mut selected_rows = Vec::new();
                for selected in 0..3 {
                    terminal
                        .draw(|frame| draw_split_picker(frame, bounds, selected))
                        .unwrap();
                    let buffer = terminal.backend().buffer();
                    let row = (0..bounds.bottom())
                        .find(|&row| row_text(buffer, bounds, row).contains('›'))
                        .expect("selected split action must be visible");
                    selected_rows.push(row);
                    assert!((row + 1..bounds.bottom()).any(|row| {
                        let text = row_text(buffer, bounds, row);
                        text.contains("Enter") && text.contains("Esc")
                    }));
                    for row in bounds.bottom()..size.height {
                        assert!(row_text(buffer, buffer.area, row).trim().is_empty());
                    }
                }
                assert!(selected_rows.windows(2).all(|rows| rows[0] < rows[1]));
            }
        }
    }

    #[test]
    fn existing_split_picker_keeps_navigation_and_errors_visible_during_scroll() {
        let choices: Vec<_> = (0..30)
            .map(|index| SplitTarget::Branch(format!("branch-{index:02}")))
            .collect();
        for size in [Size::new(30, 10), Size::new(80, 24)] {
            for notice in [None, Some("progress\nprogress\nprogress")] {
                let bounds = Rect::new(0, 0, size.width, size.height - notice_height(notice, size));
                let mut terminal =
                    Terminal::new(TestBackend::new(size.width, size.height)).unwrap();
                for error in [
                    None,
                    Some("Opening failed:\ncheckout unavailable\ntry another target"),
                ] {
                    for selected in [0, 1, 15, 29, 28, 0] {
                        terminal
                            .draw(|frame| {
                                draw_existing_split_picker(
                                    frame, bounds, &choices, selected, error,
                                );
                            })
                            .unwrap();
                        let buffer = terminal.backend().buffer();
                        let rows: Vec<_> = (0..bounds.bottom())
                            .map(|row| row_text(buffer, bounds, row))
                            .collect();
                        assert!(rows.iter().any(|row| {
                            row.contains('›') && row.contains(&format!("branch-{selected:02}"))
                        }));
                        assert!(
                            rows.iter()
                                .any(|row| row.contains("Enter") && row.contains("Esc"))
                        );
                        if error.is_some() {
                            assert!(rows.iter().any(|row| row.contains("try another target")));
                        }
                        for row in bounds.bottom()..size.height {
                            assert!(row_text(buffer, buffer.area, row).trim().is_empty());
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn existing_split_picker_distinguishes_paths_and_creation_without_filling_screen() {
        let mut first = test_workspace().tree;
        first.branch = "detached".into();
        first.path = "/repo/one".into();
        let mut second = first.clone();
        second.path = "/repo/two".into();
        let choices = [
            SplitTarget::Worktree(first),
            SplitTarget::Worktree(second),
            SplitTarget::Branch("topic".into()),
        ];
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|frame| {
                let bounds = frame.area();
                draw_existing_split_picker(frame, bounds, &choices, 2, None);
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let rows: Vec<_> = (0..buffer.area.height)
            .map(|row| row_text(buffer, buffer.area, row))
            .filter(|row| !row.trim().is_empty())
            .collect();
        assert!(
            rows.iter()
                .any(|row| row.contains("detached") && row.contains("/repo/one"))
        );
        assert!(
            rows.iter()
                .any(|row| row.contains("detached") && row.contains("/repo/two"))
        );
        assert!(
            rows.iter()
                .any(|row| row.contains("topic") && row.contains("create"))
        );
        assert!(rows.len() < buffer.area.height as usize / 2);
    }

    #[test]
    fn empty_split_picker_remains_navigable_and_opening_hints_that_esc_hides_it() {
        let size = Size::new(30, 10);
        let bounds = Rect::new(
            0,
            0,
            size.width,
            size.height - notice_height(Some("progress\nprogress\nprogress"), size),
        );
        let mut terminal = Terminal::new(TestBackend::new(size.width, size.height)).unwrap();
        terminal
            .draw(|frame| draw_existing_split_picker(frame, bounds, &[], 0, Some("No repository")))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let rows: Vec<_> = (0..bounds.bottom())
            .map(|row| row_text(buffer, bounds, row))
            .collect();
        assert!(rows.iter().any(|row| row.contains("No other")));
        assert!(rows.iter().any(|row| row.contains("No repository")));
        assert!(rows.iter().any(|row| row.contains("Esc")));
        terminal
            .draw(|frame| draw_opening_split(frame, bounds))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let rows: Vec<_> = (0..size.height)
            .map(|row| row_text(buffer, buffer.area, row))
            .filter(|row| !row.trim().is_empty())
            .collect();
        assert!(rows.iter().any(|row| row.contains("Opening")));
        assert!(rows.iter().any(|row| row.contains("Esc")));
        assert!(rows.len() < bounds.height as usize / 2);
    }

    #[test]
    fn pane_bodies_fill_available_height_without_covering_headers() {
        for size in [
            Size::new(30, 10),
            Size::new(59, 24),
            Size::new(80, 24),
            Size::new(240, 80),
        ] {
            for layout in [
                LayoutMode::Full,
                LayoutMode::Vertical,
                LayoutMode::Horizontal,
            ] {
                let (panes, count) = pane_areas(
                    layout,
                    true,
                    crate::restore::default_split_ratio(),
                    size,
                    size,
                );
                for pane in panes.into_iter().take(count) {
                    let content = body(pane);
                    assert!(content.right() <= size.width);
                    assert!(content.y > pane.y);
                    assert!(content.bottom() <= size.height);
                }
                assert_eq!(body(panes[count - 1]).bottom(), size.height);
            }
        }
    }

    #[test]
    fn vertical_ratios_fill_width_without_moving_content_into_the_divider() {
        let scale = crate::restore::SPLIT_RATIO_SCALE;
        for width in [60, 61, 80, 121, 240] {
            let size = Size::new(width, 24);
            for ratio in [0, 1, scale / 4, scale / 2, 3 * scale / 4, scale, u16::MAX] {
                let (panes, count) = pane_areas(LayoutMode::Vertical, true, ratio, size, size);
                let divider = split_divider(LayoutMode::Vertical, panes, count).unwrap();
                let [left, right] = panes.map(body);
                assert_eq!(count, 2);
                assert_eq!(panes[0].x, 0);
                assert_eq!(panes[0].width + panes[1].width, size.width);
                assert!(panes.iter().all(|pane| pane.width >= 28));
                assert_eq!(panes[0].right(), panes[1].x);
                assert_eq!(panes[1].right(), size.width);
                assert_eq!(left.y, right.y);
                assert_eq!(left.height, right.height);
                assert_eq!(left.bottom(), size.height);
                assert_eq!(divider.available_width, size.width);
                assert_eq!(
                    divider.track,
                    Rect::new(panes[0].right(), left.y, 1, left.height)
                );
                assert_eq!(left.right() + MARGIN, divider.track.x);
                assert_eq!(divider.track.x, panes[1].x);
                assert_eq!(divider.handle.right(), right.x);
                assert_eq!(right.right() + MARGIN, size.width);
                assert_eq!(divider.handle.x, left.right());
                assert_eq!(divider.handle.height, 1);
                let above = divider.handle.y - divider.track.y;
                let below = divider.track.bottom() - divider.handle.bottom();
                assert!(above.abs_diff(below) <= 1);
                if ratio == 0 {
                    assert_eq!(panes[0].width, 28);
                } else if ratio >= scale {
                    assert_eq!(panes[1].width, 28);
                }
            }
        }
    }

    #[test]
    fn resizing_preserves_unclamped_split_proportions_and_body_alignment() {
        let scale = u32::from(crate::restore::SPLIT_RATIO_SCALE);
        for ratio in [2_500, 4_000, 6_500, 7_500] {
            for size in [
                Size::new(160, 24),
                Size::new(240, 48),
                Size::new(120, 16),
                Size::new(181, 25),
                Size::new(160, 24),
            ] {
                let (panes, count) = pane_areas(LayoutMode::Vertical, true, ratio, size, size);
                let divider = split_divider(LayoutMode::Vertical, panes, count).unwrap();
                let desired = u32::from(size.width) * u32::from(ratio);
                let actual = u32::from(panes[0].width) * scale;
                assert!(actual.abs_diff(desired) <= scale / 2);
                for pane in panes {
                    let content = body(pane);
                    assert_eq!(content.width + 2 * MARGIN, pane.width);
                    assert_eq!(content.y, pane.y + HEADER_ROWS);
                    assert_eq!(content.bottom(), size.height);
                    assert_eq!(divider.track.y, content.y);
                    assert_eq!(divider.track.height, content.height);
                }
            }
        }
    }

    #[test]
    fn divider_stays_hidden_and_ratios_do_not_change_other_layouts() {
        for size in [
            Size::new(30, 10),
            Size::new(59, 24),
            Size::new(80, 15),
            Size::new(80, 24),
        ] {
            for layout in [
                LayoutMode::Full,
                LayoutMode::Horizontal,
                LayoutMode::Vertical,
            ] {
                for split in [false, true] {
                    if layout == LayoutMode::Vertical
                        && split
                        && size.width >= 60
                        && size.height >= 16
                    {
                        continue;
                    }
                    let baseline = pane_areas(
                        layout,
                        split,
                        crate::restore::default_split_ratio(),
                        size,
                        size,
                    );
                    for ratio in [0, 2_500, 7_500, u16::MAX] {
                        let (panes, count) = pane_areas(layout, split, ratio, size, size);
                        assert_eq!((panes, count), baseline);
                        assert!(split_divider(layout, panes, count).is_none());
                        if count == 1 {
                            assert_eq!(
                                panes,
                                [
                                    Rect::new(0, 1, size.width, size.height - 1),
                                    Rect::default()
                                ]
                            );
                        } else {
                            assert_eq!(count, 2);
                            assert_eq!(panes[0].width, size.width);
                            assert_eq!(panes[1].width, size.width);
                            assert_eq!(panes[0].bottom() + 1, panes[1].y);
                            assert_eq!(panes[1].bottom(), size.height);
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn scrollbar_measurement_preserves_history_view_and_hides_on_alternate_screen() {
        let mut parser = vt100::Parser::new(8, 30, 32);
        let body = Rect::new(1, 2, 30, 8);
        assert!(scrollbar(parser.screen_mut(), body).is_none());
        for row in 0..70 {
            parser.process(format!("line-{row}\r\n").as_bytes());
        }
        parser.screen_mut().set_scrollback(15);
        let contents = parser.screen().contents();
        let bar = scrollbar(parser.screen_mut(), body).unwrap();
        assert_eq!(bar.max_offset, 32);
        assert_eq!(parser.screen().scrollback(), 15);
        assert_eq!(parser.screen().contents(), contents);
        assert!(bar.thumb.y > bar.track.y);
        assert!(bar.thumb.bottom() < bar.track.bottom());
        parser.process(b"\x1b[?1049h");
        assert!(scrollbar(parser.screen_mut(), body).is_none());
    }

    #[test]
    fn long_unicode_input_scrolls_with_cursor_inside_input_row() {
        let mut terminal = Terminal::new(TestBackend::new(30, 10)).unwrap();
        let area = Rect::new(3, 4, 24, 1);
        for input in ["short".to_owned(), "界e\u{301}".repeat(100)] {
            terminal
                .draw(|frame| input_line(frame, area, &input, true))
                .unwrap();
            let cursor = terminal.get_cursor_position().unwrap();
            assert_eq!(cursor.y, area.y);
            assert!(cursor.x >= area.x && cursor.x < area.right());
            if input.width() >= area.width as usize {
                assert_eq!(
                    terminal.backend().buffer()[(cursor.x - 1, cursor.y)].symbol(),
                    "e\u{301}"
                );
            }
        }
    }

    #[test]
    fn confirmation_controls_survive_long_wrapped_message() {
        let mut terminal = Terminal::new(TestBackend::new(30, 10)).unwrap();
        terminal
            .draw(|frame| {
                let bounds = frame.area();
                let inner = modal_area_in(frame, bounds, " confirm ", Size::new(76, 9));
                draw_confirmation(frame, inner, &"Long destructive consequences ".repeat(20));
                let buffer = frame.buffer_mut();
                assert_eq!(buffer[(inner.x, inner.bottom() - 1)].fg, AMBER);
                assert_ne!(
                    buffer[(inner.right() - 2, inner.bottom() - 1)].symbol(),
                    " "
                );
            })
            .unwrap();
    }

    #[test]
    fn overflow_notice_shows_final_cause_and_overflow_marker() {
        let mut terminal = Terminal::new(TestBackend::new(30, 10)).unwrap();
        terminal
            .draw(|frame| {
                draw_notice(
                    frame,
                    "preparing\nchecking\nprogress\nFATAL\n",
                    Rect::new(1, 8, 28, 2),
                );
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(1, 8)].symbol(), "↑");
        assert_eq!(buffer[(1, 9)].symbol(), "F");
        assert_eq!(buffer[(5, 9)].symbol(), "L");
    }

    #[test]
    fn notice_chunks_preserve_unicode_and_newline_boundaries() {
        let mut chunks = Vec::new();
        notice_chunks("界界e\u{301}\nlast", 4, |chunk| chunks.push(chunk));
        assert_eq!(chunks, ["界界", "e\u{301}", "last"]);
        assert!(chunks.iter().all(|chunk| chunk.width() <= 4));
        // A VS16 emoji is one two-column unit, and a ZWJ sequence is never split.
        let vs16 = "⚠\u{fe0f}".repeat(3);
        let mut emoji = Vec::new();
        notice_chunks(&vs16, 4, |chunk| emoji.push(chunk));
        assert_eq!(emoji, ["⚠\u{fe0f}⚠\u{fe0f}", "⚠\u{fe0f}"]);
        let mut family = Vec::new();
        notice_chunks("ab👨\u{200d}👩\u{200d}👧cd", 4, |chunk| {
            family.push(chunk)
        });
        assert_eq!(family, ["ab👨\u{200d}👩\u{200d}👧", "cd"]);
        // Ratatui drops control characters, so they take no columns on either path.
        let mut controls = Vec::new();
        notice_chunks("ab\tcd\r\n界\u{1b}[0m", 4, |chunk| controls.push(chunk));
        assert_eq!(controls, ["ab\tcd\r", "界\u{1b}[0", "m"]);
    }

    #[test]
    fn list_keeps_selected_item_visible_after_overflow() {
        let mut terminal = Terminal::new(TestBackend::new(30, 10)).unwrap();
        terminal
            .draw(|frame| {
                choice_list(
                    frame,
                    Rect::new(2, 2, 24, 3),
                    (0..30).map(|index| format!("item {index}")),
                    29,
                )
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        assert!((2..5).any(|row| buffer[(2, row)].symbol() == "›"));
        assert!((2..5).any(|row| buffer[(4, row)].fg == ACCENT));
    }

    #[test]
    fn multiline_notice_geometry_leaves_modal_input_and_output_separate() {
        for size in [Size::new(30, 10), Size::new(80, 24), Size::new(240, 80)] {
            let notice = "progress\n".repeat(100);
            let height = notice_height(Some(&notice), size);
            let mut content = size;
            content.height -= height;
            let (panes, count) = pane_areas(
                LayoutMode::Horizontal,
                true,
                crate::restore::default_split_ratio(),
                size,
                content,
            );
            for pane in panes.into_iter().take(count) {
                assert!(body(pane).bottom() <= size.height - height);
            }
            let mut terminal = Terminal::new(TestBackend::new(size.width, size.height)).unwrap();
            terminal
                .draw(|frame| {
                    let available = Rect::new(0, 0, size.width, size.height - height);
                    let inner = modal_area_in(frame, available, " input ", Size::new(76, 12));
                    assert!(inner.height >= 4);
                    input_line(
                        frame,
                        Rect::new(inner.x, inner.y + 2, inner.width, 1),
                        "界".repeat(100).as_str(),
                        true,
                    );
                    assert!(inner.bottom() < size.height - height);
                })
                .unwrap();
            let cursor = terminal.get_cursor_position().unwrap();
            assert!(cursor.y < size.height - height);
        }
    }

    #[test]
    fn key_mode_hint_lists_f2_and_f3_and_compacts_on_narrow_frames() {
        // The pane's trash cell relies on this glyph taking one column, not two.
        assert_eq!("🗑".width(), 1);
        let hint = |width: u16, native_keys: bool, cpu: Option<&str>| {
            let mut terminal = Terminal::new(TestBackend::new(width, 10)).unwrap();
            let mut used = 0;
            terminal
                .draw(|frame| used = draw_key_mode(frame, native_keys, cpu))
                .unwrap();
            let buffer = terminal.backend().buffer();
            let row: String = (0..width)
                .map(|column| buffer[(column, 0)].symbol())
                .collect();
            (used, row)
        };
        // Every key is named where that takes at most half the row.
        for (width, cpu) in [(104, "50%"), (104, "OFF"), (106, "100%"), (200, "LOW")] {
            let (used, row) = hint(width, false, Some(cpu));
            let expected = format!(" F1 ASK · F2 WORKTREES · F3 CPU {cpu} · F12 CRIT KEYS ");
            assert!(row.ends_with(&expected), "{row:?}");
            assert_eq!(usize::from(used), expected.width());
            assert!(width - used >= used);
        }
        // Narrower, the keys and the limit's state still show while the tab bar keeps 15.
        for (width, cpu) in [
            (103, "50%"),
            (105, "100%"),
            (64, "50%"),
            (43, "50%"),
            (43, "OFF"),
        ] {
            let (used, row) = hint(width, false, Some(cpu));
            assert!(
                row.ends_with(&format!(" F1 · F2 · F3 CPU {cpu} · F12 ")),
                "{row:?}"
            );
            assert!(width - used >= 15);
        }
        // Down to the 30-column minimum, the keys alone; a focused tab still fits.
        for width in [30, 42] {
            let (used, row) = hint(width, false, Some("100%"));
            assert_eq!(used, 14);
            assert!(row.ends_with(" F1·F2·F3·F12 "), "{row:?}");
            assert!(width - used >= 15);
        }
        // Native mode, where F3 belongs to the terminal, and the project chooser keep their labels.
        assert_eq!(hint(100, true, Some("50%")).0, 17);
        assert_eq!(hint(100, false, None).0, 16);
    }

    #[test]
    fn clip_width_cuts_between_graphemes_and_leaves_a_column_for_the_ellipsis() {
        assert_eq!(clip_width("main", 4), ("main", false));
        assert_eq!(clip_width("main", 9), ("main", false));
        assert_eq!(clip_width("feature/login", 8), ("feature", true));
        assert_eq!(clip_width("abc", 0), ("", true));
        // A wide character is never split, and neither is a base with its combining mark.
        assert_eq!(clip_width("界界界", 5), ("界界", true));
        assert_eq!(clip_width("界界界", 4), ("界", true));
        assert_eq!(clip_width("e\u{301}xy", 2), ("e\u{301}", true));
        assert_eq!(clip_width("e\u{301}xy", 1), ("", true));
        let (text, _) = clip_width("日本語のブランチ", 7);
        assert!(text.width() <= 6 && "日本語のブランチ".starts_with(text));
    }

    #[test]
    fn worktree_window_reserves_indicator_lines_and_clamps_the_scroll() {
        // Everything fits: no indicators, and an offset has nothing to scroll.
        assert_eq!(worktree_window(0, 5, 0), (0, 0, false, false));
        assert_eq!(worktree_window(3, 5, 9), (0, 3, false, false));
        assert_eq!(worktree_window(5, 5, 1), (0, 5, false, false));
        // Ten rows in five lines: `↓` at the top of the list, both in the middle, `↑` at the end.
        assert_eq!(worktree_window(10, 5, 0), (0, 4, false, true));
        assert_eq!(worktree_window(10, 5, 3), (3, 3, true, true));
        assert_eq!(worktree_window(10, 5, 6), (6, 4, true, false));
        assert_eq!(worktree_window(10, 5, 99), (6, 4, true, false));
        // Every row is reachable, and the lines always add up.
        for total in 0..30 {
            for capacity in 0..12 {
                let mut seen = vec![false; total];
                for scroll in 0..total + 2 {
                    let (first, count, above, below) = worktree_window(total, capacity, scroll);
                    assert!(first + count <= total);
                    assert!(count + usize::from(above) + usize::from(below) <= capacity);
                    seen[first..first + count].fill(true);
                    assert_eq!(above, first > 0 && capacity >= 3 && total > capacity);
                }
                assert!(seen.iter().all(|&shown| shown) || capacity == 0);
            }
        }
    }
}
