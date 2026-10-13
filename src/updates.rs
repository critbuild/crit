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

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum State {
    Checking,
    Ready,
    Failed,
    /// Esc stopped checking before this updater finished.
    Skipped,
    /// The updater program is not installed.
    Missing,
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
    /// `None` when the program is not installed.
    fn spawn(program: &str, argument: &str) -> Result<Option<Self>> {
        let output = tempfile::tempfile().context("capture updater output")?;
        let child = match Command::new(program)
            .arg(argument)
            .stdin(Stdio::null())
            .stdout(output.try_clone()?)
            .stderr(output.try_clone()?)
            .env("NO_COLOR", "1")
            .process_group(0)
            .spawn()
        {
            Ok(child) => child,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).with_context(|| format!("start {program} {argument}")),
        };
        Ok(Some(Self {
            child,
            output,
            reaped: false,
        }))
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

    /// `None` while the updater runs; its outcome once it has exited.
    fn poll(&mut self) -> Option<Result<(), String>> {
        let status = match self.child.try_wait() {
            Ok(None) => return None,
            Ok(Some(status)) => status,
            Err(error) => return Some(Err(error.to_string())),
        };
        self.reaped = true;
        if status.success() {
            return Some(Ok(()));
        }
        let detail = self
            .diagnostic()
            .unwrap_or_else(|error| format!("could not read updater output: {error}"));
        Some(Err(if detail.is_empty() {
            status.to_string()
        } else {
            format!("{status}: {detail}")
        }))
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
    check(&TOOLS, events, |states, tick| {
        terminal.draw(|frame| ui::draw_updates(frame, states, tick))?;
        Ok(())
    })
}

/// Runs every updater at once: they share no state, so startup waits for the slowest
/// rather than the sum. Esc stops waiting for the ones still running; they are terminated
/// as this returns. `None` means Crit is quitting.
fn check(
    tools: &[(&str, &str, &str); TOOLS.len()],
    events: &Receiver<Event>,
    mut draw: impl FnMut(&[State; TOOLS.len()], usize) -> Result<()>,
) -> Result<Option<Startup>> {
    let mut states = [State::Checking; TOOLS.len()];
    let mut processes: [Option<UpdateProcess>; TOOLS.len()] = Default::default();
    let mut failures: [Option<String>; TOOLS.len()] = Default::default();
    let mut pending = VecDeque::new();
    let mut tick = 0;
    let mut last_frame = Instant::now();
    draw(&states, tick)?;
    for (index, (_, program, argument)) in tools.iter().enumerate() {
        match UpdateProcess::spawn(program, argument) {
            Ok(Some(process)) => processes[index] = Some(process),
            Ok(None) => states[index] = State::Missing,
            Err(error) => {
                states[index] = State::Failed;
                failures[index] = Some(format!("{error:#}"));
            }
        }
    }
    loop {
        for index in 0..TOOLS.len() {
            let Some(outcome) = processes[index].as_mut().and_then(UpdateProcess::poll) else {
                continue;
            };
            processes[index] = None;
            match outcome {
                Ok(()) => states[index] = State::Ready,
                Err(error) => {
                    states[index] = State::Failed;
                    failures[index] = Some(error);
                }
            }
        }
        if processes.iter().all(Option::is_none) {
            break;
        }
        if last_frame.elapsed() >= FRAME_INTERVAL {
            tick = (tick + 1) % 24;
            draw(&states, tick)?;
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
            Ok(Event::Input(Input::Key(key)))
                if key.kind != KeyEventKind::Release && key.code == KeyCode::Esc =>
            {
                for (state, process) in states.iter_mut().zip(&processes) {
                    if process.is_some() {
                        *state = State::Skipped;
                    }
                }
                break;
            }
            Ok(Event::Input(Input::Resize(_, _))) => {
                draw(&states, tick)?;
            }
            Ok(Event::Error(error)) => anyhow::bail!("{error}"),
            // Input typed during startup belongs to the picker or shell,
            // not the updater. Replay it in order when checking finishes.
            Ok(event) => pending.push_back(event),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Ok(None),
        }
    }
    draw(&states, tick)?;
    // Report failures in tool order, whichever updater finished first.
    let mut notice = String::new();
    for ((name, _, _), failure) in tools.iter().zip(&failures) {
        let Some(failure) = failure else {
            continue;
        };
        if !notice.is_empty() {
            notice.push_str("; ");
        }
        let _ = write!(
            notice,
            "{name} update failed: {failure}. Continuing with the installed version."
        );
    }
    Ok(Some(Startup {
        notice: (!notice.is_empty()).then_some(notice),
        pending,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEvent;
    use std::{fs, sync::mpsc};

    fn key(code: KeyCode) -> Event {
        Event::Input(Input::Key(KeyEvent::new(code, KeyModifiers::NONE)))
    }

    #[test]
    fn esc_skips_running_updaters_and_absent_ones_are_not_failures() {
        let (sender, events) = mpsc::channel();
        sender.send(key(KeyCode::Esc)).unwrap();
        let tools = [
            ("Slow", "/bin/sleep", "60"),
            ("Absent", "/nonexistent/crit-test-tool", "update"),
        ];
        let mut last = [State::Checking; TOOLS.len()];
        let started = Instant::now();
        let startup = check(&tools, &events, |states, _| {
            last = *states;
            Ok(())
        })
        .unwrap()
        .unwrap();
        assert_eq!(last, [State::Skipped, State::Missing]);
        assert_eq!(startup.notice, None);
        // The Esc was consumed rather than replayed, and nothing waited out the sleep.
        assert!(startup.pending.is_empty());
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn updaters_run_concurrently_and_failures_keep_tool_order() {
        // Each script exits only once the other has started, so running them one
        // after another would end in "peer never started" instead.
        let script = r#"d=$(dirname "$0"); : > "$0.started"
for _ in $(seq 300); do
    if [ "$(ls "$d"/*.started | wc -l)" -ge 2 ]; then echo "failed: $(basename "$0")" >&2; exit 1; fi
    sleep 0.01
done
echo "peer never started" >&2; exit 9"#;
        let directory = tempfile::tempdir().unwrap();
        let [first, second] = ["first.sh", "second.sh"].map(|name| {
            let path = directory.path().join(name);
            fs::write(&path, script).unwrap();
            path.to_str().unwrap().to_owned()
        });
        let tools = [
            ("First", "/bin/sh", first.as_str()),
            ("Second", "/bin/sh", second.as_str()),
        ];
        let (_sender, events) = mpsc::channel();
        let startup = check(&tools, &events, |_, _| Ok(())).unwrap().unwrap();
        assert_eq!(
            startup.notice.as_deref(),
            Some(
                "First update failed: exit status: 1: failed: first.sh. Continuing with the \
                 installed version.; Second update failed: exit status: 1: failed: second.sh. \
                 Continuing with the installed version."
            )
        );
    }
}
