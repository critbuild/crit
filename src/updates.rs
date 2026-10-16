use anyhow::{Context, Result};
use std::{
    env,
    ffi::CString,
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, SeekFrom},
    os::unix::{
        ffi::OsStrExt,
        fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
        process::{CommandExt, ExitStatusExt},
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant, SystemTime},
};

const TOOLS: [(&str, &str, &str); 2] = [
    ("OMP", "omp", "update"),
    ("OpenCode", "opencode", "upgrade"),
];
/// How long an updater may run, even after Crit has quit. One still running by then has hung, as
/// `opencode upgrade` has: left waiting in epoll for hours, holding its runtime's memory and waking
/// the CPU, until killed by hand.
const UPDATE_DEADLINE: Duration = Duration::from_secs(10 * 60);
/// How long an updater stopped at its deadline gets to clean up before it is killed.
const STOP_GRACE: Duration = Duration::from_secs(30);
const DIAGNOSTIC_BYTES: u64 = 4096;
/// In the workspace state directory. Its modification time is when the updaters last started;
/// it holds no data.
const STAMP_FILE: &str = "updates.stamp";
/// The updaters start at most this often. Each one boots a heavy runtime that a low-power machine
/// feels at every launch, while the tools release far less often than Crit starts.
const UPDATE_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

// A private, automatically removed file captures both streams without pipe
// backpressure, updater banners, or unbounded in-memory output buffers.
struct UpdateProcess {
    child: Child,
    output: File,
    started: Instant,
    /// The deadline `timeout` holds the updater to; `None` when it runs without one.
    deadline: Option<Duration>,
}

/// Why an updater did not succeed.
enum Failure {
    /// It was still running at its deadline, and was stopped.
    TimedOut(Duration),
    /// It could not start or be waited for, or it exited unsuccessfully: how, and the end of
    /// what it printed.
    Error(String),
}

