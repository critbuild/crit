use anyhow::{Context, Result};
use semver::Version;
use std::{
    env,
    ffi::CString,
    fs::{self, File},
    io::{self, Read, Seek, SeekFrom},
    os::unix::{
        ffi::OsStrExt,
        process::{CommandExt, ExitStatusExt},
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::mpsc,
    time::{Duration, Instant},
};

pub const TOOLS: [(&str, &str, &str); 2] = [
    ("OMP", "omp", "update"),
    ("OpenCode", "opencode", "upgrade"),
];

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum State {
    Checking,
    Ready,
    Failed,
    #[default]
    Missing,
}
/// How long an updater may run, even after Crit has quit. One still running by then has hung, as
/// `opencode upgrade` has: left waiting in epoll for hours, holding its runtime's memory and waking
/// the CPU, until killed by hand.
const UPDATE_DEADLINE: Duration = Duration::from_secs(10 * 60);
/// How long an updater stopped at its deadline gets to clean up before it is killed.
const STOP_GRACE: Duration = Duration::from_secs(30);
const DIAGNOSTIC_BYTES: u64 = 4096;
/// A version lookup must not leave a runtime or network request hanging.
const CHECK_DEADLINE: Duration = Duration::from_secs(15);

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
#[derive(Debug)]
enum Failure {
    /// It was still running at its deadline, and was stopped.
    TimedOut(Duration),
    /// It could not start or be waited for, or it exited unsuccessfully: how, and the end of
    /// what it printed.
    Error(String),
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TimedOut(deadline) => write!(f, "timed out after {}", in_words(*deadline)),
            Self::Error(detail) => f.write_str(detail),
        }
    }
}

impl std::error::Error for Failure {}

impl UpdateProcess {
    /// `None` when the program is not installed. Updaters outlive Crit, so only a process of
    /// their own can stop one that hangs: coreutils `timeout`, holding it to `deadline`. It leads
    /// the process group the updater runs in, so its signals reach what the updater started too:
    /// at the deadline it asks them all to stop (SIGTERM), and if the updater is still running
    /// `STOP_GRACE` later, it kills them (SIGKILL). Without `timeout`, the updater runs directly,
    /// with no deadline.
    #[cfg(test)]
    fn spawn(program: &str, argument: &str, deadline: Duration) -> Result<Option<Self>> {
        let Some(path) = locate(program).with_context(|| format!("find {program}"))? else {
            return Ok(None);
        };
        Self::spawn_at(&path, &[argument], deadline).map(Some)
    }

