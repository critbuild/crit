//! Keeps the agents from taking the CPU that a game or other apps need.
//!
//! Every OMP and OpenCode pane of a Crit window runs in one transient systemd user scope,
//! with everything it starts, so that one CPU quota holds the agents back as a whole and a low
//! CPU weight lets other apps go first whenever the CPUs are contended. Shells are never
//! limited. Where no scope can hold the agents, those started while the limit is on run at a
//! lower priority instead.

use crate::event::Event;
use std::{
    ffi::{CString, OsStr},
    fs::{self, OpenOptions},
    io::{self, Write},
    os::unix::{ffi::OsStrExt, fs::OpenOptionsExt, process::CommandExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, SyncSender},
    },
    time::{Duration, Instant},
};

/// The agents' share of the machine's CPU while the limit is on, unless `--agent-cpu` chose
/// another: half, so that a game keeps the other half.
pub const DEFAULT_PERCENT: u8 = 50;

/// The agents' CPU weight while limited. Other apps have the default 100, so whenever the CPUs
/// are contended they go first, even while the agents are within their quota.
const LIMITED_WEIGHT: &str = "20";

/// The default CPU weight, which the agents get back while the limit is off.
const DEFAULT_WEIGHT: &str = "100";

/// The niceness of agents started while the limit is on where no scope can hold them.
const NICENESS: libc::c_int = 10;

/// Runs the agent once Crit has written a line to the FIFO named by `$0`, so that the agent
/// cannot start a process before it is in the scope. `exec` keeps the process ID that Crit
/// knows the pane by. A FIFO closed without a line lets the agent go on as well.
const TRAMPOLINE: &str = "read _ < \"$0\"; exec \"$@\"";

/// The shell that runs `TRAMPOLINE`.
const SHELL: &str = "/bin/sh";

/// Names the trampolines' FIFOs in the integration directory, followed by the session ID.
const GATE_PREFIX: &str = "cpu-gate-";

/// How long a pane waits for systemd to move it into the scope before it runs anyway.
const PLACEMENT_TIMEOUT: Duration = Duration::from_secs(2);

/// How long Crit waits for a trampoline to open its FIFO.
const RELEASE_TIMEOUT: Duration = Duration::from_secs(5);

/// How `busctl` reaches the systemd user manager. systemd answers these calls at once; one
/// that hangs must not hold a pane back for long.
const MANAGER: [&str; 7] = [
    "--user",
    "--quiet",
    "--timeout=5",
    "call",
    "org.freedesktop.systemd1",
    "/org/freedesktop/systemd1",
    "org.freedesktop.systemd1.Manager",
];

/// The agent CPU limit of one Crit window. Clones share it with the threads that put panes
/// under it and with the worker that retunes it.
#[derive(Clone)]
pub struct Limiter(Arc<Inner>);

struct Inner {
    mechanism: Mechanism,
    /// The agents' share of the machine's CPU while limited, in percent.
    percent: u8,
    /// How many CPUs Crit may use: the share is of these.
    cores: u32,
    /// `percent` as the key hint shows it.
    label: String,
    limited: AtomicBool,
    /// Held while the scope is started, joined or retuned, so that whichever call comes last
    /// applies the setting as it is then. True while the scope is believed to run.
    started: parking_lot::Mutex<bool>,
    /// Whether an agent that runs unlimited has been reported: only the first one is.
    reported: AtomicBool,
}

enum Mechanism {
    /// Agents run in this transient scope of the systemd user manager, and their trampolines'
    /// FIFOs live in `directory`.
    Scope { unit: String, directory: PathBuf },
    /// No scope can hold the agents, for this reason: agents started while the limit is on run
    /// at a lower priority instead.
    Priority(String),
}

/// How `Limiter::start` puts one agent pane under the limit once it runs.
pub enum Gate {
    /// The pane runs the trampoline, which waits on this FIFO until the pane is in the scope.
    Scope(Fifo),
    /// The pane is given a lower priority.
    Priority,
    /// The pane cannot be limited, for this reason, and runs as it is.
    Unlimited(String),
}

/// A trampoline's FIFO, removed when this is dropped.
pub struct Fifo(PathBuf);

impl Drop for Fifo {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

impl Limiter {
    /// The limit for the window whose private integration directory is `directory`: its name
    /// names the scope, and the trampolines' FIFOs live in it. Whether a scope can hold the
    /// agents is checked once, here, from files alone.
    pub fn new(directory: &Path, percent: u8, limited: bool) -> Self {
        let cores = std::thread::available_parallelism().map_or(1, |cores| cores.get() as u32);
        Self::with_cores(directory, percent, cores, limited)
    }

