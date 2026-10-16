use crate::{
    event::Event,
    integration::{self, AgentState, Resume},
    keys, kitty,
    limits::Limiter,
    sandbox::Sandbox,
};
use anyhow::{Context, Result, bail};
use crossterm::event::{KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use parking_lot::Mutex;
use portable_pty::{CommandBuilder, MasterPty, PtySize};
use std::{
    io::{Read, Write},
    path::Path,
    sync::{
        Arc, LazyLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{Receiver, SyncSender, TrySendError},
    },
    time::{Duration, Instant},
};

/// Chunks queued for one pane's PTY before further input is refused. A keystroke
/// is one chunk and a scripted burst can outrun the writer thread, so this
/// matches the 4 KiB of typed-ahead input a PTY itself buffers.
const INPUT_QUEUE: usize = 4096;

/// How long an app's synchronized update (DEC mode 2026) holds its pane's frame. An app that
/// never ends its update must not freeze the pane; terminals bound the mode the same way.
const SYNC_HOLD: Duration = Duration::from_millis(100);

/// How long after a resize a pane's frame keeps waiting out its app's output bursts, each until
/// the app has been quiet for `RESIZE_QUIET`: an app settles in passes, and OMP re-renders
/// everything about 140 ms after the resize. An app that keeps writing must not freeze the pane.
const RESIZE_SETTLE_MAX: Duration = Duration::from_millis(400);

/// How long it waits for an app that has painted nothing since the resize. One that does not
/// repaint on a resize, like `sleep` or `cat`, is drawn with its grid cut, as Kitty shows it.
const RESIZE_SILENT_MAX: Duration = Duration::from_millis(80);

/// How long an app's output must have been quiet before its resized pane is drawn. An app
/// repaints in a burst: OMP follows a full re-render with a correction 17 ms later.
const RESIZE_QUIET: Duration = Duration::from_millis(30);

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
    /// A completed turn not yet acknowledged by selecting its tab. Not persisted.
    pub completed: bool,
    pub message: Option<String>,
    pub resume: Option<String>,
    pub parser: Arc<Mutex<Parser>>,
    pub dirty: Arc<AtomicBool>,
    pub alive: Arc<AtomicBool>,
    /// Set by the reader thread once text shows on the screen; the pane is blank until then.
    output: Arc<AtomicBool>,
    master: Box<dyn MasterPty + Send>,
    input: SyncSender<Input>,
    killer: Box<dyn portable_pty::ChildKiller + Send + Sync>,
    pid: Option<u32>,
    size: (u16, u16),
    /// The app's open synchronized update, shared with the parser's callbacks.
    sync: Arc<AtomicU64>,
    /// When the pane last changed size while it showed something, as `clock` stamps it; 0 while
    /// no resize waits for its app to repaint. Only the UI thread writes it, see `settling`.
    resized: AtomicU64,
    /// When the reader last processed output since that resize, as `clock` stamps it; 0: none.
    quiet_since: Arc<AtomicU64>,
}

