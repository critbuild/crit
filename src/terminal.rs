use crate::{
    event::Event,
    integration::{self, AgentState},
    keys, kitty,
};
use anyhow::{Context, Result};
use crossterm::event::{KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use parking_lot::Mutex;
use portable_pty::{CommandBuilder, MasterPty, PtySize};
use std::{
    io::{Read, Write},
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::SyncSender,
    },
};

type Writer = Arc<Mutex<Box<dyn Write + Send>>>;
pub type Parser = vt100::Parser<Responses>;

pub struct Session {
    pub id: u64,
    pub kind: String,
    pub state: AgentState,
    pub message: Option<String>,
    pub parser: Arc<Mutex<Parser>>,
    pub dirty: Arc<AtomicBool>,
    pub alive: Arc<AtomicBool>,
    master: Box<dyn MasterPty + Send>,
    writer: Writer,
    killer: Box<dyn portable_pty::ChildKiller + Send + Sync>,
    pid: Option<u32>,
    size: (u16, u16),
}

impl Session {
    pub fn spawn(
        id: u64,
        kind: &str,
        cwd: &Path,
        socket: &Path,
        tx: SyncSender<Event>,
        rows: u16,
        cols: u16,
        command: Option<(String, Vec<String>)>,
    ) -> Result<Self> {
        let (program, args, extra_env) = if let Some((program, args)) = command {
            (program, args, Vec::new())
        } else if kind == "shell" {
            (
                std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into()),
                vec!["-i".into()],
                Vec::new(),
            )
        } else {
            integration::command(kind, id, socket)?
        };
        let rows = rows.max(1);
        let cols = cols.max(1);
        let pair = portable_pty::native_pty_system().openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        let mut builder = CommandBuilder::new(&program);
        builder.args(&args);
        builder.cwd(cwd);
        for (name, _) in std::env::vars_os() {
            let name_str = name.to_string_lossy();
            if name_str.starts_with("HERDR_")
                || name_str.starts_with("CRIT_")
                || matches!(
                    name_str.as_ref(),
                    "OMPCODE"
                        | "PI_SESSION_FILE"
                        | "PI_ARTIFACTS_DIR"
                        | "PI_TOOL_BRIDGE_URL"
                        | "PI_TOOL_BRIDGE_SESSION"
                        | "PI_EVAL_LOCAL_ROOTS"
                        | "KITTY_WINDOW_ID"
                        | "KITTY_LISTEN_ON"
                        | "WINDOWID"
                        | "TMUX"
                        | "TMUX_PANE"
                )
            {
                builder.env_remove(name);
            }
        }
        builder.env("TERM", "xterm-256color");
        builder.env("TERM_PROGRAM", "crit");
        builder.env("COLORTERM", "truecolor");
        builder.env("CRIT_WORKTREE", cwd);
        for (key, value) in extra_env {
            builder.env(key, value);
        }
        let mut child = pair
            .slave
            .spawn_command(builder)
            .with_context(|| format!("launching {program} in {}", cwd.display()))?;
        drop(pair.slave);
        let pid = child.process_id();
        let killer = child.clone_killer();
        let writer = Arc::new(Mutex::new(pair.master.take_writer()?));
        let mut reader = pair.master.try_clone_reader()?;
        let parser = Arc::new(Mutex::new(Parser::new_with_callbacks(
            rows,
            cols,
            512,
            Responses::new(writer.clone()),
        )));
        let dirty = Arc::new(AtomicBool::new(true));
        let alive = Arc::new(AtomicBool::new(true));
        let reader_parser = parser.clone();
        let reader_dirty = dirty.clone();
        let reader_alive = alive.clone();
        std::thread::Builder::new()
            .name(format!("pty-{id}"))
            .spawn(move || {
                let mut buffer = [0_u8; 8192];
                loop {
                    match reader.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(count) => {
                            reader_parser.lock().process(&buffer[..count]);
                            if !reader_dirty.swap(true, Ordering::AcqRel)
                                && tx.try_send(Event::Dirty(id)).is_err()
                            {
                                reader_dirty.store(false, Ordering::Release);
                            }
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(error) => {
                            if error.raw_os_error() != Some(libc::EIO) {
                                let _ = tx.send(Event::Error(format!("PTY {id}: {error}")));
                            }
                            break;
                        }
                    }
                }
                let code = match child.wait() {
                    Ok(status) => status.to_string(),
                    Err(error) => error.to_string(),
                };
                reader_alive.store(false, Ordering::Release);
                let _ = tx.send(Event::Exited { session: id, code });
            })?;
        Ok(Self {
            id,
            kind: kind.into(),
            state: if kind == "shell" {
                AgentState::Idle
            } else {
                AgentState::Unknown
            },
            message: None,
            parser,
            dirty,
            alive,
            master: pair.master,
            writer,
            killer,
            pid,
            size: (rows, cols),
        })
    }

    pub fn resize(&mut self, rows: u16, cols: u16) -> Result<()> {
        let size = (rows.max(1), cols.max(1));
        if self.size != size {
            self.master.resize(PtySize {
                rows: size.0,
                cols: size.1,
                pixel_width: 0,
                pixel_height: 0,
            })?;
            self.parser.lock().screen_mut().set_size(size.0, size.1);
            self.size = size;
            self.dirty.store(true, Ordering::Release);
        }
        Ok(())
    }

    pub fn key(&self, key: KeyEvent) -> Result<()> {
        if !self.alive.load(Ordering::Acquire) {
            return Ok(());
        }
        let (application_cursor, keyboard) = {
            let mut parser = self.parser.lock();
            parser.screen_mut().set_scrollback(0);
            (
                parser.screen().application_cursor(),
                parser.callbacks().keyboard_flags() != 0,
            )
        };
        let mut writer = self.writer.lock();
        keys::write_key(&mut *writer, key, application_cursor, keyboard)?;
        writer.flush()?;
        Ok(())
    }

    pub fn paste(&self, text: &str) -> Result<()> {
        let bracketed = self.parser.lock().screen().bracketed_paste();
        let mut writer = self.writer.lock();
        if bracketed {
            writer.write_all(b"\x1b[200~")?;
        }
        // Strip bracketed-paste terminators so pasted text cannot escape the paste boundary.
        let mut remainder = text;
        while let Some(index) = remainder.find('\u{1b}') {
            writer.write_all(remainder[..index].as_bytes())?;
            remainder = &remainder[index + 1..];
        }
        writer.write_all(remainder.as_bytes())?;
        if bracketed {
            writer.write_all(b"\x1b[201~")?;
        }
        writer.flush()?;
        Ok(())
    }

    pub fn mouse(&self, event: MouseEvent, column: u16, row: u16) -> Result<()> {
        let mut parser = self.parser.lock();
        if parser.screen().mouse_protocol_mode() == vt100::MouseProtocolMode::None {
            let current = parser.screen().scrollback();
            match event.kind {
                MouseEventKind::ScrollUp => parser
                    .screen_mut()
                    .set_scrollback(current.saturating_add(3)),
                MouseEventKind::ScrollDown => parser
                    .screen_mut()
                    .set_scrollback(current.saturating_sub(3)),
                _ => {}
            }
            return Ok(());
        }
        let (button, suffix) = match event.kind {
            MouseEventKind::Down(button) => (
                match button {
                    MouseButton::Left => 0,
                    MouseButton::Middle => 1,
                    MouseButton::Right => 2,
                },
                'M',
            ),
            MouseEventKind::Up(button) => (
                match button {
                    MouseButton::Left => 0,
                    MouseButton::Middle => 1,
                    MouseButton::Right => 2,
                },
                'm',
            ),
            MouseEventKind::Drag(button) => (
                32 + match button {
                    MouseButton::Left => 0,
                    MouseButton::Middle => 1,
                    MouseButton::Right => 2,
                },
                'M',
            ),
            MouseEventKind::Moved => return Ok(()),
            MouseEventKind::ScrollUp => (64, 'M'),
            MouseEventKind::ScrollDown => (65, 'M'),
            _ => return Ok(()),
        };
        let button = button
            + 4 * u16::from(event.modifiers.contains(KeyModifiers::SHIFT))
            + 8 * u16::from(event.modifiers.contains(KeyModifiers::ALT))
            + 16 * u16::from(event.modifiers.contains(KeyModifiers::CONTROL));
        let encoding = parser.screen().mouse_protocol_encoding();
        drop(parser);
        let mut writer = self.writer.lock();
        if encoding == vt100::MouseProtocolEncoding::Sgr {
            write!(writer, "\x1b[<{button};{};{}{suffix}", column + 1, row + 1)?;
        } else if column < 223 && row < 223 {
            writer.write_all(&[
                27,
                b'[',
                b'M',
                if suffix == 'm' { 35 } else { button as u8 + 32 },
                column as u8 + 33,
                row as u8 + 33,
            ])?;
        }
        writer.flush()?;
        Ok(())
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if !self.alive.load(Ordering::Acquire) {
            return;
        }
        // Only signal process groups belonging to this PTY's own session.
        if let Some(pid) = self.pid {
            unsafe {
                if let Some(foreground) = self.master.process_group_leader() {
                    if foreground > 0 && libc::getsid(foreground) == pid as i32 {
                        libc::kill(-foreground, libc::SIGHUP);
                    }
                }
                if libc::getpgid(pid as i32) == pid as i32 {
                    libc::kill(-(pid as i32), libc::SIGHUP);
                }
            }
        }
        let _ = self.killer.kill();
    }
}

