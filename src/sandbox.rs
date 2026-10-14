//! The sandbox: one private, disk-backed directory that holds every pane's scratch.
//!
//! Agents leave clones, virtualenvs, builds and downloads in their temp folder. In a RAM-backed
//! `/tmp` those files hold memory, then swap, until someone deletes them, and ending the agents
//! frees none of it. So each pane's `TMPDIR` is a directory of its workspace's own in the
//! sandbox, `<root>/<name>-<digest>`, the agents are told to keep their scratch there, and a pass
//! at idle priority (`Sandbox::clean`) removes what nothing uses any more.

use crate::{restore, worktree};
use anyhow::{Context, Result, bail};
use std::collections::HashSet;
use std::ffi::{CString, OsStr, OsString};
use std::fmt::Write as _;
use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// How long scratch may sit untouched before a pass removes it.
const AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// The file in each workspace directory that names its folder. Every pane started in the
/// workspace writes it again, so its age is how long ago the workspace was last used.
const MARKER: &str = ".crit-workspace";

/// The longest a marker can be: a path of `PATH_MAX` bytes and its newline.
const MARKER_LIMIT: u64 = 4097;

/// The folder agents type by habit, whatever `TMPDIR` says. Inside a Crit pane `TMPDIR` is a
/// scratch directory on disk, so it says nothing about where a literal `/tmp/...` goes.
const TMP: &str = "/tmp";

/// `statfs` types of the filesystems that keep their files in memory.
const TMPFS_MAGIC: u32 = 0x0102_1994;
const RAMFS_MAGIC: u32 = 0x8584_58f6;

/// How full a RAM-backed `/tmp` may be before startup says so.
const TMP_NOTICE_BYTES: u64 = 1 << 30;

/// How many of the largest entries in a RAM-backed `/tmp` `crit sandbox status` lists.
const TMP_LISTED: usize = 8;

/// The digest that ends a workspace directory's name, as `worktree::digest` makes it.
const DIGEST_LEN: usize = 24;

/// The longest folder name a workspace directory's name keeps.
const NAME_LEN: usize = 24;

/// The sandbox's root. Clones share it: the UI and the worker thread each hold one.
#[derive(Clone, Debug)]
pub struct Sandbox(Arc<Root>);

#[derive(Debug)]
struct Root {
    path: PathBuf,
    /// `/tmp` is RAM-backed, so the agents' note says what files there cost.
    tmp_is_ram: bool,
}

/// A workspace's directory in the sandbox, and the system-prompt note that sends agents there.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Scratch {
    pub dir: PathBuf,
    pub note: String,
}

/// What a pass over the sandbox removed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CleanReport {
    /// The space freed: `st_blocks * 512` of everything removed.
    pub removed_bytes: u64,
    /// Workspace directories removed whole: their folder is gone, or they sat empty for `AGE`.
    pub removed_workspaces: usize,
    /// Entries removed from inside workspace directories, untouched for `AGE`.
    pub removed_entries: usize,
    /// What could not be read or removed, one line each.
    pub errors: Vec<String>,
}

impl CleanReport {
    /// What the pass freed, for the notice row and `crit sandbox clean`; `None` when it removed
    /// nothing.
    pub fn summary(&self) -> Option<String> {
        let mut parts = Vec::with_capacity(2);
        match self.removed_workspaces {
            0 => {}
            1 => parts.push("1 workspace gone".to_owned()),
            count => parts.push(format!("{count} workspaces gone")),
        }
        match self.removed_entries {
            0 => {}
            1 => parts.push("1 entry".to_owned()),
            count => parts.push(format!("{count} entries")),
        }
        (!parts.is_empty()).then(|| {
            format!(
                "Sandbox: removed {} of scratch unused for 30 days ({}).",
                size(self.removed_bytes),
                parts.join(", ")
            )
        })
    }
}

/// A place the root may go.
#[derive(Debug, PartialEq, Eq)]
struct Candidate {
    path: PathBuf,
    /// Named by `CRIT_SANDBOX`: it may be on any filesystem.
    explicit: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Backing {
    Disk,
    Ram,
}

impl Sandbox {
    /// Picks this window's root, making it when needed: the first candidate that works (see
    /// `candidates`). Startup news goes to `notices`: a RAM-backed `/tmp` that holds over 1 GiB,
    /// a `CRIT_SANDBOX` that cannot be used, and why there is no sandbox when scratch then stays
    /// in RAM.
    pub fn open(notices: &mut Vec<String>) -> Option<Self> {
        let tmp = Path::new(TMP);
        let tmp_is_ram = is_ram(tmp);
        if tmp_is_ram
            && let Ok(used) = used_bytes(tmp)
            && used > TMP_NOTICE_BYTES
        {
            notices.push(format!(
                "/tmp is RAM-backed and holds {}; crit sandbox status lists its largest entries.",
                size(used)
            ));
        }
        let mut reasons = Vec::new();
        let found = candidates(|name| std::env::var_os(name), &mut reasons);
        let Some(path) = choose(&found, true, &mut reasons) else {
            let temp = std::env::temp_dir();
            if is_ram(&temp) {
                notices.push(format!(
                    "No sandbox for agent scratch could be made ({}), so it stays in RAM-backed {}.",
                    reasons.join("; "),
                    temp.display()
                ));
            }
            return None;
        };
        // `CRIT_SANDBOX` is the first candidate, so when it is set and anything was passed
        // over, the first reason is its own.
        if std::env::var_os("CRIT_SANDBOX").is_some_and(|value| !value.is_empty())
            && let Some(reason) = reasons.first()
        {
            notices.push(format!(
                "CRIT_SANDBOX is not used ({reason}); agent scratch goes to {} instead.",
                path.display()
            ));
        }
        Some(Self(Arc::new(Root { path, tmp_is_ram })))
    }

    /// The root an earlier window made, found without making or changing anything, for the
    /// `crit` commands. The error says why there is none.
    pub fn locate() -> Result<Self, String> {
        let mut reasons = Vec::new();
        let found = candidates(|name| std::env::var_os(name), &mut reasons);
        match choose(&found, false, &mut reasons) {
            Some(path) => Ok(Self(Arc::new(Root {
                path,
                tmp_is_ram: is_ram(Path::new(TMP)),
            }))),
            None => Err(reasons.join("; ")),
        }
    }

    /// A sandbox at `root` as it is, for tests.
    #[cfg(test)]
    pub fn at(root: &Path, tmp_is_ram: bool) -> Self {
        Self(Arc::new(Root {
            path: root.to_owned(),
            tmp_is_ram,
        }))
    }

    pub fn root(&self) -> &Path {
        &self.0.path
    }

    /// The scratch directory of the workspace in `folder`, made when missing, and the note for
    /// its agents. It stamps the workspace as used now: see `MARKER`.
    pub fn workspace(&self, folder: &Path) -> io::Result<Scratch> {
        let (dir, folder) = self.directory(folder)?;
        // Not recursive: only startup makes the root, after checking it. A root that has gone
        // since is not quietly made again, by whoever asks first, without those checks.
        match DirBuilder::new().mode(0o700).create(&dir) {
            Ok(()) => {}
            // Through a symlink, scratch would land where cleaning never looks.
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                if !fs::symlink_metadata(&dir)?.is_dir() {
                    return Err(io::Error::other(format!(
                        "{} is not a directory",
                        dir.display()
                    )));
                }
            }
            Err(error) => return Err(error),
        }
        stamp(&dir, &folder)?;
        Ok(Scratch {
            note: note(&dir, self.0.tmp_is_ram),
            dir,
        })
    }

