use crate::{
    event::Event,
    integration::{self, AgentState},
    keys, kitty,
};
use anyhow::{Context, Result, bail};
use crossterm::event::{KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use parking_lot::Mutex;
use portable_pty::{CommandBuilder, MasterPty, PtySize};
use std::{
    io::{Read, Write},
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{Receiver, SyncSender, TrySendError},
    },
    time::{Duration, Instant},
};

/// Chunks queued for one pane's PTY before further input is refused. A keystroke
/// is one chunk and a scripted burst can outrun the writer thread, so this
/// matches the 4 KiB of typed-ahead input a PTY itself buffers.
const INPUT_QUEUE: usize = 4096;

/// Everything bound for a pane's PTY. One queue keeps typed input and terminal
/// replies in order, and only the writer thread ever blocks on the PTY.
enum Input {
    Bytes(Vec<u8>),
    /// An OSC 11 query; the writer thread asks Kitty so parsing never waits on it.
    Background,
}

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
    input: SyncSender<Input>,
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
        let rows = rows.max(2);
        let cols = cols.max(2);
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
        // These only need the master; taking them first means a failure here
        // cannot leave a started child behind.
        let writer = pair.master.take_writer()?;
        let mut reader = pair.master.try_clone_reader()?;
        let mut child = pair
            .slave
            .spawn_command(builder)
            .with_context(|| format!("launching {program} in {}", cwd.display()))?;
        drop(pair.slave);
        let pid = child.process_id();
        let killer = child.clone_killer();
        let (input, queue) = std::sync::mpsc::sync_channel(INPUT_QUEUE);
        let parser = Arc::new(Mutex::new(Parser::new_with_callbacks(
            rows,
            cols,
            512,
            Responses::new(input.clone()),
        )));
        let dirty = Arc::new(AtomicBool::new(true));
        let alive = Arc::new(AtomicBool::new(true));
        let reader_parser = parser.clone();
        let reader_dirty = dirty.clone();
        let wait_alive = alive.clone();
        let wait_tx = tx.clone();
        // The child is running; a thread that cannot start must not leave it unreaped.
        let abandon = |error: std::io::Error| {
            kill_and_reap(pid);
            error
        };
        std::thread::Builder::new()
            .name(format!("pty-write-{id}"))
            .spawn(move || write_pty(writer, queue))
            .map_err(abandon)?;
        std::thread::Builder::new()
            .name(format!("pty-{id}"))
            .spawn(move || {
                let mut buffer = [0_u8; 8192];
                loop {
                    match reader.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(count) => {
                            // Only the UI thread writes to the outer terminal, so
                            // hand it the copy once the parser lock is released.
                            let copy = {
                                let mut parser = reader_parser.lock();
                                parser.process(&buffer[..count]);
                                parser.callbacks_mut().clipboard.take()
                            };
                            if let Some((ty, data)) = copy {
                                let _ = tx.send(Event::Clipboard { ty, data });
                            }
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
            })
            .map_err(abandon)?;
        // A background job can hold the PTY open after the child has exited, so
        // the child is reaped here rather than when the reader sees EOF.
        std::thread::Builder::new()
            .name(format!("pty-wait-{id}"))
            .spawn(move || {
                let code = match child.wait() {
                    Ok(status) => status.to_string(),
                    Err(error) => error.to_string(),
                };
                wait_alive.store(false, Ordering::Release);
                let _ = wait_tx.send(Event::Exited { session: id, code });
            })
            .map_err(abandon)?;
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
            input,
            killer,
            pid,
            size: (rows, cols),
        })
    }

    pub fn resize(&mut self, rows: u16, cols: u16) -> Result<()> {
        let size = (rows.max(2), cols.max(2));
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
        let mut bytes = Vec::new();
        keys::write_key(&mut bytes, key, application_cursor, keyboard)?;
        self.send(bytes)
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
        self.send(paste_bytes(text, bracketed))
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
        let mut bytes = Vec::new();
        write_mouse(&mut bytes, event, column, row, mode, encoding)?;
        self.send(bytes)
    }

    /// Queue bytes for the PTY without waiting on the child. A full queue means
    /// the pane's program has stopped reading, so the input is refused.
    fn send(&self, bytes: Vec<u8>) -> Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        let count = bytes.len();
        match self.input.try_send(Input::Bytes(bytes)) {
            Err(TrySendError::Full(_)) => {
                bail!("this pane's program is not reading input; dropped {count} bytes")
            }
            // The writer thread only stops once the PTY is gone.
            Ok(()) | Err(TrySendError::Disconnected(_)) => Ok(()),
        }
    }
}

