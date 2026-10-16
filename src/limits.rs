//! Keeps the agents from taking the CPU and memory that a game or other apps need.
//!
//! Every OMP and OpenCode pane of a Crit window runs in one transient systemd user scope,
//! with everything it starts, so that one CPU quota and one memory share hold the agents back
//! as a whole and a low CPU weight lets other apps go first whenever the CPUs are contended.
//! The memory share is `MemoryHigh`: past it the kernel slows the agents down and reclaims
//! their memory, so that their pages go to swap before a game's, where a hard cap
//! (`MemoryMax`) would kill a build midway. The scope also keeps the agents on the machine's
//! least-preferred cores, which leaves the best ones to a game. While limited, the agents run
//! in the batch scheduling class, whose threads never preempt others when they wake up, and
//! those started while limited have a coarse timer slack. Shells are never limited. Where no
//! scope can hold the agents, those started while the limit is on run at a lower priority
//! instead, which holds no memory and no cores.

use crate::event::Event;
use std::{
    cmp::Reverse,
    collections::BTreeMap,
    ffi::{CString, OsStr},
    fs::{self, OpenOptions},
    io::{self, Write},
    marker::PhantomData,
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

/// The agents' share of the machine's CPU and memory while the limit is on, unless
/// `--agent-cpu` chose another: half, so that a game keeps the other half.
pub const DEFAULT_PERCENT: u8 = 50;

/// The agents' CPU weight while limited. Other apps have the default 100, so whenever the CPUs
/// are contended they go first, even while the agents are within their quota.
const LIMITED_WEIGHT: &str = "20";

/// The default CPU weight, which the agents get back while the limit is off.
const DEFAULT_WEIGHT: &str = "100";

/// The niceness of agents started while the limit is on where no scope can hold them.
const NICENESS: libc::c_int = 10;

/// The timer slack of agents started while the limit is on, in nanoseconds: 1 ms rather than
/// the 50 µs a process gets by default, so that the kernel may serve their timers together, in
/// fewer wake-ups of the CPUs.
const TIMER_SLACK: libc::c_ulong = 1_000_000;

/// The files in `/sys/devices/system/cpu/cpuN` that rank CPU N, higher for a CPU the scheduler
/// prefers, in the order they are trusted: AMD's preferred-core ranking, the highest
/// performance that ACPI CPPC reports, and the capacity that hybrid and ARM CPUs report.
const RANKINGS: [&str; 3] = [
    "cpufreq/amd_pstate_prefcore_ranking",
    "acpi_cppc/highest_perf",
    "cpu_capacity",
];

/// The files in `/sys/devices/system/cpu/cpuN/topology` that list the CPUs of N's physical
/// core: the current name, then the older one.
const SIBLINGS: [&str; 2] = ["core_cpus_list", "thread_siblings_list"];

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
    /// The agents' share of the machine's CPU and memory while limited, in percent.
    percent: u8,
    /// How many CPUs Crit may use: the share is of these.
    cores: u32,
    /// `percent` as the key hint shows it.
    label: String,
    limited: AtomicBool,
    /// Held while the scope is started, joined or retuned, so that whichever call comes last
    /// applies the setting as it is then.
    scope: parking_lot::Mutex<ScopeState>,
    /// Whether an agent that runs unlimited has been reported: only the first one is.
    reported: AtomicBool,
}

/// What Crit knows of its scope.
#[derive(Default)]
struct ScopeState {
    /// Whether the scope is believed to run.
    started: bool,
    /// The scope's cgroup directory, once an agent was seen in it: where `retune` finds the
    /// threads of the agents that run.
    cgroup: Option<PathBuf>,
}