    /// Removes the scratch of the workspace in `folder`, exactly its directory, as when its
    /// checkout is deleted. Nothing to remove is no error.
    pub fn remove_workspace(&self, folder: &Path) -> io::Result<()> {
        let (dir, _) = self.directory(folder)?;
        match fs::symlink_metadata(&dir) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
            Ok(_) => delete(&dir, fs::symlink_metadata(self.root())?.dev()).1,
        }
    }

    /// The pass a window runs, at startup and then daily, at idle priority. It first stamps
    /// the workspaces `open` in this window, so that passes of other windows, which do not know
    /// them, see them in use, then cleans. The error, for the notice row, is the root's own.
    pub fn pass(&self, open: &[PathBuf]) -> Result<CleanReport, String> {
        for folder in open {
            // The scratch of a folder that has gone is the cleaning's to remove.
            if folder.exists() {
                let _ = self.workspace(folder);
            }
        }
        self.clean(open).map_err(|error| {
            format!(
                "Sandbox {} could not be cleaned: {error}",
                self.root().display()
            )
        })
    }

    /// Removes from each workspace directory, `<root>/<name>-<digest>` with a readable marker:
    /// (a) all of it when its recorded folder is gone; else (b) each entry inside whose whole
    /// tree, walked without following symlinks, has no mtime or ctime within `AGE`, and then
    /// (c) the directory itself when only its marker is left, the marker is older than `AGE`,
    /// and its folder is not one of `open`. Nothing on another filesystem is touched, and a
    /// directory without a readable marker is left alone and listed in `errors`. The error is
    /// the root's: it could not be read.
    pub fn clean(&self, open: &[PathBuf]) -> io::Result<CleanReport> {
        self.clean_at(open, unix_now())
    }

    /// `clean` as if it were `now`, in seconds since the epoch.
    fn clean_at(&self, open: &[PathBuf], now: i64) -> io::Result<CleanReport> {
        let cutoff = now.saturating_sub(AGE.as_secs() as i64);
        let open: HashSet<PathBuf> = open
            .iter()
            .filter_map(|folder| self.directory(folder).ok())
            .map(|(dir, _)| dir)
            .collect();
        let mut report = CleanReport::default();
        let (device, directories) = self.workspaces(&mut report.errors)?;
        for dir in directories {
            let open = open.contains(&dir);
            if let Err(error) = clean_workspace(&dir, device, cutoff, open, &mut report) {
                report.errors.push(format!("{}: {error}", dir.display()));
            }
        }
        Ok(report)
    }

    /// Removes every workspace directory, whatever its age, for `crit sandbox clean --all`.
    fn clear(&self) -> io::Result<CleanReport> {
        let mut report = CleanReport::default();
        let (device, directories) = self.workspaces(&mut report.errors)?;
        for dir in directories {
            let (bytes, result) = delete(&dir, device);
            report.removed_bytes += bytes;
            match result {
                Ok(()) => report.removed_workspaces += 1,
                Err(error) => report.errors.push(format!("{}: {error}", dir.display())),
            }
        }
        Ok(report)
    }

    /// The root's device and its workspace directories, known by their names: nothing else in
    /// it, such as Crit's own temp files, is a workspace's. Entries that cannot be listed go to
    /// `errors`. The error is the root's: it cannot be read.
    fn workspaces(&self, errors: &mut Vec<String>) -> io::Result<(u64, Vec<PathBuf>)> {
        let root = self.root();
        let device = fs::symlink_metadata(root)?.dev();
        let mut directories = Vec::new();
        for entry in fs::read_dir(root)? {
            match entry {
                Ok(entry) if is_workspace_name(&entry.file_name()) => {
                    directories.push(entry.path());
                }
                Ok(_) => {}
                Err(error) => errors.push(format!("{}: {error}", root.display())),
            }
        }
        Ok((device, directories))
    }

    /// The directory of the workspace in `folder`, and the canonical folder it is named after.
    fn directory(&self, folder: &Path) -> io::Result<(PathBuf, PathBuf)> {
        let folder = worktree::canonical_path(folder)
            .map_err(|error| io::Error::other(format!("{error:#}")))?;
        Ok((self.root().join(directory_name(&folder)), folder))
    }

    /// The root, how it is backed and its free space, then one line per workspace directory,
    /// largest first, for `crit sandbox status`.
    fn describe(&self, out: &mut String) {
        let root = self.root();
        let backing = match backing(root) {
            Ok(Backing::Disk) => "disk-backed",
            Ok(Backing::Ram) => "RAM-backed",
            Err(_) => "backing unknown",
        };
        let free = statfs(root).map_or_else(
            |error| format!("free space unknown: {error}"),
            |stats| format!("{} free", size(stats.f_bavail * stats.f_frsize as u64)),
        );
        let _ = writeln!(out, "Sandbox {} ({backing}, {free})", root.display());
        let (device, directories) = match self.workspaces(&mut Vec::new()) {
            Ok(found) => found,
            Err(error) => {
                let _ = writeln!(out, "  It cannot be read: {error}");
                return;
            }
        };
        let mut rows: Vec<(u64, String, Option<PathBuf>)> = directories
            .iter()
            .map(|dir| {
                (
                    survey(dir, device).bytes,
                    dir.file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned(),
                    read_marker(dir).ok(),
                )
            })
            .collect();
        if rows.is_empty() {
            let _ = writeln!(out, "  No workspace has scratch here yet.");
            return;
        }
        rows.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));
        let width = rows.iter().map(|row| row.1.len()).max().unwrap_or(0);
        for (bytes, name, folder) in &rows {
            let missing = folder.as_deref().is_some_and(|folder| {
                matches!(fs::symlink_metadata(folder), Err(error) if error.kind() == io::ErrorKind::NotFound)
            });
            out.push_str(&status_line(
                *bytes,
                name,
                width,
                folder.as_deref(),
                missing,
            ));
        }
    }
}

/// Where the root may go, in order: `$CRIT_SANDBOX`, on any filesystem; `$TMPDIR/crit-<user>`,
/// or inside a Crit pane the sandbox that `TMPDIR` is in; `/var/tmp/crit-<user>`;
/// `$XDG_CACHE_HOME/crit/sandbox`, else `~/.cache/crit/sandbox`. Each but the first must turn
/// out to be on a disk. `env` reads the environment; a `CRIT_SANDBOX` that cannot name a root
/// adds its reason to `reasons`.
fn candidates(env: impl Fn(&str) -> Option<OsString>, reasons: &mut Vec<String>) -> Vec<Candidate> {
    let value = |name: &str| {
        env(name)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
    };
    let absolute = |name: &str| value(name).filter(|path| path.is_absolute());
    let leaf = format!("crit-{}", user_name(env("USER")));
    let mut candidates = Vec::with_capacity(4);
    match value("CRIT_SANDBOX") {
        Some(path) if path.is_absolute() => candidates.push(Candidate {
            path,
            explicit: true,
        }),
        Some(path) => reasons.push(format!(
            "CRIT_SANDBOX={} is not an absolute path",
            path.display()
        )),
        None => {}
    }
    if let Some(temp) = absolute("TMPDIR") {
        // Inside a Crit pane `TMPDIR` is a workspace's directory: a Crit started there, and the
        // `crit sandbox` commands, use its sandbox rather than nest another one in it.
        let path = match temp.parent() {
            Some(root) if is_workspace_directory(&temp) => root.to_owned(),
            _ => temp.join(&leaf),
        };
        candidates.push(Candidate {
            path,
            explicit: false,
        });
    }
    candidates.push(Candidate {
        path: Path::new("/var/tmp").join(&leaf),
        explicit: false,
    });
    if let Some(cache) =
        absolute("XDG_CACHE_HOME").or_else(|| absolute("HOME").map(|home| home.join(".cache")))
    {
        candidates.push(Candidate {
            path: cache.join("crit/sandbox"),
            explicit: false,
        });
    }
    candidates
}