impl Session {
    // Everything a launch needs, each used once; a parameter struct would only move the list.
    #[expect(clippy::too_many_arguments)]
    pub fn spawn(
        id: u64,
        kind: &str,
        resume: Option<Resume<'_>>,
        cwd: &Path,
        socket: &Path,
        tx: SyncSender<Event>,
        rows: u16,
        cols: u16,
        command: Option<(String, Vec<String>)>,
        limiter: Option<&Limiter>,
        sandbox: Option<&Sandbox>,
    ) -> Result<Self> {
        // Every pane keeps its scratch in its workspace's directory in the sandbox. One that
        // cannot be made is no reason to refuse the pane: it then uses Crit's own temp folder.
        let scratch = sandbox.and_then(|sandbox| sandbox.workspace(cwd).ok());
        let (program, args, extra_env) = if let Some((program, args)) = command {
            (program, args, Vec::new())
        } else if kind == "shell" {
            (
                std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into()),
                vec!["-i".into()],
                Vec::new(),
            )
        } else {
            integration::command(kind, id, socket, resume, scratch.as_ref())?
        };
        // Agent panes, with everything they start, run under the agent CPU limit; shells and
        // log followers never do.
        let (program, args, gate) = match limiter {
            Some(limiter) if matches!(kind, "omp" | "opencode") => {
                limiter.prepare(id, program, args, cwd)
            }
            _ => (program, args, None),
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
        if let Some(scratch) = &scratch {
            for name in ["TMPDIR", "TMP", "TEMP"] {
                builder.env(name, &scratch.dir);
            }
        }
        for (key, value) in extra_env {
            builder.env(key, value);
        }
        // These only need the master; taking them first means a failure here
        // cannot leave a started child behind.
        let writer = pair.master.take_writer()?;
        let mut reader = pair.master.try_clone_reader()?;
        let spawned = {
            // An agent started while the limit is on takes the batch class and the coarse timer
            // slack from this thread, which has them only while the child is created.
            let _batch = limiter
                .zip(gate.as_ref())
                .and_then(|(limiter, gate)| limiter.batch(gate));
            pair.slave.spawn_command(builder)
        };
        let mut child =
            spawned.with_context(|| format!("launching {program} in {}", cwd.display()))?;
        drop(pair.slave);
        let pid = child.process_id();
        let killer = child.clone_killer();
        let (input, queue) = std::sync::mpsc::sync_channel(INPUT_QUEUE);
        let responses = Responses::new(input.clone());
        let sync = responses.sync.clone();
        let parser = Arc::new(Mutex::new(Parser::new_with_callbacks(
            rows, cols, 512, responses,
        )));
        let dirty = Arc::new(AtomicBool::new(true));
        let alive = Arc::new(AtomicBool::new(true));
        let output = Arc::new(AtomicBool::new(false));
        let quiet_since = Arc::new(AtomicU64::new(0));
        let reader_parser = parser.clone();
        let reader_dirty = dirty.clone();
        let reader_output = output.clone();
        let reader_quiet = quiet_since.clone();
        let wait_alive = alive.clone();
        let wait_tx = tx.clone();
        let limit_tx = tx.clone();
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
                let mut painted = false;
                loop {
                    match reader.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(count) => {
                            // Only the UI thread writes to the outer terminal, so
                            // hand it the copy once the parser lock is released.
                            let copy = {
                                let mut parser = reader_parser.lock();
                                parser.process(&buffer[..count]);
                                // A resized pane is drawn once its app's output has gone quiet.
                                reader_quiet.store(clock(), Ordering::Release);
                                // Terminal setup (alternate screen, mouse modes, queries) and
                                // a screen of blanks show nothing, and an app can send both
                                // seconds before its first frame: the pane has output once a
                                // visible character is on its screen. Checked until then, under
                                // the lock of the bytes, so a frame sees both or neither.
                                if !painted && shows_text(parser.screen()) {
                                    painted = true;
                                    reader_output.store(true, Ordering::Release);
                                }
                                parser.callbacks_mut().clipboard.take()
                            };
                            if let Some((ty, data)) = copy {
                                let _ = tx.send(Event::Clipboard { ty, data });
                            }
                            // Wakes the UI once per frame: the flag stays set until a frame has
                            // drawn the pane. A full queue is waited out, as dropping the wake-up
                            // would leave the pane stale until the app wrote again. Only this
                            // thread waits, and it holds no lock, so the UI never waits on it.
                            if !reader_dirty.swap(true, Ordering::AcqRel) {
                                let _ = tx.send(Event::Dirty(id));
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
                // This thread starts right after the child, so this is when the child started.
                let started = Instant::now();
                let (code, success) = match child.wait() {
                    Ok(status) => (status.to_string(), status.success()),
                    Err(error) => (error.to_string(), false),
                };
                wait_alive.store(false, Ordering::Release);
                let _ = wait_tx.send(Event::Exited {
                    session: id,
                    code,
                    success,
                    ran: started.elapsed(),
                });
            })
            .map_err(abandon)?;
        // Only now that nothing can fail: an abandoned pane drops its gate, which removes it.
        if let (Some(limiter), Some(gate)) = (limiter, gate) {
            limiter.start(gate, id, pid, alive.clone(), limit_tx);
        }
        Ok(Self {
            id,
            kind: kind.into(),
            state: if kind == "shell" {
                AgentState::Idle
            } else {
                AgentState::Unknown
            },
            completed: false,
            message: None,
            resume: match resume {
                Some(Resume::Chat(identity)) => Some(identity.to_owned()),
                _ => None,
            },
            parser,
            dirty,
            alive,
            output,
            master: pair.master,
            input,
            killer,
            pid,
            size: (rows, cols),
            sync,
            resized: AtomicU64::new(0),
            quiet_since,
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
            // A pane with something on it is drawn again once its app has repainted at the new
            // size. One that has painted nothing shows a placeholder, and its first frame must
            // not wait.
            if self.has_output() {
                // Output from before the resize is not the app's answer to it.
                self.quiet_since.store(0, Ordering::Release);
                self.resized.store(clock(), Ordering::Release);
            }
        }
        Ok(())
    }

    /// Sends a key to the program, showing its live output again: returns whether the pane was
    /// scrolled back, so that its view moved.
    pub fn key(&self, key: KeyEvent) -> Result<bool> {
        if !self.alive.load(Ordering::Acquire) {
            return Ok(false);
        }
        let (scrolled, application_cursor, keyboard) = {
            let mut parser = self.parser.lock();
            let scrolled = parser.screen().scrollback() != 0;
            parser.screen_mut().set_scrollback(0);
            (
                scrolled,
                parser.screen().application_cursor(),
                parser
                    .callbacks()
                    .keyboard_flags(parser.screen().alternate_screen())
                    != 0,
            )
        };
        let mut bytes = Vec::new();
        keys::write_key(&mut bytes, key, application_cursor, keyboard)?;
        self.send(bytes)?;
        Ok(scrolled)
    }

    /// Pastes into the program, showing its live output again: returns whether the pane was
    /// scrolled back, as `key` does.
    pub fn paste(&self, text: &str) -> Result<bool> {
        if !self.alive.load(Ordering::Acquire) {
            return Ok(false);
        }
        let (scrolled, bracketed) = {
            let mut parser = self.parser.lock();
            let scrolled = parser.screen().scrollback() != 0;
            parser.screen_mut().set_scrollback(0);
            (scrolled, parser.screen().bracketed_paste())
        };
        self.send(paste_bytes(text, bracketed))?;
        Ok(scrolled)
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

    /// Whether the app in this pane is partway through a synchronized update (DEC mode 2026),
    /// so that drawing now would show half a repaint. Stops being true `SYNC_HOLD` after the
    /// update began: an app that never ends it must not freeze the pane.
    pub fn synchronized(&self) -> bool {
        held(self.sync.load(Ordering::Acquire), clock())
    }

    /// Whether the app in this pane has yet to repaint after the pane changed size, so that
    /// drawing now would show its old content cut to the new grid. The app is waited for until
    /// its output has been quiet for `RESIZE_QUIET`, or for `RESIZE_SILENT_MAX` if it paints
    /// nothing, as `sleep` and `cat` do not. Output that follows within `RESIZE_SETTLE_MAX`, such
    /// as a settle pass, is waited out the same way. The resize is then forgotten, so that
    /// later output never holds a frame.
    pub fn settling(&self) -> bool {
        self.settling_at(clock())
    }

    /// `settling` as of `now`.
    fn settling_at(&self, now: u64) -> bool {
        let resized = self.resized.load(Ordering::Acquire);
        if resized == 0 {
            return false;
        }
        let quiet_since = self.quiet_since.load(Ordering::Acquire);
        let since = |stamp: u64| Duration::from_millis(now.saturating_sub(stamp));
        if since(resized) >= RESIZE_SETTLE_MAX
            || (quiet_since == 0 && since(resized) >= RESIZE_SILENT_MAX)
        {
            self.resized.store(0, Ordering::Release);
            return false;
        }
        quiet_since == 0 || since(quiet_since) < RESIZE_QUIET
    }

    /// Whether drawing this pane now would show it half done: its app is partway through a
    /// synchronized update, or has yet to repaint after a resize.
    pub fn holds_frame(&self) -> bool {
        self.synchronized() || self.settling()
    }

    /// Whether the program in this pane has painted anything yet: a visible character on its
    /// screen. Until it has, the pane is blank. Terminal setup (alternate screen, mouse modes,
    /// queries) and blanks paint nothing, and an agent can send them seconds before its first
    /// frame, for instance while a large saved chat loads.
    pub fn has_output(&self) -> bool {
        self.output.load(Ordering::Acquire)
    }

    /// Marks the pane as painted, for tests that put the program's output in `parser` directly.
    #[cfg(test)]
    pub fn mark_output(&self) {
        self.output.store(true, Ordering::Release);
    }

    /// Stamps output as the reader does, for tests that put the program's output in `parser`
    /// directly.
    #[cfg(test)]
    pub fn note_output(&self) {
        self.quiet_since.store(clock(), Ordering::Release);
    }

    /// The pane's own process, which leads its process group.
    pub fn pid(&self) -> Option<u32> {
        self.pid
    }

    /// The agent whose TUI this pane's shell runs in the foreground, if any.
    pub fn foreground_agent(&self) -> Option<&'static str> {
        let group = self.master.process_group_leader()?;
        let shell = self.pid? as libc::pid_t;
        // The shell at its own prompt runs no agent; a group outside the pane is not its own.
        if group == shell || unsafe { libc::getsid(group) } != shell {
            return None;
        }
        integration::adopted_agent(group)
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

/// Whether any cell of the screen holds a character that shows: a blank does not.
fn shows_text(screen: &vt100::Screen) -> bool {
    screen.has_text()
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

/// The origin of the millisecond stamps in `Responses::sync`.
static START: LazyLock<Instant> = LazyLock::new(Instant::now);

/// Milliseconds since `START`, plus one so that no stamp is the 0 that means "no update open".
fn clock() -> u64 {
    START.elapsed().as_millis() as u64 + 1
}

/// Whether an update begun at `stamp` (0: none open) still holds the frame at `now`.
fn held(stamp: u64, now: u64) -> bool {
    stamp != 0 && Duration::from_millis(now.saturating_sub(stamp)) < SYNC_HOLD
}

pub struct Responses {
    input: SyncSender<Input>,
    keyboard: [KeyboardModes; 2],
    /// The latest OSC 52 copy since the reader last looked.
    clipboard: Option<(Vec<u8>, Vec<u8>)>,
    /// When the app began the synchronized update it has not ended yet, as `clock` stamps it;
    /// 0 while none is open. The UI thread reads it to hold the pane's frame.
    sync: Arc<AtomicU64>,
}

impl Responses {
    fn new(input: SyncSender<Input>) -> Self {
        Self {
            input,
            keyboard: [KeyboardModes::default(); 2],
            clipboard: None,
            sync: Arc::new(AtomicU64::new(0)),
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
            // The app brackets a repaint of its screen; the UI holds its frame in between.
            // vt100 hands the mode over as unhandled, and no reply is due. Any parameter of
            // the list can be the mode, since the callback sees the whole list.
            (Some(b'?'), None, 'h' | 'l')
                if params.iter().any(|param| param.first() == Some(&2026)) =>
            {
                if c == 'h' {
                    // Only a begin that finds no update open counts, so an app that never ends
                    // its update cannot keep renewing the hold.
                    if self.sync.load(Ordering::Acquire) == 0 {
                        self.sync.store(clock(), Ordering::Release);
                    }
                } else {
                    self.sync.store(0, Ordering::Release);
                }
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
                    2026 => {
                        if self.sync.load(Ordering::Acquire) != 0 {
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
    use crate::limits;

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
            None,
            None,
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
    fn synchronized_output_is_advertised_and_tracked() {
        let (mut parser, queue) = response_parser();
        let stamp = |parser: &Parser| parser.callbacks().sync.load(Ordering::Acquire);
        // Supported from the start, with no update open.
        parser.process(b"\x1b[?2026$p");
        assert_eq!(queued(&queue), b"\x1b[?2026;2$y");
        assert!(!held(stamp(&parser), clock()));

        parser.process(b"\x1b[?2026h\x1b[?2026$p");
        assert_eq!(queued(&queue), b"\x1b[?2026;1$y");
        let begun = stamp(&parser);
        assert!(held(begun, begun));
        // A begin while an update is open keeps its stamp, so an app that never ends its
        // update cannot keep renewing the hold.
        parser.callbacks().sync.store(5, Ordering::Release);
        parser.process(b"\x1b[?2026h");
        assert_eq!(stamp(&parser), 5);
        // The hold runs out once the update is `SYNC_HOLD` old.
        assert!(held(5, 5 + 99));
        assert!(!held(5, 5 + 100));

        parser.process(b"\x1b[?2026l\x1b[?2026$p");
        assert_eq!(queued(&queue), b"\x1b[?2026;2$y");
        assert!(!held(stamp(&parser), clock()));

        // The mode may sit anywhere in a list of private modes.
        parser.process(b"\x1b[?25;2026h");
        assert_ne!(stamp(&parser), 0);
        parser.process(b"\x1b[?25;2026l");
        assert_eq!(stamp(&parser), 0);
        // Setting and resetting the mode send no reply of their own.
        assert!(queue.try_recv().is_err());
    }

    #[test]
    fn resize_settles_until_the_app_repaints_or_goes_quiet() {
        let (tx, _events) = std::sync::mpsc::sync_channel(32);
        let mut session = spawn_program("/bin/sh", &["-c", "printf ready; exec sleep 30"], tx);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !session.has_output() {
            assert!(Instant::now() < deadline, "the program never painted");
            std::thread::sleep(Duration::from_millis(5));
        }
        // The reader stamps the output it processes, before the pane counts as painted.
        assert_ne!(session.quiet_since.load(Ordering::Acquire), 0);
        // Nothing waits until the pane changes size, and its own size is no change.
        assert!(!session.settling());
        session.resize(24, 80).unwrap();
        assert!(!session.settling());
        // A resize waits for the app's answer: what it wrote before is not one.
        session.resize(10, 40).unwrap();
        assert_eq!(session.quiet_since.load(Ordering::Acquire), 0);
        assert!(session.settling());
        // Once its output has been quiet the pane is drawn.
        session.note_output();
        std::thread::sleep(Duration::from_millis(40));
        assert!(!session.settling());
        // Each resize starts a new wait.
        session.resize(12, 50).unwrap();
        assert_eq!(session.quiet_since.load(Ordering::Acquire), 0);
        assert!(session.settling());

        // The wait at `now`, for a resize stamped 1000 and the app's last output since (0: none).
        let waits = |session: &Session, quiet_since: u64, now: u64| {
            session.resized.store(1_000, Ordering::Release);
            session.quiet_since.store(quiet_since, Ordering::Release);
            session.settling_at(now)
        };
        // An app that has not answered is waited for, but `sleep` and `cat` never will.
        assert!(waits(&session, 0, 1_000));
        assert!(waits(&session, 0, 1_000 + 79));
        assert!(!waits(&session, 0, 1_000 + 80));
        // Such an app is forgotten, and its later output holds nothing.
        assert_eq!(session.resized.load(Ordering::Acquire), 0);
        session.quiet_since.store(1_200, Ordering::Release);
        assert!(!session.settling_at(1_200));
        // Output is waited out until it has been quiet for 30 ms.
        assert!(waits(&session, 1_010, 1_010));
        assert!(waits(&session, 1_010, 1_010 + 29));
        assert!(!waits(&session, 1_010, 1_010 + 30));
        // The app's next pass, as OMP's settle pass 140 ms on, is waited out the same way.
        session.quiet_since.store(1_150, Ordering::Release);
        assert!(session.settling_at(1_150));
        assert!(session.settling_at(1_150 + 29));
        assert!(!session.settling_at(1_150 + 30));
        // An app that never stops writing is let through 400 ms after the resize.
        assert!(waits(&session, 1_399, 1_399));
        assert!(!waits(&session, 1_399, 1_400));
        assert_eq!(session.resized.load(Ordering::Acquire), 0);
        // Without a resize there is nothing to wait for.
        session.quiet_since.store(1_400, Ordering::Release);
        assert!(!session.settling_at(1_400));
    }

    #[test]
    fn resize_before_first_paint_does_not_settle() {
        let (tx, _events) = std::sync::mpsc::sync_channel(32);
        let mut session = spawn_program("/bin/sleep", &["30"], tx);
        // A pane that has painted nothing shows a placeholder; its first frame must not wait.
        session.resize(10, 40).unwrap();
        assert!(!session.settling());
        assert!(!session.holds_frame());
        session.mark_output();
        session.resize(12, 50).unwrap();
        assert!(session.settling());
        assert!(session.holds_frame());
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
            None,
            None,
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
    fn output_wakes_the_ui_even_when_its_queue_was_full() {
        // The queue's one slot is taken before the child writes, so the wake-up for its output
        // finds the queue full.
        let (tx, events) = std::sync::mpsc::sync_channel(1);
        tx.send(Event::Shutdown).unwrap();
        let session = spawn_program(
            "/bin/sh",
            &["-c", "sleep 0.2; printf painted; exec sleep 30"],
            tx,
        );
        // As a frame does, so that the next output wakes the UI.
        session.dirty.store(false, Ordering::Release);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !session.has_output() {
            assert!(
                Instant::now() < deadline,
                "the child's output never reached the parser"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        // Once the UI catches up with its queue, it still hears of the output.
        assert!(matches!(events.recv(), Ok(Event::Shutdown)));
        assert!(matches!(
            events.recv_timeout(Duration::from_secs(5)),
            Ok(Event::Dirty(1))
        ));
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
            None,
            None,
        )
        .unwrap();
        // vt100 panics on 1-row and 1-column grids, which would kill the reader thread.
        assert_eq!(session.parser.lock().screen().size(), (2, 2));
        session.resize(10, 20).unwrap();
        assert_eq!(session.parser.lock().screen().size(), (10, 20));
        session.resize(0, 1).unwrap();
        assert_eq!(session.parser.lock().screen().size(), (2, 2));
    }

    /// The scheduling class of thread `tid`, or of the calling thread for 0, without the flag
    /// that resets it in children; `None` for a thread that is gone.
    fn class(tid: libc::pid_t) -> Option<libc::c_int> {
        let policy = unsafe { libc::sched_getscheduler(tid) };
        (policy >= 0).then_some(policy & !libc::SCHED_RESET_ON_FORK)
    }

    #[test]
    fn agents_started_while_limited_take_the_batch_class_and_slack_but_shells_never() {
        // On a thread of its own in the normal class with the default timer slack, which it
        // hands down to the panes it starts.
        std::thread::spawn(|| {
            // The tests may run in the batch class, which any thread may leave.
            let policy = unsafe { libc::sched_getscheduler(0) };
            if class(0) == Some(libc::SCHED_BATCH) {
                let normal = libc::SCHED_OTHER | (policy & libc::SCHED_RESET_ON_FORK);
                let parameters = libc::sched_param { sched_priority: 0 };
                unsafe { libc::sched_setscheduler(0, normal, &parameters) };
            }
            let (slack, unused): (libc::c_ulong, libc::c_ulong) = (50_000, 0);
            unsafe { libc::prctl(libc::PR_SET_TIMERSLACK, slack, unused, unused, unused) };
            if class(0) != Some(libc::SCHED_OTHER) {
                eprintln!("skipped: this thread cannot take the normal scheduling class");
                return;
            }
            // A limit without a scope never calls systemd; agents start the same with one.
            let limiter = Limiter::off();
            limiter.set_limited(true);
            let (tx, _events) = std::sync::mpsc::sync_channel(32);
            let script =
                "read slack < /proc/self/timerslack_ns; echo \"slack $slack\"; exec sleep 30";
            let spawn = |kind: &str| {
                Session::spawn(
                    1,
                    kind,
                    None,
                    Path::new("/"),
                    Path::new("/unused.sock"),
                    tx.clone(),
                    24,
                    80,
                    Some(("/bin/sh".into(), vec!["-c".into(), script.into()])),
                    Some(&limiter),
                    None,
                )
                .unwrap()
            };
            let agent = spawn("omp");
            let shell = spawn("shell");
            limiter.set_limited(false);
            let unlimited = spawn("opencode");
            // The thread has its own class back.
            assert_eq!(class(0), Some(libc::SCHED_OTHER));
            for (session, expected, slack) in [
                (&agent, libc::SCHED_BATCH, "slack 1000000"),
                (&shell, libc::SCHED_OTHER, "slack 50000"),
                (&unlimited, libc::SCHED_OTHER, "slack 50000"),
            ] {
                let deadline = Instant::now() + Duration::from_secs(5);
                while !session.parser.lock().screen().contents().contains("slack ") {
                    assert!(
                        Instant::now() < deadline,
                        "{} reported no slack",
                        session.kind
                    );
                    std::thread::sleep(Duration::from_millis(10));
                }
                let contents = session.parser.lock().screen().contents();
                assert!(contents.contains(slack), "{}: {contents:?}", session.kind);
                let pid = session.pid.unwrap() as libc::pid_t;
                assert_eq!(class(pid), Some(expected), "{}", session.kind);
            }
        })
        .join()
        .unwrap();
    }

    /// The memory that `limited_pane`'s limit takes its share of: whole pages, which the kernel
    /// keeps as they are, and far more than the panes' busy loops use.
    const PANE_MEMORY: u64 = 4 << 30;

    /// An OMP pane that runs `script` under a limit of `percent` of one CPU and of
    /// `PANE_MEMORY`, on the cores that a limit of half the machine would leave the agents,
    /// with the cgroup of the scope it runs in; `None` where no systemd user scope can hold
    /// agents.
    fn limited_pane(
        directory: &Path,
        percent: u8,
        script: &str,
    ) -> Option<(Limiter, Session, std::path::PathBuf)> {
        let cpus = limits::agent_cpus(Path::new("/sys"), 50);
        let limiter = Limiter::with_cores(directory, percent, 1, cpus, PANE_MEMORY, true);
        let unit = match limiter.unit() {
            Ok(unit) => unit.to_owned(),
            Err(reason) => {
                eprintln!("skipped: no systemd user scope can hold agents here: {reason}");
                return None;
            }
        };
        if limiter.memory_high().is_none() {
            eprintln!("memory not checked: the systemd user manager cannot limit memory here");
        }
        if limiter.cpus().is_none() {
            eprintln!("CPUs not checked: the scope cannot hold the agents to some CPUs here");
        }
        if !matches!(class(0), Some(libc::SCHED_OTHER | libc::SCHED_BATCH)) {
            eprintln!("class not checked: the tests run in a class that agents would inherit");
        }
        let (tx, events) = std::sync::mpsc::sync_channel(32);
        let session = Session::spawn(
            1,
            "omp",
            None,
            directory,
            &directory.join("unused.sock"),
            tx,
            24,
            80,
            Some(("/bin/sh".into(), vec!["-c".into(), script.into()])),
            Some(&limiter),
            None,
        )
        .unwrap();
        // The pane is released only once it is in the scope, but its trampoline may not have
        // been moved yet when this first looks.
        let pid = session.pid.unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let cgroup = loop {
            let groups = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).unwrap();
            if let Some(path) = groups.lines().find_map(|line| line.strip_prefix("0::"))
                && path.ends_with(&format!("/{unit}"))
            {
                break Path::new("/sys/fs/cgroup").join(path.trim_start_matches('/'));
            }
            if Instant::now() >= deadline {
                let reports: Vec<String> = events
                    .try_iter()
                    .filter_map(|event| match event {
                        Event::CpuLimit(report) => Some(report),
                        _ => None,
                    })
                    .collect();
                panic!("the pane never joined {unit}: {reports:?}");
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        Some((limiter, session, cgroup))
    }

    /// How many CPUs the processes in `cgroup` kept busy over `window`.
    fn cpu_rate(cgroup: &Path, window: Duration) -> f64 {
        let usage = || {
            let stat = std::fs::read_to_string(cgroup.join("cpu.stat")).unwrap();
            let micros: u64 = stat
                .lines()
                .find_map(|line| line.strip_prefix("usage_usec "))
                .unwrap()
                .parse()
                .unwrap();
            Duration::from_micros(micros)
        };
        let (before, started) = (usage(), Instant::now());
        std::thread::sleep(window);
        (usage() - before).as_secs_f64() / started.elapsed().as_secs_f64()
    }

    /// Waits until the pane's shell has started its busy loops, which share its cgroup.
    fn wait_for_processes(cgroup: &Path, count: usize) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let procs = std::fs::read_to_string(cgroup.join("cgroup.procs")).unwrap();
            if procs.lines().count() >= count {
                return;
            }
            assert!(Instant::now() < deadline, "only {procs:?} run in the scope");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// The scheduling class and the CPUs of every thread in `cgroup`, leaving out any that
    /// ends meanwhile.
    fn placement(cgroup: &Path) -> Vec<(libc::c_int, Vec<u32>)> {
        let tids = std::fs::read_to_string(cgroup.join("cgroup.threads")).unwrap();
        tids.lines()
            .filter_map(|tid| {
                let status = std::fs::read_to_string(format!("/proc/{tid}/status")).ok()?;
                let cpus = status
                    .lines()
                    .find_map(|line| line.strip_prefix("Cpus_allowed_list:"))?;
                Some((class(tid.parse().ok()?)?, limits::cpu_list(cpus).unwrap()))
            })
            .collect()
    }

    /// Asserts that the agents' threads in `cgroup` run as `limiter` holds them while it is
    /// `limited`: in the batch class and on its CPUs, or else in the normal class on more
    /// CPUs. A class that the tests' own thread could not hand down, such as the idle one, is
    /// not checked, nor are the CPUs where the scope holds the agents to none.
    fn assert_placed(cgroup: &Path, limiter: &Limiter, limited: bool) {
        let classed = matches!(class(0), Some(libc::SCHED_OTHER | libc::SCHED_BATCH));
        let expected = if limited {
            libc::SCHED_BATCH
        } else {
            libc::SCHED_OTHER
        };
        let threads = placement(cgroup);
        assert!(!threads.is_empty(), "no thread runs in the scope");
        for (class, cpus) in threads {
            if classed {
                assert_eq!(class, expected, "limited: {limited}");
            }
            if let Some(chosen) = limiter.cpus() {
                if limited {
                    assert_eq!(cpus, chosen);
                } else {
                    assert!(
                        cpus.len() > chosen.len() && chosen.iter().all(|cpu| cpus.contains(cpu)),
                        "lifted, the agents still run on {cpus:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn agent_panes_run_in_their_scope_at_its_shares() {
        let directory = tempfile::Builder::new()
            .prefix("crit-test-")
            .tempdir()
            .unwrap();
        let Some((limiter, _session, cgroup)) = limited_pane(
            directory.path(),
            100,
            "yes >/dev/null & yes >/dev/null & sleep 3; kill $!",
        ) else {
            return;
        };
        let read = |name| std::fs::read_to_string(cgroup.join(name)).unwrap();
        // All of one CPU, and other apps go first while the CPUs are contended.
        assert_eq!(read("cpu.max"), "100000 100000\n");
        assert_eq!(read("cpu.weight"), "20\n");
        // All of `PANE_MEMORY` too, past which the kernel throttles and reclaims the agents
        // rather than killing them.
        if limiter.memory_high().is_some() {
            assert_eq!(read("memory.high"), format!("{PANE_MEMORY}\n"));
        }
        // What the agent starts runs in its scope too: the shell, both loops and `sleep`, in the
        // batch class and on the scope's cores.
        wait_for_processes(&cgroup, 4);
        assert_placed(&cgroup, &limiter, true);
        // Two busy processes would keep two CPUs busy; the quota holds them to one. Other
        // apps that contend for the CPUs can leave them less.
        let rate = cpu_rate(&cgroup, Duration::from_secs(2));
        assert!(rate <= 1.3, "{rate:.2} CPUs");
    }

    #[test]
    fn retuning_lifts_and_restores_the_shares_of_running_agents() {
        let directory = tempfile::Builder::new()
            .prefix("crit-test-")
            .tempdir()
            .unwrap();
        // All of one CPU, for three busy loops.
        let Some((limiter, _session, cgroup)) = limited_pane(
            directory.path(),
            100,
            "yes >/dev/null & yes >/dev/null & yes >/dev/null & wait",
        ) else {
            return;
        };
        let read = |name| std::fs::read_to_string(cgroup.join(name)).unwrap();
        let memory = limiter.memory_high().is_some();
        wait_for_processes(&cgroup, 4);
        assert_placed(&cgroup, &limiter, true);
        // Over a window the rate strays a little from the quota, either way.
        let window = Duration::from_secs(2);
        assert_eq!(read("cpu.max"), "100000 100000\n");
        let limited = cpu_rate(&cgroup, window);
        assert!(limited <= 1.3, "limited: {limited:.2} CPUs");
        // As the worker does after F3: the scope has changed by the time `retune` returns.
        limiter.set_limited(false);
        limiter.retune().unwrap();
        assert_eq!(
            (read("cpu.max"), read("cpu.weight")),
            ("max 100000\n".into(), "100\n".into())
        );
        // The memory share goes with the quota, back to no limit at all, and returns with it.
        if memory {
            assert_eq!(read("memory.high"), "max\n");
        }
        // The agents' threads go back to the normal class and to every core, and return.
        assert_placed(&cgroup, &limiter, false);
        limiter.set_limited(true);
        limiter.retune().unwrap();
        assert_eq!(
            (read("cpu.max"), read("cpu.weight")),
            ("100000 100000\n".into(), "20\n".into())
        );
        if memory {
            assert_eq!(read("memory.high"), format!("{PANE_MEMORY}\n"));
        }
        assert_placed(&cgroup, &limiter, true);
        // The scheduler lets the agents overrun the quota for a moment as it returns, and
        // takes that time back right after.
        std::thread::sleep(Duration::from_secs(1));
        let restored = cpu_rate(&cgroup, window);
        assert!(restored <= 1.3, "restored: {restored:.2} CPUs");
    }
}
