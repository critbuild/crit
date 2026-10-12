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
    pub resume: Option<String>,
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
        resume: Option<&str>,
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
            integration::command(kind, id, socket, resume)?
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
            resume: resume.map(str::to_owned),
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
            // SIGWINCH can trigger an immediate repaint. Do not let the reader
            // process it against the old grid while the kernel has the new size.
            let mut parser = self.parser.lock();
            self.master.resize(PtySize {
                rows: size.0,
                cols: size.1,
                pixel_width: 0,
                pixel_height: 0,
            })?;
            parser.screen_mut().set_size(size.0, size.1);
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
                parser
                    .callbacks()
                    .keyboard_flags(parser.screen().alternate_screen())
                    != 0,
            )
        };
        let mut writer = self.writer.lock();
        keys::write_key(&mut *writer, key, application_cursor, keyboard)?;
        writer.flush()?;
        Ok(())
    }

    pub fn paste(&self, text: &str) -> Result<()> {
        if !self.alive.load(Ordering::Acquire) {
            return Ok(());
        }
        let bracketed = {
            let mut parser = self.parser.lock();
            parser.screen_mut().set_scrollback(0);
            parser.screen().bracketed_paste()
        };
        let mut writer = self.writer.lock();
        if bracketed {
            writer.write_all(b"\x1b[200~")?;
        }
        // Strip escape bytes so pasted text cannot escape the paste boundary.
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

    pub fn scroll_to(&self, offset: usize) {
        self.parser.lock().screen_mut().set_scrollback(offset);
    }

    pub fn mouse(&self, event: MouseEvent, column: u16, row: u16) -> Result<()> {
        let mut parser = self.parser.lock();
        if parser.screen().mouse_protocol_mode() == vt100::MouseProtocolMode::None
            || (event.modifiers.contains(KeyModifiers::SHIFT)
                && matches!(
                    event.kind,
                    MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
                ))
        {
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
        let mode = parser.screen().mouse_protocol_mode();
        let encoding = parser.screen().mouse_protocol_encoding();
        drop(parser);
        if !self.alive.load(Ordering::Acquire) {
            return Ok(());
        }
        let mut writer = self.writer.lock();
        write_mouse(&mut *writer, event, column, row, mode, encoding)?;
        writer.flush()?;
        Ok(())
    }
}

fn write_mouse(
    writer: &mut impl Write,
    event: MouseEvent,
    column: u16,
    row: u16,
    mode: vt100::MouseProtocolMode,
    encoding: vt100::MouseProtocolEncoding,
) -> std::io::Result<()> {
    use vt100::{MouseProtocolEncoding as Encoding, MouseProtocolMode as Mode};
    let button_number = |button| match button {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
    };
    if mode == Mode::None {
        return Ok(());
    }
    let (button, released) = match event.kind {
        MouseEventKind::Down(button) => (button_number(button), false),
        MouseEventKind::Up(button) if mode != Mode::Press => (button_number(button), true),
        MouseEventKind::Drag(button) if matches!(mode, Mode::ButtonMotion | Mode::AnyMotion) => {
            (32 + button_number(button), false)
        }
        MouseEventKind::Moved if mode == Mode::AnyMotion => (35, false),
        MouseEventKind::ScrollUp => (64, false),
        MouseEventKind::ScrollDown => (65, false),
        MouseEventKind::ScrollLeft => (66, false),
        MouseEventKind::ScrollRight => (67, false),
        _ => return Ok(()),
    };
    let modifiers = if mode == Mode::Press {
        0
    } else {
        4 * u16::from(event.modifiers.contains(KeyModifiers::SHIFT))
            + 8 * u16::from(event.modifiers.contains(KeyModifiers::ALT))
            + 16 * u16::from(event.modifiers.contains(KeyModifiers::CONTROL))
    };
    if encoding == Encoding::Sgr {
        let button = button + modifiers;
        let suffix = if released { 'm' } else { 'M' };
        return write!(
            writer,
            "\x1b[<{button};{};{}{suffix}",
            u32::from(column) + 1,
            u32::from(row) + 1
        );
    }
    let button = if released { 3 } else { button } + modifiers + 32;
    match encoding {
        Encoding::Default if column < 223 && row < 223 => writer.write_all(&[
            27,
            b'[',
            b'M',
            button as u8,
            column as u8 + 33,
            row as u8 + 33,
        ]),
        Encoding::Utf8 if column < 2015 && row < 2015 => {
            writer.write_all(b"\x1b[M")?;
            for value in [button, column + 33, row + 33] {
                let mut bytes = [0; 4];
                writer.write_all(
                    char::from_u32(u32::from(value))
                        .unwrap()
                        .encode_utf8(&mut bytes)
                        .as_bytes(),
                )?;
            }
            Ok(())
        }
        _ => Ok(()),
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
    keyboard: [KeyboardModes; 2],
}

impl Responses {
    fn new(writer: Writer) -> Self {
        Self {
            writer,
            keyboard: [KeyboardModes::default(); 2],
        }
    }
    fn keyboard_flags(&self, alternate_screen: bool) -> u16 {
        self.keyboard[usize::from(alternate_screen)].flags
    }
}

#[derive(Clone, Copy, Default)]
struct KeyboardModes {
    flags: u16,
    stack: [u16; 16],
    depth: usize,
}

impl KeyboardModes {
    fn push(&mut self, flags: u16) {
        if self.depth == self.stack.len() {
            self.stack.copy_within(1.., 0);
            self.depth -= 1;
        }
        self.stack[self.depth] = self.flags;
        self.depth += 1;
        self.flags = flags;
    }

    fn pop(&mut self, count: usize) {
        if count > self.depth {
            self.depth = 0;
            self.flags = 0;
        } else {
            self.depth -= count;
            self.flags = self.stack[self.depth];
        }
    }
}

impl vt100::Callbacks for Responses {
    fn copy_to_clipboard(&mut self, _: &mut vt100::Screen, ty: &[u8], data: &[u8]) {
        // Clipboard queries intentionally keep the trait's no-op implementation.
        let _ = crate::clipboard::copy_encoded(ty, data);
    }

    fn unhandled_csi(
        &mut self,
        screen: &mut vt100::Screen,
        first: Option<u8>,
        second: Option<u8>,
        params: &[&[u16]],
        c: char,
    ) {
        let p = params.first().and_then(|p| p.first()).copied().unwrap_or(0);
        let keyboard = &mut self.keyboard[usize::from(screen.alternate_screen())];
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
            (Some(b'?'), None, 'u') => write!(writer, "\x1b[?{}u", keyboard.flags),
            (Some(b'>'), None, 'u') => {
                keyboard.push(p & 1);
                Ok(())
            }
            (Some(b'<'), None, 'u') => {
                keyboard.pop(usize::from(p.max(1)));
                Ok(())
            }
            (Some(b'='), None, 'u') => {
                let mode = params.get(1).and_then(|p| p.first()).copied().unwrap_or(1);
                keyboard.flags = match mode {
                    2 => keyboard.flags | (p & 1),
                    3 => keyboard.flags & !(p & 1),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<u8>>>);

    impl Write for Capture {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn response_parser() -> (Parser, Capture) {
        let capture = Capture::default();
        let writer: Writer = Arc::new(Mutex::new(Box::new(capture.clone())));
        (
            Parser::new_with_callbacks(24, 80, 32, Responses::new(writer)),
            capture,
        )
    }

    #[test]
    fn keyboard_protocol_stacks_follow_main_and_alternate_screens() {
        let (mut parser, capture) = response_parser();
        parser.process(
            b"\x1b[=1u\x1b[?u\
              \x1b[?1049h\x1b[?u\x1b[>1u\x1b[?u\
              \x1b[?1049l\x1b[?u\x1b[<u\x1b[?u\
              \x1b[?1049h\x1b[?u\x1b[<u\x1b[?u",
        );
        assert_eq!(
            capture.0.lock().as_slice(),
            b"\x1b[?1u\x1b[?0u\x1b[?1u\x1b[?1u\x1b[?0u\x1b[?1u\x1b[?0u"
        );
    }

    #[test]
    fn keyboard_stack_overflow_evicts_oldest_saved_mode() {
        let (mut parser, capture) = response_parser();
        parser.process(b"\x1b[=1u\x1b[>0u");
        for _ in 0..16 {
            parser.process(b"\x1b[>1u");
        }
        parser.process(b"\x1b[<16u\x1b[?u");
        assert_eq!(capture.0.lock().as_slice(), b"\x1b[?0u");
    }

    #[test]
    fn keyboard_stack_underflow_resets_flags() {
        let (mut parser, capture) = response_parser();
        parser.process(b"\x1b[=1u\x1b[<u\x1b[?u");
        assert_eq!(capture.0.lock().as_slice(), b"\x1b[?0u");
    }

    #[test]
    fn mouse_tracking_modes_filter_releases_and_motion() {
        for (mode, expected) in [
            (9, "\x1b[<0;11;21M"),
            (1000, "\x1b[<4;11;21M\x1b[<4;11;21m"),
            (1002, "\x1b[<4;11;21M\x1b[<4;11;21m\x1b[<36;11;21M"),
            (
                1003,
                "\x1b[<4;11;21M\x1b[<4;11;21m\x1b[<36;11;21M\x1b[<39;11;21M",
            ),
        ] {
            let (mut parser, _) = response_parser();
            parser.process(format!("\x1b[?{mode}h\x1b[?1006h").as_bytes());
            let mut bytes = Vec::new();
            for kind in [
                MouseEventKind::Down(MouseButton::Left),
                MouseEventKind::Up(MouseButton::Left),
                MouseEventKind::Drag(MouseButton::Left),
                MouseEventKind::Moved,
            ] {
                write_mouse(
                    &mut bytes,
                    MouseEvent {
                        kind,
                        column: 10,
                        row: 20,
                        modifiers: KeyModifiers::SHIFT,
                    },
                    10,
                    20,
                    parser.screen().mouse_protocol_mode(),
                    parser.screen().mouse_protocol_encoding(),
                )
                .unwrap();
            }
            assert_eq!(bytes, expected.as_bytes(), "tracking mode {mode}");
        }
    }

    #[test]
    fn utf8_mouse_coordinates_and_legacy_release_modifiers_are_preserved() {
        let (mut parser, _) = response_parser();
        parser.process(b"\x1b[?1000h\x1b[?1005h");
        let event = MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: 300,
            row: 120,
            modifiers: KeyModifiers::CONTROL,
        };
        let mut bytes = Vec::new();
        write_mouse(
            &mut bytes,
            event,
            300,
            120,
            parser.screen().mouse_protocol_mode(),
            parser.screen().mouse_protocol_encoding(),
        )
        .unwrap();
        assert_eq!(bytes, "\x1b[M3\u{14d}\u{99}".as_bytes());
        parser.process(b"\x1b[?1005l");
        bytes.clear();
        write_mouse(
            &mut bytes,
            event,
            10,
            20,
            parser.screen().mouse_protocol_mode(),
            parser.screen().mouse_protocol_encoding(),
        )
        .unwrap();
        assert_eq!(bytes, b"\x1b[M3+5");
    }

    #[test]
    fn horizontal_wheel_events_reach_mouse_applications() {
        let mut bytes = Vec::new();
        for kind in [MouseEventKind::ScrollLeft, MouseEventKind::ScrollRight] {
            write_mouse(
                &mut bytes,
                MouseEvent {
                    kind,
                    column: 0,
                    row: 0,
                    modifiers: KeyModifiers::NONE,
                },
                0,
                0,
                vt100::MouseProtocolMode::PressRelease,
                vt100::MouseProtocolEncoding::Sgr,
            )
            .unwrap();
        }
        assert_eq!(bytes, b"\x1b[<66;1;1M\x1b[<67;1;1M");
    }

    #[test]
    fn paste_returns_scrollback_to_live_terminal() {
        let directory = tempfile::tempdir().unwrap();
        let (tx, _rx) = std::sync::mpsc::sync_channel(32);
        let session = Session::spawn(
            1,
            "shell",
            None,
            directory.path(),
            &directory.path().join("unused.sock"),
            tx,
            3,
            20,
            Some((
                "/bin/sh".into(),
                vec!["-c".into(), "IFS= read -r line".into()],
            )),
        )
        .unwrap();
        {
            let mut parser = session.parser.lock();
            parser.process(b"one\r\ntwo\r\nthree\r\nfour\r\n");
            parser.screen_mut().set_scrollback(2);
            assert!(parser.screen().scrollback() > 0);
        }
        session.paste("pasted text").unwrap();
        assert_eq!(session.parser.lock().screen().scrollback(), 0);
    }
}