    /// `new`, with the share taken of `cores` CPUs.
    pub fn with_cores(directory: &Path, percent: u8, cores: u32, limited: bool) -> Self {
        let mechanism = match probe() {
            Ok(()) => match directory.file_name().and_then(OsStr::to_str) {
                // `crit-<pid>-<16 hex>` is unique to this run, so a scope left by an earlier
                // Crit with the same process ID cannot be mistaken for this one.
                Some(name) => Mechanism::Scope {
                    unit: format!("{name}.scope"),
                    directory: directory.to_owned(),
                },
                None => Mechanism::Priority("Crit's runtime folder name is not UTF-8".into()),
            },
            Err(reason) => Mechanism::Priority(reason),
        };
        Self::build(mechanism, percent, cores, limited)
    }

    fn build(mechanism: Mechanism, percent: u8, cores: u32, limited: bool) -> Self {
        Self(Arc::new(Inner {
            mechanism,
            percent,
            cores,
            label: format!("{percent}%"),
            limited: AtomicBool::new(limited),
            started: parking_lot::Mutex::new(false),
            reported: AtomicBool::new(false),
        }))
    }

    pub fn limited(&self) -> bool {
        self.0.limited.load(Ordering::Acquire)
    }

    pub fn percent(&self) -> u8 {
        self.0.percent
    }

    /// Turns the limit on or off for agents started from now on; `retune` applies it to the
    /// running ones.
    pub fn set_limited(&self, limited: bool) {
        self.0.limited.store(limited, Ordering::Release);
    }

    /// Whether the agents are held by a scope, which `retune` changes for running agents too.
    /// Otherwise they run at a lower priority, which no process can undo.
    pub fn uses_scope(&self) -> bool {
        matches!(self.0.mechanism, Mechanism::Scope { .. })
    }

    /// The limit's state in the key hint: the share, LOW for the lower priority, or OFF.
    pub fn hint(&self) -> &str {
        match (&self.0.mechanism, self.limited()) {
            (_, false) => "OFF",
            (Mechanism::Scope { .. }, true) => &self.0.label,
            (Mechanism::Priority(_), true) => "LOW",
        }
    }

    /// What the limit does now, for the notice after F3.
    pub fn describe(&self) -> String {
        let Inner { percent, cores, .. } = *self.0;
        match (&self.0.mechanism, self.limited()) {
            (Mechanism::Scope { .. }, true) => format!(
                "Agents limited to {percent}% of the CPU ({} of {cores} core{}).",
                share(percent, cores),
                if cores == 1 { "" } else { "s" }
            ),
            (Mechanism::Scope { .. }, false) => "Agent CPU limit off.".into(),
            (Mechanism::Priority(reason), true) => format!(
                "Agents run at a lower CPU priority: {reason}, so they cannot be held to {percent}% of the CPU."
            ),
            (Mechanism::Priority(_), false) => "Agent CPU limit off for agents started from now on; running agents keep their lower priority.".into(),
        }
    }

    /// The startup notice, when the limit is on but only the lower priority can hold agents.
    pub fn startup_notice(&self) -> Option<String> {
        match &self.0.mechanism {
            Mechanism::Priority(reason) if self.limited() => Some(format!(
                "Agent CPU limit: {reason}, so agents run at a lower priority instead of at {}% of the CPU.",
                self.0.percent
            )),
            _ => None,
        }
    }

    /// The quota while limited, in microseconds of CPU time per second: all of one CPU is
    /// 1 000 000.
    fn quota(&self) -> u64 {
        quota(self.0.percent, self.0.cores)
    }

    /// Applies the setting to the agents that already run. `systemctl` waits on systemd, so
    /// only the worker thread calls this. A scope that is not running holds no agent: there is
    /// nothing to change, and the next agent starts it with the setting as it is then.
    pub fn retune(&self) -> Result<(), String> {
        let Mechanism::Scope { unit, .. } = &self.0.mechanism else {
            return Ok(());
        };
        let _calls = self.0.started.lock();
        let quota = self.limited().then(|| self.quota());
        match run("systemctl", &retune_arguments(unit, quota)) {
            // "Unit crit-….scope not found."
            Err(error) if error.contains("not found") || error.contains("not loaded") => Ok(()),
            result => result.map_err(|error| {
                format!("The agent CPU limit could not be changed for running agents: {error}")
            }),
        }
    }