enum Mechanism {
    /// Agents run in this transient scope of the systemd user manager, and their trampolines'
    /// FIFOs live in `directory`. While limited, the scope holds their memory to `memory_high`
    /// bytes too; `None` where the machine's memory is not known or the user manager cannot
    /// limit memory, and then the agents get no memory share. It holds them to the CPUs `cpus`
    /// as well, in ascending order; `None` where they keep every CPU.
    Scope {
        unit: String,
        directory: PathBuf,
        memory_high: Option<u64>,
        cpus: Option<Vec<u32>>,
    },
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
    /// agents, and the cores it holds them to, are settled once, here, from files alone.
    pub fn new(directory: &Path, percent: u8, limited: bool) -> Self {
        // The share is of the machine, like the memory share: `available_parallelism` would
        // report a quota Crit itself runs under, as when it is started from an agent pane.
        let cores = match unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) } {
            count if count >= 1 => count as u32,
            _ => 1,
        };
        // Where it cannot be read, the agents get no memory share.
        let memory = fs::read_to_string("/proc/meminfo")
            .ok()
            .as_deref()
            .and_then(total_memory)
            .unwrap_or(0);
        let cpus = agent_cpus(Path::new("/sys"), percent);
        Self::with_cores(directory, percent, cores, cpus, memory, limited)
    }

    /// `new`, with the shares taken of `cores` CPUs and of `memory` bytes: 0 is memory that is
    /// not known, of which the agents get no share. While limited, the agents run on `cpus`,
    /// if any: `None` keeps every CPU.
    pub fn with_cores(
        directory: &Path,
        percent: u8,
        cores: u32,
        cpus: Option<Vec<u32>>,
        memory: u64,
        limited: bool,
    ) -> Self {
        let mechanism = match probe() {
            Ok(controllers) => match directory.file_name().and_then(OsStr::to_str) {
                // `crit-<pid>-<16 hex>` is unique to this run, so a scope left by an earlier
                // Crit with the same process ID cannot be mistaken for this one.
                Some(name) => Mechanism::Scope {
                    unit: format!("{name}.scope"),
                    directory: directory.to_owned(),
                    // Where the user manager cannot limit memory, or CPUs, systemd would ignore
                    // them.
                    memory_high: memory_high(percent, memory).filter(|_| controllers.memory),
                    cpus: cpus.filter(|_| controllers.cpuset),
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
            scope: parking_lot::Mutex::new(ScopeState::default()),
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

    /// The startup warning when only a lower priority can hold agents.
    pub fn startup_warning(&self) -> Option<String> {
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

    /// Applies the setting to the agents that already run. The call waits on systemd, so only
    /// the worker thread makes it. A scope that is not running holds no agent: there is nothing
    /// to change, and the next agent starts it with the setting as it is then. The agents'
    /// threads change scheduling class too, but keep the timer slack they started with: a
    /// process can change another's only through `/proc/PID/timerslack_ns`, which takes
    /// CAP_SYS_NICE.
    pub fn retune(&self) -> Result<(), String> {
        let Mechanism::Scope {
            unit,
            memory_high,
            cpus,
            ..
        } = &self.0.mechanism
        else {
            return Ok(());
        };
        let scope = self.0.scope.lock();
        let limited = self.limited();
        let quota = limited.then(|| self.quota());
        let arguments = retune_arguments(unit, quota, *memory_high, cpus.as_deref());
        let result = match run("busctl", &arguments) {
            // "Unit crit-….scope not found."
            Err(error) if error.contains("not found") || error.contains("not loaded") => Ok(()),
            result => result.map_err(|error| {
                format!("The agent CPU limit could not be changed for running agents: {error}")
            }),
        };
        // The class is each thread's own, which systemd leaves alone, whatever it answered.
        if let Some(cgroup) = &scope.cgroup {
            reclass(cgroup, limited);
        }
        result
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

    /// While the limit is on, gives the calling thread the agents' scheduling class and timer
    /// slack until the guard is dropped, so that the agent pane it spawns meanwhile, which
    /// `gate` limits, starts with them; `None` while the limit is off, and for a pane that runs
    /// unlimited.
    pub fn batch(&self, gate: &Gate) -> Option<Batch> {
        (self.limited() && !matches!(gate, Gate::Unlimited(_))).then(Batch::enter)
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

    /// Moves `pid` into the scope, starting the scope if no agent holds it now, waits until
    /// systemd has moved it, and remembers the scope's cgroup, where `retune` finds the agents.
    fn place(&self, pid: u32, alive: &AtomicBool) -> Result<(), String> {
        let Mechanism::Scope {
            unit,
            memory_high,
            cpus,
            ..
        } = &self.0.mechanism
        else {
            return Ok(());
        };
        {
            let mut scope = self.0.scope.lock();
            let quota = self.limited().then(|| self.quota());
            let start = start_arguments(unit, pid, quota, *memory_high, cpus.as_deref());
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
            let mut result = join(scope.started);
            if result.is_err()
                && alive.load(Ordering::Acquire)
                && began.elapsed() < Duration::from_secs(1)
            {
                std::thread::sleep(Duration::from_millis(100));
                result = join(scope.started);
            }
            scope.started = result.is_ok();
            result?;
        }
        // Starting the scope returns once systemd has queued the job, before the pane has
        // moved; released earlier, the agent could start processes outside the scope.
        let deadline = Instant::now() + PLACEMENT_TIMEOUT;
        let cgroup = loop {
            if let Some(cgroup) = unit_cgroup(pid, unit) {
                break cgroup;
            }
            if !alive.load(Ordering::Acquire) {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(format!("systemd did not move the agent into {unit}"));
            }
            std::thread::sleep(Duration::from_millis(2));
        };
        let mut scope = self.0.scope.lock();
        // The pane started in the class of the limit as it was then. Had F3 been pressed since,
        // a retune that ran meanwhile could not find the pane in the scope, so it takes the
        // class of the limit as it is now. Until it is released, its trampoline is its only
        // thread.
        set_class(pid as libc::pid_t, self.limited());
        scope.cgroup = Some(cgroup);
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
    /// A limit held by the scope `unit`, whatever this machine offers, with the memory share
    /// taken of `memory` bytes.
    pub fn scope(
        directory: &Path,
        unit: &str,
        percent: u8,
        cores: u32,
        memory: u64,
        limited: bool,
    ) -> Self {
        let mechanism = Mechanism::Scope {
            unit: unit.into(),
            directory: directory.to_owned(),
            memory_high: memory_high(percent, memory),
            cpus: None,
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

    /// The agents' memory share while limited, in bytes, if they get one.
    pub fn memory_high(&self) -> Option<u64> {
        match &self.0.mechanism {
            Mechanism::Scope { memory_high, .. } => *memory_high,
            Mechanism::Priority(_) => None,
        }
    }

    /// The CPUs the agents run on while limited, if they are held to some.
    pub fn cpus(&self) -> Option<&[u32]> {
        match &self.0.mechanism {
            Mechanism::Scope { cpus, .. } => cpus.as_deref(),
            Mechanism::Priority(_) => None,
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

/// The agents' scheduling class and timer slack on the calling thread, from which an agent it
/// spawns meanwhile takes them, and keeps them through `exec`: the batch class, whose threads
/// never preempt others when they wake up, and `TIMER_SLACK`. The thread gets its own back
/// when this is dropped.
pub struct Batch {
    /// The thread's own policy, if the batch class took the place of the normal one.
    policy: Option<libc::c_int>,
    /// The thread's own timer slack, if it was finer than the agents'.
    slack: Option<libc::c_ulong>,
    /// Both are the calling thread's, so this must be dropped on it.
    _thread: PhantomData<*const ()>,
}

impl Batch {
    fn enter() -> Self {
        // Only a thread in the normal class trades it for the batch class. One in the idle
        // class, which a tool like ananicy may give Crit, already starts the agents lower and
        // could not leave that class without a privilege, and a real-time one could not
        // return to its class without one.
        let own = unsafe { libc::sched_getscheduler(0) };
        let policy = (own >= 0
            && class(own) == libc::SCHED_OTHER
            && set_policy(0, libc::SCHED_BATCH | (own & libc::SCHED_RESET_ON_FORK)))
        .then_some(own);
        // A coarser slack stays as it is.
        let slack = timer_slack()
            .filter(|&slack| (1..TIMER_SLACK).contains(&slack) && set_timer_slack(TIMER_SLACK));
        Self {
            policy,
            slack,
            _thread: PhantomData,
        }
    }
}

impl Drop for Batch {
    fn drop(&mut self) {
        // Any thread may leave the batch class for the normal one.
        if let Some(policy) = self.policy {
            set_policy(0, policy);
        }
        if let Some(slack) = self.slack {
            set_timer_slack(slack);
        }
    }
}

/// The scheduling class in `policy`, as `sched_getscheduler` returns it: without the flag that
/// resets the class in children.
fn class(policy: libc::c_int) -> libc::c_int {
    policy & !libc::SCHED_RESET_ON_FORK
}

/// Puts thread `tid`, or the calling thread for 0, in scheduling `policy`, at the priority 0
/// that every class but the real-time ones takes; its nice value stays. Whether it worked.
fn set_policy(tid: libc::pid_t, policy: libc::c_int) -> bool {
    let parameters = libc::sched_param { sched_priority: 0 };
    unsafe { libc::sched_setscheduler(tid, policy, &parameters) == 0 }
}

/// Moves thread `tid` from the normal scheduling class to the batch class if `batch`, or back.
/// A thread in any other class is left alone: one in the idle class, where a tool like ananicy
/// puts some of the tools that agents run, is lower still, and a real-time one is someone's
/// choice. So is a thread that is gone. The flag that resets the class in children stays as it
/// is: only a privileged process may clear it.
fn set_class(tid: libc::pid_t, batch: bool) {
    let (from, to) = if batch {
        (libc::SCHED_OTHER, libc::SCHED_BATCH)
    } else {
        (libc::SCHED_BATCH, libc::SCHED_OTHER)
    };
    let policy = unsafe { libc::sched_getscheduler(tid) };
    if policy >= 0 && class(policy) == from {
        // It fails only for a thread that is gone, or that is not Crit's user's to change.
        set_policy(tid, to | (policy & libc::SCHED_RESET_ON_FORK));
    }
}

/// `set_class` for every thread in `cgroup` and in the cgroups below it, which the agents may
/// make: each lists its own threads in `cgroup.threads`.
fn reclass(cgroup: &Path, batch: bool) {
    let mut cgroups = vec![cgroup.to_owned()];
    while let Some(cgroup) = cgroups.pop() {
        // A cgroup that is gone holds no thread.
        if let Ok(threads) = fs::read_to_string(cgroup.join("cgroup.threads")) {
            for tid in threads.lines().filter_map(|tid| tid.parse().ok()) {
                set_class(tid, batch);
            }
        }
        if let Ok(entries) = fs::read_dir(&cgroup) {
            cgroups.extend(
                entries
                    .flatten()
                    .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
                    .map(|entry| entry.path()),
            );
        }
    }
}

/// The calling thread's timer slack in nanoseconds, if it can be told.
fn timer_slack() -> Option<libc::c_ulong> {
    let unused: libc::c_ulong = 0;
    let slack = unsafe { libc::prctl(libc::PR_GET_TIMERSLACK, unused, unused, unused, unused) };
    libc::c_ulong::try_from(slack).ok()
}

/// Sets the calling thread's timer slack to `slack` nanoseconds, where 0 would mean its
/// default. Whether it worked.
fn set_timer_slack(slack: libc::c_ulong) -> bool {
    let unused: libc::c_ulong = 0;
    unsafe { libc::prctl(libc::PR_SET_TIMERSLACK, slack, unused, unused, unused) == 0 }
}

/// The quota for `percent` of `cores` CPUs, in microseconds of CPU time per second.
fn quota(percent: u8, cores: u32) -> u64 {
    u64::from(percent) * u64::from(cores) * 10_000
}

/// The agents' memory share, `percent` of `memory` bytes, in bytes. Memory that is not known
/// is 0, which gives them none rather than a share that would stop them.
fn memory_high(percent: u8, memory: u64) -> Option<u64> {
    // In 128 bits, the product of any amount of memory and percentage fits.
    let bytes = u128::from(memory) * u128::from(percent) / 100;
    u64::try_from(bytes).ok().filter(|&bytes| bytes > 0)
}

/// The machine's memory in bytes, from the text of `/proc/meminfo`, whose `MemTotal` line
/// gives it in kiB.
fn total_memory(meminfo: &str) -> Option<u64> {
    let mut words = meminfo
        .lines()
        .find_map(|line| line.strip_prefix("MemTotal:"))?
        .split_whitespace();
    let kib: u64 = words.next()?.parse().ok()?;
    // The kernel's kB are kiB; any other unit is not understood.
    if words.next() != Some("kB") {
        return None;
    }
    kib.checked_mul(1024)
}

/// The CPUs the agents run on while limited, in ascending order: the machine's least-preferred
/// physical cores, whole, as few as cover `percent` of its online CPUs. A core ranks as its
/// best CPU, by the first of `RANKINGS` that every online CPU has, and Intel's efficiency cores
/// rank below all others. Of cores that rank the same, the highest-numbered go first, which
/// leaves CPU 0 to the system and its interrupts. `None` where that is every online CPU, or
/// where `sys`, the root of sysfs, does not tell the topology.
pub fn agent_cpus(sys: &Path, percent: u8) -> Option<Vec<u32>> {
    let cpu = sys.join("devices/system/cpu");
    let read = |path: PathBuf| fs::read_to_string(path).ok();
    let online = cpu_list(&read(cpu.join("online"))?)?;
    let ranks = RANKINGS
        .iter()
        .find_map(|file| {
            online
                .iter()
                .map(|n| {
                    read(cpu.join(format!("cpu{n}/{file}")))?
                        .trim()
                        .parse()
                        .ok()
                })
                .collect::<Option<Vec<u64>>>()
        })
        // Without any, every CPU ranks the same.
        .unwrap_or_else(|| vec![0; online.len()]);
    // Listed on Intel's hybrid CPUs alone.
    let atoms = read(sys.join("devices/cpu_atom/cpus"))
        .and_then(|list| cpu_list(&list))
        .unwrap_or_default();
    // Each physical core by its first online CPU, with its rank and its online CPUs.
    let mut cores = BTreeMap::<u32, ((bool, u64), Vec<u32>)>::new();
    for (&n, &rank) in online.iter().zip(&ranks) {
        let siblings = SIBLINGS
            .iter()
            .find_map(|file| read(cpu.join(format!("cpu{n}/topology/{file}"))))?;
        let first = cpu_list(&siblings)?
            .into_iter()
            .find(|sibling| online.binary_search(sibling).is_ok())
            .unwrap_or(n);
        let rank = (atoms.binary_search(&n).is_err(), rank);
        let core = cores.entry(first).or_insert((rank, Vec::new()));
        core.0 = core.0.max(rank);
        core.1.push(n);
    }
    let mut cores: Vec<_> = cores.into_iter().collect();
    cores.sort_by_key(|&(first, (rank, _))| (rank, Reverse(first)));
    let wanted = (online.len() * usize::from(percent)).div_ceil(100);
    let mut cpus = Vec::with_capacity(online.len());
    for (_, (_, core)) in cores {
        if cpus.len() >= wanted {
            break;
        }
        cpus.extend(core);
    }
    cpus.sort_unstable();
    (!cpus.is_empty() && cpus.len() < online.len()).then_some(cpus)
}

/// The CPUs in `list`, written as sysfs and `/proc/PID/status` write them, such as
/// `0-3,8,10-11`, in ascending order; `None` for anything else.
pub fn cpu_list(list: &str) -> Option<Vec<u32>> {
    let mut cpus = Vec::new();
    for range in list.trim().split(',').filter(|range| !range.is_empty()) {
        let (first, last) = range.split_once('-').unwrap_or((range, range));
        let (first, last): (u32, u32) = (first.parse().ok()?, last.parse().ok()?);
        if first > last {
            return None;
        }
        cpus.extend(first..=last);
    }
    cpus.sort_unstable();
    cpus.dedup();
    Some(cpus)
}

/// `cpus` as the bytes of systemd's `AllowedCPUs`: CPU 8j+i is bit i of byte j.
fn cpu_mask(cpus: &[u32]) -> Vec<u8> {
    let length = cpus.iter().max().map_or(0, |&last| last as usize / 8 + 1);
    let mut mask = vec![0; length];
    for &cpu in cpus {
        mask[cpu as usize / 8] |= 1 << (cpu % 8);
    }
    mask
}

/// The `AllowedCPUs` property, as `busctl` takes it, that holds the agents to `cpus`; none
/// lets them use every CPU.
fn allowed_cpus(cpus: &[u32]) -> Vec<String> {
    let mask = cpu_mask(cpus);
    [
        "AllowedCPUs".to_owned(),
        "ay".to_owned(),
        mask.len().to_string(),
    ]
    .into_iter()
    .chain(mask.iter().map(u8::to_string))
    .collect()
}

/// `busctl` arguments that start `unit` holding `pid`: with `quota`, the low CPU weight, and
/// the memory share `memory_high` and the CPUs `cpus`, for those the agents get, while the
/// limit is on, and at the default weight with none of them while it is off.
fn start_arguments(
    unit: &str,
    pid: u32,
    quota: Option<u64>,
    memory_high: Option<u64>,
    cpus: Option<&[u32]>,
) -> Vec<String> {
    let pid = pid.to_string();
    let quota = quota.map(|quota| quota.to_string());
    let memory_high = memory_high
        .filter(|_| quota.is_some())
        .map(|bytes| bytes.to_string());
    let cpus = cpus.filter(|_| quota.is_some()).map(allowed_cpus);
    let mut properties = vec!["PIDs", "au", "1", &pid];
    if let Some(quota) = &quota {
        properties.extend(["CPUQuotaPerSecUSec", "t", quota.as_str()]);
    }
    if let Some(memory_high) = &memory_high {
        properties.extend(["MemoryHigh", "t", memory_high.as_str()]);
    }
    if let Some(cpus) = &cpus {
        properties.extend(cpus.iter().map(String::as_str));
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
    // PIDs, CPUWeight, CollectMode and Delegate, and the shares and CPUs while limited.
    let count = 4
        + usize::from(quota.is_some())
        + usize::from(memory_high.is_some())
        + usize::from(cpus.is_some());
    let count = count.to_string();
    MANAGER
        .into_iter()
        .chain([
            "StartTransientUnit",
            "ssa(sv)a(sa(sv))",
            unit,
            "fail",
            count.as_str(),
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

/// `busctl` arguments that apply `quota`, the low weight, and the memory share `memory_high`
/// and the CPUs `cpus`, for those the agents get, to the running `unit`, or lift them. Lifting
/// sets the quota and the share to infinity (`u64::MAX`), which is how systemd spells "no
/// limit", and the CPUs to none, which is every CPU. The same bus as `start_arguments`, so
/// that retuning reaches the manager wherever starting a scope did.
fn retune_arguments(
    unit: &str,
    quota: Option<u64>,
    memory_high: Option<u64>,
    cpus: Option<&[u32]>,
) -> Vec<String> {
    let infinity = u64::MAX.to_string();
    let quota_value = quota.map_or_else(|| infinity.clone(), |quota| quota.to_string());
    let memory_value = memory_high.map(|bytes| {
        if quota.is_some() {
            bytes.to_string()
        } else {
            infinity.clone()
        }
    });
    let cpus = cpus.map(|cpus| allowed_cpus(if quota.is_some() { cpus } else { &[] }));
    let mut properties = vec!["CPUQuotaPerSecUSec", "t", quota_value.as_str()];
    if let Some(memory_value) = &memory_value {
        properties.extend(["MemoryHigh", "t", memory_value.as_str()]);
    }
    if let Some(cpus) = &cpus {
        properties.extend(cpus.iter().map(String::as_str));
    }
    properties.extend([
        "CPUWeight",
        "t",
        if quota.is_some() {
            LIMITED_WEIGHT
        } else {
            DEFAULT_WEIGHT
        },
    ]);
    let count = 2 + usize::from(memory_value.is_some()) + usize::from(cpus.is_some());
    let count = count.to_string();
    MANAGER
        .into_iter()
        // Runtime only: the scope is gone with its last agent anyway.
        .chain(["SetUnitProperties", "sba(sv)", unit, "true", count.as_str()])
        .chain(properties)
        .map(str::to_owned)
        .collect()
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

/// The cgroup directory of `unit`, if `pid` runs in the unit's own cgroup.
fn unit_cgroup(pid: u32, unit: &str) -> Option<PathBuf> {
    let groups = fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?;
    let path = groups.lines().find_map(|line| line.strip_prefix("0::"))?;
    (path.rsplit('/').next() == Some(unit))
        .then(|| Path::new("/sys/fs/cgroup").join(path.trim_start_matches('/')))
}

/// Creates the FIFO only Crit's user may use.
fn make_fifo(path: &Path) -> io::Result<Fifo> {
    let name = CString::new(path.as_os_str().as_bytes())?;
    if unsafe { libc::mkfifo(name.as_ptr(), 0o600) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(Fifo(path.to_owned()))
}

/// What the systemd user manager can hold in a scope besides CPU time.
struct Controllers {
    /// The agents' memory share.
    memory: bool,
    /// The CPUs the agents run on.
    cpuset: bool,
}

/// Why no scope can hold the agents here, if none can, or else what else it can hold.
/// Checked from files alone, without starting a process: cgroup v2, a systemd user session to
/// call, `busctl` to call it with, and a user manager that runs Crit and may hand out the `cpu`
/// controller, and the `memory` one for the memory share and `cpuset` for the agents' CPUs.
fn probe() -> Result<Controllers, String> {
    if fs::read_to_string("/sys/fs/cgroup/cgroup.controllers").is_err() {
        return Err("cgroup v2 is not available".into());
    }
    if std::env::var_os("XDG_RUNTIME_DIR").is_none_or(|value| value.is_empty()) {
        return Err("there is no systemd user session (XDG_RUNTIME_DIR is not set)".into());
    }
    if !runnable("busctl", Path::new(".")) {
        return Err("busctl is not installed".into());
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
    let hands_out = |name: &str| {
        controllers
            .split_whitespace()
            .any(|controller| controller == name)
    };
    if !hands_out("cpu") {
        return Err("the systemd user manager cannot limit CPU use".into());
    }
    Ok(Controllers {
        memory: hands_out("memory"),
        cpuset: hands_out("cpuset"),
    })
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
        // Retuning sends the same share, in the same unit.
        assert_eq!(
            retune_arguments("u", Some(quota(50, 16)), None, None)[MANAGER.len() + 7],
            "8000000"
        );
    }

    #[test]
    fn the_memory_share_is_the_same_share_of_the_machines_memory_in_bytes() {
        // Half of 32 GiB is 16 GiB.
        assert_eq!(memory_high(50, 32 << 30), Some(17_179_869_184));
        assert_eq!(memory_high(100, 32 << 30), Some(32 << 30));
        // Rounded down to whole bytes, and no amount of memory overflows.
        assert_eq!(memory_high(33, 1001), Some(330));
        assert_eq!(memory_high(100, u64::MAX), Some(u64::MAX));
        assert_eq!(memory_high(50, u64::MAX), Some(u64::MAX / 2));
        // Memory that is not known gives no share, rather than one that would stop the agents.
        assert_eq!(memory_high(50, 0), None);
        assert_eq!(memory_high(1, 99), None);
    }

    #[test]
    fn the_machines_memory_is_read_from_proc_meminfo_in_kib() {
        let meminfo = "MemTotal:       32779832 kB\n\
                       MemFree:         1607220 kB\n\
                       MemAvailable:   19432516 kB\n";
        assert_eq!(total_memory(meminfo), Some(32_779_832 * 1024));
        assert_eq!(total_memory("MemFree: 1 kB\nMemTotal: 2 kB\n"), Some(2048));
        // Anything else is memory that is not known.
        for meminfo in [
            "",
            "MemFree:         1607220 kB\n",
            "MemTotal:       lots kB\n",
            "MemTotal:       32779832\n",
            "MemTotal:       32779832 MB\n",
            "MemTotal:       18446744073709551615 kB\n",
        ] {
            assert_eq!(total_memory(meminfo), None, "{meminfo:?}");
        }
        // This machine's.
        let meminfo = fs::read_to_string("/proc/meminfo").unwrap();
        assert!(total_memory(&meminfo).is_some_and(|bytes| bytes > 0));
    }

    #[test]
    fn scope_calls_carry_the_shares_and_cpus_only_while_limited() {
        let manager = MANAGER.map(str::to_owned).to_vec();
        let expected = |tail: &[&str]| {
            let mut expected = manager.clone();
            expected.extend(tail.iter().map(|&word| word.to_owned()));
            expected
        };
        // The 5800X3D's least-preferred cores, at half of its 16 CPUs.
        let cpus: &[u32] = &[1, 4, 6, 7, 9, 12, 14, 15];
        assert_eq!(
            start_arguments(
                "crit-7-00ff.scope",
                42,
                Some(4_000_000),
                Some(16 << 30),
                Some(cpus)
            ),
            expected(&[
                "StartTransientUnit",
                "ssa(sv)a(sa(sv))",
                "crit-7-00ff.scope",
                "fail",
                "7",
                "PIDs",
                "au",
                "1",
                "42",
                "CPUQuotaPerSecUSec",
                "t",
                "4000000",
                "MemoryHigh",
                "t",
                "17179869184",
                "AllowedCPUs",
                "ay",
                "2",
                "210",
                "210",
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
        // Without a memory share where the machine's memory is not known, nor CPUs where the
        // agents keep every one.
        assert_eq!(
            start_arguments("crit-7-00ff.scope", 42, Some(4_000_000), None, None),
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
        // While the limit is off, with neither share and every CPU.
        assert_eq!(
            start_arguments("crit-7-00ff.scope", 42, None, Some(16 << 30), Some(cpus)),
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
        let retune = |quota, memory_high, cpus| {
            retune_arguments("crit-7-00ff.scope", quota, memory_high, cpus)
        };
        assert_eq!(
            retune(Some(1_500_000), Some(16 << 30), Some(cpus)),
            expected(&[
                "SetUnitProperties",
                "sba(sv)",
                "crit-7-00ff.scope",
                "true",
                "4",
                "CPUQuotaPerSecUSec",
                "t",
                "1500000",
                "MemoryHigh",
                "t",
                "17179869184",
                "AllowedCPUs",
                "ay",
                "2",
                "210",
                "210",
                "CPUWeight",
                "t",
                "20"
            ])
        );
        // Lifted, both shares go to infinity, which systemd spells as the largest u64, and the
        // CPUs to none, which is every CPU.
        assert_eq!(
            retune(None, Some(16 << 30), Some(cpus)),
            expected(&[
                "SetUnitProperties",
                "sba(sv)",
                "crit-7-00ff.scope",
                "true",
                "4",
                "CPUQuotaPerSecUSec",
                "t",
                "18446744073709551615",
                "MemoryHigh",
                "t",
                "18446744073709551615",
                "AllowedCPUs",
                "ay",
                "0",
                "CPUWeight",
                "t",
                "100"
            ])
        );
        // Without a memory share or CPUs, the scope's memory and CPUs are left alone either way.
        assert_eq!(
            retune(Some(1_500_000), None, None),
            expected(&[
                "SetUnitProperties",
                "sba(sv)",
                "crit-7-00ff.scope",
                "true",
                "2",
                "CPUQuotaPerSecUSec",
                "t",
                "1500000",
                "CPUWeight",
                "t",
                "20"
            ])
        );
        assert_eq!(
            retune(None, None, None),
            expected(&[
                "SetUnitProperties",
                "sba(sv)",
                "crit-7-00ff.scope",
                "true",
                "2",
                "CPUQuotaPerSecUSec",
                "t",
                "18446744073709551615",
                "CPUWeight",
                "t",
                "100"
            ])
        );
        // CPUs alone.
        assert_eq!(
            retune(None, None, Some(&[3])),
            expected(&[
                "SetUnitProperties",
                "sba(sv)",
                "crit-7-00ff.scope",
                "true",
                "3",
                "CPUQuotaPerSecUSec",
                "t",
                "18446744073709551615",
                "AllowedCPUs",
                "ay",
                "0",
                "CPUWeight",
                "t",
                "100"
            ])
        );
    }

    #[test]
    fn allowed_cpus_is_a_little_endian_bitmask_of_bytes() {
        // CPU 8j+i is bit i of byte j.
        assert_eq!(cpu_mask(&[1, 4, 6, 7, 9, 12, 14, 15]), [0b1101_0010; 2]);
        assert_eq!(cpu_mask(&[0]), [1]);
        assert_eq!(cpu_mask(&[7]), [128]);
        assert_eq!(cpu_mask(&[8]), [0, 1]);
        assert_eq!(cpu_mask(&[3, 63]), [8, 0, 0, 0, 0, 0, 0, 128]);
        assert_eq!(cpu_mask(&[]), [0_u8; 0]);
        assert_eq!(
            allowed_cpus(&[0, 9, 16]),
            ["AllowedCPUs", "ay", "3", "1", "2", "1"]
        );
        // None: every CPU.
        assert_eq!(allowed_cpus(&[]), ["AllowedCPUs", "ay", "0"]);
    }

    #[test]
    fn cpu_lists_are_read_as_sysfs_writes_them() {
        assert_eq!(cpu_list("0-15\n"), Some((0..16).collect()));
        assert_eq!(
            cpu_list("1,4,6-7,9,12,14-15"),
            Some(vec![1, 4, 6, 7, 9, 12, 14, 15])
        );
        assert_eq!(cpu_list("8,0"), Some(vec![0, 8]));
        assert_eq!(cpu_list("3\n"), Some(vec![3]));
        assert_eq!(cpu_list("\n"), Some(Vec::new()));
        for list in ["a", "3-1", "1-", "-1", "0-3:2/4", "1;2"] {
            assert_eq!(cpu_list(list), None, "{list:?}");
        }
    }

    /// Writes `text` to `path` below `root`, making the folders on the way.
    fn put(root: &Path, path: &str, text: &str) {
        let path = root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    /// A sysfs root with `count` online CPUs, CPU N on the physical core that `core` says, with
    /// `siblings` naming the file that lists each core's CPUs.
    fn topology(
        count: u32,
        siblings: &str,
        core: impl Fn(u32) -> &'static [u32],
    ) -> tempfile::TempDir {
        let sys = tempfile::tempdir().unwrap();
        put(
            sys.path(),
            "devices/system/cpu/online",
            &format!("0-{}\n", count - 1),
        );
        for cpu in 0..count {
            let list: Vec<String> = core(cpu).iter().map(u32::to_string).collect();
            put(
                sys.path(),
                &format!("devices/system/cpu/cpu{cpu}/topology/{siblings}"),
                &format!("{}\n", list.join(",")),
            );
        }
        sys
    }

    #[test]
    fn the_agents_get_the_least_preferred_whole_cores_by_amds_ranking() {
        // A Ryzen 7 5800X3D: CPUs N and N+8 share core N.
        static CORES: [[u32; 2]; 8] = [
            [0, 8],
            [1, 9],
            [2, 10],
            [3, 11],
            [4, 12],
            [5, 13],
            [6, 14],
            [7, 15],
        ];
        let sys = topology(16, "thread_siblings_list", |cpu| &CORES[cpu as usize % 8]);
        let ranking = [186, 181, 196, 196, 171, 191, 176, 166];
        for cpu in 0..16 {
            let rank = ranking[cpu % 8];
            let path = |file| format!("devices/system/cpu/cpu{cpu}/{file}");
            put(
                sys.path(),
                &path("cpufreq/amd_pstate_prefcore_ranking"),
                &format!("{rank}\n"),
            );
            // Trusted after AMD's ranking: these would pick other cores.
            put(
                sys.path(),
                &path("acpi_cppc/highest_perf"),
                &format!("{}\n", 400 - rank),
            );
            put(sys.path(), &path("cpu_capacity"), "1024\n");
        }
        // Cores 7, 4, 6 and 1 rank lowest: both CPUs of each.
        assert_eq!(
            agent_cpus(sys.path(), 50),
            Some(vec![1, 4, 6, 7, 9, 12, 14, 15])
        );
        // As few whole cores as cover the share: 1% of 16 CPUs is one core.
        assert_eq!(agent_cpus(sys.path(), 1), Some(vec![7, 15]));
        assert_eq!(agent_cpus(sys.path(), 25), Some(vec![4, 7, 12, 15]));
        assert_eq!(agent_cpus(sys.path(), 26), Some(vec![4, 6, 7, 12, 14, 15]));
        // Where a CPU lacks AMD's ranking, the next ranking every CPU has is the one used.
        fs::remove_file(
            sys.path()
                .join("devices/system/cpu/cpu3/cpufreq/amd_pstate_prefcore_ranking"),
        )
        .unwrap();
        assert_eq!(agent_cpus(sys.path(), 25), Some(vec![2, 3, 10, 11]));
        // Every CPU, or an unreadable topology, holds the agents to none.
        assert_eq!(agent_cpus(sys.path(), 100), None);
        assert_eq!(agent_cpus(sys.path(), 95), None);
        fs::remove_dir_all(sys.path().join("devices/system/cpu/cpu5/topology")).unwrap();
        assert_eq!(agent_cpus(sys.path(), 50), None);
        fs::remove_dir_all(sys.path().join("devices")).unwrap();
        assert_eq!(agent_cpus(sys.path(), 50), None);
    }

    #[test]
    fn intels_efficiency_cores_rank_below_all_others() {
        // Six performance cores with two CPUs each, then four efficiency cores with one.
        static CORES: [&[u32]; 16] = [
            &[0, 1],
            &[0, 1],
            &[2, 3],
            &[2, 3],
            &[4, 5],
            &[4, 5],
            &[6, 7],
            &[6, 7],
            &[8, 9],
            &[8, 9],
            &[10, 11],
            &[10, 11],
            &[12],
            &[13],
            &[14],
            &[15],
        ];
        let sys = topology(16, "core_cpus_list", |cpu| CORES[cpu as usize]);
        put(sys.path(), "devices/cpu_atom/cpus", "12-15\n");
        put(sys.path(), "devices/cpu_core/cpus", "0-11\n");
        assert_eq!(agent_cpus(sys.path(), 25), Some(vec![12, 13, 14, 15]));
        assert_eq!(agent_cpus(sys.path(), 10), Some(vec![14, 15]));
        // Then the performance cores, the highest-numbered first.
        assert_eq!(
            agent_cpus(sys.path(), 30),
            Some(vec![10, 11, 12, 13, 14, 15])
        );
        assert_eq!(agent_cpus(sys.path(), 50), Some((8..16).collect()));
        // Below all others even where a ranking would put them higher.
        for cpu in 0..16 {
            let capacity = if cpu < 12 { 512 } else { 1024 };
            put(
                sys.path(),
                &format!("devices/system/cpu/cpu{cpu}/cpu_capacity"),
                &format!("{capacity}\n"),
            );
        }
        assert_eq!(agent_cpus(sys.path(), 25), Some(vec![12, 13, 14, 15]));
    }

    #[test]
    fn without_a_ranking_the_highest_numbered_cores_go_first() {
        // Four cores whose two CPUs are N and N+4, and CPU 0 stays free.
        static CORES: [[u32; 2]; 4] = [[0, 4], [1, 5], [2, 6], [3, 7]];
        let sys = topology(8, "core_cpus_list", |cpu| &CORES[cpu as usize % 4]);
        assert_eq!(agent_cpus(sys.path(), 50), Some(vec![2, 3, 6, 7]));
        assert_eq!(agent_cpus(sys.path(), 1), Some(vec![3, 7]));
        assert_eq!(agent_cpus(sys.path(), 75), Some(vec![1, 2, 3, 5, 6, 7]));
        // Whole cores: 76% of 8 CPUs is 7 of them, which takes all four cores.
        assert_eq!(agent_cpus(sys.path(), 76), None);
        // An offline CPU is neither counted nor chosen: 40% of the seven online CPUs is three,
        // which core 3, now one CPU, and core 2 cover.
        put(sys.path(), "devices/system/cpu/online", "0-6\n");
        assert_eq!(agent_cpus(sys.path(), 40), Some(vec![2, 3, 6]));
        // On a machine of one CPU, any share is every CPU.
        let single = topology(1, "core_cpus_list", |_| &[0]);
        assert_eq!(agent_cpus(single.path(), 50), None);
    }

    /// The scheduling policy of thread `tid`, or of the calling thread for 0.
    fn policy(tid: u32) -> libc::c_int {
        unsafe { libc::sched_getscheduler(tid as libc::pid_t) }
    }

    /// Runs `test` on a thread of its own, in the normal scheduling class with the default
    /// timer slack, so that what it does to its thread's class stays there; skipped where no
    /// thread here may take the normal class, as from the idle one.
    fn on_a_normal_thread(test: impl FnOnce() + Send + 'static) {
        std::thread::spawn(move || {
            // The tests may run in the batch class, which any thread may leave.
            set_class(0, false);
            if class(policy(0)) != libc::SCHED_OTHER {
                eprintln!("skipped: this thread cannot take the normal scheduling class");
                return;
            }
            assert!(set_timer_slack(50_000));
            test();
        })
        .join()
        .unwrap();
    }

    #[test]
    fn agents_start_in_the_batch_class_with_a_coarse_timer_slack() {
        on_a_normal_thread(|| {
            // Only for agents that the limit holds.
            let limiter = Limiter::off();
            assert!(limiter.batch(&Gate::Priority).is_none());
            limiter.set_limited(true);
            assert!(limiter.batch(&Gate::Unlimited("none".into())).is_none());
            let own = policy(0);
            let batch = limiter.batch(&Gate::Priority).unwrap();
            assert_eq!(class(policy(0)), libc::SCHED_BATCH);
            assert_eq!(timer_slack(), Some(TIMER_SLACK));
            // A child takes both from the thread that starts it, and keeps them through `exec`:
            // the shell reads its own slack.
            let mut child = Command::new("sleep").arg("60").spawn().unwrap();
            let report = Command::new("/bin/sh")
                .args([
                    "-c",
                    "read slack < /proc/self/timerslack_ns; echo \"$slack\"",
                ])
                .output()
                .unwrap();
            drop(batch);
            let child_policy = policy(child.id());
            child.kill().unwrap();
            child.wait().unwrap();
            // The thread has its own back, and the child keeps the agents'.
            assert_eq!(policy(0), own);
            assert_eq!(timer_slack(), Some(50_000));
            assert_eq!(class(child_policy), libc::SCHED_BATCH);
            assert_eq!(String::from_utf8_lossy(&report.stdout), "1000000\n");
        });
    }

    #[test]
    fn a_thread_in_the_idle_class_or_with_a_coarser_slack_keeps_them() {
        std::thread::spawn(|| {
            // Any thread may take the idle class, as ananicy gives some.
            assert!(set_policy(
                0,
                libc::SCHED_IDLE | (policy(0) & libc::SCHED_RESET_ON_FORK)
            ));
            assert!(set_timer_slack(5_000_000));
            let batch = Batch::enter();
            assert_eq!(class(policy(0)), libc::SCHED_IDLE);
            assert_eq!(timer_slack(), Some(5_000_000));
            drop(batch);
            assert_eq!(class(policy(0)), libc::SCHED_IDLE);
            assert_eq!(timer_slack(), Some(5_000_000));
        })
        .join()
        .unwrap();
    }

    #[test]
    fn retuning_moves_threads_between_the_normal_and_batch_classes_alone() {
        // Two agents' processes: one in the normal class, and one in the idle class, where a
        // tool like ananicy puts some, which stays there.
        let spawn = || Command::new("sleep").arg("60").spawn().unwrap();
        let (mut normal, mut idle) = (spawn(), spawn());
        set_class(normal.id() as libc::pid_t, false);
        set_policy(
            idle.id() as libc::pid_t,
            libc::SCHED_IDLE | (policy(idle.id()) & libc::SCHED_RESET_ON_FORK),
        );
        let classes = || (class(policy(normal.id())), class(policy(idle.id())));
        let checked = classes() == (libc::SCHED_OTHER, libc::SCHED_IDLE);
        // Their threads, as cgroups list them: the idle one in a cgroup below, and with a
        // thread that is gone and a line that is no thread.
        let threads = |pid: u32| {
            let tasks = fs::read_dir(format!("/proc/{pid}/task")).unwrap();
            let tids: Vec<String> = tasks
                .map(|task| task.unwrap().file_name().into_string().unwrap())
                .collect();
            tids.join("\n")
        };
        let cgroup = tempfile::tempdir().unwrap();
        let listed = format!("{}\n{}\nnone\n", threads(normal.id()), i32::MAX);
        fs::write(cgroup.path().join("cgroup.threads"), listed).unwrap();
        fs::create_dir(cgroup.path().join("tools")).unwrap();
        let listed = format!("{}\n", threads(idle.id()));
        fs::write(cgroup.path().join("tools/cgroup.threads"), listed).unwrap();
        let mut seen = Vec::new();
        if checked {
            reclass(cgroup.path(), true);
            seen.push(classes());
            reclass(cgroup.path(), false);
            seen.push(classes());
        }
        for child in [&mut normal, &mut idle] {
            child.kill().unwrap();
            child.wait().unwrap();
        }
        if !checked {
            eprintln!("skipped: a child here cannot take the normal scheduling class");
            return;
        }
        assert_eq!(
            seen,
            [
                (libc::SCHED_BATCH, libc::SCHED_IDLE),
                (libc::SCHED_OTHER, libc::SCHED_IDLE)
            ]
        );
        // Gone threads and cgroups are no error.
        reclass(&cgroup.path().join("gone"), true);
    }

    #[test]
    fn the_hint_and_startup_warning_say_what_holds_the_agents() {
        let directory = tempfile::tempdir().unwrap();
        let scope = Limiter::scope(directory.path(), "crit-1-0.scope", 50, 16, 32 << 30, true);
        assert_eq!(scope.hint(), "50%");
        assert_eq!(scope.startup_warning(), None);
        scope.set_limited(false);
        assert_eq!(scope.hint(), "OFF");

        let fallback = Limiter::build(
            Mechanism::Priority("busctl is not installed".into()),
            50,
            8,
            true,
        );
        assert_eq!(fallback.hint(), "LOW");
        assert!(!fallback.uses_scope());
        assert!(fallback.startup_warning().is_some_and(|warning| {
            warning.contains("busctl is not installed") && warning.contains("50%")
        }));
        fallback.set_limited(false);
        assert_eq!(fallback.hint(), "OFF");
        // Off, there is nothing to warn about.
        assert_eq!(fallback.startup_warning(), None);
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
        let scope = Limiter::scope(directory.path(), "crit-1-0.scope", 50, 8, 32 << 30, false);
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
        let limiter = Limiter::scope(directory.path(), "not a unit", 50, 1, 32 << 30, true);
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
        let limiter = Limiter::with_cores(directory.path(), 50, 2, Some(vec![1]), 32 << 30, true);
        match limiter.unit() {
            Ok(unit) => assert_eq!(
                unit,
                format!(
                    "{}.scope",
                    directory.path().file_name().unwrap().to_str().unwrap()
                )
            ),
            // The lower priority holds no memory, and no CPUs.
            Err(reason) => assert!(
                !reason.is_empty() && limiter.memory_high().is_none() && limiter.cpus().is_none()
            ),
        }
        // The scope holds the agents to the CPUs it was given, where the user manager can.
        assert!(matches!(limiter.cpus(), None | Some([1])));
        assert!(["50%", "LOW"].contains(&limiter.hint()));
    }
}