/// `$USER` when it is 1 to 32 of `[A-Za-z0-9._-]`, which any filesystem takes as a name;
/// otherwise the effective user ID.
fn user_name(user: Option<OsString>) -> String {
    match user.as_deref().and_then(OsStr::to_str) {
        Some(user)
            if (1..=32).contains(&user.len())
                && user.bytes().all(|byte| {
                    byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-')
                }) =>
        {
            user.to_owned()
        }
        // SAFETY: `geteuid` cannot fail and touches no memory.
        _ => unsafe { libc::geteuid() }.to_string(),
    }
}

/// The first of `candidates` that `prepare` accepts; why each one before it was passed over
/// goes to `reasons`.
fn choose(candidates: &[Candidate], create: bool, reasons: &mut Vec<String>) -> Option<PathBuf> {
    candidates
        .iter()
        .find_map(|candidate| match prepare(candidate, create) {
            Ok(()) => Some(candidate.path.clone()),
            Err(reason) => {
                reasons.push(reason);
                None
            }
        })
}

/// Whether `candidate` can be the root, or why not: it must be a directory, not a symlink,
/// owned by this user and, unless `CRIT_SANDBOX` named it, on a disk. `create` makes it when
/// it is missing, and private as the workspace state directory is; otherwise nothing changes.
fn prepare(candidate: &Candidate, create: bool) -> Result<(), String> {
    let path = &candidate.path;
    let shown = path.display();
    if !candidate.explicit {
        match backing(path) {
            Ok(Backing::Disk) => {}
            Ok(Backing::Ram) => return Err(format!("{shown} is RAM-backed")),
            Err(error) => return Err(format!("{shown}: {error}")),
        }
    }
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(format!("{shown} is a symlink"));
        }
        Ok(metadata) if !metadata.is_dir() => return Err(format!("{shown} is not a directory")),
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound && create => {
            DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(path)
                .map_err(|error| format!("{shown}: {error}"))?;
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(format!("{shown} does not exist"));
        }
        Err(error) => return Err(format!("{shown}: {error}")),
    }
    // Opened without following a symlink, so the checks below are about this very directory.
    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .map_err(|error| format!("{shown}: {error}"))?;
    let metadata = directory
        .metadata()
        .map_err(|error| format!("{shown}: {error}"))?;
    // SAFETY: `geteuid` cannot fail and touches no memory.
    if metadata.uid() != unsafe { libc::geteuid() } {
        return Err(format!("{shown} belongs to another user"));
    }
    if create {
        directory
            .set_permissions(fs::Permissions::from_mode(0o700))
            .map_err(|error| format!("{shown}: making it private: {error}"))?;
    }
    Ok(())
}

/// What holds the files under `path`, judged by its nearest existing ancestor (itself, if it
/// exists).
fn backing(path: &Path) -> io::Result<Backing> {
    let mut path = path;
    let stats = loop {
        match statfs(path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                path = path.parent().ok_or(error)?;
            }
            result => break result?,
        }
    };
    // The magic numbers are 32 bits wide; `f_type` is a signed word of the platform's width.
    Ok(match stats.f_type as u32 {
        TMPFS_MAGIC | RAMFS_MAGIC => Backing::Ram,
        _ => Backing::Disk,
    })
}

/// Whether the files under `path` are kept in memory; one that cannot be inspected is not.
fn is_ram(path: &Path) -> bool {
    backing(path).is_ok_and(|backing| backing == Backing::Ram)
}

/// The space in use on the filesystem that holds `path`.
fn used_bytes(path: &Path) -> io::Result<u64> {
    let stats = statfs(path)?;
    Ok(stats.f_blocks.saturating_sub(stats.f_bfree) * stats.f_frsize as u64)
}