/// The bytes for one paste: bracketed when the app asked for it, with escape
/// bytes stripped so pasted text cannot escape the paste boundary.
fn paste_bytes(text: &str, bracketed: bool) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(text.len() + 12);
    if bracketed {
        bytes.extend_from_slice(b"\x1b[200~");
    }
    for segment in text.split('\u{1b}') {
        bytes.extend_from_slice(segment.as_bytes());
    }
    if bracketed {
        bytes.extend_from_slice(b"\x1b[201~");
    }
    bytes
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
        // X10 mode reports presses of buttons 1-3 only; a wheel step would read as a click.
        MouseEventKind::ScrollUp if mode != Mode::Press => (64, false),
        MouseEventKind::ScrollDown if mode != Mode::Press => (65, false),
        MouseEventKind::ScrollLeft if mode != Mode::Press => (66, false),
        MouseEventKind::ScrollRight if mode != Mode::Press => (67, false),
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
        let foreground = self.master.process_group_leader();
        if let Some(pid) = self.pid {
            signal_pty_groups(pid, foreground, libc::SIGHUP);
        }
        let _ = self.killer.kill();
        // SIGHUP is only a request, and a child that ignores it would outlive its
        // pane. Escalate from a detached thread so closing never waits; gating on
        // `alive` spares processes that already released the PTY.
        if let Some(pid) = self.pid {
            let alive = self.alive.clone();
            if let Ok(reaper) = std::thread::Builder::new()
                .name(format!("pty-reap-{}", self.id))
                .spawn(move || {
                    let deadline = Instant::now() + Duration::from_secs(2);
                    while alive.load(Ordering::Acquire) {
                        if Instant::now() >= deadline {
                            signal_pty_groups(pid, foreground, libc::SIGKILL);
                            unsafe { libc::kill(pid as i32, libc::SIGKILL) };
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(50));
                    }
                })
            {
                let mut reapers = REAPERS.lock();
                // Dropping a finished handle detaches it, so long sessions don't pile up.
                reapers.retain(|reaper| !reaper.is_finished());
                reapers.push(reaper);
            }
        }
    }
}

/// The SIGKILL escalation threads of dropped sessions, for `join_reapers`.
static REAPERS: Mutex<Vec<std::thread::JoinHandle<()>>> = Mutex::new(Vec::new());

/// Waits for the escalation threads, which take at most about 2 s. Crit calls this just
/// before it exits: a thread cut short by the exit would leave a child that ignores SIGHUP
/// running after Crit is gone.
pub fn join_reapers() {
    // Take the handles first, so the lock is not held while joining.
    let reapers = std::mem::take(&mut *REAPERS.lock());
    for reaper in reapers {
        let _ = reaper.join();
    }
}

/// Signal the PTY's foreground group and the child's own group, but only groups
/// that belong to the child's session.
fn signal_pty_groups(pid: u32, foreground: Option<libc::pid_t>, signal: libc::c_int) {
    unsafe {
        if let Some(foreground) = foreground
            && foreground > 0
            && libc::getsid(foreground) == pid as i32
        {
            libc::kill(-foreground, signal);
        }
        if libc::getpgid(pid as i32) == pid as i32 {
            libc::kill(-(pid as i32), signal);
        }
    }
}

/// Kill and collect a child that no waiter thread will ever reap.
fn kill_and_reap(pid: Option<u32>) {
    if let Some(pid) = pid {
        unsafe {
            libc::kill(pid as i32, libc::SIGKILL);
            libc::waitpid(pid as i32, std::ptr::null_mut(), 0);
        }
    }
}

/// Body of a session's writer thread, the only place that blocks on the PTY: a
/// child that stops reading stalls this thread, never the UI or the reader. It
/// is never joined. It ends when the queue closes or the PTY rejects a write,
/// and it alone drops the writer, whose drop writes to the PTY.
fn write_pty(mut writer: Box<dyn Write + Send>, queue: Receiver<Input>) {
    for input in queue {
        let written = match input {
            Input::Bytes(bytes) => writer.write_all(&bytes),
            Input::Background => {
                let Some(rgb) = kitty::background() else {
                    continue;
                };
                let [red, green, blue] = rgb.map(|component| u16::from(component) * 257);
                write!(writer, "\x1b]11;rgb:{red:04x}/{green:04x}/{blue:04x}\x1b\\")
            }
        };
        if written.and_then(|()| writer.flush()).is_err() {
            break;
        }
    }
}