    fn spawn_at(path: &Path, arguments: &[&str], deadline: Duration) -> Result<Self> {
        let context = || format!("start {} {}", path.display(), arguments.join(" "));
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
            .args(arguments)
            .stdin(Stdio::null())
            .stdout(output.try_clone()?)
            .stderr(output.try_clone()?)
            .env("NO_COLOR", "1")
            .process_group(0)
            .spawn()
            .with_context(context)?;
        Ok(Self {
            child,
            output,
            started,
            deadline: timeout.is_some().then_some(deadline),
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

    fn wait(&mut self) -> Result<(), Failure> {
        loop {
            if let Some(outcome) = self.poll() {
                return outcome;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

enum Check {
    Running(UpdateProcess),
    Ready,
    Missing,
}

/// Background release checks and the installers they start. Dropping this never interrupts an
/// installer; its separate `timeout` process still holds it to the deadline.
#[derive(Default)]
pub struct Checks {
    pending: Option<mpsc::Receiver<(&'static str, Result<Check, String>)>>,
    updaters: Vec<(String, Result<UpdateProcess, String>)>,
    states: [State; TOOLS.len()],
}

/// Checks every owning-window launch, without waiting for either the network or an agent runtime.
/// Other windows leave the installed tools alone.
pub fn start(owns_workspace: bool) -> Checks {
    if !owns_workspace {
        return Checks::default();
    }
    let (tx, rx) = mpsc::channel();
    let mut checks = Checks {
        pending: Some(rx),
        states: [State::Checking; TOOLS.len()],
        ..Checks::default()
    };
    for (name, program, argument) in TOOLS {
        let tx = tx.clone();
        if let Err(error) = std::thread::Builder::new()
            .name(format!("{program}-update"))
            .spawn(move || {
                let outcome =
                    check_and_update(program, argument).map_err(|error| format!("{error:#}"));
                let _ = tx.send((name, outcome));
            })
        {
            checks
                .updaters
                .push((name.to_owned(), Err(error.to_string())));
        }
    }
    checks
}

impl Checks {
    /// Startup may launch agents only after every version check and installer has finished.
    pub fn is_running(&self) -> bool {
        self.pending.is_some() || !self.updaters.is_empty()
    }

    pub fn states(&self) -> &[State; TOOLS.len()] {
        &self.states
    }

    #[cfg(test)]
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
        Self {
            updaters,
            pending: None,
            ..Self::default()
        }
    }

    /// Errors for the updaters that failed since the last call. Successful updates are silent.
    pub fn poll(&mut self) -> Vec<String> {
        while let Some(pending) = &self.pending {
            match pending.try_recv() {
                Ok((name, Ok(check))) => {
                    let state = match check {
                        Check::Running(process) => {
                            self.updaters.push((name.to_owned(), Ok(process)));
                            State::Checking
                        }
                        Check::Ready => State::Ready,
                        Check::Missing => State::Missing,
                    };
                    if let Some(index) = TOOLS.iter().position(|tool| tool.0 == name) {
                        self.states[index] = state;
                    }
                }
                Ok((name, Err(error))) => {
                    self.updaters.push((name.to_owned(), Err(error)));
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => self.pending = None,
            }
        }
        let mut errors = Vec::new();
        let states = &mut self.states;
        self.updaters.retain_mut(|(name, process)| {
            let outcome = match process {
                Ok(process) => match process.poll() {
                    None => return true,
                    Some(outcome) => outcome,
                },
                Err(error) => Err(Failure::Error(std::mem::take(error))),
            };
            if let Some(index) = TOOLS.iter().position(|tool| tool.0 == name.as_str()) {
                states[index] = if outcome.is_ok() {
                    State::Ready
                } else {
                    State::Failed
                };
            }
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

fn check_and_update(program: &str, argument: &str) -> Result<Check> {
    let Some(path) = locate(program).with_context(|| format!("find {program}"))? else {
        return Ok(Check::Missing);
    };
    // OMP's own updater already performs a metadata-only check before downloading anything.
    // Let it resolve its build-service override, installation method and stable/canary setting.
    if program == "omp" {
        return UpdateProcess::spawn_at(&path, &[argument], UPDATE_DEADLINE).map(Check::Running);
    }
    let installed = installed_version(&path)?;
    let Some(tag) = opencode_tag(&installed) else {
        // Other majors and custom builds carry channel information that `--version` cannot
        // reveal. Preserve their native release check rather than guessing another feed.
        return UpdateProcess::spawn_at(&path, &[argument], UPDATE_DEADLINE).map(Check::Running);
    };
    let metadata = release_json(&format!("https://registry.npmjs.org/opencode-ai/{tag}"))?;
    let published = parse_version(
        metadata["version"]
            .as_str()
            .context("release metadata has no version")?,
    )?;
    update_if_newer(&path, argument, &installed, &published)
        .map(|process| process.map_or(Check::Ready, Check::Running))
}

fn opencode_tag(version: &Version) -> Option<&str> {
    if version.major > 1 {
        return None;
    }
    if version.pre.is_empty() {
        return Some("latest");
    }
    // Official previews encode the complete channel before the final UTC YYYYMMDDHHmm suffix.
    // A custom version need not identify its channel: never silently move it to stable.
    if let Some((channel, timestamp)) = version.pre.as_str().rsplit_once('-')
        && (version.major, version.minor, version.patch) == (0, 0, 0)
        && !channel.is_empty()
        && timestamp.len() == 12
        && timestamp.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Some(if channel == "local" {
            "latest"
        } else {
            channel
        });
    }
    None
}

fn parse_version(text: &str) -> Result<Version> {
    let text = text.trim();
    Version::parse(text.strip_prefix('v').unwrap_or(text))
        .with_context(|| format!("invalid tool version {text:?}"))
}

/// npm's manifest describes this exact executable. Reading it avoids booting OpenCode's runtime
/// just to ask its version; standalone binaries and other installation layouts use `--version`.
fn package_version(path: &Path) -> Option<Version> {
    let binary = path.canonicalize().ok()?;
    let root = binary.parent()?.parent()?;
    let package: serde_json::Value =
        serde_json::from_reader(File::open(root.join("package.json")).ok()?).ok()?;
    if package["name"] != "opencode-ai" {
        return None;
    }
    let entry = package["bin"]["opencode"].as_str()?;
    if root.join(entry).canonicalize().ok()? != binary {
        return None;
    }
    parse_version(package["version"].as_str()?).ok()
}

fn installed_version(path: &Path) -> Result<Version> {
    if let Some(version) = package_version(path) {
        return Ok(version);
    }
    let mut process = UpdateProcess::spawn_at(path, &["--version"], CHECK_DEADLINE)?;
    process.wait().context("read installed version")?;
    parse_version(&process.diagnostic()?)
}

fn release_json(url: &str) -> Result<serde_json::Value> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(CHECK_DEADLINE))
        .user_agent(concat!("crit/", env!("CARGO_PKG_VERSION")))
        .build()
        .into();
    let body = agent
        .get(url)
        .header("Accept", "application/json")
        .call()
        .with_context(|| format!("check release at {url}"))?
        .body_mut()
        .with_config()
        .limit(1024 * 1024)
        .read_to_string()?;
    serde_json::from_str(&body).context("read release metadata")
}

fn update_if_newer(
    path: &Path,
    argument: &str,
    installed: &Version,
    published: &Version,
) -> Result<Option<UpdateProcess>> {
    if published.major != installed.major || !published.cmp_precedence(installed).is_gt() {
        return Ok(None);
    }
    // Pin the version we compared. The native installer still selects its installation method
    // and registry, but cannot downgrade or cross a major if its feed changes during the check.
    let target = published.to_string();
    UpdateProcess::spawn_at(path, &[argument, &target], UPDATE_DEADLINE).map(Some)
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
    use std::os::unix::fs::PermissionsExt;

    /// Polls until every updater has finished; the errors, in the order they came.
    fn finish(checks: &mut Checks) -> Vec<String> {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut errors = Vec::new();
        while checks.is_running() {
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
        let errors = finish(&mut checks);
        assert_eq!(errors.len(), 2);
        assert!(
            errors
                .iter()
                .any(|error| error.contains("Failing") && error.contains("refused"))
        );
        assert!(
            errors
                .iter()
                .any(|error| error.contains("Locked") && error.contains("Permission denied"))
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
    fn current_newer_installed_or_other_major_never_starts_an_upgrade() {
        let directory = tempfile::tempdir().unwrap();
        let updater = directory.path().join("updater");
        fs::write(&updater, "#!/bin/sh\nprintf upgraded > \"$0.installed\"\n").unwrap();
        fs::set_permissions(&updater, fs::Permissions::from_mode(0o755)).unwrap();
        let installed = Version::parse("1.19.1+local").unwrap();
        for candidate in ["1.19.1", "1.19.1+rebuilt", "1.18.9", "2.0.0"] {
            assert!(
                update_if_newer(
                    &updater,
                    "upgrade",
                    &installed,
                    &Version::parse(candidate).unwrap()
                )
                .unwrap()
                .is_none(),
                "{candidate}"
            );
        }
        assert!(!updater.with_extension("installed").exists());

        let mut upgrade = update_if_newer(
            &updater,
            "upgrade",
            &installed,
            &Version::parse("1.19.2").unwrap(),
        )
        .unwrap()
        .unwrap();
        upgrade.wait().unwrap();
        assert_eq!(
            fs::read(updater.with_extension("installed")).unwrap(),
            b"upgraded"
        );
    }

    #[test]
    fn preview_channels_keep_hyphens_and_custom_versions_are_not_assumed_stable() {
        let preview = Version::parse("0.0.0-snapshot-opentui-upgrade-202610161232").unwrap();
        assert_eq!(opencode_tag(&preview).unwrap(), "snapshot-opentui-upgrade");
        assert!(opencode_tag(&Version::parse("0.0.0-custom").unwrap()).is_none());
        assert!(parse_version("1.19.1\nunexpected output").is_err());
    }
}