fn statfs(path: &Path) -> io::Result<libc::statfs> {
    let name = CString::new(path.as_os_str().as_bytes())
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    let mut stats = std::mem::MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: `name` is a NUL-terminated path and `stats` has room for one `statfs`, which the
    // call fills in full when it succeeds.
    if unsafe { libc::statfs(name.as_ptr(), stats.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the call succeeded, so it filled `stats`.
    Ok(unsafe { stats.assume_init() })
}

/// `<name>-<digest>` for the canonical `folder`: its name kept to what every filesystem and
/// shell takes as it is, then the digest of its whole path, as worktree IDs are made.
fn directory_name(folder: &Path) -> String {
    format!(
        "{}-{}",
        folder_name(folder),
        worktree::digest(folder.as_os_str().as_bytes())
    )
}

/// `folder`'s last component with every byte outside `[A-Za-z0-9._-]` made `-`, runs of `-`
/// made one, trimmed of `-` and cut to `NAME_LEN`; `folder` when nothing is left.
fn folder_name(folder: &Path) -> String {
    let mut name = String::with_capacity(NAME_LEN);
    for &byte in folder.file_name().map_or(&[][..], OsStr::as_bytes) {
        let kept = byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_');
        if kept {
            name.push(char::from(byte));
        } else if !name.is_empty() && !name.ends_with('-') {
            name.push('-');
        }
    }
    name.truncate(NAME_LEN);
    let name = name.trim_end_matches('-');
    if name.is_empty() {
        "folder".to_owned()
    } else {
        name.to_owned()
    }
}

/// Whether `name` is a workspace directory's, as `directory_name` makes them.
fn is_workspace_name(name: &OsStr) -> bool {
    let name = name.as_bytes();
    name.len() > DIGEST_LEN + 1 && {
        let (prefix, digest) = name.split_at(name.len() - DIGEST_LEN);
        prefix.ends_with(b"-")
            && digest
                .iter()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
    }
}

/// Whether `dir` is a workspace's directory in a sandbox: named as one, with its marker.
fn is_workspace_directory(dir: &Path) -> bool {
    dir.file_name().is_some_and(is_workspace_name)
        && fs::symlink_metadata(dir.join(MARKER)).is_ok_and(|marker| marker.is_file())
}

/// Writes `folder` to `dir`'s marker through a rename, so that a pass reading it at the same
/// time finds the whole path, never a torn one it could take for a folder that is gone.
fn stamp(dir: &Path, folder: &Path) -> io::Result<()> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let temporary = dir.join(format!(
        "{MARKER}.{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let mut content = folder.as_os_str().as_bytes().to_vec();
    content.push(b'\n');
    let result = (|| {
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&temporary)?
            .write_all(&content)?;
        fs::rename(&temporary, dir.join(MARKER))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

/// The folder `dir`'s marker names, as `stamp` wrote it.
fn read_marker(dir: &Path) -> io::Result<PathBuf> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(dir.join(MARKER))?;
    let invalid =
        |what: &str| io::Error::new(io::ErrorKind::InvalidData, format!("{MARKER} {what}"));
    if !file.metadata()?.is_file() {
        return Err(invalid("is not a file"));
    }
    let mut bytes = Vec::new();
    file.take(MARKER_LIMIT + 1).read_to_end(&mut bytes)?;
    let path = bytes
        .strip_suffix(b"\n")
        .filter(|path| path.len() < MARKER_LIMIT as usize && !path.contains(&0))
        .map(|path| PathBuf::from(OsStr::from_bytes(path)))
        .filter(|path| path.is_absolute())
        .ok_or_else(|| invalid("does not name a folder"))?;
    Ok(path)
}

/// The system-prompt guidance for agents whose scratch goes to `dir`. The one place for its
/// wording: OMP gets it through `--append-system-prompt`, OpenCode through Crit's plugin.
fn note(dir: &Path, tmp_is_ram: bool) -> String {
    let mut note = format!(
        "Scratch space: create every temporary clone, worktree, virtualenv, build copy, download and other scratch file under $TMPDIR ({}), which Crit keeps on disk for this workspace and cleans automatically; never under /tmp.",
        dir.display()
    );
    if tmp_is_ram {
        note.push_str(" On this machine /tmp is RAM-backed: files there consume memory and swap.");
    }
    note
}

/// Applies `Sandbox::clean`'s rules to the workspace directory `dir`, adding what it removed to
/// `report`. `open` says its folder is open in this window.
fn clean_workspace(
    dir: &Path,
    device: u64,
    cutoff: i64,
    open: bool,
    report: &mut CleanReport,
) -> io::Result<()> {
    let metadata = fs::symlink_metadata(dir)?;
    if !metadata.is_dir() || metadata.dev() != device {
        return Err(io::Error::other(
            "not a directory on the sandbox's filesystem",
        ));
    }
    let folder = read_marker(dir)
        .map_err(|error| io::Error::new(error.kind(), format!("no readable {MARKER}: {error}")))?;
    // (a) Nothing can use the scratch of a folder that is gone.
    if matches!(fs::symlink_metadata(&folder), Err(error) if error.kind() == io::ErrorKind::NotFound)
    {
        let (bytes, result) = delete(dir, device);
        report.removed_bytes += bytes;
        result?;
        report.removed_workspaces += 1;
        return Ok(());
    }
    // (b) An entry is judged by the newest stamp in its tree: a clone or a virtualenv that is
    // still in use goes on whole.
    let mut kept = false;
    for entry in fs::read_dir(dir)? {
        let path = match entry {
            Ok(entry) if entry.file_name() == MARKER => continue,
            Ok(entry) => entry.path(),
            Err(error) => {
                kept = true;
                report.errors.push(format!("{}: {error}", dir.display()));
                continue;
            }
        };
        let tree = survey(&path, device);
        if let Some(error) = &tree.error {
            kept = true;
            report.errors.push(format!("{}: {error}", path.display()));
            continue;
        }
        if tree.foreign || tree.newest >= cutoff {
            kept = true;
            continue;
        }
        let (bytes, result) = delete_surveyed(&path, &tree, device);
        report.removed_bytes += bytes;
        match result {
            Ok(()) => report.removed_entries += 1,
            Err(error) => {
                kept = true;
                report.errors.push(format!("{}: {error}", path.display()));
            }
        }
    }
    // (c) An empty directory of a workspace that nothing has used for `AGE`.
    if kept || open {
        return Ok(());
    }
    let marker = dir.join(MARKER);
    let stamped = fs::symlink_metadata(&marker)?;
    if stamped.mtime().max(stamped.ctime()) >= cutoff {
        return Ok(());
    }
    fs::remove_file(&marker)?;
    if let Err(error) = fs::remove_dir(dir) {
        // A pane started there since it was listed: the workspace is in use, so it keeps its
        // marker.
        let _ = stamp(dir, &folder);
        return Err(error);
    }
    report.removed_bytes += (stamped.blocks() + metadata.blocks()) * 512;
    report.removed_workspaces += 1;
    Ok(())
}

/// What a walk over a tree found. It follows no symlink and enters no other filesystem.
#[derive(Debug)]
struct Tree {
    /// `st_blocks * 512` of each inode once: the space the tree takes on its disk.
    bytes: u64,
    /// The newest mtime or ctime in it, in seconds since the epoch.
    newest: i64,
    /// Its top is a directory.
    directory: bool,
    /// It holds the mount point of another filesystem, which was not walked.
    foreign: bool,
    /// The first error that kept part of it from being read.
    error: Option<io::Error>,
}

fn survey(top: &Path, device: u64) -> Tree {
    let mut tree = Tree {
        bytes: 0,
        newest: i64::MIN,
        directory: fs::symlink_metadata(top).is_ok_and(|metadata| metadata.is_dir()),
        foreign: false,
        error: None,
    };
    let mut linked = HashSet::new();
    let mut pending = vec![top.to_path_buf()];
    while let Some(path) = pending.pop() {
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            // Gone since it was listed: nothing of it is left to count or keep.
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => {
                tree.error.get_or_insert(error);
                continue;
            }
        };
        if metadata.dev() != device {
            tree.foreign = true;
            continue;
        }
        tree.newest = tree.newest.max(metadata.mtime()).max(metadata.ctime());
        // A file linked more than once in the tree takes its space once.
        if metadata.is_dir() || metadata.nlink() < 2 || linked.insert(metadata.ino()) {
            tree.bytes += metadata.blocks() * 512;
        }
        if metadata.is_dir() {
            match fs::read_dir(&path) {
                Ok(entries) => {
                    for entry in entries {
                        match entry {
                            Ok(entry) => pending.push(entry.path()),
                            Err(error) => {
                                tree.error.get_or_insert(error);
                            }
                        }
                    }
                }
                Err(error) => {
                    tree.error.get_or_insert(error);
                }
            }
        }
    }
    tree
}

/// Removes `path` and everything under it, without following symlinks. Returns the space freed
/// with the outcome; see `delete_surveyed`.
fn delete(path: &Path, device: u64) -> (u64, io::Result<()>) {
    delete_surveyed(path, &survey(path, device), device)
}

/// Removes `path`, which `tree` describes, and everything under it, without following symlinks.
/// A tree with another filesystem mounted inside is refused whole, as removing it would empty
/// that filesystem. A removal that fails midway counts the space of what did go.
fn delete_surveyed(path: &Path, tree: &Tree, device: u64) -> (u64, io::Result<()>) {
    if tree.foreign {
        return (
            0,
            Err(io::Error::other(
                "another filesystem is mounted inside; unmount it first",
            )),
        );
    }
    let result = if tree.directory {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    };
    match result {
        Ok(()) => (tree.bytes, Ok(())),
        // Someone else removed it first.
        Err(error) if error.kind() == io::ErrorKind::NotFound => (0, Ok(())),
        Err(error) => (
            tree.bytes.saturating_sub(survey(path, device).bytes),
            Err(error),
        ),
    }
}

/// Lowers the calling thread to the lowest CPU priority (nice 19) and the idle I/O class, for
/// work that must never compete with a game or the agents. On Linux both act on the calling
/// thread alone; a call that fails leaves it at its priority, which only costs speed.
pub fn idle_priority() {
    // `IOPRIO_WHO_PROCESS` with ID 0 names the calling thread; `IOPRIO_CLASS_IDLE` goes in the
    // class bits, above the 13 data bits.
    const IOPRIO_WHO_PROCESS: libc::c_int = 1;
    const IOPRIO_CLASS_IDLE: libc::c_int = 3 << 13;
    // SAFETY: both take plain integers and change nothing but this thread's scheduling.
    unsafe {
        libc::setpriority(libc::PRIO_PROCESS, 0, 19);
        libc::syscall(
            libc::SYS_ioprio_set,
            IOPRIO_WHO_PROCESS,
            0,
            IOPRIO_CLASS_IDLE,
        );
    }
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs() as i64)
}

/// `bytes` in binary units with one decimal, as `1.7 GiB`; under 1 KiB in bytes.
fn size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["KiB", "MiB", "GiB", "TiB", "PiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    // What would round up to 1024.0 is shown in the next unit.
    while value >= 1023.95 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

/// How long ago, in its largest whole unit: `40s`, `12m`, `5h`, `3d`.
fn age(seconds: i64) -> String {
    match seconds.max(0) {
        seconds @ 0..60 => format!("{seconds}s"),
        seconds @ 60..3600 => format!("{}m", seconds / 60),
        seconds @ 3600..86400 => format!("{}h", seconds / 3600),
        seconds => format!("{}d", seconds / 86400),
    }
}

/// One workspace directory's line in `crit sandbox status`, its name padded to `width`.
fn status_line(
    bytes: u64,
    name: &str,
    width: usize,
    folder: Option<&Path>,
    missing: bool,
) -> String {
    let folder = folder.map_or_else(
        || format!("(no readable {MARKER})"),
        |folder| folder.display().to_string(),
    );
    let mut line = format!("{:>10}  {name:<width$}  {folder}", size(bytes));
    if missing {
        line.push_str("  missing");
    }
    line.push('\n');
    line
}

/// `crit sandbox status`: the sandbox and its workspace directories, then, when `/tmp` is
/// RAM-backed, its use and the largest of its top-level entries that are yours.
pub fn status() -> String {
    let mut out = String::new();
    match Sandbox::locate() {
        Ok(sandbox) => sandbox.describe(&mut out),
        Err(reasons) => {
            let _ = writeln!(
                out,
                "No sandbox yet; Crit makes one when a window opens ({reasons})."
            );
        }
    }
    describe_tmp(Path::new(TMP), &mut out);
    out
}

/// `tmp`'s use and its largest top-level entries owned by this user, when it is RAM-backed.
/// Crit lists them only: it did not make them, so it never deletes them.
fn describe_tmp(tmp: &Path, out: &mut String) {
    if !is_ram(tmp) {
        return;
    }
    let used = used_bytes(tmp).map_or_else(
        |error| format!("use unknown: {error}"),
        |used| format!("{} used", size(used)),
    );
    let _ = writeln!(
        out,
        "RAM-backed {} ({used}). Your largest entries there, which Crit never deletes:",
        tmp.display()
    );
    let listed =
        fs::symlink_metadata(tmp).and_then(|metadata| Ok((metadata.dev(), fs::read_dir(tmp)?)));
    let (device, listing) = match listed {
        Ok(listed) => listed,
        Err(error) => {
            let _ = writeln!(out, "  It cannot be read: {error}");
            return;
        }
    };
    // SAFETY: `geteuid` cannot fail and touches no memory.
    let user = unsafe { libc::geteuid() };
    let mut entries: Vec<(u64, i64, OsString)> = listing
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let metadata = entry.metadata().ok()?;
            (metadata.uid() == user).then(|| {
                let bytes = survey(&entry.path(), device).bytes;
                (bytes, metadata.mtime(), entry.file_name())
            })
        })
        .collect();
    if entries.is_empty() {
        let _ = writeln!(out, "  None of its entries are yours.");
        return;
    }
    entries.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.2.cmp(&right.2)));
    let now = unix_now();
    for (bytes, modified, name) in entries.iter().take(TMP_LISTED) {
        let _ = writeln!(
            out,
            "{:>10}  {:>4}  {}",
            size(*bytes),
            age(now - modified),
            name.to_string_lossy()
        );
    }
}