    /// Prepares the launch of agent pane `id`, which runs `program` with `args` in `cwd`:
    /// returns what to spawn in its place, and how `start` limits it once it runs. Never fails:
    /// a pane that cannot be limited starts all the same, and its gate says why.
    pub fn prepare(
        &self,
        id: u64,
        program: String,
        args: Vec<String>,
        cwd: &Path,
    ) -> (String, Vec<String>, Option<Gate>) {
        let directory = match &self.0.mechanism {
            Mechanism::Priority(_) => {
                return (program, args, self.limited().then_some(Gate::Priority));
            }
            // A program that cannot be found is spawned as it is, so that the spawn fails with
            // the reason rather than the trampoline exiting in the pane.
            Mechanism::Scope { .. } if !runnable(&program, cwd) => return (program, args, None),
            Mechanism::Scope { .. } if !executable(Path::new(SHELL)) => {
                let reason = format!("{SHELL} is missing");
                return (program, args, Some(Gate::Unlimited(reason)));
            }
            Mechanism::Scope { directory, .. } => directory,
        };
        let path = directory.join(format!("{GATE_PREFIX}{id}"));
        let Some(name) = path.to_str().map(str::to_owned) else {
            let reason = format!("{} is not UTF-8", path.display());
            return (program, args, Some(Gate::Unlimited(reason)));
        };
        let fifo = match make_fifo(&path) {
            Ok(fifo) => fifo,
            Err(error) => {
                let reason = format!("could not create {name}: {error}");
                return (program, args, Some(Gate::Unlimited(reason)));
            }
        };
        let mut trampoline = Vec::with_capacity(args.len() + 4);
        trampoline.extend(["-c".to_owned(), TRAMPOLINE.to_owned(), name, program]);
        trampoline.extend(args);
        (SHELL.to_owned(), trampoline, Some(Gate::Scope(fifo)))
    }

    /// Puts pane `id`, which `prepare` gated and which now runs as `pid`, under the limit and
    /// lets it go on. The systemd calls run on a short thread of their own, or on this one if no
    /// thread can start, and never hold a lock the UI takes. An agent that cannot be limited
    /// runs unlimited, which `tx` reports once.
    pub fn start(
        &self,
        gate: Gate,
        id: u64,
        pid: Option<u32>,
        alive: Arc<AtomicBool>,
        tx: SyncSender<Event>,
    ) {
        let fifo = match gate {
            Gate::Priority => {
                if let Some(pid) = pid {
                    lower_priority(pid);
                }
                return;
            }
            Gate::Unlimited(reason) => {
                // The UI thread itself spawns panes: it must not wait on its own queue.
                self.report(|event| tx.try_send(event).is_ok(), &reason);
                return;
            }
            Gate::Scope(fifo) => fifo,
        };
        let enrollment = Enrollment {
            limiter: self.clone(),
            fifo,
            pid,
            alive,
        };
        // Handed over only once the thread runs, so that it is still here if none can.
        let (handover, receiver) = mpsc::sync_channel::<Enrollment>(1);
        let thread_tx = tx.clone();
        let thread = std::thread::Builder::new()
            .name(format!("cpu-limit-{id}"))
            .spawn(move || {
                if let Ok(enrollment) = receiver.recv() {
                    enrollment.run(|event| thread_tx.send(event).is_ok());
                }
            });
        match thread {
            Ok(_) => {
                let _ = handover.send(enrollment);
            }
            // The pane must not wait forever. This is the UI thread, which must not wait on
            // its own queue either.
            Err(_) => enrollment.run(|event| tx.try_send(event).is_ok()),
        }
    }

    /// Removes the FIFOs of panes that are still starting. Their threads can outlive Crit,
    /// which removes the integration directory as it exits, and only an empty one can go.
    pub fn remove_gates(&self) {
        let Mechanism::Scope { directory, .. } = &self.0.mechanism else {
            return;
        };
        let Ok(entries) = fs::read_dir(directory) else {
            return;
        };
        for entry in entries.flatten() {
            if entry
                .file_name()
                .as_bytes()
                .starts_with(GATE_PREFIX.as_bytes())
            {
                let _ = fs::remove_file(entry.path());
            }
        }
    }