pub struct Responses {
    writer: Writer,
    keyboard: [u16; 16],
    keyboard_depth: usize,
}

impl Responses {
    fn new(writer: Writer) -> Self {
        Self {
            writer,
            keyboard: [0; 16],
            keyboard_depth: 0,
        }
    }
    fn keyboard_flags(&self) -> u16 {
        self.keyboard[self.keyboard_depth]
    }
}

impl vt100::Callbacks for Responses {
    fn unhandled_csi(
        &mut self,
        screen: &mut vt100::Screen,
        first: Option<u8>,
        second: Option<u8>,
        params: &[&[u16]],
        c: char,
    ) {
        let p = params.first().and_then(|p| p.first()).copied().unwrap_or(0);
        let mut writer = self.writer.lock();
        let result = match (first, second, c) {
            (None, None, 'n') if p == 6 => {
                let (row, column) = screen.cursor_position();
                write!(writer, "\x1b[{};{}R", row + 1, column + 1)
            }
            (None, None, 'n') if p == 5 => writer.write_all(b"\x1b[0n"),
            (None, None, 'c') => writer.write_all(b"\x1b[?62;22c"),
            (Some(b'>'), None, 'c') => writer.write_all(b"\x1b[>0;100;0c"),
            (Some(b'>'), None, 'q') => writer.write_all(b"\x1bP>|crit 0.1\x1b\\"),
            (Some(b'?'), None, 'u') => write!(writer, "\x1b[?{}u", self.keyboard_flags()),
            (Some(b'>'), None, 'u') => {
                if self.keyboard_depth < self.keyboard.len() - 1 {
                    self.keyboard_depth += 1;
                }
                self.keyboard[self.keyboard_depth] = p & 1;
                Ok(())
            }
            (Some(b'<'), None, 'u') => {
                self.keyboard_depth = self.keyboard_depth.saturating_sub(usize::from(p.max(1)));
                Ok(())
            }
            (Some(b'='), None, 'u') => {
                let mode = params.get(1).and_then(|p| p.first()).copied().unwrap_or(1);
                let old = self.keyboard[self.keyboard_depth];
                self.keyboard[self.keyboard_depth] = match mode {
                    2 => old | (p & 1),
                    3 => old & !(p & 1),
                    _ => p & 1,
                };
                Ok(())
            }
            (Some(b'?'), Some(b'$'), 'p') => {
                let state = match p {
                    25 => {
                        if screen.hide_cursor() {
                            2
                        } else {
                            1
                        }
                    }
                    2004 => {
                        if screen.bracketed_paste() {
                            1
                        } else {
                            2
                        }
                    }
                    _ => 0,
                };
                write!(writer, "\x1b[?{p};{state}$y")
            }
            (None, None, 't') if p == 18 => {
                let (rows, cols) = screen.size();
                write!(writer, "\x1b[8;{rows};{cols}t")
            }
            _ => Ok(()),
        };
        if result.is_ok() {
            let _ = writer.flush();
        }
    }

    fn unhandled_osc(&mut self, _: &mut vt100::Screen, params: &[&[u8]]) {
        let background = match params {
            [b"11", b"?"] => {
                let Some(rgb) = kitty::background() else {
                    return;
                };
                Some(rgb.map(|component| u16::from(component) * 257))
            }
            [b"10", b"?"] => None,
            _ => return,
        };
        let mut writer = self.writer.lock();
        if let Some([red, green, blue]) = background {
            let _ = write!(writer, "\x1b]11;rgb:{red:04x}/{green:04x}/{blue:04x}\x1b\\");
        } else {
            let _ = writer.write_all(b"\x1b]10;rgb:e0e0/e4e4/e8e8\x1b\\");
        }
        let _ = writer.flush();
    }
}
