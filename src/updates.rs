use crate::{event::Event, ui};
use anyhow::{Context, Result};
use crossterm::event::{Event as Input, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::{Terminal, backend::CrosstermBackend};
use std::{
    collections::VecDeque,
    fmt::Write as _,
    fs::File,
    io::{self, Read, Seek, SeekFrom},
    os::unix::process::CommandExt,
    process::{Child, Command, Stdio},
    sync::mpsc::{Receiver, RecvTimeoutError},
    time::{Duration, Instant},
};

pub const TOOLS: [(&str, &str, &str); 2] = [
    ("OMP", "omp", "update"),
    ("OpenCode", "opencode", "upgrade"),
];
const FRAME_INTERVAL: Duration = Duration::from_millis(80);
const DIAGNOSTIC_BYTES: u64 = 4096;

#[derive(Clone, Copy)]
pub enum State {
    Waiting,
    Checking,
    Ready,
    Failed,
}

pub struct Startup {
    pub notice: Option<String>,
    pub pending: VecDeque<Event>,
}

// A private, automatically removed file captures both streams without pipe
// backpressure, updater banners, or unbounded in-memory output buffers.
struct UpdateProcess {
    child: Child,
    output: File,
    reaped: bool,
}

impl UpdateProcess {
    fn spawn(program: &str, argument: &str) -> Result<Self> {
        let output = tempfile::tempfile().context("capture updater output")?;
        let child = Command::new(program)
            .arg(argument)
            .stdin(Stdio::null())
            .stdout(output.try_clone()?)
            .stderr(output.try_clone()?)
            .env("NO_COLOR", "1")
            .process_group(0)
            .spawn()
            .with_context(|| format!("start {program} {argument}"))?;
        Ok(Self {
            child,
            output,
            reaped: false,
        })
    }

    fn diagnostic(&mut self) -> Result<String> {
        let length = self.output.metadata()?.len();
        self.output
            .seek(SeekFrom::Start(length.saturating_sub(DIAGNOSTIC_BYTES)))?;
        let mut bytes = Vec::with_capacity(length.min(DIAGNOSTIC_BYTES) as usize);
        (&mut self.output)
            .take(DIAGNOSTIC_BYTES)
            .read_to_end(&mut bytes)?;
        // Keep terminal control bytes out of the UI. The child receives NO_COLOR;
        // preserving the end of its output keeps the actual error, not its logo.
        let message: String = String::from_utf8_lossy(&bytes)
            .trim()
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect();
        Ok(message)
    }
}

impl Drop for UpdateProcess {
    fn drop(&mut self) {
        if !self.reaped {
            // Give the installer a chance to clean up before stopping stragglers.
            // Observe without reaping: the process-group ID stays reserved until
            // every group signal has been sent.
            unsafe {
                libc::kill(-(self.child.id() as libc::pid_t), libc::SIGTERM);
            }
            let deadline = Instant::now() + Duration::from_secs(1);
            while Instant::now() < deadline {
                let mut info = unsafe { std::mem::zeroed::<libc::siginfo_t>() };
                let result = unsafe {
                    libc::waitid(
                        libc::P_PID,
                        self.child.id(),
                        &mut info,
                        libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                    )
                };
                if (result == 0 && unsafe { info.si_pid() } != 0)
                    || (result == -1
                        && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted)
                {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            unsafe {
                libc::kill(-(self.child.id() as libc::pid_t), libc::SIGKILL);
            }
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

pub fn run(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    events: &Receiver<Event>,
) -> Result<Option<Startup>> {
    let mut states = [State::Waiting; TOOLS.len()];
    let mut failures = String::new();
    let mut pending = VecDeque::new();
    let mut tick = 0;
    let mut last_frame = Instant::now();
    for (index, (name, program, argument)) in TOOLS.iter().enumerate() {
        states[index] = State::Checking;
        terminal.draw(|frame| ui::draw_updates(frame, &states, tick))?;
        let result = match UpdateProcess::spawn(program, argument) {
            Err(error) => Err(format!("{error:#}")),
            Ok(mut process) => loop {
                match process.child.try_wait() {
                    Ok(Some(status)) => {
                        process.reaped = true;
                        break if status.success() {
                            Ok(())
                        } else {
                            let detail = process.diagnostic().unwrap_or_else(|error| {
                                format!("could not read updater output: {error}")
                            });
                            Err(if detail.is_empty() {
                                status.to_string()
                            } else {
                                format!("{status}: {detail}")
                            })
                        };
                    }
                    Ok(None) => {}
                    Err(error) => break Err(error.to_string()),
                }
                if last_frame.elapsed() >= FRAME_INTERVAL {
                    tick = (tick + 1) % 24;
                    terminal.draw(|frame| ui::draw_updates(frame, &states, tick))?;
                    last_frame = Instant::now();
                }
                match events.recv_timeout(FRAME_INTERVAL.saturating_sub(last_frame.elapsed())) {
                    Ok(Event::Shutdown) => return Ok(None),
                    Ok(Event::Input(Input::Key(key)))
                        if key.kind != KeyEventKind::Release
                            && key.code == KeyCode::Char('c')
                            && key.modifiers.contains(KeyModifiers::CONTROL) =>
                    {
                        return Ok(None);
                    }
                    Ok(Event::Input(Input::Resize(_, _))) => {
                        terminal.draw(|frame| ui::draw_updates(frame, &states, tick))?;
                    }
                    Ok(Event::Error(error)) => anyhow::bail!("{error}"),
                    // Input typed during startup belongs to the picker or shell,
                    // not the updater. Replay it in order when checking finishes.
                    Ok(event) => pending.push_back(event),
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => return Ok(None),
                }
            },
        };
        match result {
            Ok(()) => states[index] = State::Ready,
            Err(error) => {
                states[index] = State::Failed;
                if !failures.is_empty() {
                    failures.push_str("; ");
                }
                let _ = write!(
                    failures,
                    "{name} update failed: {error}. Continuing with the installed version."
                );
            }
        }
    }
    terminal.draw(|frame| ui::draw_updates(frame, &states, tick))?;
    Ok(Some(Startup {
        notice: (!failures.is_empty()).then_some(failures),
        pending,
    }))
}
