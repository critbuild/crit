use crate::{
    app::{
        App, COMMANDS, Confirm, IntentDialog, LayoutMode, Modal, Pane, ProjectPicker, Scrollbar,
        TabHit, Workspace, project_slots,
    },
    integration::AgentState,
    updates::{State as UpdateState, TOOLS as UPDATE_TOOLS},
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
use std::sync::atomic::Ordering;
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
    pane_areas(app.project.layout, app.project.secondary.is_some(), content)
}

fn pane_areas(layout: LayoutMode, split: bool, size: Size) -> ([Rect; 2], usize) {
    // Only visible notices reserve bottom rows; areas() subtracts their height.
    let main = Rect::new(0, 1, size.width, size.height.saturating_sub(1));
    if !split || layout == LayoutMode::Full || size.width < 60 || size.height < 16 {
        return ([main, Rect::default()], 1);
    }
    match layout {
        LayoutMode::Vertical => {
            let width = main.width.saturating_sub(1) / 2;
            (
                [
                    Rect::new(main.x, main.y, width, main.height),
                    Rect::new(
                        main.x + width + 1,
                        main.y,
                        main.width - width - 1,
                        main.height,
                    ),
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

pub fn resize_sessions(app: &mut App, size: Size) -> Result<()> {
    let (areas, count) = areas(app, size);
    for (pane, area) in areas.iter().enumerate().take(count) {
        let index = if count == 1 {
            app.focused_index()
        } else if pane == 0 {
            app.project.active
        } else {
            app.project.secondary.unwrap_or(app.project.active)
        };
        let rect = body(*area);
        let workspace = &mut app.project.workspaces[index];
        if let Some(session) = workspace.sessions.get_mut(workspace.selected) {
            if app
                .selection
                .as_ref()
                .is_some_and(|selection| selection.session == session.id && selection.body != rect)
            {
                app.selection = None;
            }
            session.resize(rect.height, rect.width)?;
        }
    }
    Ok(())
}

pub fn draw(frame: &mut Frame, app: &mut App) {
    let bounds = frame.area();
    app.interaction_visible = bounds.width >= 30 && bounds.height >= 10;
    frame
        .buffer_mut()
        .set_style(bounds, Style::default().fg(FG).bg(BG));
    app.tab_hits.clear();
    app.panes.clear();
    if bounds.width < 30 || bounds.height < 10 {
        frame.render_widget(
            Paragraph::new("crit — enlarge terminal to at least 30×10. F12 toggles Kitty keys.")
                .wrap(Wrap { trim: false }),
            bounds,
        );
        return;
    }
    let width = draw_key_mode(frame, app.native_keys, true);
    draw_tabs(frame, app, bounds.width.saturating_sub(width));
    let (regions, count) = areas(app, Size::new(bounds.width, bounds.height));
    if count == 2 {
        if app.project.layout == LayoutMode::Vertical {
            let x = regions[0].right();
            for y in regions[0].top()..regions[0].bottom() {
                frame.buffer_mut()[(x, y)].set_symbol("│").set_fg(BORDER);
            }
        } else {
            line_horizontal(
                frame.buffer_mut(),
                0,
                regions[0].bottom(),
                bounds.width,
                BORDER,
            );
        }
    }
    for (pane, area) in regions.iter().copied().enumerate().take(count) {
        let index = if count == 1 {
            app.focused_index()
        } else if pane == 0 {
            app.project.active
        } else {
            app.project.secondary.unwrap_or(app.project.active)
        };
        let focused = app.focused_index() == index;
        let pane_body = body(area);
        let scrollbar = draw_workspace(
            frame,
            &app.project.workspaces[index],
            area,
            pane_body,
            focused,
            app.modal.is_none(),
            app.selection.as_ref(),
        );
        app.panes.push(Pane {
            workspace: index,
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
                bounds.width,
                HEADER_DIVIDER,
            );
        }
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
        if cell_width < hit.area.width.min(5) {
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

fn draw_workspace(
    frame: &mut Frame,
    workspace: &Workspace,
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
        .get(workspace.selected)
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
        let active = index == workspace.selected;
        header.push(Span::styled(
            format!(
                " {}{} {} · {}{} ",
                if active { "[" } else { "" },
                session.kind,
                index + 1,
                state_name(session.state),
                if active { "]" } else { "" }
            ),
            Style::default().fg(if active { color } else { MUTED }),
        ));
    }
    let selected_width = header.get(workspace.selected).map_or(0, Span::width);
    let label_width = label
        .width()
        .min(usize::from(area.width).saturating_sub(selected_width)) as u16;
    let session_width = area.width - label_width;
    let mut start = workspace.selected.min(header.len());
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
    if let Some(session) = workspace.sessions.get(workspace.selected) {
        let mut parser = session.parser.lock();
        // Clear while holding the parser lock: output after this frame must wake us.
        session.dirty.store(false, Ordering::Release);
        let selection =
            selection.filter(|selection| selection.session == session.id && selection.body == body);
        let scrollbar = scrollbar(parser.screen_mut(), body);
        let screen = parser.screen();
        for row in 0..body.height {
            for col in 0..body.width {
                let Some(cell) = selection.map_or_else(
                    || screen.cell(row, col),
                    |selection| selection.cell(row, col),
                ) else {
                    continue;
                };
                let target = &mut frame.buffer_mut()[(body.x + col, body.y + row)];
                if cell.is_wide_continuation() {
                    target.set_diff_option(ratatui::buffer::CellDiffOption::Skip);
                    continue;
                }
                target.set_symbol(if cell.has_contents() {
                    cell.contents()
                } else {
                    " "
                });
                let mut style = Style::default()
                    .fg(vt_color(cell.fgcolor(), FG))
                    .bg(vt_color(cell.bgcolor(), BG));
                if cell.bold() {
                    style = style.add_modifier(Modifier::BOLD);
                }
                if cell.dim() {
                    style = style.add_modifier(Modifier::DIM);
                }
                if cell.italic() {
                    style = style.add_modifier(Modifier::ITALIC);
                }
                if cell.underline() {
                    style = style.add_modifier(Modifier::UNDERLINED);
                }
                if cell.inverse() {
                    style = style.add_modifier(Modifier::REVERSED);
                }
                if cell.hyperlink().is_some() {
                    style = style.add_modifier(Modifier::UNDERLINED);
                }
                if selection.is_some_and(|selection| selection.selected(row, col)) {
                    style = style
                        .fg(SURFACE)
                        .bg(ACCENT)
                        .remove_modifier(Modifier::REVERSED);
                }
                target.set_style(style);
            }
        }
        if focused
            && show_cursor
            && selection.is_none()
            && !screen.hide_cursor()
            && screen.scrollback() == 0
        {
            let (row, col) = screen.cursor_position();
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

fn draw_key_mode(frame: &mut Frame, native_keys: bool, has_project: bool) -> u16 {
    let mode = if native_keys {
        " F12  KITTY KEYS "
    } else if has_project {
        " F1 ASK · F12 CRIT KEYS "
    } else {
        " F12  CRIT KEYS "
    };
    let width = mode.width() as u16;
    frame.render_widget(
        Paragraph::new(mode).style(
            Style::default()
                .fg(if native_keys { AMBER } else { ACCENT })
                .bg(BG),
        ),
        Rect::new(frame.area().width.saturating_sub(width), 0, width, 1),
    );
    width
}

pub fn draw_updates(frame: &mut Frame, states: &[UpdateState; 2], tick: usize) {
    let bounds = frame.area();
    if bounds.is_empty() {
        return;
    }
    let buffer = frame.buffer_mut();
    buffer.set_style(bounds, Style::default().fg(FG).bg(BG));
    let width = bounds.width.min(48);
    let x = bounds.x + (bounds.width - width) / 2;
    let y = bounds.y + bounds.height.saturating_sub(7) / 2;
    buffer.set_stringn(
        x,
        y,
        "crit",
        width as usize,
        Style::default().fg(ACCENT).bold(),
    );
    if width > 4 {
        let checking = states
            .iter()
            .any(|state| matches!(state, UpdateState::Waiting | UpdateState::Checking));
        let label = if !checking {
            " is ready"
        } else if width >= 43 {
            " is checking for updates"
        } else {
            " is checking"
        };
        buffer.set_stringn(
            x + 4,
            y,
            label,
            (width - 4) as usize,
            Style::default().fg(FG),
        );
    }
    if width >= 30 {
        let start = x + width - 13;
        let head = if tick < 12 { tick } else { 23 - tick };
        // A compact travelling wave, not a percentage or simulated download bar.
        for column in 0_usize..12 {
            let (glyph, color) = match column.abs_diff(head) {
                0 => ("⣿", ACCENT),
                1 => ("⣶", Color::Rgb(120, 162, 220)),
                2 => ("⣤", Color::Rgb(85, 118, 164)),
                3 => ("⣀", Color::Rgb(60, 80, 110)),
                _ => ("·", BORDER),
            };
            buffer[(start + column as u16, y)]
                .set_symbol(glyph)
                .set_fg(color);
        }
    } else if width > 17 {
        const FRAMES: [&str; 4] = ["⣀", "⣤", "⣶", "⣿"];
        buffer[(x + width - 1, y)]
            .set_symbol(FRAMES[tick % FRAMES.len()])
            .set_fg(ACCENT);
    }
    for (index, state) in states.iter().enumerate() {
        let row = y + 2 + index as u16;
        if row >= bounds.bottom() || width < 3 {
            break;
        }
        let (mark, text, color) = match state {
            UpdateState::Waiting => ("·", "waiting", MUTED),
            UpdateState::Checking => ("›", "checking", ACCENT),
            UpdateState::Ready => ("✓", "ready", GREEN),
            UpdateState::Failed => ("!", "using installed version", AMBER),
        };
        buffer.set_stringn(
            x + 2,
            row,
            mark,
            (width - 2) as usize,
            Style::default().fg(color),
        );
        if width > 5 {
            buffer.set_stringn(
                x + 5,
                row,
                UPDATE_TOOLS[index].0,
                (width - 5) as usize,
                Style::default().fg(FG),
            );
        }
        if width > 15 {
            buffer.set_stringn(
                x + 15,
                row,
                text,
                (width - 15) as usize,
                Style::default().fg(color),
            );
        }
    }
    if y + 5 < bounds.bottom() {
        buffer.set_stringn(
            x,
            y + 5,
            "ctrl+c cancel",
            width as usize,
            Style::default().fg(MUTED),
        );
    }
}

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
    let mode_width = draw_key_mode(frame, native_keys, false);
    frame.render_widget(
        Paragraph::new(" crit · start").style(Style::default().fg(FG).bold()),
        Rect::new(0, 0, bounds.width.saturating_sub(mode_width), 1),
    );
    let height = notice_height(Some(notice), Size::new(bounds.width, bounds.height));
    let available = Rect::new(bounds.x, bounds.y, bounds.width, bounds.height - height);
    let inner = modal_area_in(frame, available, " new terminal / open project ", 22);
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
    let status = if let Some(error) = &dialog.error {
        error.as_str()
    } else if dialog.loading {
        "Finding projects and worktrees..."
    } else if dialog.pending {
        "Interpreting with Haiku 5.5... Esc cancels."
    } else {
        "Enter to act · destructive actions still require confirmation."
    };
    frame.render_widget(
        Paragraph::new(status)
            .style(Style::default().fg(if dialog.error.is_some() { AMBER } else { MUTED }))
            .wrap(Wrap { trim: false }),
        Rect::new(inner.x, inner.y + 11, inner.width, inner.height - 13),
    );
    frame.render_widget(
        Paragraph::new("Enter send · Esc cancel · Ctrl+U clear · F2 manual controls")
            .style(Style::default().fg(MUTED)),
        Rect::new(inner.x, inner.bottom() - 1, inner.width, 1),
    );
}

fn modal_area_in(
    frame: &mut Frame,
    bounds: Rect,
    title: &'static str,
    desired_height: u16,
) -> Rect {
    let width = bounds.width.saturating_sub(4).min(76);
    let height = bounds.height.saturating_sub(2).min(desired_height);
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

fn draw_modal(frame: &mut Frame, app: &App, modal: &Modal) {
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
        Modal::Split { .. } => (
            " choose second worktree ",
            (app.project.workspaces.len() as u16 + 5).min(20),
        ),
        Modal::Preview { urls, .. } => (" open endpoint ", (urls.len() as u16 + 4).min(20)),
        Modal::Confirm(_) => (" confirm ", 9),
    };
    let mut available = frame.area();
    if app.notice.is_some() {
        available.height = available.height.saturating_sub(notice_height(
            app.notice.as_deref(),
            Size::new(available.width, available.height),
        ));
    }
    let inner = modal_area_in(frame, available, title, desired_height);
    match modal {
        Modal::Intent(dialog) => intent_dialog(frame, inner, dialog),
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
                        base = app.focused().tree.branch,
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
                .map(|tree| format!("{}  {}", tree.branch, tree.path.display())),
            *selected,
        ),
        Modal::Split { selected, .. } => choice_list(
            frame,
            inner,
            std::iter::once("New branch / worktree…".to_owned()).chain(
                app.project.workspaces.iter().map(|workspace| {
                    format!(
                        "{}{}",
                        workspace.tree.branch,
                        if workspace.tree.id == app.focused().tree.id {
                            " (current)"
                        } else {
                            ""
                        }
                    )
                }),
            ),
            *selected,
        ),
        Modal::Preview { urls, selected } => choice_list(
            frame,
            inner,
            urls.iter().map(|(name, url)| format!("{name}  {url}")),
            *selected,
        ),
        Modal::Confirm(confirm) => {
            let message = match confirm {
                Confirm::Quit => {
                    "Exit Crit; stop sessions?\nSaved chats reopen.\nDocker/checkouts stay."
                }
                Confirm::CloseWorktree if app.project.repository.is_none() => {
                    "Close tab; end its sessions?\nOther projects and sessions remain."
                }
                Confirm::CloseWorktree => {
                    "Close tab; end its sessions?\nCheckout, branch and Docker remain."
                }
                Confirm::CloseSession => {
                    "End the focused session?\nOther sessions and Docker remain."
                }
                Confirm::StopEnvironment => {
                    "Stop this worktree's Docker?\nOther worktrees unaffected.\nVolumes remain."
                }
                Confirm::RemoveWorktree => {
                    "Delete this checkout?\nDirty/main/locked refused.\nStop Docker first.\nBranch remains."
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
        for (offset, ch) in line.char_indices() {
            let next = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0) as u16;
            if columns + next > width && offset > start {
                emit(&line[start..offset]);
                start = offset;
                columns = 0;
            }
            columns += next;
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
fn choice_list(
    frame: &mut Frame,
    area: Rect,
    items: impl Iterator<Item = String>,
    selected: usize,
) {
    let list = List::new(items.map(ListItem::new).collect::<Vec<_>>())
        .highlight_style(Style::default().fg(ACCENT).bg(BG))
        .highlight_symbol("› ");
    let mut state = ListState::default().with_selected(Some(selected));
    frame.render_stateful_widget(list, area, &mut state);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

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
                let (panes, count) = pane_areas(layout, true, size);
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
                let inner = modal_area_in(frame, bounds, " confirm ", 9);
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
            let (panes, count) = pane_areas(LayoutMode::Horizontal, true, content);
            for pane in panes.into_iter().take(count) {
                assert!(body(pane).bottom() <= size.height - height);
            }
            let mut terminal = Terminal::new(TestBackend::new(size.width, size.height)).unwrap();
            terminal
                .draw(|frame| {
                    let available = Rect::new(0, 0, size.width, size.height - height);
                    let inner = modal_area_in(frame, available, " input ", 12);
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
}
