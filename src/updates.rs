use anyhow::{Context, Result};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, SeekFrom},
    os::unix::{
        fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
        process::CommandExt,
    },
    path::Path,
    process::{Child, Command, Stdio},
    time::{Duration, SystemTime},
};

const TOOLS: [(&str, &str, &str); 2] = [
    ("OMP", "omp", "update"),
    ("OpenCode", "opencode", "upgrade"),
];
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
        Ok(Some(Self { child, output }))
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

/// The updaters running in the background, each reported on once, when it finishes. Dropping
/// this leaves the ones still running alone: an interrupted install is worse than an orphan.
#[derive(Default)]
pub struct Checks {
    /// In tool order, by the name their notice carries. An updater that could not start keeps
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
        Self::spawn(tools)
    }

    fn spawn(tools: &[(&str, &str, &str)]) -> Self {
        let updaters = tools
            .iter()
            .filter_map(|&(name, program, argument)| {
                let process = match UpdateProcess::spawn(program, argument) {
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

    /// Notices for the updaters that failed since the last call. Successful updates are silent.
    pub fn poll(&mut self) -> Vec<String> {
        let mut notices = Vec::new();
        self.updaters.retain_mut(|(name, process)| {
            let outcome = match process {
                Ok(process) => match process.poll() {
                    None => return true,
                    Some(outcome) => outcome,
                },
                Err(error) => Err(std::mem::take(error)),
            };
            if let Err(failure) = outcome {
                notices.push(format!(
                    "{name} update failed: {failure}. Continuing with the installed version."
                ));
            }
            false
        });
        notices
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    /// Polls until every updater has finished; the notices, in the order they came.
    fn finish(checks: &mut Checks) -> Vec<String> {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut notices = Vec::new();
        while !checks.updaters.is_empty() {
            assert!(Instant::now() < deadline, "an updater never finished");
            notices.extend(checks.poll());
            std::thread::sleep(Duration::from_millis(5));
        }
        notices
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
        ];
        let mut checks = Checks::spawn(&tools);
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
        let mut checks = Checks::spawn(&tools);
        let mut notices = finish(&mut checks);
        notices.sort();
        assert_eq!(
            notices,
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
        let mut notices = finish(&mut Checks::spawn(&tools));
        notices.sort();
        assert_eq!(
            notices,
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
        let checks = Checks::spawn(&tools);
        wait_for(&directory.path().join("started"));
        drop(checks);
        fs::write(directory.path().join("go"), "").unwrap();
        wait_for(&directory.path().join("finished"));
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