/// `crit sandbox clean [--all]`. A pass keeps the empty directories of the saved workspace's
/// folders, which reopen at the next launch; `--all` removes every workspace directory, and
/// is refused while a window owns the saved workspace, whose panes use them.
pub fn clean_command(all: bool) -> Result<()> {
    let sandbox = match Sandbox::locate() {
        Ok(sandbox) => sandbox,
        Err(reasons) => {
            println!("No sandbox to clean ({reasons}).");
            return Ok(());
        }
    };
    let root = sandbox.root().display();
    let report = if all {
        // Held until the removal is done: no window can open the saved workspace meanwhile.
        let _owner = match restore::Store::open() {
            Err(error) if error.downcast_ref::<restore::Busy>().is_some() => bail!(
                "a Crit window owns the saved workspace and its panes use the sandbox; quit it first, or run crit sandbox clean without --all"
            ),
            store => store.ok(),
        };
        sandbox.clear()
    } else {
        // Read without the lock, which a running window holds; unreadable, it keeps nothing.
        let open: Vec<PathBuf> = restore::peek()
            .ok()
            .flatten()
            .map(|snapshot| {
                snapshot
                    .projects
                    .into_iter()
                    .flat_map(|project| project.workspaces)
                    .map(|workspace| workspace.path)
                    .collect()
            })
            .unwrap_or_default();
        sandbox.clean(&open)
    }
    .with_context(|| format!("cleaning sandbox {root}"))?;
    for error in &report.errors {
        eprintln!("crit: {error}");
    }
    if all {
        println!(
            "Sandbox {root}: removed {} of scratch ({} workspace director{}).",
            size(report.removed_bytes),
            report.removed_workspaces,
            if report.removed_workspaces == 1 {
                "y"
            } else {
                "ies"
            }
        );
    } else {
        match report.summary() {
            Some(summary) => println!("{summary}"),
            None => println!(
                "Sandbox {root}: nothing unused for 30 days, and no workspace folder is gone."
            ),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::fs::File;

    const DAY: i64 = 24 * 60 * 60;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
        let pairs: HashMap<String, OsString> = pairs
            .iter()
            .map(|(name, value)| ((*name).to_owned(), OsString::from(value)))
            .collect();
        move |name| pairs.get(name).cloned()
    }

    fn paths(candidates: &[Candidate]) -> Vec<PathBuf> {
        candidates
            .iter()
            .map(|candidate| candidate.path.clone())
            .collect()
    }

    fn explicit(path: &Path) -> Candidate {
        Candidate {
            path: path.to_owned(),
            explicit: true,
        }
    }

    /// Sets `path`'s mtime to `seconds` since the epoch. Its ctime becomes now: no fixture can
    /// set that, so the tests that age by ctime clean "now" well in the future.
    fn touch(path: &Path, seconds: i64) {
        File::open(path)
            .unwrap()
            .set_modified(UNIX_EPOCH + Duration::from_secs(seconds as u64))
            .unwrap();
    }

    fn mode(path: &Path) -> u32 {
        fs::symlink_metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn candidates_follow_the_documented_order() {
        let mut reasons = Vec::new();
        let found = candidates(
            env(&[
                ("CRIT_SANDBOX", "/explicit"),
                ("TMPDIR", "/scratch"),
                ("USER", "jyq"),
                ("XDG_CACHE_HOME", "/cache"),
                ("HOME", "/home/jyq"),
            ]),
            &mut reasons,
        );
        assert_eq!(
            paths(&found),
            [
                "/explicit",
                "/scratch/crit-jyq",
                "/var/tmp/crit-jyq",
                "/cache/crit/sandbox"
            ]
            .map(PathBuf::from)
        );
        assert!(found[0].explicit && found[1..].iter().all(|candidate| !candidate.explicit));
        assert!(reasons.is_empty());

        // Without XDG_CACHE_HOME the cache is under HOME; relative paths are passed over.
        let uid = unsafe { libc::geteuid() };
        let found = candidates(
            env(&[
                ("CRIT_SANDBOX", "relative/root"),
                ("TMPDIR", "relative"),
                ("USER", "has space"),
                ("XDG_CACHE_HOME", "cache"),
                ("HOME", "/home/someone"),
            ]),
            &mut reasons,
        );
        assert_eq!(
            paths(&found),
            [
                PathBuf::from(format!("/var/tmp/crit-{uid}")),
                PathBuf::from("/home/someone/.cache/crit/sandbox")
            ]
        );
        assert_eq!(
            reasons,
            ["CRIT_SANDBOX=relative/root is not an absolute path"]
        );
        assert_eq!(
            paths(&candidates(env(&[("USER", "x")]), &mut Vec::new())),
            [PathBuf::from("/var/tmp/crit-x")]
        );

        // Inside a Crit pane TMPDIR is a workspace's directory, whose sandbox is the one to use.
        let root = tempfile::tempdir().unwrap();
        let folder = tempfile::tempdir().unwrap();
        let scratch = Sandbox::at(root.path(), false)
            .workspace(folder.path())
            .unwrap();
        let pane = [("TMPDIR", scratch.dir.to_str().unwrap()), ("USER", "x")];
        let found = candidates(env(&pane), &mut Vec::new());
        assert_eq!(
            paths(&found)[..2],
            [root.path(), Path::new("/var/tmp/crit-x")]
        );
    }

    #[test]
    fn user_names_are_kept_only_when_any_filesystem_takes_them() {
        let uid = unsafe { libc::geteuid() }.to_string();
        let (longest, too_long) = ("x".repeat(32), "x".repeat(33));
        for (user, expected) in [
            (Some("jyq"), "jyq"),
            (Some("a.b_c-D9"), "a.b_c-D9"),
            (Some(longest.as_str()), longest.as_str()),
            (Some(too_long.as_str()), uid.as_str()),
            (Some(""), uid.as_str()),
            (Some("a/b"), uid.as_str()),
            (Some("jöran"), uid.as_str()),
            (None, uid.as_str()),
        ] {
            assert_eq!(user_name(user.map(OsString::from)), expected, "{user:?}");
        }
    }

    #[test]
    fn the_first_usable_candidate_is_the_root() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("target");
        fs::create_dir(&target).unwrap();
        let link = temp.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let file = temp.path().join("file");
        fs::write(&file, "").unwrap();
        let missing = temp.path().join("made/root");
        let mut reasons = Vec::new();
        let chosen = choose(
            &[explicit(&link), explicit(&file), explicit(&missing)],
            true,
            &mut reasons,
        );
        assert_eq!(chosen, Some(missing.clone()));
        assert_eq!(
            reasons,
            [
                format!("{} is a symlink", link.display()),
                format!("{} is not a directory", file.display())
            ]
        );
        assert_eq!(mode(&missing), 0o700);

        // An existing root is made private; finding one changes nothing.
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            choose(&[explicit(&target)], false, &mut reasons),
            Some(target.clone())
        );
        assert_eq!(mode(&target), 0o755);
        assert_eq!(
            choose(&[explicit(&target)], true, &mut reasons),
            Some(target.clone())
        );
        assert_eq!(mode(&target), 0o700);

        // Finding one never makes one.
        let absent = temp.path().join("absent");
        reasons.clear();
        assert_eq!(choose(&[explicit(&absent)], false, &mut reasons), None);
        assert!(!absent.exists());
        assert_eq!(reasons, [format!("{} does not exist", absent.display())]);

        // Another user's directory is never the root.
        if unsafe { libc::geteuid() } != 0 {
            reasons.clear();
            assert_eq!(
                choose(&[explicit(Path::new("/"))], false, &mut reasons),
                None
            );
            assert_eq!(reasons, ["/ belongs to another user"]);
        }
    }

    #[test]
    fn only_crit_sandbox_may_be_in_ram() {
        let shm = Path::new("/dev/shm");
        if backing(shm).ok() != Some(Backing::Ram) {
            eprintln!("skipped: /dev/shm is not a tmpfs here");
            return;
        }
        // A path that does not exist yet is judged by the folder it would be made in.
        let root = shm.join(format!("crit-sandbox-test-{}/root", std::process::id()));
        assert_eq!(backing(&root).unwrap(), Backing::Ram);
        let mut reasons = Vec::new();
        let automatic = Candidate {
            path: root.clone(),
            explicit: false,
        };
        assert_eq!(choose(&[automatic], true, &mut reasons), None);
        assert_eq!(reasons, [format!("{} is RAM-backed", root.display())]);
        assert!(!root.exists());
    }

    #[test]
    fn workspace_directories_are_named_after_the_folder_and_its_path() {
        let root = tempfile::tempdir().unwrap();
        let folders = tempfile::tempdir().unwrap();
        let sandbox = Sandbox::at(root.path(), false);
        for (name, expected) in [
            ("my project", "my-project"),
            ("café ünïcode", "caf-n-code"),
            (
                "abcdefghijklmnopqrstuvwxyz0123456789ABCD",
                "abcdefghijklmnopqrstuvwx",
            ),
            ("aaaaaaaaaaaaaaaaaaaaaaa-bbbb", "aaaaaaaaaaaaaaaaaaaaaaa"),
            ("--a---b--", "a-b"),
            ("日本", "folder"),
            (".venv", ".venv"),
        ] {
            let folder = folders.path().join(name);
            fs::create_dir(&folder).unwrap();
            let scratch = sandbox.workspace(&folder).unwrap();
            let canonical = folder.canonicalize().unwrap();
            assert_eq!(
                scratch.dir,
                root.path().join(format!(
                    "{expected}-{}",
                    worktree::digest(canonical.as_os_str().as_bytes())
                )),
                "{name:?}"
            );
            assert!(is_workspace_name(scratch.dir.file_name().unwrap()));
        }
        assert_eq!(folder_name(Path::new("/")), "folder");
        // The same folder by another path is the same workspace.
        let link = folders.path().join("link");
        std::os::unix::fs::symlink(folders.path().join("my project"), &link).unwrap();
        assert_eq!(
            sandbox.workspace(&link).unwrap().dir,
            sandbox
                .workspace(&folders.path().join("my project"))
                .unwrap()
                .dir
        );
        for name in [
            "",
            "-0123456789abcdef01234567",
            "x-0123456789abcdef0123456",
            "x-0123456789ABCDEF01234567",
            "x_0123456789abcdef01234567",
        ] {
            assert!(!is_workspace_name(OsStr::new(name)), "{name:?}");
        }
        assert!(is_workspace_name(OsStr::new("x-0123456789abcdef01234567")));
    }

    #[test]
    fn every_use_stamps_the_marker_with_the_folder() {
        let root = tempfile::tempdir().unwrap();
        let folder = tempfile::tempdir().unwrap();
        let canonical = folder.path().canonicalize().unwrap();
        let sandbox = Sandbox::at(root.path(), false);
        let scratch = sandbox.workspace(folder.path()).unwrap();
        let marker = scratch.dir.join(MARKER);
        let mut expected = canonical.as_os_str().as_bytes().to_vec();
        expected.push(b'\n');
        assert_eq!(fs::read(&marker).unwrap(), expected);
        assert_eq!(read_marker(&scratch.dir).unwrap(), canonical);
        assert_eq!((mode(&scratch.dir), mode(&marker)), (0o700, 0o600));

        let long_ago = unix_now() - 40 * DAY;
        touch(&marker, long_ago);
        assert_eq!(sandbox.workspace(folder.path()).unwrap(), scratch);
        let stamped = fs::symlink_metadata(&marker).unwrap().mtime();
        assert!(stamped >= unix_now() - 60, "{stamped}");
        // Nothing but the marker is left behind.
        let names: Vec<_> = fs::read_dir(&scratch.dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, [OsString::from(MARKER)]);

        for bad in [&b""[..], b"relative\n", b"/no-newline", b"/nul\0\n"] {
            fs::write(&marker, bad).unwrap();
            assert_eq!(
                read_marker(&scratch.dir).unwrap_err().kind(),
                io::ErrorKind::InvalidData,
                "{bad:?}"
            );
        }
    }

    #[test]
    fn removing_a_workspace_removes_exactly_its_directory() {
        let root = tempfile::tempdir().unwrap();
        let folders = tempfile::tempdir().unwrap();
        let sandbox = Sandbox::at(root.path(), false);
        let (one, two) = (folders.path().join("one"), folders.path().join("two"));
        fs::create_dir(&one).unwrap();
        fs::create_dir(&two).unwrap();
        let first = sandbox.workspace(&one).unwrap();
        let second = sandbox.workspace(&two).unwrap();
        fs::create_dir_all(first.dir.join("clone/src")).unwrap();
        fs::write(first.dir.join("clone/src/main.rs"), "fn main() {}").unwrap();
        // A symlink to the other workspace goes, not what it points at.
        fs::write(second.dir.join("kept"), "kept").unwrap();
        std::os::unix::fs::symlink(&second.dir, first.dir.join("other")).unwrap();

        sandbox.remove_workspace(&one).unwrap();
        assert!(!first.dir.exists());
        assert_eq!(fs::read_to_string(second.dir.join("kept")).unwrap(), "kept");
        // Twice, or for a folder that never had scratch, there is nothing to do.
        sandbox.remove_workspace(&one).unwrap();
        sandbox
            .remove_workspace(&folders.path().join("never"))
            .unwrap();
        assert!(second.dir.join(MARKER).exists());
    }

    #[test]
    fn the_note_names_the_directory_and_what_ram_costs() {
        let note_on_disk = note(Path::new("/var/tmp/crit-u/app-0123"), false);
        assert_eq!(
            note_on_disk,
            "Scratch space: create every temporary clone, worktree, virtualenv, build copy, download and other scratch file under $TMPDIR (/var/tmp/crit-u/app-0123), which Crit keeps on disk for this workspace and cleans automatically; never under /tmp."
        );
        assert_eq!(
            note(Path::new("/var/tmp/crit-u/app-0123"), true),
            format!(
                "{note_on_disk} On this machine /tmp is RAM-backed: files there consume memory and swap."
            )
        );
        let root = tempfile::tempdir().unwrap();
        let folder = tempfile::tempdir().unwrap();
        let scratch = Sandbox::at(root.path(), true)
            .workspace(folder.path())
            .unwrap();
        assert_eq!(scratch.note, note(&scratch.dir, true));
    }

    #[test]
    fn a_pass_removes_what_is_gone_or_unused_and_keeps_the_rest() {
        let root = tempfile::tempdir().unwrap();
        let folders = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let sandbox = Sandbox::at(root.path(), false);
        let workspace = |name: &str| {
            let folder = folders.path().join(name);
            fs::create_dir(&folder).unwrap();
            (sandbox.workspace(&folder).unwrap().dir, folder)
        };
        // Fixtures are made now, so their ctimes are now. The pass runs 60 days ahead: what is
        // only made now is 60 days old by both stamps, and "fresh" is a day before then.
        let now = unix_now() + 60 * DAY;
        let fresh = now - DAY;

        let (gone, gone_folder) = workspace("gone");
        fs::write(gone.join("download"), vec![1; 8192]).unwrap();
        fs::remove_dir(&gone_folder).unwrap();

        let (used, _) = workspace("used");
        fs::create_dir_all(used.join("old-clone/.git")).unwrap();
        fs::write(used.join("old-clone/.git/pack"), vec![1; 65536]).unwrap();
        fs::write(used.join("old-file"), "old").unwrap();
        fs::create_dir_all(used.join("venv/lib")).unwrap();
        fs::write(used.join("venv/lib/site.py"), "fresh").unwrap();
        touch(&used.join("venv/lib/site.py"), fresh);
        fs::write(elsewhere.path().join("target"), "kept").unwrap();
        std::os::unix::fs::symlink(elsewhere.path().join("target"), used.join("link")).unwrap();

        let (idle, _) = workspace("idle");
        let (open_idle, open_folder) = workspace("open-idle");
        let (fresh_idle, _) = workspace("fresh-idle");
        touch(&fresh_idle.join(MARKER), fresh);

        let unmarked = root.path().join("unmarked-0123456789abcdef01234567");
        fs::create_dir(&unmarked).unwrap();
        fs::write(unmarked.join("data"), "data").unwrap();
        let own = root.path().join("omp-shell-snapshots-1000");
        fs::create_dir(&own).unwrap();

        let report = sandbox.clean_at(&[open_folder], now).unwrap();
        assert_eq!(
            (report.removed_workspaces, report.removed_entries),
            (2, 3),
            "{report:?}"
        );
        assert!(report.removed_bytes >= 65536 + 8192, "{report:?}");
        assert_eq!(report.errors.len(), 1, "{report:?}");
        assert!(report.errors[0].contains("unmarked-") && report.errors[0].contains(MARKER));
        // (a)
        assert!(!gone.exists());
        // (b): the fresh tree and the marker stay, so the directory does too.
        assert!(!used.join("old-clone").exists() && !used.join("old-file").exists());
        assert!(!used.join("link").exists());
        assert_eq!(
            fs::read_to_string(elsewhere.path().join("target")).unwrap(),
            "kept"
        );
        assert!(used.join("venv/lib/site.py").exists() && used.join(MARKER).exists());
        // (c): only the empty directory of a workspace neither open nor used lately goes.
        assert!(!idle.exists());
        assert!(open_idle.join(MARKER).exists() && fresh_idle.join(MARKER).exists());
        assert!(unmarked.join("data").exists() && own.exists());

        // Again, nothing is left to remove.
        let report = sandbox
            .clean_at(&[folders.path().join("open-idle")], now)
            .unwrap();
        assert_eq!(report.summary(), None, "{report:?}");
    }

    #[test]
    fn a_recent_ctime_keeps_an_entry_whatever_its_mtime() {
        let root = tempfile::tempdir().unwrap();
        let folder = tempfile::tempdir().unwrap();
        let sandbox = Sandbox::at(root.path(), false);
        let scratch = sandbox.workspace(folder.path()).unwrap();
        // An archive unpacked today keeps the mtimes it was packed with.
        fs::write(scratch.dir.join("unpacked"), "data").unwrap();
        touch(&scratch.dir.join("unpacked"), unix_now() - 400 * DAY);
        let report = sandbox.clean(&[]).unwrap();
        assert_eq!(report, CleanReport::default());
        assert!(scratch.dir.join("unpacked").exists());
    }

    #[test]
    fn a_pass_stamps_the_open_workspaces_first() {
        let root = tempfile::tempdir().unwrap();
        let folder = tempfile::tempdir().unwrap();
        let sandbox = Sandbox::at(root.path(), false);
        let scratch = sandbox.workspace(folder.path()).unwrap();
        let marker = scratch.dir.join(MARKER);
        touch(&marker, unix_now() - 40 * DAY);
        assert_eq!(
            sandbox.pass(&[folder.path().to_owned()]).unwrap(),
            CleanReport::default()
        );
        assert!(fs::symlink_metadata(&marker).unwrap().mtime() >= unix_now() - 60);
        // A root that cannot be read is the one error a window shows.
        let gone = Sandbox::at(&root.path().join("gone"), false);
        let error = gone.pass(&[]).unwrap_err();
        assert!(
            error.starts_with(&format!(
                "Sandbox {} could not be cleaned: ",
                gone.root().display()
            )),
            "{error}"
        );
    }

    #[test]
    fn clearing_removes_every_workspace_directory_and_nothing_else() {
        let root = tempfile::tempdir().unwrap();
        let folder = tempfile::tempdir().unwrap();
        let sandbox = Sandbox::at(root.path(), false);
        let scratch = sandbox.workspace(folder.path()).unwrap();
        fs::write(scratch.dir.join("file"), "data").unwrap();
        let unmarked = root.path().join("x-0123456789abcdef01234567");
        fs::create_dir(&unmarked).unwrap();
        let own = root.path().join("tmp.crit-helper");
        fs::write(&own, "").unwrap();
        let report = sandbox.clear().unwrap();
        assert_eq!((report.removed_workspaces, report.errors.len()), (2, 0));
        assert!(!scratch.dir.exists() && !unmarked.exists() && own.exists());
    }

    #[test]
    fn sizes_use_binary_units() {
        for (bytes, shown) in [
            (0, "0 B"),
            (1023, "1023 B"),
            (1024, "1.0 KiB"),
            (1536, "1.5 KiB"),
            (1024 * 1024 - 1, "1.0 MiB"),
            (1_825_361_101, "1.7 GiB"),
            (12_025_908_428, "11.2 GiB"),
            (3 << 40, "3.0 TiB"),
        ] {
            assert_eq!(size(bytes), shown, "{bytes}");
        }
        for (seconds, shown) in [
            (-5, "0s"),
            (59, "59s"),
            (60, "1m"),
            (7200, "2h"),
            (3 * DAY + 5, "3d"),
        ] {
            assert_eq!(age(seconds), shown);
        }
    }

    #[test]
    fn summaries_say_what_went() {
        let report = |bytes, workspaces, entries| CleanReport {
            removed_bytes: bytes,
            removed_workspaces: workspaces,
            removed_entries: entries,
            errors: vec!["ignored".into()],
        };
        assert_eq!(report(0, 0, 0).summary(), None);
        assert_eq!(
            report(2_469_606_195, 2, 5).summary().unwrap(),
            "Sandbox: removed 2.3 GiB of scratch unused for 30 days (2 workspaces gone, 5 entries)."
        );
        assert_eq!(
            report(4096, 1, 0).summary().unwrap(),
            "Sandbox: removed 4.0 KiB of scratch unused for 30 days (1 workspace gone)."
        );
        assert_eq!(
            report(512, 0, 1).summary().unwrap(),
            "Sandbox: removed 512 B of scratch unused for 30 days (1 entry)."
        );
    }

    #[test]
    fn status_lines_align_sizes_and_names() {
        let name = "crit-0123456789abcdef01234567";
        assert_eq!(
            status_line(
                1_825_361_101,
                name,
                32,
                Some(Path::new("/home/u/crit")),
                false
            ),
            "   1.7 GiB  crit-0123456789abcdef01234567     /home/u/crit\n"
        );
        assert_eq!(
            status_line(
                4096,
                "x-0123456789abcdef01234567",
                26,
                Some(Path::new("/gone")),
                true
            ),
            "   4.0 KiB  x-0123456789abcdef01234567  /gone  missing\n"
        );
        assert_eq!(
            status_line(0, "y-0123456789abcdef01234567", 26, None, false),
            "       0 B  y-0123456789abcdef01234567  (no readable .crit-workspace)\n"
        );
    }

    #[test]
    fn describing_lists_workspaces_largest_first() {
        let root = tempfile::tempdir().unwrap();
        let folders = tempfile::tempdir().unwrap();
        let sandbox = Sandbox::at(root.path(), false);
        let small = folders.path().join("small");
        let large = folders.path().join("large");
        fs::create_dir(&small).unwrap();
        fs::create_dir(&large).unwrap();
        sandbox.workspace(&small).unwrap();
        let big = sandbox.workspace(&large).unwrap();
        fs::write(big.dir.join("blob"), vec![1; 1 << 20]).unwrap();
        fs::remove_dir(&large).unwrap();
        let mut out = String::new();
        sandbox.describe(&mut out);
        let lines: Vec<&str> = out.lines().collect();
        assert!(
            lines[0].starts_with(&format!("Sandbox {} (", root.path().display())),
            "{out}"
        );
        assert_eq!(lines.len(), 3, "{out}");
        assert!(
            lines[1].contains("large-") && lines[1].ends_with("  missing"),
            "{out}"
        );
        assert!(lines[1].trim_start().starts_with("1.0 MiB"), "{out}");
        let small = small.canonicalize().unwrap();
        assert!(lines[2].contains("small-"), "{out}");
        assert!(lines[2].ends_with(&small.display().to_string()), "{out}");
    }

    #[test]
    fn idle_priority_lowers_only_the_calling_thread() {
        let own = unsafe { libc::getpriority(libc::PRIO_PROCESS, 0) };
        let (nice, class) = std::thread::spawn(|| {
            idle_priority();
            // SAFETY: both only read this thread's scheduling.
            unsafe {
                (
                    libc::getpriority(libc::PRIO_PROCESS, 0),
                    libc::syscall(libc::SYS_ioprio_get, 1, 0) >> 13,
                )
            }
        })
        .join()
        .unwrap();
        assert_eq!((nice, class), (19, 3));
        assert_eq!(unsafe { libc::getpriority(libc::PRIO_PROCESS, 0) }, own);
    }
}