pub struct Responses {
    input: SyncSender<Input>,
    keyboard: [KeyboardModes; 2],
    /// The latest OSC 52 copy since the reader last looked.
    clipboard: Option<(Vec<u8>, Vec<u8>)>,
}

impl Responses {
    fn new(input: SyncSender<Input>) -> Self {
        Self {
            input,
            keyboard: [KeyboardModes::default(); 2],
            clipboard: None,
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

// Parsing runs under the pane's parser lock, so nothing here may block: replies
// are queued with `try_send` and dropped when the child is not reading.
impl vt100::Callbacks for Responses {
    fn copy_to_clipboard(&mut self, _: &mut vt100::Screen, ty: &[u8], data: &[u8]) {
        // Clipboard queries intentionally keep the trait's no-op implementation.
        // Parsing must not write to stdout; the reader loop forwards the latest
        // copy once it has released the parser lock.
        self.clipboard = Some((ty.to_vec(), data.to_vec()));
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
        let reply = match (first, second, c) {
            (None, None, 'n') if p == 6 => {
                let (row, column) = screen.cursor_position();
                // a pending wrap leaves the cursor one past the last column
                let column = column.min(screen.size().1.saturating_sub(1));
                format!("\x1b[{};{}R", row + 1, column + 1).into_bytes()
            }
            (None, None, 'n') if p == 5 => b"\x1b[0n".to_vec(),
            (None, None, 'c') => b"\x1b[?62;22c".to_vec(),
            (Some(b'>'), None, 'c') => b"\x1b[>0;100;0c".to_vec(),
            (Some(b'>'), None, 'q') => b"\x1bP>|crit 0.1\x1b\\".to_vec(),
            (Some(b'?'), None, 'u') => format!("\x1b[?{}u", keyboard.flags).into_bytes(),
            (Some(b'>'), None, 'u') => {
                keyboard.push(p & 1);
                return;
            }
            (Some(b'<'), None, 'u') => {
                keyboard.pop(usize::from(p.max(1)));
                return;
            }
            (Some(b'='), None, 'u') => {
                let mode = params.get(1).and_then(|p| p.first()).copied().unwrap_or(1);
                keyboard.flags = match mode {
                    2 => keyboard.flags | (p & 1),
                    3 => keyboard.flags & !(p & 1),
                    _ => p & 1,
                };
                return;
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
                format!("\x1b[?{p};{state}$y").into_bytes()
            }
            (None, None, 't') if p == 18 => {
                let (rows, cols) = screen.size();
                format!("\x1b[8;{rows};{cols}t").into_bytes()
            }
            _ => return,
        };
        let _ = self.input.try_send(Input::Bytes(reply));
    }

    fn unhandled_osc(&mut self, _: &mut vt100::Screen, params: &[&[u8]]) {
        let input = match params {
            [b"11", b"?"] => Input::Background,
            [b"10", b"?"] => Input::Bytes(b"\x1b]10;rgb:e0e0/e4e4/e8e8\x1b\\".to_vec()),
            _ => return,
        };
        let _ = self.input.try_send(input);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response_parser() -> (Parser, Receiver<Input>) {
        let (input, queue) = std::sync::mpsc::sync_channel(INPUT_QUEUE);
        (
            Parser::new_with_callbacks(24, 80, 32, Responses::new(input)),
            queue,
        )
    }

    /// Everything the parser has queued for the child so far, in order.
    fn queued(queue: &Receiver<Input>) -> Vec<u8> {
        queue
            .try_iter()
            .flat_map(|input| match input {
                Input::Bytes(bytes) => bytes,
                Input::Background => Vec::new(),
            })
            .collect()
    }

    fn spawn_program(program: &str, args: &[&str], tx: SyncSender<Event>) -> Session {
        Session::spawn(
            1,
            "shell",
            None,
            Path::new("/"),
            Path::new("/unused.sock"),
            tx,
            24,
            80,
            Some((program.into(), args.iter().map(|&arg| arg.into()).collect())),
        )
        .unwrap()
    }

    #[test]
    fn keyboard_protocol_stacks_follow_main_and_alternate_screens() {
        let (mut parser, queue) = response_parser();
        parser.process(
            b"\x1b[=1u\x1b[?u\
              \x1b[?1049h\x1b[?u\x1b[>1u\x1b[?u\
              \x1b[?1049l\x1b[?u\x1b[<u\x1b[?u\
              \x1b[?1049h\x1b[?u\x1b[<u\x1b[?u",
        );
        assert_eq!(
            queued(&queue),
            b"\x1b[?1u\x1b[?0u\x1b[?1u\x1b[?1u\x1b[?0u\x1b[?1u\x1b[?0u"
        );
    }

    #[test]
    fn keyboard_stack_overflow_evicts_oldest_saved_mode() {
        let (mut parser, queue) = response_parser();
        parser.process(b"\x1b[=1u\x1b[>0u");
        for _ in 0..16 {
            parser.process(b"\x1b[>1u");
        }
        parser.process(b"\x1b[<16u\x1b[?u");
        assert_eq!(queued(&queue), b"\x1b[?0u");
    }

    #[test]
    fn keyboard_stack_underflow_resets_flags() {
        let (mut parser, queue) = response_parser();
        parser.process(b"\x1b[=1u\x1b[<u\x1b[?u");
        assert_eq!(queued(&queue), b"\x1b[?0u");
    }

    #[test]
    fn cpr_reports_last_column_while_wrap_pending() {
        let (mut parser, queue) = response_parser();
        parser.process("x".repeat(80).as_bytes());
        assert_eq!(parser.screen().cursor_position(), (0, 80));
        parser.process(b"\x1b[6n");
        assert_eq!(queued(&queue), b"\x1b[1;80R");
    }

    #[test]
    fn huge_insert_and_scroll_counts_return_promptly() {
        // the counts past the row or the scroll region change nothing, but
        // used to be looped over: ICH alone took over a second
        let run = |ops: &[u8]| {
            let mut parser = vt100::Parser::new(24, 80, 0);
            parser.process(b"top\r\nsome text\x1b[1;2H");
            let start = std::time::Instant::now();
            parser.process(ops);
            (parser.screen().contents_formatted(), start.elapsed())
        };
        let (expected, _) = run(b"\x1b[80@\x1b[24L\x1b[24T");
        let (screen, elapsed) = run(b"\x1b[65535@\x1b[65535L\x1b[65535T");
        assert_eq!(screen, expected);
        assert!(
            elapsed < std::time::Duration::from_millis(500),
            "{elapsed:?}"
        );
    }

    #[test]
    fn mouse_tracking_modes_filter_releases_and_motion() {
        for (mode, expected) in [
            (9, "\x1b[<0;11;21M"),
            (1000, "\x1b[<4;11;21M\x1b[<4;11;21m\x1b[<68;11;21M"),
            (
                1002,
                "\x1b[<4;11;21M\x1b[<4;11;21m\x1b[<36;11;21M\x1b[<68;11;21M",
            ),
            (
                1003,
                "\x1b[<4;11;21M\x1b[<4;11;21m\x1b[<36;11;21M\x1b[<39;11;21M\x1b[<68;11;21M",
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
                MouseEventKind::ScrollUp,
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

    #[test]
    fn replies_never_block_the_reader() {
        let (input, queue) = std::sync::mpsc::sync_channel(1);
        input.send(Input::Bytes(b"pending".to_vec())).unwrap();
        let mut parser = Parser::new_with_callbacks(24, 80, 32, Responses::new(input));
        let (done, finished) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            parser.process(b"\x1b[6n\x1b[?2026h\x1b]11;?\x07\x1b]10;?\x07");
            let _ = done.send(());
        });
        finished
            .recv_timeout(Duration::from_secs(5))
            .expect("parsing waited on a full reply queue");
        assert!(matches!(queue.try_recv(), Ok(Input::Bytes(bytes)) if bytes == b"pending"));
        assert!(queue.try_recv().is_err());
    }

    #[test]
    fn osc_colour_queries_are_queued_for_the_writer_thread() {
        let (mut parser, queue) = response_parser();
        parser.process(b"\x1b]11;?\x07\x1b]10;?\x07");
        assert!(matches!(queue.try_recv(), Ok(Input::Background)));
        assert!(matches!(
            queue.try_recv(),
            Ok(Input::Bytes(bytes)) if bytes == b"\x1b]10;rgb:e0e0/e4e4/e8e8\x1b\\"
        ));
        assert!(queue.try_recv().is_err());
    }

    #[test]
    fn child_osc_52_copy_is_held_for_the_reader_loop() {
        let (mut parser, _) = response_parser();
        parser.process(b"\x1b]52;c;Zm9v\x07");
        assert_eq!(
            parser.callbacks().clipboard,
            Some((b"c".to_vec(), b"Zm9v".to_vec()))
        );
    }

    #[test]
    fn child_osc_52_copy_reaches_the_ui_thread_as_an_event() {
        let (tx, events) = std::sync::mpsc::sync_channel(32);
        let _session = spawn_program("/bin/sh", &["-c", "printf '\\033]52;c;Zm9v\\007'"], tx);
        loop {
            match events.recv_timeout(Duration::from_secs(5)) {
                Ok(Event::Clipboard { ty, data }) => {
                    assert_eq!((ty, data), (b"c".to_vec(), b"Zm9v".to_vec()));
                    break;
                }
                Ok(_) => {}
                Err(_) => panic!("the OSC 52 copy was not forwarded as an event"),
            }
        }
    }

    #[test]
    fn paste_strips_escape_bytes_and_brackets_once() {
        assert_eq!(
            paste_bytes("a\x1b[201~b\u{1f980}\x1b", true),
            "\x1b[200~a[201~b\u{1f980}\x1b[201~".as_bytes()
        );
        assert_eq!(paste_bytes("plain\x1b", false), b"plain");
    }

    #[test]
    fn paste_into_non_reading_child_returns_promptly() {
        let (tx, _events) = std::sync::mpsc::sync_channel(32);
        let session = spawn_program("/bin/sleep", &["5"], tx);
        let (done, finished) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            // Far more than a PTY buffers, so the writer thread stalls on the first chunk.
            let text = "line of pasted text\n".repeat(10_000);
            let mut slowest = Duration::ZERO;
            for _ in 0..2 {
                let started = Instant::now();
                let _ = session.paste(&text);
                slowest = slowest.max(started.elapsed());
            }
            // Once the queue is full, input is refused instead of blocking.
            let refused = (0..INPUT_QUEUE + 2).find_map(|_| session.paste("x").err());
            let _ = done.send((session, slowest, refused.map(|error| error.to_string())));
        });
        let (_session, slowest, refused) = finished
            .recv_timeout(Duration::from_secs(5))
            .expect("paste waited on a child that is not reading");
        assert!(slowest < Duration::from_millis(500), "{slowest:?}");
        let refused = refused.expect("a full queue must refuse input");
        assert!(refused.contains("not reading input"), "{refused}");
    }

    #[test]
    fn exit_is_reported_while_background_job_holds_pty() {
        let (tx, events) = std::sync::mpsc::sync_channel(32);
        let session = spawn_program("/bin/sh", &["-c", "set -m; sleep 3 & exit 0"], tx);
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match events.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(Event::Exited { session: 1, .. }) => break,
                Ok(_) => {}
                Err(_) => panic!("the exit was not reported while a background job held the PTY"),
            }
        }
        assert!(!session.alive.load(Ordering::Acquire));
    }

    #[test]
    fn drop_kills_children_that_ignore_sighup() {
        let (tx, _events) = std::sync::mpsc::sync_channel(32);
        let session = spawn_program(
            "/bin/sh",
            &["-c", "trap '' HUP; echo ready; while :; do sleep 1; done"],
            tx,
        );
        // Dropping before the trap exists would let the first SIGHUP kill the shell.
        let deadline = Instant::now() + Duration::from_secs(5);
        while !session.parser.lock().screen().contents().contains("ready") {
            assert!(
                Instant::now() < deadline,
                "the child never installed its trap"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let proc = format!("/proc/{}", session.pid.unwrap());
        assert!(Path::new(&proc).exists());
        drop(session);
        // Crit joins the reapers just before it exits, so by then the child must be killed,
        // or be a zombie that the waiter thread has yet to collect.
        join_reapers();
        let state = || -> Option<char> {
            let stat = std::fs::read_to_string(format!("{proc}/stat")).ok()?;
            stat.rsplit_once(") ")?.1.chars().next()
        };
        let deadline = Instant::now() + Duration::from_millis(500);
        while !matches!(state(), None | Some('Z')) {
            assert!(
                Instant::now() < deadline,
                "the child survived the escalation: state {:?}",
                state()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn spawn_floors_degenerate_sizes() {
        let (tx, _events) = std::sync::mpsc::sync_channel(32);
        let mut session = Session::spawn(
            1,
            "shell",
            None,
            Path::new("/"),
            Path::new("/unused.sock"),
            tx,
            1,
            1,
            Some(("/bin/sleep".into(), vec!["5".into()])),
        )
        .unwrap();
        // vt100 panics on 1-row and 1-column grids, which would kill the reader thread.
        assert_eq!(session.parser.lock().screen().size(), (2, 2));
        session.resize(10, 20).unwrap();
        assert_eq!(session.parser.lock().screen().size(), (10, 20));
        session.resize(0, 1).unwrap();
        assert_eq!(session.parser.lock().screen().size(), (2, 2));
    }
}
