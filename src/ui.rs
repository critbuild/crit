use crate::{
    app::{
        App, COMMANDS, Confirm, IntentDialog, LayoutMode, Modal, Pane, ProjectPicker, Workspace,
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
const SURFACE: Color = Color::Rgb(32, 44, 56);
const ACCENT: Color = Color::Rgb(144, 191, 255);
const GREEN: Color = Color::Rgb(165, 214, 179);
const AMBER: Color = Color::Rgb(232, 188, 128);

fn areas(app: &App, size: Size) -> ([Rect; 2], usize) {
    let main = Rect::new(0, 1, size.width, size.height.saturating_sub(1));
    if app.project.secondary.is_none()
        || app.project.layout == LayoutMode::Full
        || size.width < 60
        || size.height < 16
    {
        return ([main, Rect::default()], 1);
    }
    match app.project.layout {
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
fn body(area: Rect) -> Rect {
    Rect::new(
        area.x,
        area.y + 1,
        area.width,
        area.height.saturating_sub(2),
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
            session.resize(rect.height, rect.width)?;
        }
    }
    Ok(())
}

pub fn draw(frame: &mut Frame, app: &mut App) {
    let bounds = frame.area();
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
    let project_width = (app.project.name.width() + 2)
        .min(22)
        .min(bounds.width.saturating_sub(width) as usize / 3) as u16;
    frame.render_widget(
        Paragraph::new(format!(" {} ", app.project.name)).style(Style::default().fg(FG).bold()),
        Rect::new(0, 0, project_width, 1),
    );
    draw_tabs(
        frame,
        app,
        bounds.width.saturating_sub(width),
        project_width,
    );
    let (regions, count) = areas(app, Size::new(bounds.width, bounds.height));
    if count == 2 {
        if app.project.layout == LayoutMode::Vertical {
            let x = regions[0].right();
            for y in regions[0].top()..regions[0].bottom() {
                frame.buffer_mut()[(x, y)].set_symbol("│").set_fg(BORDER);
            }
        } else {
            line_horizontal(frame.buffer_mut(), 0, regions[0].bottom(), bounds.width);
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
        draw_workspace(
            frame,
            &app.project.workspaces[index],
            area,
            pane_body,
            focused,
            app.modal.is_none(),
            app.project.repository.is_some(),
        );
        app.panes.push(Pane {
            workspace: index,
            body: pane_body,
        });
    }
    if let Some(notice) = &app.notice {
        frame.render_widget(
            Paragraph::new(notice.as_str()).style(Style::default().fg(AMBER).bg(BG)),
            Rect::new(0, bounds.height - 1, bounds.width, 1),
        );
    }
    if let Some(modal) = &app.modal {
        draw_modal(frame, app, modal);
    }
}

fn draw_tabs(frame: &mut Frame, app: &mut App, width: u16, offset: u16) {
    let focused = app.focused_index();
    let width_of = |index: usize| -> u16 {
        (app.project.workspaces[index].label().width() + 8).min(34) as u16
    };
    let mut start = focused;
    let mut used = width_of(focused);
    while start > 0 && used + width_of(start - 1) < width.saturating_sub(offset + 4) {
        start -= 1;
        used += width_of(start);
    }
    let mut x = offset;
    if start > 0 {
        frame.render_widget(
            Paragraph::new("‹").style(Style::default().fg(MUTED)),
            Rect::new(x, 0, 1, 1),
        );
        x += 2;
    }
    for index in start..app.project.workspaces.len() {
        let tab_width = width_of(index).min(width.saturating_sub(x));
        if tab_width < 5 {
            break;
        }
        let workspace = &app.project.workspaces[index];
        let (mark, color) = state_style(workspace.state());
        let selected = index == focused;
        let style = Style::default()
            .fg(if selected { ACCENT } else { MUTED })
            .bg(BG);
        let label = Line::from(vec![
            Span::raw(format!(" {} ", index + 1)),
            Span::raw(workspace.label()),
            Span::styled(format!(" {mark} "), Style::default().fg(color)),
        ]);
        let rect = Rect::new(x, 0, tab_width, 1);
        frame.render_widget(Paragraph::new(label).style(style), rect);
        app.tab_hits.push((rect, index));
        x += tab_width;
        if x >= width.saturating_sub(2) {
            break;
        }
    }
}

fn draw_workspace(
    frame: &mut Frame,
    workspace: &Workspace,
    area: Rect,
    body: Rect,
    focused: bool,
    show_cursor: bool,
    is_git: bool,
) {
    if area.height < 3 || area.width < 3 {
        return;
    }
    let mut header = vec![Span::styled(
        format!(" {}{} ", if focused { "› " } else { "" }, workspace.label()),
        Style::default()
            .fg(if focused { ACCENT } else { FG })
            .bold(),
    )];
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
    frame.render_widget(
        Paragraph::new(Line::from(header)),
        Rect::new(area.x, area.y, area.width, 1),
    );
    if let Some(session) = workspace.sessions.get(workspace.selected) {
        let parser = session.parser.lock();
        // Clear while holding the parser lock: output after this frame must wake us.
        session.dirty.store(false, Ordering::Release);
        let screen = parser.screen();
        for row in 0..body.height {
            for col in 0..body.width {
                let Some(cell) = screen.cell(row, col) else {
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
                target.set_style(style);
            }
        }
        if focused && show_cursor && !screen.hide_cursor() && screen.scrollback() == 0 {
            let (row, col) = screen.cursor_position();
            if row < body.height && col < body.width {
                frame.set_cursor_position((body.x + col, body.y + row));
            }
        }
        if session.state == AgentState::Exited && body.height > 0 {
            frame.render_widget(
                Paragraph::new(format!(
                    "Session exited ({}). F1 → ask to start another.",
                    session.message.as_deref().unwrap_or("finished")
                ))
                .style(Style::default().fg(AMBER).bg(SURFACE)),
                Rect::new(body.x, body.bottom() - 1, body.width, 1),
            );
        }
    } else {
        frame.render_widget(
            Paragraph::new("No session open. F1 → ask to start OMP, OpenCode, or a shell.")
                .style(Style::default().fg(MUTED))
                .wrap(Wrap { trim: false }),
            body,
        );
    }
    let mut environment = Vec::new();
    if !is_git {
        environment.push(Span::styled(
            format!(
                " terminal only · {} · F1 / F2 open project ",
                workspace.tree.path.display()
            ),
            Style::default().fg(MUTED),
        ));
    } else if workspace.pending {
        environment.push(Span::styled(
            " environment: updating... ",
            Style::default().fg(AMBER),
        ));
    } else if let Some(env) = &workspace.environment {
        if env.services.is_empty() {
            environment.push(Span::styled(" stack stopped ", Style::default().fg(MUTED)));
        }
        for service in &env.services {
            let healthy = service.state == "running" && service.health != "unhealthy";
            environment.push(Span::styled(
                format!(" {} {}", if healthy { "+" } else { "!" }, service.name),
                Style::default().fg(if healthy { GREEN } else { AMBER }),
            ));
            for port in &service.ports {
                environment.push(Span::styled(
                    format!(" :{}", port.host),
                    Style::default().fg(ACCENT),
                ));
            }
        }
    } else {
        environment.push(Span::styled(
            " environment: not checked ",
            Style::default().fg(MUTED),
        ));
    }
    frame.render_widget(
        Paragraph::new(Line::from(environment)).style(Style::default().bg(BG)),
        Rect::new(area.x, area.bottom() - 1, area.width, 1),
    );
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
fn line_horizontal(buffer: &mut Buffer, x: u16, y: u16, width: u16) {
    for column in x..x + width {
        buffer[(column, y)].set_symbol("─").set_fg(BORDER);
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
    if bounds.width < 30 || bounds.height < 14 {
        frame.render_widget(
            Paragraph::new("crit — enlarge terminal to at least 30×14. Esc quits.")
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
    let inner = modal_area(frame, " new terminal / open project ", 22);
    project_folder(frame, inner, picker);
    frame.render_widget(
        Paragraph::new(notice).style(Style::default().fg(AMBER).bg(BG)),
        Rect::new(0, bounds.height - 1, bounds.width, 1),
    );
}

fn project_folder(frame: &mut Frame, inner: Rect, picker: &ProjectPicker) {
    if inner.height < 8 || inner.width < 3 {
        frame.render_widget(
            Paragraph::new("Enlarge the terminal to choose a project."),
            inner,
        );
        return;
    }
    frame.render_widget(
        Paragraph::new(if picker.terminal_path.is_some() {
            "New terminal, or filter projects / enter a Git folder:"
        } else {
            "Type to filter projects, or enter a Git folder path:"
        }),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );
    let cursor = (picker.input.width() + 2).min(u16::MAX as usize) as u16;
    let scroll = cursor.saturating_sub(inner.width - 1);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::raw("› "),
            Span::raw(picker.input.as_str()),
        ]))
        .scroll((0, scroll)),
        Rect::new(inner.x, inner.y + 2, inner.width, 1),
    );
    if !picker.pending {
        frame.set_cursor_position((inner.x + cursor - scroll, inner.y + 2));
    }
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
        Rect::new(inner.x, inner.y + 4, inner.width, 1),
    );
    choice_list(
        frame,
        Rect::new(inner.x, inner.y + 5, inner.width, inner.height - 8),
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
        Paragraph::new("↑/↓ select · Enter open · Esc cancel · Ctrl+U clear")
            .style(Style::default().fg(MUTED)),
        Rect::new(inner.x, inner.bottom() - 2, inner.width, 1),
    );
    frame.render_widget(
        Paragraph::new(format!(
            "Paths: absolute, ~/ or relative to {}",
            picker.base.display()
        ))
        .style(Style::default().fg(MUTED)),
        Rect::new(inner.x, inner.bottom() - 1, inner.width, 1),
    );
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
    let cursor = (dialog.input.width() + 2).min(u16::MAX as usize) as u16;
    let scroll = cursor.saturating_sub(inner.width - 1);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::raw("› "),
            Span::raw(dialog.input.as_str()),
        ]))
        .scroll((0, scroll)),
        Rect::new(inner.x, inner.y + 9, inner.width, 1),
    );
    if !dialog.pending {
        frame.set_cursor_position((inner.x + cursor - scroll, inner.y + 9));
    }
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

fn modal_area(frame: &mut Frame, title: &'static str, desired_height: u16) -> Rect {
    let bounds = frame.area();
    let width = bounds.width.saturating_sub(4).min(76);
    let height = bounds.height.saturating_sub(2).min(desired_height);
    let area = Rect::new(
        (bounds.width - width) / 2,
        (bounds.height - height) / 2,
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
        Modal::OpenProject(_) => (" open project ", 22),
        Modal::SwitchProject { .. } => (
            " switch project ",
            (app.saved_projects.len() as u16 + 4).min(20),
        ),
        Modal::NewWorktree { .. } => (" new worktree tab ", 8),
        Modal::NewSession { .. } => (" new session ", 8),
        Modal::OpenWorktree { trees, .. } => {
            (" open existing worktree ", (trees.len() as u16 + 4).min(20))
        }
        Modal::Split { .. } => (
            " choose second worktree ",
            (app.project.workspaces.len() as u16 + 4).min(20),
        ),
        Modal::Preview { urls, .. } => (" open endpoint ", (urls.len() as u16 + 4).min(20)),
        Modal::Confirm(_) => (" confirm ", 9),
    };
    let inner = modal_area(frame, title, desired_height);
    match modal {
        Modal::Intent(dialog) => intent_dialog(frame, inner, dialog),
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
        Modal::NewWorktree { input } => {
            let text = format!(
                "Branch name (existing or new):\n\n› {input}\n\nEnter creates checkout + starts its Compose stack.\nEsc cancels. Base: {}",
                app.focused().tree.branch
            );
            frame.render_widget(Paragraph::new(text).wrap(Wrap { trim: false }), inner);
            let x = inner.x + 2 + input.width() as u16;
            if x < inner.right() && inner.height > 2 {
                frame.set_cursor_position((x, inner.y + 2));
            }
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
                    "Exit Crit and terminate its agent/shell sessions?\n\nDocker environments keep running; checkouts remain."
                }
                Confirm::CloseWorktree if app.project.repository.is_none() => {
                    "Close this terminal tab and terminate its sessions?\n\nOther open projects and sessions remain."
                }
                Confirm::CloseWorktree => {
                    "Close this tab and terminate its sessions?\n\nCheckout, branch, and Docker environment remain."
                }
                Confirm::CloseSession => {
                    "Terminate the focused agent/shell session?\n\nOther sessions and Docker services remain."
                }
                Confirm::StopEnvironment => {
                    "Stop this worktree's Docker environment?\n\nOther worktrees are unaffected. Volumes remain."
                }
                Confirm::RemoveWorktree => {
                    "Remove this worktree's checkout?\n\nDirty/main/locked checkouts are refused.\nStop its Docker environment first. Branch remains."
                }
            };
            frame.render_widget(
                Paragraph::new(format!("{message}\n\ny confirm · n / Esc cancel"))
                    .wrap(Wrap { trim: false }),
                inner,
            );
        }
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