impl UpdateProcess {
    /// `None` when the program is not installed. Updaters outlive Crit, so only a process of
    /// their own can stop one that hangs: coreutils `timeout`, holding it to `deadline`. It leads
    /// the process group the updater runs in, so its signals reach what the updater started too:
    /// at the deadline it asks them all to stop (SIGTERM), and if the updater is still running
    /// `STOP_GRACE` later, it kills them (SIGKILL). Without `timeout`, the updater runs directly,
    /// with no deadline.
    fn spawn(program: &str, argument: &str, deadline: Duration) -> Result<Option<Self>> {
        let context = || format!("start {program} {argument}");
        // Looked up before anything starts: `timeout` starts whether or not the tool is
        // installed, and would report one that is not as a failure.
        let Some(path) = locate(program).with_context(context)? else {
            return Ok(None);
        };
        let timeout = locate("timeout").ok().flatten();
        let mut command = match &timeout {
            Some(timeout) => {
                let mut command = Command::new(timeout);
                command
                    .arg(format!("--kill-after={}s", STOP_GRACE.as_secs_f64()))
                    .arg(format!("{}s", deadline.as_secs_f64()))
                    .arg(&path);
                command
            }
            None => Command::new(&path),
        };
        let output = tempfile::tempfile().context("capture updater output")?;
        let started = Instant::now();
        let child = command
            .arg(argument)
            .stdin(Stdio::null())
            .stdout(output.try_clone()?)
            .stderr(output.try_clone()?)
            .env("NO_COLOR", "1")
            .process_group(0)
            .spawn()
            .with_context(context)?;
        Ok(Some(Self {
            child,
            output,
            started,
            deadline: timeout.is_some().then_some(deadline),
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
    fn poll(&mut self) -> Option<Result<(), Failure>> {
        let status = match self.child.try_wait() {
            Ok(None) => return None,
            Ok(Some(status)) => status,
            Err(error) => return Some(Err(Failure::Error(error.to_string()))),
        };
        if status.success() {
            return Some(Ok(()));
        }
        // How `timeout` ends at the deadline: exit status 124 once the updater has stopped or,
        // when it had to be killed, 137, or SIGKILL itself where `timeout` kills its whole
        // process group, itself included, as GNU's does. Before the deadline, these statuses are
        // the updater's own.
        if let Some(deadline) = self.deadline
            && self.started.elapsed() >= deadline
            && (matches!(status.code(), Some(124 | 137)) || status.signal() == Some(libc::SIGKILL))
        {
            return Some(Err(Failure::TimedOut(deadline)));
        }
        let detail = self
            .diagnostic()
            .unwrap_or_else(|error| format!("could not read updater output: {error}"));
        Some(Err(Failure::Error(if detail.is_empty() {
            status.to_string()
        } else {
            format!("{status}: {detail}")
        })))
    }
}

/// The updaters running in the background, each reported on once, when it finishes. Dropping
/// this leaves the ones still running alone: an interrupted install is worse than an orphan, and
/// each stays held to its deadline (see `UpdateProcess::spawn`).
#[derive(Default)]
pub struct Checks {
    /// In tool order, by the name their diagnostic carries. An updater that could not start keeps
    /// its error here for the first `poll`.
    updaters: Vec<(String, Result<UpdateProcess, String>)>,
}

/// Starts every updater at once, unless they already started less than `UPDATE_INTERVAL` ago, and
/// returns without waiting for any of them. `state_dir` is the directory of the workspace store
/// this window owns, which keeps the stamp. Without one nothing starts: updating is the job of the
/// window that owns the saved workspace, and with no usable state directory at all, nothing
/// would keep every launch from updating again.
pub fn start(state_dir: Option<&Path>) -> Checks {
    Checks::start(state_dir, &TOOLS)
}

impl Checks {
    /// `start`, with `tools` in place of the real updaters.
    fn start(state_dir: Option<&Path>, tools: &[(&str, &str, &str)]) -> Self {
        let Some(directory) = state_dir else {
            return Self::default();
        };
        let stamp = directory.join(STAMP_FILE);
        let now = SystemTime::now();
        if fresh(&stamp, now) {
            return Self::default();
        }
        // Updating matters more than the stamp: one that cannot be written only means the next
        // launch updates again.
        let _ = touch(&stamp, now);
        Self::spawn(tools, UPDATE_DEADLINE)
    }

    /// Starts each of `tools`, held to `deadline`.
    fn spawn(tools: &[(&str, &str, &str)], deadline: Duration) -> Self {
        let updaters = tools
            .iter()
            .filter_map(|&(name, program, argument)| {
                let process = match UpdateProcess::spawn(program, argument, deadline) {
                    Ok(Some(process)) => Ok(process),
                    // Not installed: nothing to update, nothing to report.
                    Ok(None) => return None,
                    Err(error) => Err(format!("{error:#}")),
                };
                Some((name.to_owned(), process))
            })
            .collect();
        Self { updaters }
    }

    /// Errors for the updaters that failed since the last call. Successful updates are silent.
    pub fn poll(&mut self) -> Vec<String> {
        let mut errors = Vec::new();
        self.updaters.retain_mut(|(name, process)| {
            let outcome = match process {
                Ok(process) => match process.poll() {
                    None => return true,
                    Some(outcome) => outcome,
                },
                Err(error) => Err(Failure::Error(std::mem::take(error))),
            };
            let failure = match outcome {
                Ok(()) => return false,
                Err(Failure::TimedOut(deadline)) => {
                    format!("timed out after {}", in_words(deadline))
                }
                Err(Failure::Error(detail)) => format!("failed: {detail}"),
            };
            errors.push(format!(
                "{name} update {failure}. Continuing with the installed version."
            ));
            false
        });
        errors
    }
}

/// Whether `stamp` says the updaters started less than `UPDATE_INTERVAL` before `now`. Only a
/// private file counts. A time in the future counts as fresh: the clock has gone back since it
/// was written (as on a machine without a battery-backed clock, before it syncs), so the stamp's
/// own time is the likelier one.
fn fresh(stamp: &Path, now: SystemTime) -> bool {
    fs::symlink_metadata(stamp).is_ok_and(|metadata| {
        private_file(&metadata)
            && metadata.modified().is_ok_and(|modified| {
                now.duration_since(modified)
                    .map_or(true, |age| age < UPDATE_INTERVAL)
            })
    })
}

/// Sets `stamp`'s modification time to `now`, creating it if it is missing. Like the workspace
/// state beside it, it is owner-private and never written through a link.
fn touch(stamp: &Path, now: SystemTime) -> io::Result<()> {
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(stamp)?;
    if !private_file(&file.metadata()?) {
        return Err(io::Error::other(
            "updates stamp is not a regular, singly linked file owned by the current user",
        ));
    }
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    file.set_modified(now)
}

/// What `restore` requires of its state files: a regular, singly linked file owned by the
/// current user.
fn private_file(metadata: &fs::Metadata) -> bool {
    metadata.is_file() && metadata.uid() == unsafe { libc::geteuid() } && metadata.nlink() == 1
}

/// Where `program` is, found the way spawning it finds it: a path with a slash as it is, a name in
/// the first `PATH` folder with an executable file by that name. `None` when there is no file by
/// that name. One that is there but cannot be run, with no other to run instead, is a permission
/// error, as it is to spawn: a failure to start, not a tool that is not installed.
fn locate(program: &str) -> io::Result<Option<PathBuf>> {
    let mut denied = false;
    let mut runnable = |candidate: PathBuf| match fs::metadata(&candidate) {
        Ok(metadata) if metadata.is_file() && executable(&candidate) => Some(candidate),
        Ok(_) => {
            denied = true;
            None
        }
        Err(error) => {
            denied |= error.kind() == io::ErrorKind::PermissionDenied;
            None
        }
    };
    let found = if program.contains('/') {
        runnable(program.into())
    } else {
        env::var_os("PATH").and_then(|path| {
            env::split_paths(&path).find_map(|folder| runnable(folder.join(program)))
        })
    };
    match found {
        None if denied => Err(io::Error::from_raw_os_error(libc::EACCES)),
        found => Ok(found),
    }
}

/// Whether this process may execute `path`.
fn executable(path: &Path) -> bool {
    CString::new(path.as_os_str().as_bytes())
        .is_ok_and(|path| unsafe { libc::access(path.as_ptr(), libc::X_OK) } == 0)
}

/// `duration` as a report words it: in whole minutes when it is some, otherwise in seconds.
fn in_words(duration: Duration) -> String {
    let seconds = duration.as_secs();
    let (count, unit) = if seconds >= 60 && seconds.is_multiple_of(60) {
        (seconds / 60, "minute")
    } else {
        (seconds, "second")
    };
    format!("{count} {unit}{}", if count == 1 { "" } else { "s" })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Polls until every updater has finished; the errors, in the order they came.
    fn finish(checks: &mut Checks) -> Vec<String> {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut errors = Vec::new();
        while !checks.updaters.is_empty() {
            assert!(Instant::now() < deadline, "an updater never finished");
            errors.extend(checks.poll());
            std::thread::sleep(Duration::from_millis(5));
        }
        errors
    }

    fn wait_for(path: &Path) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !path.exists() {
            assert!(
                Instant::now() < deadline,
                "{} never appeared",
                path.display()
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn missing_programs_are_silent() {
        let tools = [
            ("Absent", "/nonexistent/crit-test-tool", "update"),
            ("Also absent", "/nonexistent/crit-test-other", "upgrade"),
            // Looked up in PATH, as the real updaters are.
            ("Not on PATH", "crit-test-tool-not-on-path", "update"),
        ];
        let mut checks = Checks::spawn(&tools, UPDATE_DEADLINE);
        assert!(checks.updaters.is_empty());
        assert!(checks.poll().is_empty());
    }

    #[test]
    fn failed_updaters_are_reported_once() {
        let directory = tempfile::tempdir().unwrap();
        let failing = directory.path().join("failing.sh");
        fs::write(&failing, "echo refused >&2; exit 1").unwrap();
        // A program that exists but cannot run is a failure to start, not an absent tool.
        let locked = directory.path().join("locked");
        fs::write(&locked, "").unwrap();
        let tools = [
            ("Failing", "/bin/sh", failing.to_str().unwrap()),
            ("Succeeding", "/bin/true", "update"),
            ("Locked", locked.to_str().unwrap(), "update"),
        ];
        let mut checks = Checks::spawn(&tools, UPDATE_DEADLINE);
        let mut errors = finish(&mut checks);
        errors.sort();
        assert_eq!(
            errors,
            [
                "Failing update failed: exit status: 1: refused. Continuing with the installed version.".to_owned(),
                format!(
                    "Locked update failed: start {} update: Permission denied (os error 13). \
                     Continuing with the installed version.",
                    locked.display()
                ),
            ]
        );
        assert!(checks.poll().is_empty());
    }

    #[test]
    fn updaters_run_concurrently() {
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
        let mut errors = finish(&mut Checks::spawn(&tools, UPDATE_DEADLINE));
        errors.sort();
        assert_eq!(
            errors,
            [
                "First update failed: exit status: 1: failed: first.sh. Continuing with the \
                 installed version.",
                "Second update failed: exit status: 1: failed: second.sh. Continuing with the \
                 installed version.",
            ]
        );
    }

    #[test]
    fn dropping_checks_leaves_running_updaters_alone() {
        let directory = tempfile::tempdir().unwrap();
        let script = directory.path().join("slow.sh");
        // Finishes once `go` exists, which only happens after the drop below.
        fs::write(
            &script,
            r#"d=$(dirname "$0"); : > "$d/started"
for _ in $(seq 500); do
    if [ -e "$d/go" ]; then : > "$d/finished"; exit 0; fi
    sleep 0.01
done
exit 9"#,
        )
        .unwrap();
        let tools = [("Slow", "/bin/sh", script.to_str().unwrap())];
        let checks = Checks::spawn(&tools, UPDATE_DEADLINE);
        wait_for(&directory.path().join("started"));
        drop(checks);
        fs::write(directory.path().join("go"), "").unwrap();
        wait_for(&directory.path().join("finished"));
    }

    /// Whether process `pid` still runs: it is neither gone nor dead and waiting to be reaped.
    fn running(pid: u32) -> bool {
        // The state follows the command name, which ends at the last parenthesis.
        fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
            stat.rsplit_once(") ")
                .is_some_and(|(_, fields)| !fields.starts_with('Z'))
        })
    }

    #[test]
    fn updaters_past_the_deadline_are_stopped_with_everything_they_started() {
        let directory = tempfile::tempdir().unwrap();
        let hanging = directory.path().join("hanging.sh");
        // Records itself and a child of its own, then waits for the child, which would outlast
        // the deadline by far.
        fs::write(
            &hanging,
            r#"d=$(dirname "$0"); sleep 60 & echo $! > "$d/child"; echo $$ > "$d/updater"; wait"#,
        )
        .unwrap();
        let tools = [
            ("Hanging", "/bin/sh", hanging.to_str().unwrap()),
            // Found in PATH, as the real updaters are, and done well within the deadline.
            ("Quick", "true", "update"),
        ];
        let mut checks = Checks::spawn(&tools, Duration::from_secs(1));
        assert_eq!(checks.updaters.len(), 2);
        assert_eq!(
            finish(&mut checks),
            ["Hanging update timed out after 1 second. Continuing with the installed version."]
        );
        for process in ["updater", "child"] {
            let pid = fs::read_to_string(directory.path().join(process)).unwrap();
            let pid = pid.trim().parse().unwrap();
            let deadline = Instant::now() + Duration::from_secs(10);
            while running(pid) {
                assert!(
                    Instant::now() < deadline,
                    "the {process} outlived the deadline"
                );
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }

    #[test]
    fn ending_like_a_timeout_before_the_deadline_is_a_plain_failure() {
        // `timeout` passes the updater's exit status on, and dies of the signal that killed it.
        let directory = tempfile::tempdir().unwrap();
        let exiting = directory.path().join("exiting.sh");
        fs::write(&exiting, "exit 124").unwrap();
        let killed = directory.path().join("killed.sh");
        fs::write(&killed, "kill -KILL $$").unwrap();
        let tools = [
            ("Exiting", "/bin/sh", exiting.to_str().unwrap()),
            ("Killed", "/bin/sh", killed.to_str().unwrap()),
        ];
        let mut errors = finish(&mut Checks::spawn(&tools, UPDATE_DEADLINE));
        errors.sort();
        assert_eq!(
            errors,
            [
                "Exiting update failed: exit status: 124. Continuing with the installed version.",
                "Killed update failed: signal: 9 (SIGKILL). Continuing with the installed version.",
            ]
        );
    }

    #[test]
    fn deadlines_are_worded_in_whole_minutes_where_possible() {
        assert_eq!(in_words(UPDATE_DEADLINE), "10 minutes");
        assert_eq!(in_words(Duration::from_secs(60)), "1 minute");
        assert_eq!(in_words(Duration::from_secs(90)), "90 seconds");
        assert_eq!(in_words(Duration::from_secs(1)), "1 second");
    }

    /// One updater that always starts, so a start that is not skipped has one running.
    const ALWAYS: [(&str, &str, &str); 1] = [("True", "/bin/true", "update")];

    fn modified(path: &Path) -> SystemTime {
        fs::metadata(path).unwrap().modified().unwrap()
    }

    fn set_modified(path: &Path, time: SystemTime) {
        File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(time)
            .unwrap();
    }

    #[test]
    fn without_the_saved_workspace_nothing_starts() {
        assert!(Checks::start(None, &ALWAYS).updaters.is_empty());
    }

    #[test]
    fn a_missing_stamp_starts_the_updaters_and_is_written() {
        let directory = tempfile::tempdir().unwrap();
        let stamp = directory.path().join(STAMP_FILE);
        let mut checks = Checks::start(Some(directory.path()), &ALWAYS);
        assert_eq!(checks.updaters.len(), 1);
        assert!(finish(&mut checks).is_empty());
        let metadata = fs::symlink_metadata(&stamp).unwrap();
        assert!(metadata.is_file());
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
        // A launch later the same day leaves both the updaters and the stamp alone.
        assert!(
            Checks::start(Some(directory.path()), &ALWAYS)
                .updaters
                .is_empty()
        );
        assert_eq!(modified(&stamp), metadata.modified().unwrap());
    }

    #[test]
    fn a_stale_stamp_starts_the_updaters_and_is_renewed() {
        let directory = tempfile::tempdir().unwrap();
        let stamp = directory.path().join(STAMP_FILE);
        fs::write(&stamp, "").unwrap();
        fs::set_permissions(&stamp, fs::Permissions::from_mode(0o644)).unwrap();
        let stale = SystemTime::now() - UPDATE_INTERVAL - Duration::from_secs(60);
        set_modified(&stamp, stale);
        let mut checks = Checks::start(Some(directory.path()), &ALWAYS);
        assert_eq!(checks.updaters.len(), 1);
        assert!(finish(&mut checks).is_empty());
        assert!(modified(&stamp) > stale + UPDATE_INTERVAL);
        assert_eq!(
            fs::metadata(&stamp).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(
            Checks::start(Some(directory.path()), &ALWAYS)
                .updaters
                .is_empty()
        );
    }

    #[test]
    fn a_fresh_stamp_starts_nothing_and_is_left_alone() {
        let directory = tempfile::tempdir().unwrap();
        let stamp = directory.path().join(STAMP_FILE);
        fs::write(&stamp, "").unwrap();
        let now = SystemTime::now();
        // A stamp from the future is fresh too: the clock went back after it was written.
        for written in [
            now - UPDATE_INTERVAL + Duration::from_secs(60),
            now + UPDATE_INTERVAL,
        ] {
            set_modified(&stamp, written);
            let before = modified(&stamp);
            assert!(
                Checks::start(Some(directory.path()), &ALWAYS)
                    .updaters
                    .is_empty()
            );
            assert_eq!(modified(&stamp), before);
        }
    }

    #[test]
    fn a_stamp_that_cannot_be_written_does_not_hold_updates_back() {
        let directory = tempfile::tempdir().unwrap();
        let stamp = directory.path().join(STAMP_FILE);
        let target = directory.path().join("elsewhere");
        fs::write(&target, "").unwrap();
        // Stale, so a stamp written through the link would renew it.
        set_modified(&target, SystemTime::now() - UPDATE_INTERVAL * 2);
        let before = modified(&target);
        std::os::unix::fs::symlink(&target, &stamp).unwrap();
        // The link is never followed, so every launch updates, as before there were stamps.
        for _ in 0..2 {
            let mut checks = Checks::start(Some(directory.path()), &ALWAYS);
            assert_eq!(checks.updaters.len(), 1);
            assert!(finish(&mut checks).is_empty());
        }
        assert_eq!(modified(&target), before);
        assert!(fs::symlink_metadata(&stamp).unwrap().is_symlink());
    }
}