    /// Moves `pid` into the scope, starting the scope if no agent holds it now, and waits until
    /// systemd has moved it.
    fn place(&self, pid: u32, alive: &AtomicBool) -> Result<(), String> {
        let Mechanism::Scope { unit, .. } = &self.0.mechanism else {
            return Ok(());
        };
        {
            let mut started = self.0.started.lock();
            let start = start_arguments(unit, pid, self.limited().then(|| self.quota()));
            let attach = attach_arguments(unit, pid);
            // The scope goes once its last agent exits: joining it then fails, and starting it
            // works, while a running scope refuses to start again. So when the call for what
            // the scope was last known to be fails, the other one is tried, and if both fail,
            // starting's error is the one that explains it. Right after the scope emptied, both
            // fail at once until systemd has collected it, so a pair that failed quickly is tried
            // once more a moment later.
            let join = |started: bool| {
                if started {
                    run("busctl", &attach).or_else(|_| run("busctl", &start))
                } else {
                    run("busctl", &start).or_else(|error| run("busctl", &attach).map_err(|_| error))
                }
            };
            let began = Instant::now();
            let mut result = join(*started);
            if result.is_err()
                && alive.load(Ordering::Acquire)
                && began.elapsed() < Duration::from_secs(1)
            {
                std::thread::sleep(Duration::from_millis(100));
                result = join(*started);
            }
            *started = result.is_ok();
            result?;
        }
        // Starting the scope returns once systemd has queued the job, before the pane has
        // moved; released earlier, the agent could start processes outside the scope.
        let deadline = Instant::now() + PLACEMENT_TIMEOUT;
        while !in_unit(pid, unit) {
            if !alive.load(Ordering::Acquire) {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(format!("systemd did not move the agent into {unit}"));
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        Ok(())
    }

    /// Reports, through `send`, that an agent runs unlimited and why: once per window, or
    /// again later if this report could not be sent.
    fn report(&self, send: impl FnOnce(Event) -> bool, reason: &str) {
        if self.0.reported.swap(true, Ordering::AcqRel) {
            return;
        }
        let message = format!(
            "The agent CPU limit could not be applied, so an agent runs unlimited: {reason}"
        );
        if !send(Event::CpuLimit(message)) {
            self.0.reported.store(false, Ordering::Release);
        }
    }
}

#[cfg(test)]
impl Limiter {
    /// A limit held by the scope `unit`, whatever this machine offers.
    pub fn scope(directory: &Path, unit: &str, percent: u8, cores: u32, limited: bool) -> Self {
        let mechanism = Mechanism::Scope {
            unit: unit.into(),
            directory: directory.to_owned(),
        };
        Self::build(mechanism, percent, cores, limited)
    }

    /// A limit that is off and can only lower priorities: it never calls systemd.
    pub fn off() -> Self {
        Self::build(
            Mechanism::Priority("no scope in tests".into()),
            DEFAULT_PERCENT,
            4,
            false,
        )
    }

    /// The scope's unit name, or why there is none.
    pub fn unit(&self) -> Result<&str, &str> {
        match &self.0.mechanism {
            Mechanism::Scope { unit, .. } => Ok(unit),
            Mechanism::Priority(reason) => Err(reason),
        }
    }
}

/// One pane's way into the scope.
struct Enrollment {
    limiter: Limiter,
    fifo: Fifo,
    pid: Option<u32>,
    alive: Arc<AtomicBool>,
}

impl Enrollment {
    /// Places the pane, releases it whatever happened, and tells the UI through `send` what
    /// went wrong, if anything did and the pane still runs.
    fn run(self, send: impl Fn(Event) -> bool) {
        let placed = match self.pid {
            Some(pid) => self.limiter.place(pid, &self.alive),
            None => Err("the pane has no process ID".into()),
        };
        // An agent that cannot be limited still runs.
        let released = release(&self.fifo.0, &self.alive);
        // A pane closed meanwhile is past caring.
        if !self.alive.load(Ordering::Acquire) {
            return;
        }
        match (placed, released) {
            (_, Err(error)) => {
                send(Event::CpuLimit(format!(
                    "An agent could not start: {error}"
                )));
            }
            (Err(error), Ok(())) => self.limiter.report(send, &error),
            (Ok(()), Ok(())) => {}
        }
    }
}

/// Lets a trampoline go on to start its agent. Until the trampoline has opened its FIFO,
/// opening the write end fails with ENXIO, and is tried again while the pane lives.
fn release(fifo: &Path, alive: &AtomicBool) -> Result<(), String> {
    let deadline = Instant::now() + RELEASE_TIMEOUT;
    loop {
        match OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(fifo)
        {
            Ok(mut file) => {
                return file
                    .write_all(b"\n")
                    .map_err(|error| format!("writing {}: {error}", fifo.display()));
            }
            Err(error) if error.raw_os_error() == Some(libc::ENXIO) => {
                // A pane closed before its trampoline opened the FIFO waits for nothing.
                if !alive.load(Ordering::Acquire) {
                    return Ok(());
                }
                if Instant::now() >= deadline {
                    return Err(format!("its trampoline never opened {}", fifo.display()));
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            Err(error) => return Err(format!("opening {}: {error}", fifo.display())),
        }
    }
}

/// Runs every process of the group that `pid` leads at a lower priority: a pane's own process
/// leads its group. Children inherit the niceness, and an unprivileged process cannot undo it.
pub fn lower_priority(pid: u32) {
    // It fails only for a group that is gone, which needs nothing.
    unsafe { libc::setpriority(libc::PRIO_PGRP, pid as libc::id_t, NICENESS) };
}

/// The quota for `percent` of `cores` CPUs, in microseconds of CPU time per second.
fn quota(percent: u8, cores: u32) -> u64 {
    u64::from(percent) * u64::from(cores) * 10_000
}

/// `percent` of `cores` CPUs as a number of CPUs, such as `4`, `1.5` or `5.28`.
fn share(percent: u8, cores: u32) -> String {
    let hundredths = u64::from(percent) * u64::from(cores);
    match hundredths % 100 {
        0 => format!("{}", hundredths / 100),
        fraction if fraction % 10 == 0 => format!("{}.{}", hundredths / 100, fraction / 10),
        fraction => format!("{}.{fraction:02}", hundredths / 100),
    }
}

/// `busctl` arguments that start `unit` holding `pid`: with `quota` and the low CPU weight
/// while the limit is on, and without a quota at the default weight while it is off.
fn start_arguments(unit: &str, pid: u32, quota: Option<u64>) -> Vec<String> {
    let pid = pid.to_string();
    let quota = quota.map(|quota| quota.to_string());
    let mut properties = vec!["PIDs", "au", "1", &pid];
    if let Some(quota) = &quota {
        properties.extend(["CPUQuotaPerSecUSec", "t", quota.as_str()]);
    }
    properties.extend([
        "CPUWeight",
        "t",
        if quota.is_some() {
            LIMITED_WEIGHT
        } else {
            DEFAULT_WEIGHT
        },
        // Collected even after an agent was killed, so that a later agent can start it again.
        "CollectMode",
        "s",
        "inactive-or-failed",
        // Agents may manage cgroups below it, as container tools do.
        "Delegate",
        "b",
        "true",
    ]);
    let count = if quota.is_some() { "5" } else { "4" };
    MANAGER
        .into_iter()
        .chain([
            "StartTransientUnit",
            "ssa(sv)a(sa(sv))",
            unit,
            "fail",
            count,
        ])
        .chain(properties)
        // No auxiliary units.
        .chain(["0"])
        .map(str::to_owned)
        .collect()
}

/// `busctl` arguments that move `pid` into the running `unit`.
fn attach_arguments(unit: &str, pid: u32) -> Vec<String> {
    let pid = pid.to_string();
    MANAGER
        .into_iter()
        .chain([
            "AttachProcessesToUnit",
            "ssau",
            unit,
            "/",
            "1",
            pid.as_str(),
        ])
        .map(str::to_owned)
        .collect()
}

/// `systemctl` arguments that apply `quota` and the low weight to the running `unit`, or
/// lift them: an empty quota is no quota. `CPUQuota` is a share of one CPU.
fn retune_arguments(unit: &str, quota: Option<u64>) -> Vec<String> {
    let mut arguments: Vec<String> = ["--user", "set-property", "--runtime", unit]
        .map(str::to_owned)
        .into();
    match quota {
        Some(quota) => arguments.extend([
            format!("CPUQuota={}%", quota / 10_000),
            format!("CPUWeight={LIMITED_WEIGHT}"),
        ]),
        None => arguments.extend([
            "CPUQuota=".to_owned(),
            format!("CPUWeight={DEFAULT_WEIGHT}"),
        ]),
    }
    arguments
}

/// Runs `program` to its end: what it says on standard error, or its exit status, is the
/// error. It runs in a process group of its own, out of reach of signals sent to Crit's.
fn run(program: &str, arguments: &[String]) -> Result<(), String> {
    let output = Command::new(program)
        .args(arguments)
        .stdin(Stdio::null())
        .process_group(0)
        .output()
        .map_err(|error| format!("{program}: {error}"))?;
    if output.status.success() {
        return Ok(());
    }
    // Terminal control bytes stay out of the UI.
    let message: String = String::from_utf8_lossy(&output.stderr)
        .trim()
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    Err(if message.is_empty() {
        format!("{program} exited with {}", output.status)
    } else {
        format!("{program}: {message}")
    })
}

/// Whether `pid` runs in `unit`'s own cgroup.
fn in_unit(pid: u32, unit: &str) -> bool {
    fs::read_to_string(format!("/proc/{pid}/cgroup")).is_ok_and(|groups| {
        groups.lines().any(|line| {
            line.strip_prefix("0::")
                .is_some_and(|path| path.rsplit('/').next() == Some(unit))
        })
    })
}

/// Creates the FIFO only Crit's user may use.
fn make_fifo(path: &Path) -> io::Result<Fifo> {
    let name = CString::new(path.as_os_str().as_bytes())?;
    if unsafe { libc::mkfifo(name.as_ptr(), 0o600) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(Fifo(path.to_owned()))
}

/// Why no scope can hold the agents here, if none can. Checked from files alone, without
/// starting a process: cgroup v2, a systemd user session to call, its tools, and a user
/// manager that runs Crit and may hand out the `cpu` controller.
fn probe() -> Result<(), String> {
    if fs::read_to_string("/sys/fs/cgroup/cgroup.controllers").is_err() {
        return Err("cgroup v2 is not available".into());
    }
    if std::env::var_os("XDG_RUNTIME_DIR").is_none_or(|value| value.is_empty()) {
        return Err("there is no systemd user session (XDG_RUNTIME_DIR is not set)".into());
    }
    for tool in ["busctl", "systemctl"] {
        if !runnable(tool, Path::new(".")) {
            return Err(format!("{tool} is not installed"));
        }
    }
    let groups = fs::read_to_string("/proc/self/cgroup").unwrap_or_default();
    let path = groups
        .lines()
        .find_map(|line| line.strip_prefix("0::"))
        .unwrap_or_default();
    // The user manager's own cgroup, such as `/user.slice/user-1000.slice/user@1000.service`.
    // It can only move processes that run below it, as Crit and its panes then do.
    let manager = path.find("/user@").and_then(|start| {
        let end = path[start + 1..]
            .find('/')
            .map_or(path.len(), |end| start + 1 + end);
        path[..end].ends_with(".service").then(|| &path[..end])
    });
    let Some(manager) = manager else {
        return Err("Crit does not run under the systemd user manager".into());
    };
    let controllers = fs::read_to_string(format!("/sys/fs/cgroup{manager}/cgroup.subtree_control"))
        .unwrap_or_default();
    if !controllers
        .split_whitespace()
        .any(|controller| controller == "cpu")
    {
        return Err("the systemd user manager cannot limit CPU use".into());
    }
    Ok(())
}

/// Whether `program` can be run, found the way a pane's spawn finds it: a path with a slash
/// from `cwd`, or a name in a `PATH` folder (relative ones from `cwd` as well).
fn runnable(program: &str, cwd: &Path) -> bool {
    if program.contains('/') {
        return executable(&cwd.join(program));
    }
    std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path).any(|folder| executable(&cwd.join(folder).join(program)))
    })
}

/// Whether `path` is a file this process may execute.
fn executable(path: &Path) -> bool {
    !path.is_dir()
        && CString::new(path.as_os_str().as_bytes())
            .is_ok_and(|path| unsafe { libc::access(path.as_ptr(), libc::X_OK) } == 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::FileTypeExt;

    #[test]
    fn quota_is_the_share_of_every_cpu_in_microseconds_per_second() {
        // All of one CPU is a million microseconds of CPU time per second.
        assert_eq!(quota(100, 1), 1_000_000);
        assert_eq!(quota(50, 8), 4_000_000);
        assert_eq!(quota(25, 16), 4_000_000);
        assert_eq!(quota(1, 1), 10_000);
        assert_eq!(quota(100, 256), 256_000_000);
        // systemctl's CPUQuota is the same share, in percent of one CPU.
        assert_eq!(
            retune_arguments("u", Some(quota(50, 16)))[4],
            "CPUQuota=800%"
        );
        assert_eq!(share(50, 8), "4");
        assert_eq!(share(25, 6), "1.5");
        assert_eq!(share(33, 16), "5.28");
        assert_eq!(share(1, 1), "0.01");
    }

    #[test]
    fn scope_calls_carry_the_quota_only_while_limited() {
        let manager = MANAGER.map(str::to_owned).to_vec();
        let expected = |tail: &[&str]| {
            let mut expected = manager.clone();
            expected.extend(tail.iter().map(|&word| word.to_owned()));
            expected
        };
        assert_eq!(
            start_arguments("crit-7-00ff.scope", 42, Some(4_000_000)),
            expected(&[
                "StartTransientUnit",
                "ssa(sv)a(sa(sv))",
                "crit-7-00ff.scope",
                "fail",
                "5",
                "PIDs",
                "au",
                "1",
                "42",
                "CPUQuotaPerSecUSec",
                "t",
                "4000000",
                "CPUWeight",
                "t",
                "20",
                "CollectMode",
                "s",
                "inactive-or-failed",
                "Delegate",
                "b",
                "true",
                "0",
            ])
        );
        assert_eq!(
            start_arguments("crit-7-00ff.scope", 42, None),
            expected(&[
                "StartTransientUnit",
                "ssa(sv)a(sa(sv))",
                "crit-7-00ff.scope",
                "fail",
                "4",
                "PIDs",
                "au",
                "1",
                "42",
                "CPUWeight",
                "t",
                "100",
                "CollectMode",
                "s",
                "inactive-or-failed",
                "Delegate",
                "b",
                "true",
                "0",
            ])
        );
        assert_eq!(
            attach_arguments("crit-7-00ff.scope", 42),
            expected(&[
                "AttachProcessesToUnit",
                "ssau",
                "crit-7-00ff.scope",
                "/",
                "1",
                "42"
            ])
        );
        let retune = |quota| retune_arguments("crit-7-00ff.scope", quota);
        assert_eq!(
            retune(Some(1_500_000)),
            [
                "--user",
                "set-property",
                "--runtime",
                "crit-7-00ff.scope",
                "CPUQuota=150%",
                "CPUWeight=20"
            ]
        );
        assert_eq!(
            retune(None),
            [
                "--user",
                "set-property",
                "--runtime",
                "crit-7-00ff.scope",
                "CPUQuota=",
                "CPUWeight=100"
            ]
        );
    }

    #[test]
    fn the_hint_and_notices_say_what_holds_the_agents() {
        let directory = tempfile::tempdir().unwrap();
        let scope = Limiter::scope(directory.path(), "crit-1-0.scope", 50, 8, true);
        assert_eq!(scope.hint(), "50%");
        assert_eq!(
            scope.describe(),
            "Agents limited to 50% of the CPU (4 of 8 cores)."
        );
        assert_eq!(scope.startup_notice(), None);
        scope.set_limited(false);
        assert_eq!(scope.hint(), "OFF");
        assert_eq!(scope.describe(), "Agent CPU limit off.");
        let one = Limiter::scope(directory.path(), "crit-1-0.scope", 25, 1, true);
        assert_eq!(
            one.describe(),
            "Agents limited to 25% of the CPU (0.25 of 1 core)."
        );

        let fallback = Limiter::build(
            Mechanism::Priority("busctl is not installed".into()),
            50,
            8,
            true,
        );
        assert_eq!(fallback.hint(), "LOW");
        assert!(!fallback.uses_scope());
        assert!(fallback.describe().contains("busctl is not installed"));
        assert!(fallback.startup_notice().is_some_and(|notice| {
            notice.contains("busctl is not installed") && notice.contains("50%")
        }));
        fallback.set_limited(false);
        assert_eq!(fallback.hint(), "OFF");
        assert!(fallback.describe().contains("started from now on"));
        // Off, there is nothing to warn about.
        assert_eq!(fallback.startup_notice(), None);
    }

    #[test]
    fn only_the_first_unlimited_agent_is_reported_unless_its_report_was_lost() {
        let limiter = Limiter::off();
        let mut sent = Vec::new();
        limiter.report(|_| false, "queue full");
        limiter.report(
            |event| {
                sent.push(event);
                true
            },
            "first",
        );
        limiter.report(
            |event| {
                sent.push(event);
                true
            },
            "second",
        );
        assert_eq!(sent.len(), 1);
        assert!(matches!(&sent[0], Event::CpuLimit(message) if message.ends_with(": first")));
    }

    #[test]
    fn panes_are_gated_only_where_a_scope_holds_them() {
        let directory = tempfile::tempdir().unwrap();
        let program = || {
            (
                "/bin/sh".to_owned(),
                vec!["-c".to_owned(), "exit 0".to_owned()],
            )
        };
        // Only the lower priority: nothing to wait for, and nothing at all while off.
        let fallback = Limiter::build(Mechanism::Priority("none".into()), 50, 8, true);
        let (spawned, args, gate) = fallback.prepare(1, program().0, program().1, directory.path());
        assert_eq!((spawned.as_str(), args.len()), ("/bin/sh", 2));
        assert!(matches!(gate, Some(Gate::Priority)));
        fallback.set_limited(false);
        assert!(
            fallback
                .prepare(1, program().0, program().1, directory.path())
                .2
                .is_none()
        );

        // A scope, even while off: turning the limit on later must reach this agent too.
        let scope = Limiter::scope(directory.path(), "crit-1-0.scope", 50, 8, false);
        let (spawned, args, gate) = scope.prepare(7, program().0, program().1, directory.path());
        let fifo = directory.path().join("cpu-gate-7");
        assert_eq!(spawned, "/bin/sh");
        assert_eq!(
            args,
            [
                "-c",
                TRAMPOLINE,
                fifo.to_str().unwrap(),
                "/bin/sh",
                "-c",
                "exit 0"
            ]
        );
        assert!(matches!(&gate, Some(Gate::Scope(Fifo(path))) if *path == fifo));
        assert!(fs::metadata(&fifo).unwrap().file_type().is_fifo());
        // Exiting Crit removes what is left of it.
        scope.remove_gates();
        assert!(!fifo.exists());
        drop(gate);

        // A program that cannot be found is spawned as it is, so its spawn fails as before.
        let (spawned, args, gate) =
            scope.prepare(8, "crit-no-such-agent".into(), Vec::new(), directory.path());
        assert_eq!((spawned.as_str(), args.len()), ("crit-no-such-agent", 0));
        assert!(gate.is_none());
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 0);
    }

    #[test]
    fn a_pane_that_systemd_refuses_still_starts_and_is_reported() {
        let directory = tempfile::tempdir().unwrap();
        // systemd refuses this unit name, and a machine without busctl fails the same way.
        let limiter = Limiter::scope(directory.path(), "not a unit", 50, 1, true);
        let (program, args, gate) = limiter.prepare(
            3,
            "/bin/sh".into(),
            vec![
                "-c".into(),
                "echo started > \"$0\"".into(),
                directory.path().join("ran").to_str().unwrap().into(),
            ],
            directory.path(),
        );
        let mut child = Command::new(&program).args(&args).spawn().unwrap();
        let (tx, events) = mpsc::sync_channel(4);
        let alive = Arc::new(AtomicBool::new(true));
        limiter.start(gate.unwrap(), 3, Some(child.id()), alive.clone(), tx);
        // Reported while the pane lives: one closed meanwhile is past caring.
        let Ok(Event::CpuLimit(message)) = events.recv_timeout(Duration::from_secs(5)) else {
            panic!("the failure was not reported");
        };
        assert!(message.contains("runs unlimited"), "{message}");
        assert!(child.wait().unwrap().success());
        alive.store(false, Ordering::Release);
        assert_eq!(
            fs::read_to_string(directory.path().join("ran")).unwrap(),
            "started\n"
        );
        // The trampoline's FIFO is gone once it is released.
        let deadline = Instant::now() + Duration::from_secs(5);
        while directory.path().join("cpu-gate-3").exists() {
            assert!(Instant::now() < deadline, "the FIFO was left behind");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn the_probe_finds_the_user_manager_from_files_alone() {
        // Whatever this machine offers, the probe answers without starting a process, and a
        // limiter it builds always has an answer for the hint.
        let directory = tempfile::Builder::new()
            .prefix("crit-probe-")
            .tempdir()
            .unwrap();
        let limiter = Limiter::with_cores(directory.path(), 50, 2, true);
        match limiter.unit() {
            Ok(unit) => assert_eq!(
                unit,
                format!(
                    "{}.scope",
                    directory.path().file_name().unwrap().to_str().unwrap()
                )
            ),
            Err(reason) => assert!(!reason.is_empty()),
        }
        assert!(["50%", "LOW"].contains(&limiter.hint()));
    }
}
