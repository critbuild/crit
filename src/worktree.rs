use anyhow::{Context, Result, bail, ensure};
use sha2::{Digest, Sha256};
use std::collections::VecDeque;
use std::ffi::OsStr;
use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
pub struct Repository {
    pub root: PathBuf,
    pub common_dir: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Worktree {
    pub id: String,
    pub path: PathBuf,
    pub branch: String,
    pub head: String,
    pub is_main: bool,
    pub locked: bool,
    pub bare: bool,
    pub prunable: bool,
}

impl Worktree {
    pub fn label(&self) -> &str {
        if !self.branch.is_empty() {
            &self.branch
        } else if !self.head.is_empty() {
            "detached HEAD"
        } else {
            "terminal"
        }
    }

    /// Bare repositories have no files and prunable entries (checkout deleted
    /// outside Git) have no directory, so neither can be opened.
    pub fn is_checkout(&self) -> bool {
        !self.bare && !self.prunable
    }
}

fn git(cwd: &Path) -> Command {
    let mut command = Command::new("git");
    command.arg("-C").arg(cwd);
    // A parent process's Git routing must not redirect operations to another tree.
    for key in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_COMMON_DIR",
        "GIT_INDEX_FILE",
    ] {
        command.env_remove(key);
    }
    command
}

fn checked(command: &mut Command, operation: &str) -> Result<Output> {
    let output = command
        .output()
        .with_context(|| format!("could not run {operation}"))?;
    ensure!(
        output.status.success(),
        "{operation}: {}",
        String::from_utf8_lossy(&output.stderr).trim_end()
    );
    Ok(output)
}

fn output_path(bytes: &[u8]) -> PathBuf {
    let bytes = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    PathBuf::from(OsStr::from_bytes(bytes))
}

pub(crate) fn digest(bytes: &[u8]) -> String {
    let hash = Sha256::digest(bytes);
    let mut result = String::with_capacity(24);
    use std::fmt::Write;
    for byte in &hash[..12] {
        write!(result, "{byte:02x}").expect("writing to a string cannot fail");
    }
    result
}

// Git may retain registrations for deleted worktrees. Resolve the existing parent
// in that case as well, so recreating the checkout does not change its identity.
pub(crate) fn canonical_path(path: &Path) -> Result<PathBuf> {
    match fs::canonicalize(path) {
        Ok(path) => Ok(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let parent = path.parent().context("worktree path has no parent")?;
            let name = path.file_name().context("worktree path has no file name")?;
            Ok(canonical_path(parent)?.join(name))
        }
        Err(error) => Err(error).with_context(|| format!("resolve {}", path.display())),
    }
}

pub fn discover(path: &Path) -> Result<Repository> {
    discover_optional(path)?
        .with_context(|| format!("{} is not inside a Git repository", path.display()))
}

/// `Ok(None)` when `path` is outside every Git repository; any other Git
/// failure (unreadable, dubious ownership, bare repository) is an error.
pub fn discover_optional(path: &Path) -> Result<Option<Repository>> {
    let cwd = if path.is_file() {
        path.parent().unwrap_or(path)
    } else {
        path
    };
    // The "not a git repository" match below needs Git's untranslated message.
    let output = git(cwd)
        .env("LC_ALL", "C")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .context("could not run discover Git worktree")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr = stderr.trim_end();
        ensure!(
            stderr.contains("not a git repository"),
            "discover Git worktree: {stderr}"
        );
        return Ok(None);
    }
    let root = fs::canonicalize(output_path(&output.stdout)).context("resolve repository root")?;
    let common = checked(
        git(&root).args(["rev-parse", "--path-format=absolute", "--git-common-dir"]),
        "discover Git common directory",
    )?;
    let common_dir =
        fs::canonicalize(output_path(&common.stdout)).context("resolve Git common directory")?;
    Ok(Some(Repository { root, common_dir }))
}

// A one-shot, bounded search keeps the launcher responsive without indexing
// dependency trees or following directory symlinks outside the chosen roots.
pub fn projects(start: &Path, home: Option<&Path>) -> Vec<PathBuf> {
    let start = fs::canonicalize(start).ok();
    let home = home
        .and_then(|path| fs::canonicalize(path).ok())
        .filter(|path| Some(path) != start.as_ref());
    let mut found = start
        .as_deref()
        .and_then(|path| discover(path).ok())
        .map(|repository| vec![repository.root])
        .unwrap_or_default();
    let roots = [start, home];
    for root in roots.iter().flatten() {
        let mut pending = VecDeque::from([(root.clone(), 0)]);
        let mut queued = 1;
        let mut entries_seen = 0;
        'scan: while let Some((directory, depth)) = pending.pop_front() {
            // A stale .git marker must not hide every project beneath this folder.
            if directory.join(".git").exists()
                && git(&directory)
                    .args(["rev-parse", "--show-toplevel"])
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status()
                    .is_ok_and(|status| status.success())
            {
                found.push(directory);
                continue;
            }
            if depth == 4 {
                continue;
            }
            let Ok(entries) = fs::read_dir(&directory) else {
                continue;
            };
            for entry in entries.flatten() {
                entries_seen += 1;
                if entries_seen > 32_768 {
                    break 'scan;
                }
                let name = entry.file_name();
                if name.as_bytes().starts_with(b".")
                    || matches!(
                        name.to_str(),
                        Some("node_modules" | "target" | "vendor" | "dist" | "build")
                    )
                    || !entry.file_type().is_ok_and(|kind| kind.is_dir())
                {
                    continue;
                }
                let path = entry.path();
                if roots
                    .iter()
                    .flatten()
                    .any(|other| other != root && *other == path)
                {
                    continue;
                }
                if queued < 2_048 {
                    pending.push_back((path, depth + 1));
                    queued += 1;
                }
            }
        }
    }
    found.sort();
    found.dedup();
    found
}

pub fn list(repository: &Repository) -> Result<Vec<Worktree>> {
    // The discovered root may be removed or moved later; the common directory
    // outlives every linked checkout and Git lists identically from it.
    let output = git(&repository.common_dir)
        .args(["worktree", "list", "--porcelain", "-z"])
        .output()
        .context("could not run list Git worktrees")?;
    let mut trees = if output.status.code() == Some(129) {
        // Git < 2.36 has no -z; its newline porcelain never quotes paths.
        let output = checked(
            git(&repository.common_dir).args(["worktree", "list", "--porcelain"]),
            "list Git worktrees",
        )?;
        parse_list(&output.stdout, b'\n')?
    } else {
        ensure!(
            output.status.success(),
            "list Git worktrees: {}",
            String::from_utf8_lossy(&output.stderr).trim_end()
        );
        parse_list(&output.stdout, 0)?
    };
    // Git reports a submodule's Git directory as its main worktree; the real
    // checkout is the one `core.worktree` names.
    if let Some(main) = trees.first_mut()
        && !main.bare
        && main.path == repository.common_dir
        && let Ok(top) = checked(
            git(&repository.common_dir).args(["--git-dir=.", "rev-parse", "--show-toplevel"]),
            "locate submodule checkout",
        )
        && let Ok(path) = fs::canonicalize(output_path(&top.stdout))
    {
        main.id = digest(path.as_os_str().as_bytes());
        main.path = path;
    }
    Ok(trees)
}

/// How long a listing is trusted while its fingerprint holds, for what `stat` cannot see: two
/// updates within one timestamp tick that reuse an inode, a symlink retargeted above a
/// checkout, configuration included from outside the repository.
const LISTING_REFRESH: Duration = Duration::from_secs(30);

/// Directory entries one fingerprint may visit, so a poll's cost stays bounded. A repository
/// with more loose branch refs and linked worktrees is listed on every poll, as if it had no
/// fingerprint.
const FINGERPRINT_ENTRIES: usize = 4096;

/// One path in a fingerprint. A directory records its identity only: its mtime moves with
/// every file created inside it (an index rewritten by `git status`, an editor's temporary
/// file), none of which changes the listing.
#[derive(Hash)]
enum Stamp {
    Missing(std::io::ErrorKind),
    Directory(u64),
    /// Git replaces refs and `HEAD` by renaming a lock file over them, so the inode marks an
    /// update even when the mtime cannot.
    File {
        inode: u64,
        size: u64,
        modified: (i64, i64),
    },
}

impl Stamp {
    fn of(metadata: std::io::Result<fs::Metadata>) -> Self {
        match metadata {
            Ok(metadata) if metadata.is_dir() => Self::Directory(metadata.ino()),
            Ok(metadata) => Self::File {
                inode: metadata.ino(),
                size: metadata.size(),
                modified: (metadata.mtime(), metadata.mtime_nsec()),
            },
            Err(error) => Self::Missing(error.kind()),
        }
    }
}

fn hash_one(value: impl Hash) -> u64 {
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

/// `directory/name` in a reused buffer: a poll stats a fixed set of names per repository.
fn join_into<'a>(buffer: &'a mut PathBuf, directory: &Path, name: &str) -> &'a Path {
    buffer.as_mut_os_string().clear();
    buffer.push(directory);
    buffer.push(name);
    buffer
}

/// Hashes what `git worktree list` reads from the common directory, without running Git: the
/// main `HEAD`; the branches every `HEAD` resolves through (loose refs at any depth under
/// `refs/heads`, `packed-refs`, or a reftable stack); the configuration (`core.bare`, a
/// submodule's `core.worktree`); and per linked worktree its `HEAD`, the `gitdir` file naming
/// its checkout, and its `locked` note. Indexes, logs and lock files are left out: Git rewrites
/// them on every `git status` or commit without changing the listing. `None` once the walk
/// passes `FINGERPRINT_ENTRIES`.
fn metadata_fingerprint(common_dir: &Path) -> Option<u64> {
    let mut budget = FINGERPRINT_ENTRIES;
    let mut hasher = DefaultHasher::new();
    let mut path = PathBuf::new();
    for name in [
        "HEAD",
        "packed-refs",
        "config",
        "config.worktree",
        "reftable/tables.list",
    ] {
        Stamp::of(fs::metadata(join_into(&mut path, common_dir, name))).hash(&mut hasher);
    }
    refs_fingerprint(common_dir.join("refs/heads"), &mut budget)?.hash(&mut hasher);
    linked_fingerprint(&common_dir.join("worktrees"), &mut budget)?.hash(&mut hasher);
    Some(hasher.finish())
}

/// Every loose branch ref, at any depth: `feature/x` lives in `refs/heads/feature`. Entries
/// are combined regardless of order, which is the filesystem's.
fn refs_fingerprint(root: PathBuf, budget: &mut usize) -> Option<u64> {
    let mut sum = 0u64;
    let mut pending = vec![root];
    while let Some(directory) = pending.pop() {
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            // A reftable repository keeps a placeholder file here.
            Err(error) => {
                sum = sum.wrapping_add(hash_one((&directory, Stamp::Missing(error.kind()))));
                continue;
            }
        };
        for entry in entries {
            *budget = budget.checked_sub(1)?;
            let entry = entry.ok()?;
            let name = entry.file_name();
            // A ref being updated: the rename that completes the update changes the ref.
            if name.as_bytes().ends_with(b".lock") {
                continue;
            }
            // Not following symlinks keeps the walk inside `refs/heads`.
            let metadata = entry.metadata();
            if metadata.as_ref().is_ok_and(fs::Metadata::is_dir) {
                pending.push(entry.path());
            }
            sum = sum.wrapping_add(hash_one((&directory, &name, Stamp::of(metadata))));
        }
    }
    Some(sum)
}

/// Each linked worktree's registration, combined regardless of order. Git removes the
/// directory along with the last one.
fn linked_fingerprint(directory: &Path, budget: &mut usize) -> Option<u64> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) => return Some(hash_one(Stamp::Missing(error.kind()))),
    };
    let mut sum = 0u64;
    let mut path = PathBuf::new();
    for entry in entries {
        *budget = budget.checked_sub(1)?;
        let registration = entry.ok()?.path();
        let mut hasher = DefaultHasher::new();
        registration.hash(&mut hasher);
        for name in ["HEAD", "gitdir", "locked", "reftable/tables.list"] {
            Stamp::of(fs::metadata(join_into(&mut path, &registration, name))).hash(&mut hasher);
        }
        sum = sum.wrapping_add(hasher.finish());
    }
    Some(sum)
}

/// Each listed checkout and its `.git`: a checkout deleted outside Git turns prunable without
/// touching the common directory.
fn checkouts_fingerprint(paths: &[PathBuf]) -> u64 {
    let mut hasher = DefaultHasher::new();
    let mut git = PathBuf::new();
    for path in paths {
        path.hash(&mut hasher);
        Stamp::of(fs::symlink_metadata(path)).hash(&mut hasher);
        Stamp::of(fs::metadata(join_into(&mut git, path, ".git"))).hash(&mut hasher);
    }
    hasher.finish()
}

/// A repository's last listing, as `ListingCache` remembers it.
struct Listed {
    common_dir: PathBuf,
    /// Taken before the listing ran; `None` when the repository was too large to fingerprint.
    metadata: Option<u64>,
    checkouts: u64,
    /// The listed checkouts, which the next `checkouts_fingerprint` stamps.
    paths: Vec<PathBuf>,
    at: Instant,
}

/// Lists a polled repository again only when something `list` reads may have changed, so an
/// idle repository costs a `stat` per branch ref, worktree file and checkout per poll instead
/// of a Git process.
#[derive(Default)]
pub struct ListingCache {
    listed: Vec<Listed>,
}

impl ListingCache {
    /// `Ok(None)` while nothing the repository's last listing depends on has changed and that
    /// listing is younger than `LISTING_REFRESH`; otherwise lists afresh. A failed listing
    /// changes nothing here, so the next call tries again.
    pub fn refresh(&mut self, repository: &Repository) -> Result<Option<Vec<Worktree>>> {
        self.refresh_at(repository, Instant::now())
    }

    fn refresh_at(
        &mut self,
        repository: &Repository,
        now: Instant,
    ) -> Result<Option<Vec<Worktree>>> {
        let index = self
            .listed
            .iter()
            .position(|listed| listed.common_dir == repository.common_dir);
        // Taken before Git runs, so a change racing the listing differs from it next time.
        let metadata = metadata_fingerprint(&repository.common_dir);
        let mut checkouts =
            checkouts_fingerprint(index.map_or(&[], |index| self.listed[index].paths.as_slice()));
        if let Some(listed) = index.map(|index| &self.listed[index])
            && metadata.is_some()
            && listed.metadata == metadata
            && listed.checkouts == checkouts
            && now.saturating_duration_since(listed.at) < LISTING_REFRESH
        {
            return Ok(None);
        }
        let trees = list(repository)?;
        let index = index.unwrap_or_else(|| {
            self.listed.push(Listed {
                common_dir: repository.common_dir.clone(),
                metadata: None,
                checkouts: 0,
                paths: Vec::new(),
                at: now,
            });
            self.listed.len() - 1
        });
        let listed = &mut self.listed[index];
        if !trees.iter().map(|tree| &tree.path).eq(&listed.paths) {
            // Checkouts new to this listing are stamped after it: one deleted in between
            // shows at the next `LISTING_REFRESH`.
            listed.paths.clear();
            listed
                .paths
                .extend(trees.iter().map(|tree| tree.path.clone()));
            checkouts = checkouts_fingerprint(&listed.paths);
        }
        listed.metadata = metadata;
        listed.checkouts = checkouts;
        listed.at = now;
        Ok(Some(trees))
    }
}

fn branch_exists(repository: &Repository, branch: &str) -> Result<bool> {
    let exists = git(&repository.root)
        .args([
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ])
        .output()
        .context("check whether branch exists")?;
    ensure!(
        exists.status.success() || exists.status.code() == Some(1),
        "could not inspect branch {branch:?}: {}",
        String::from_utf8_lossy(&exists.stderr).trim_end()
    );
    Ok(exists.status.success())
}

fn parse_list(bytes: &[u8], separator: u8) -> Result<Vec<Worktree>> {
    let mut trees = Vec::new();
    let mut current: Option<Worktree> = None;
    for field in bytes.split(|byte| *byte == separator) {
        if let Some(path) = field.strip_prefix(b"worktree ") {
            if let Some(tree) = current.take() {
                trees.push(tree);
            }
            // An unresolvable registered path (dead mount, no permission) must not
            // break the whole listing; Git prints absolute paths.
            let raw = Path::new(OsStr::from_bytes(path));
            let path = canonical_path(raw).unwrap_or_else(|_| raw.to_path_buf());
            current = Some(Worktree {
                id: digest(path.as_os_str().as_bytes()),
                path,
                branch: String::new(),
                head: String::new(),
                is_main: trees.is_empty(),
                locked: false,
                bare: false,
                prunable: false,
            });
        } else if let Some(tree) = current.as_mut() {
            if let Some(head) = field.strip_prefix(b"HEAD ") {
                tree.head = String::from_utf8(head.to_vec()).context("non-UTF-8 Git object id")?;
            } else if let Some(branch) = field.strip_prefix(b"branch refs/heads/") {
                // Display and comparison only: checkouts are reopened by path.
                tree.branch = String::from_utf8_lossy(branch).into_owned();
            } else if field == b"locked" || field.starts_with(b"locked ") {
                tree.locked = true;
            } else if field == b"bare" {
                tree.bare = true;
            } else if field == b"prunable" || field.starts_with(b"prunable ") {
                tree.prunable = true;
            }
        }
    }
    if let Some(tree) = current {
        trees.push(tree);
    }
    Ok(trees)
}

/// Only ignored, untracked files are eligible for local configuration copying.
/// Checking the index separately prevents copying a tracked file deleted locally.
pub(crate) fn ignored_local_file(root: &Path, relative: &Path) -> Result<bool> {
    let tracked = checked(
        git(root).args(["ls-files", "-z", "--"]).arg(relative),
        "inspect local configuration tracking",
    )?;
    if !tracked.stdout.is_empty() {
        return Ok(false);
    }
    let ignored = git(root)
        .args(["check-ignore", "--quiet", "--"])
        .arg(relative)
        .output()
        .context("inspect local configuration ignore rule")?;
    ensure!(
        ignored.status.success() || ignored.status.code() == Some(1),
        "could not inspect local configuration ignore rule: {}",
        String::from_utf8_lossy(&ignored.stderr).trim_end()
    );
    Ok(ignored.status.success())
}

fn branch_slug(branch: &str) -> String {
    let mut slug = String::new();
    for byte in branch.bytes() {
        if slug.len() >= 48 {
            break;
        }
        if byte.is_ascii_alphanumeric() || byte == b'_' {
            slug.push(char::from(byte));
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let slug = slug.trim_end_matches('-');
    if slug.is_empty() {
        "branch".to_owned()
    } else {
        slug.to_owned()
    }
}

pub fn create(repository: &Repository, branch: &str, base: Option<&str>) -> Result<Worktree> {
    ensure!(
        !branch.is_empty() && !branch.starts_with('-'),
        "invalid branch name: {branch:?}"
    );
    checked(
        git(&repository.root).args(["check-ref-format", &format!("refs/heads/{branch}")]),
        "validate branch name",
    )?;
    if let Some(base) = base {
        ensure!(
            !base.is_empty() && !base.starts_with('-'),
            "invalid base revision: {base:?}"
        );
    }
    let exists = branch_exists(repository, branch)?;
    let base_commit = if exists && base.is_none() {
        None
    } else {
        let revision = format!("{}^{{commit}}", base.unwrap_or("HEAD"));
        let output = checked(
            git(&repository.root).args(["rev-parse", "--verify", "--end-of-options", &revision]),
            "resolve worktree base commit",
        )?;
        if exists {
            // A base applies to new branches only; never silently open the
            // existing branch at another commit.
            let existing = checked(
                git(&repository.root).args([
                    "rev-parse",
                    "--verify",
                    "--end-of-options",
                    &format!("refs/heads/{branch}^{{commit}}"),
                ]),
                "resolve existing branch commit",
            )?;
            if existing.stdout != output.stdout {
                let id = String::from_utf8_lossy(&existing.stdout);
                let id = id.trim_end();
                bail!(
                    "branch {branch} already exists at {}; it was not created from {}. Omit the base to open the existing branch, or choose a new name",
                    id.get(..12).unwrap_or(id),
                    base.unwrap_or("HEAD")
                );
            }
            None
        } else {
            Some(
                String::from_utf8(output.stdout)
                    .context("non-UTF-8 Git commit id")?
                    .trim_end()
                    .to_owned(),
            )
        }
    };
    let trees = list(repository)?;
    add_checkout(repository, branch, base_commit.as_deref(), &trees)
}

fn add_checkout(
    repository: &Repository,
    branch: &str,
    base_commit: Option<&str>,
    trees: &[Worktree],
) -> Result<Worktree> {
    let main = trees
        .iter()
        .find(|tree| tree.is_main)
        .context("repository has no main worktree")?;
    let parent = main
        .path
        .parent()
        .context("repository root has no parent directory")?;
    let directory = parent
        .join(".crit-worktrees")
        .join(digest(repository.common_dir.as_os_str().as_bytes()));
    let target = directory.join(format!(
        "{}-{}",
        branch_slug(branch),
        digest(branch.as_bytes())
    ));
    ensure!(
        !target.try_exists()?,
        "worktree destination already exists: {}",
        target.display()
    );
    fs::create_dir_all(&directory).with_context(|| format!("create {}", directory.display()))?;
    let mut command = git(&repository.root);
    command.args(["worktree", "add"]);
    if let Some(commit) = base_commit {
        command
            .arg("-b")
            .arg(branch)
            .arg("--")
            .arg(&target)
            .arg(commit);
    } else {
        command.arg("--").arg(&target).arg(branch);
    }
    checked(&mut command, "create Git worktree")?;
    let target = fs::canonicalize(&target).context("resolve new worktree")?;
    list(repository)?
        .into_iter()
        .find(|tree| tree.path == target)
        .context("Git created the checkout but did not report it in the worktree list")
}

pub fn remove(repository: &Repository, worktree: &Worktree) -> Result<()> {
    let path = canonical_path(&worktree.path)?;
    let current = list(repository)?
        .into_iter()
        .find(|tree| tree.path == path && tree.id == worktree.id)
        .context("worktree is not registered in this repository")?;
    ensure!(!current.is_main, "refusing to remove the main worktree");
    ensure!(!current.locked, "refusing to remove a locked worktree");
    // A checkout deleted outside Git leaves only its registration: nothing to inspect.
    let missing = matches!(
        fs::symlink_metadata(&current.path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound
    );
    if !missing {
        let status = checked(
            git(&current.path).args([
                "status",
                "--porcelain=v1",
                "-z",
                "--untracked-files=all",
                "--ignore-submodules=none",
            ]),
            "check worktree for local changes",
        )?;
        if !status.stdout.is_empty() {
            bail!(
                "refusing to remove dirty worktree {}",
                current.path.display()
            );
        }
        ensure_deletable(&current.path)?;
    }
    // Git performs its own final cleanliness/lock checks. Never force, and never
    // delete the branch: a checkout and its branch have independent lifetimes.
    checked(
        git(&repository.common_dir)
            .args(["worktree", "remove", "--"])
            .arg(&current.path),
        "remove Git worktree",
    )?;
    Ok(())
}

/// Git deletes ignored files along with the checkout and, when one deletion
/// fails midway, still drops the registration, leaving a half-deleted checkout.
/// Refuse up front when a directory cannot be emptied.
fn ensure_deletable(root: &Path) -> Result<()> {
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let name = std::ffi::CString::new(directory.as_os_str().as_bytes())
            .with_context(|| format!("inspect {}", directory.display()))?;
        ensure!(
            unsafe { libc::access(name.as_ptr(), libc::R_OK | libc::W_OK | libc::X_OK) } == 0,
            "refusing to remove: {} is not writable by you (often created by a container); fix its ownership or delete it, then retry",
            directory.display()
        );
        let entries =
            fs::read_dir(&directory).with_context(|| format!("read {}", directory.display()))?;
        for entry in entries {
            let entry = entry.with_context(|| format!("read {}", directory.display()))?;
            // Symlinks are never followed: Git removes the link itself.
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                pending.push(entry.path());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn repository() -> (TempDir, Repository) {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("repo with spaces");
        fs::create_dir(&root).unwrap();
        checked(
            git(&root).args(["init", "-b", "main"]),
            "init test repository",
        )
        .unwrap();
        checked(
            git(&root).args([
                "-c",
                "user.name=Crit Test",
                "-c",
                "user.email=crit@example.invalid",
                "commit",
                "--allow-empty",
                "-m",
                "initial",
            ]),
            "commit test repository",
        )
        .unwrap();
        let repository = discover(&root).unwrap();
        (directory, repository)
    }

    #[test]
    fn rediscovery_tracks_external_head_changes_per_worktree() {
        let (_directory, repository) = repository();
        let linked = create(&repository, "linked", None).unwrap();
        let main = list(&repository).unwrap().remove(0);
        checked(
            git(&repository.root).args(["switch", "-c", "switched"]),
            "switch external branch",
        )
        .unwrap();
        let trees = list(&repository).unwrap();
        let refreshed = trees.iter().find(|tree| tree.id == main.id).unwrap();
        assert_eq!(refreshed.label(), "switched");
        assert_eq!(
            trees
                .iter()
                .find(|tree| tree.id == linked.id)
                .unwrap()
                .label(),
            "linked"
        );

        checked(
            git(&repository.root).args(["branch", "-m", "renamed"]),
            "rename external branch",
        )
        .unwrap();
        assert_eq!(list(&repository).unwrap()[0].label(), "renamed");
        checked(
            git(&linked.path).args(["checkout", "--detach", "HEAD"]),
            "detach linked checkout",
        )
        .unwrap();
        let trees = list(&repository).unwrap();
        let detached = trees.iter().find(|tree| tree.id == linked.id).unwrap();
        assert!(detached.branch.is_empty());
        assert_eq!(detached.head, linked.head);
        assert_eq!(detached.label(), "detached HEAD");
        assert_eq!(
            trees
                .iter()
                .find(|tree| tree.id == main.id)
                .unwrap()
                .label(),
            "renamed"
        );
    }

    #[test]
    fn ignored_local_configuration_excludes_tracked_and_unignored_files() {
        let (_directory, repository) = repository();
        fs::write(repository.root.join(".gitignore"), "**/env/*.env*\n").unwrap();
        fs::create_dir(repository.root.join("env")).unwrap();
        let relative = Path::new("env/keycloak.env");
        fs::write(repository.root.join(relative), "fixture").unwrap();
        assert!(ignored_local_file(&repository.root, relative).unwrap());
        assert!(!ignored_local_file(&repository.root, Path::new("env/database.db")).unwrap());
        checked(
            git(&repository.root)
                .args(["add", "--force", "--"])
                .arg(relative),
            "track ignored configuration",
        )
        .unwrap();
        fs::remove_file(repository.root.join(relative)).unwrap();
        assert!(!ignored_local_file(&repository.root, relative).unwrap());
    }

    #[test]
    fn project_search_finds_checkouts_without_duplicate_roots_or_dependency_trees() {
        let (directory, repository) = repository();
        fs::write(directory.path().join(".git"), "stale Git marker").unwrap();
        let linked = directory.path().join("linked checkout");
        checked(
            git(&repository.root)
                .args(["worktree", "add", "-b", "linked"])
                .arg(&linked),
            "create linked checkout",
        )
        .unwrap();
        let dependency = directory.path().join("node_modules/dependency");
        fs::create_dir_all(&dependency).unwrap();
        checked(
            git(&dependency).args(["init", "-b", "main"]),
            "init dependency",
        )
        .unwrap();
        let outside = tempfile::tempdir().unwrap();
        checked(
            git(outside.path()).args(["init", "-b", "main"]),
            "init outside checkout",
        )
        .unwrap();
        std::os::unix::fs::symlink(outside.path(), directory.path().join("shortcut")).unwrap();
        let nested = linked.join("nested");
        fs::create_dir(&nested).unwrap();
        assert_eq!(projects(&nested, None), vec![linked.clone()]);
        assert_eq!(
            projects(&repository.root, Some(directory.path())),
            vec![linked, repository.root],
        );
    }

    #[test]
    fn nul_porcelain_preserves_path_whitespace_and_lock_reason() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("space\nand\ttab");
        fs::create_dir(&path).unwrap();
        let mut bytes = b"worktree ".to_vec();
        bytes.extend_from_slice(path.as_os_str().as_bytes());
        bytes.extend_from_slice(
            b"\0HEAD abc\0branch refs/heads/topic\0locked because work matters\0\0",
        );
        let trees = parse_list(&bytes, 0).unwrap();
        assert_eq!(trees.len(), 1);
        assert_eq!(trees[0].path, fs::canonicalize(path).unwrap());
        assert_eq!(trees[0].branch, "topic");
        assert!(trees[0].locked);
    }

    #[test]
    fn removal_refuses_main_dirty_and_locked_and_keeps_branch() {
        let (_directory, repository) = repository();
        assert!(remove(&repository, &list(&repository).unwrap()[0]).is_err());
        let tree = create(&repository, "feature/a", None).unwrap();
        fs::write(tree.path.join("untracked"), "keep me").unwrap();
        assert!(
            remove(&repository, &tree)
                .unwrap_err()
                .to_string()
                .contains("dirty")
        );
        fs::remove_file(tree.path.join("untracked")).unwrap();
        checked(
            git(&repository.root)
                .args(["worktree", "lock", "--"])
                .arg(&tree.path),
            "lock test worktree",
        )
        .unwrap();
        assert!(
            remove(&repository, &tree)
                .unwrap_err()
                .to_string()
                .contains("locked")
        );
        checked(
            git(&repository.root)
                .args(["worktree", "unlock", "--"])
                .arg(&tree.path),
            "unlock test worktree",
        )
        .unwrap();
        remove(&repository, &tree).unwrap();
        assert!(!tree.path.exists());
        checked(
            git(&repository.root).args(["show-ref", "--verify", "refs/heads/feature/a"]),
            "retain removed worktree branch",
        )
        .unwrap();
        let recreated = create(&repository, "feature/a", None).unwrap();
        assert_eq!(recreated.id, tree.id);
        assert_eq!(recreated.head, tree.head);
    }

    #[test]
    fn colliding_slugs_and_option_like_refs_are_safe() {
        let (_directory, repository) = repository();
        assert!(create(&repository, "--detach", None).is_err());
        assert!(create(&repository, "bad..name", None).is_err());
        assert!(create(&repository, "valid", Some("--help")).is_err());
        let first = create(&repository, "topic/a", None).unwrap();
        let second = create(&repository, "topic-a", None).unwrap();
        assert_ne!(first.path, second.path);
        assert_ne!(first.id, second.id);
    }

    #[test]
    fn list_and_remove_survive_loss_of_the_discovered_root() {
        let (_directory, repository) = repository();
        let first = create(&repository, "first", None).unwrap();
        let second = create(&repository, "second", None).unwrap();
        // Crit may have been opened inside a linked checkout that is removed later.
        let opened = discover(&first.path).unwrap();
        assert_eq!(opened.root, first.path);
        remove(&opened, &first).unwrap();
        assert!(!opened.root.exists());
        let trees = list(&opened).unwrap();
        assert_eq!(trees.len(), 2);
        assert!(trees[0].is_main);
        assert!(trees.iter().any(|tree| tree.id == second.id));
        remove(&opened, &second).unwrap();
        assert_eq!(list(&opened).unwrap().len(), 1);
    }

    #[test]
    fn newline_porcelain_from_git_2_34_parses() {
        // Verbatim output of Git 2.34.1, which has no `-z`: main, a deleted checkout, a locked one.
        let bytes = b"worktree /tmp/tmp.DxGRnf9rz2/repo\nHEAD 892b551236f0a36cd8a198af890d0cd3c388c3a9\nbranch refs/heads/main\n\nworktree /tmp/tmp.DxGRnf9rz2/gone\nHEAD 892b551236f0a36cd8a198af890d0cd3c388c3a9\nbranch refs/heads/gone\nprunable gitdir file points to non-existent location\n\nworktree /tmp/tmp.DxGRnf9rz2/locked\nHEAD 892b551236f0a36cd8a198af890d0cd3c388c3a9\nbranch refs/heads/locked\nlocked on usb\n\n";
        let trees = parse_list(bytes, b'\n').unwrap();
        assert_eq!(trees.len(), 3);
        assert_eq!(
            trees.iter().map(|tree| tree.is_main).collect::<Vec<_>>(),
            [true, false, false]
        );
        assert_eq!(
            trees.iter().map(|tree| tree.prunable).collect::<Vec<_>>(),
            [false, true, false]
        );
        assert_eq!(
            trees.iter().map(|tree| tree.locked).collect::<Vec<_>>(),
            [false, false, true]
        );
        assert_eq!(trees[1].branch, "gone");
        assert!(trees[2].path.ends_with("locked"));
    }

    #[test]
    fn submodule_main_worktree_is_its_checkout() {
        let (directory, library) = repository();
        let superproject = directory.path().join("super");
        fs::create_dir(&superproject).unwrap();
        checked(
            git(&superproject).args(["init", "-b", "main"]),
            "init superproject",
        )
        .unwrap();
        checked(
            git(&superproject)
                .args(["-c", "protocol.file.allow=always", "submodule", "add", "--"])
                .arg(&library.root)
                .arg("libs/lib"),
            "add submodule",
        )
        .unwrap();
        let checkout = fs::canonicalize(superproject.join("libs/lib")).unwrap();
        let submodule = discover(&checkout).unwrap();
        assert_ne!(submodule.common_dir, checkout);
        let trees = list(&submodule).unwrap();
        assert_eq!(trees[0].path, checkout);
        assert!(trees[0].is_main && trees[0].is_checkout());
    }

    #[test]
    fn missing_checkouts_are_hidden_removable_and_recreatable() {
        let (_directory, repository) = repository();
        let tree = create(&repository, "feat", None).unwrap();
        fs::remove_dir_all(&tree.path).unwrap();
        let trees = list(&repository).unwrap();
        let listed = trees.iter().find(|listed| listed.id == tree.id).unwrap();
        assert!(listed.prunable && !listed.is_checkout());
        remove(&repository, &tree).unwrap();
        assert_eq!(list(&repository).unwrap().len(), 1);
        let recreated = create(&repository, "feat", None).unwrap();
        assert_eq!(recreated.id, tree.id);
    }

    #[test]
    fn bare_repository_entry_is_not_offered_as_a_checkout() {
        let (directory, source) = repository();
        checked(
            git(directory.path())
                .args(["clone", "--bare", "--quiet", "--"])
                .arg(&source.root)
                .arg("bare.git"),
            "clone bare repository",
        )
        .unwrap();
        checked(
            git(&directory.path().join("bare.git")).args([
                "worktree",
                "add",
                "--quiet",
                "../wt-main",
                "main",
            ]),
            "add checkout to bare repository",
        )
        .unwrap();
        let repository = discover(&directory.path().join("wt-main")).unwrap();
        let trees = list(&repository).unwrap();
        assert!(trees[0].is_main && trees[0].bare && !trees[0].is_checkout());
        let created = create(&repository, "topic", None).unwrap();
        let expected = fs::canonicalize(directory.path())
            .unwrap()
            .join(".crit-worktrees");
        assert!(created.path.starts_with(expected));
    }

    #[test]
    fn non_utf8_branch_names_do_not_break_listing() {
        let (_directory, repository) = repository();
        let name = OsStr::from_bytes(b"caf\xe9");
        checked(
            git(&repository.root).arg("branch").arg(name),
            "create non-UTF-8 branch",
        )
        .unwrap();
        checked(
            git(&repository.root).arg("switch").arg(name),
            "check out non-UTF-8 branch",
        )
        .unwrap();
        let trees = list(&repository).unwrap();
        assert!(trees[0].label().contains('\u{FFFD}'));
    }

    #[test]
    fn existing_branch_at_another_commit_refuses_an_explicit_base() {
        let (_directory, repository) = repository();
        for name in ["topic", "same"] {
            checked(
                git(&repository.root).args(["branch", name]),
                "create existing test branch",
            )
            .unwrap();
        }
        let old = list(&repository).unwrap().remove(0).head;
        checked(
            git(&repository.root).args([
                "-c",
                "user.name=Crit Test",
                "-c",
                "user.email=crit@example.invalid",
                "commit",
                "--allow-empty",
                "-m",
                "advance main",
            ]),
            "advance main",
        )
        .unwrap();
        let error = create(&repository, "topic", Some("main"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("already exists"), "{error}");
        assert_eq!(list(&repository).unwrap().len(), 1);
        assert_eq!(create(&repository, "topic", None).unwrap().head, old);
        // A base that names the branch's own commit is not a conflict.
        assert_eq!(create(&repository, "same", Some("same")).unwrap().head, old);
    }

    #[test]
    fn ignore_rule_errors_include_git_reason() {
        let (_directory, repository) = repository();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), repository.root.join("env")).unwrap();
        let error = ignored_local_file(&repository.root, Path::new("env/secret.env"))
            .unwrap_err()
            .to_string();
        // Git echoes the pathspec in every locale: only its stderr can put it here.
        assert!(error.contains("env/secret.env"), "{error}");
    }

    #[test]
    fn removal_refuses_unwritable_directories_and_keeps_checkout() {
        use std::os::unix::fs::PermissionsExt;
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let (_directory, repository) = repository();
        fs::create_dir_all(repository.common_dir.join("info")).unwrap();
        fs::write(repository.common_dir.join("info/exclude"), "data/\n").unwrap();
        let tree = create(&repository, "db", None).unwrap();
        let data = tree.path.join("data/pg");
        fs::create_dir_all(&data).unwrap();
        fs::write(data.join("PG_VERSION"), "17").unwrap();
        fs::set_permissions(&data, fs::Permissions::from_mode(0o500)).unwrap();
        let refused = remove(&repository, &tree);
        let registered = list(&repository)
            .unwrap()
            .iter()
            .any(|listed| listed.id == tree.id);
        // Restore access before asserting so a failure still lets the directory go.
        fs::set_permissions(&data, fs::Permissions::from_mode(0o700)).unwrap();
        let error = refused.unwrap_err().to_string();
        assert!(error.contains("not writable"), "{error}");
        assert!(registered && tree.path.join(".git").exists());
        assert!(data.join("PG_VERSION").exists());
        remove(&repository, &tree).unwrap();
        assert!(!tree.path.exists());
    }

    #[test]
    fn inaccessible_worktree_does_not_break_listing() {
        use std::os::unix::fs::PermissionsExt;
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let (directory, repository) = repository();
        let parent = directory.path().join("mnt");
        fs::create_dir(&parent).unwrap();
        checked(
            git(&repository.root)
                .args(["worktree", "add", "-b", "far"])
                .arg(parent.join("far")),
            "create distant checkout",
        )
        .unwrap();
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o000)).unwrap();
        let trees = list(&repository);
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(trees.unwrap().len(), 2);
    }

    fn run(command: &mut Command) {
        checked(command, "change test repository").unwrap();
    }

    fn commit(checkout: &Path, message: &str) {
        run(git(checkout).args([
            "-c",
            "user.name=Crit Test",
            "-c",
            "user.email=crit@example.invalid",
            "commit",
            "--allow-empty",
            "-m",
            message,
        ]));
    }

    fn listed_paths(repository: &Repository) -> Vec<PathBuf> {
        list(repository)
            .unwrap()
            .into_iter()
            .map(|tree| tree.path)
            .collect()
    }

    /// What `ListingCache` compares: the common directory now, and the checkouts of the
    /// listing taken before.
    fn fingerprint(repository: &Repository, paths: &[PathBuf]) -> (Option<u64>, u64) {
        (
            metadata_fingerprint(&repository.common_dir),
            checkouts_fingerprint(paths),
        )
    }

    fn assert_changes(repository: &Repository, label: &str, change: impl FnOnce()) {
        let paths = listed_paths(repository);
        let before = fingerprint(repository, &paths);
        assert!(before.0.is_some(), "{label}: walk bound");
        change();
        assert_ne!(fingerprint(repository, &paths), before, "{label}");
    }

    #[test]
    fn fingerprint_sees_every_change_the_listing_shows() {
        let (directory, repository) = repository();
        let root = &repository.root;
        assert_changes(&repository, "commit", || commit(root, "second"));
        assert_changes(&repository, "switch -c", || {
            run(git(root).args(["switch", "-c", "topic"]))
        });
        assert_changes(&repository, "branch -m", || {
            run(git(root).args(["branch", "-m", "renamed"]))
        });
        let linked = directory.path().join("linked");
        assert_changes(&repository, "worktree add", || {
            run(git(root)
                .args(["worktree", "add", "-b", "feature/x", "--"])
                .arg(&linked))
        });
        assert_changes(&repository, "commit in a linked checkout", || {
            commit(&linked, "linked")
        });
        assert_changes(&repository, "detached linked checkout", || {
            run(git(&linked).args(["checkout", "--detach"]))
        });
        assert_changes(&repository, "worktree lock", || {
            run(git(root).args(["worktree", "lock", "--"]).arg(&linked))
        });
        assert_changes(&repository, "worktree unlock", || {
            run(git(root).args(["worktree", "unlock", "--"]).arg(&linked))
        });
        let moved = directory.path().join("moved");
        assert_changes(&repository, "worktree move", || {
            run(git(root)
                .args(["worktree", "move", "--"])
                .arg(&linked)
                .arg(&moved))
        });
        assert_changes(&repository, "worktree remove", || {
            run(git(root).args(["worktree", "remove", "--"]).arg(&moved))
        });
        let deleted = directory.path().join("deleted");
        run(git(root)
            .args(["worktree", "add", "-b", "deleted", "--"])
            .arg(&deleted));
        assert_changes(&repository, "checkout deleted outside Git", || {
            fs::remove_dir_all(&deleted).unwrap()
        });
        assert!(list(&repository).unwrap()[1].prunable);
        assert_changes(&repository, "worktree prune", || {
            run(git(root).args(["worktree", "prune"]))
        });
    }

    #[test]
    fn fingerprint_ignores_work_that_leaves_the_listing_alone() {
        let (directory, repository) = repository();
        let linked = directory.path().join("linked");
        run(git(&repository.root)
            .args(["worktree", "add", "-b", "linked", "--"])
            .arg(&linked));
        let paths = listed_paths(&repository);
        let before = fingerprint(&repository, &paths);
        // Agents and shell prompts do this all the time: it rewrites indexes and both roots.
        for checkout in [&repository.root, &linked] {
            fs::write(checkout.join("new file"), "contents").unwrap();
            run(git(checkout).args(["add", "--", "new file"]));
            run(git(checkout).args(["status", "--porcelain"]));
        }
        assert_eq!(listed_paths(&repository), paths);
        assert_eq!(fingerprint(&repository, &paths), before);
    }

    #[test]
    fn fingerprint_sees_reftable_updates() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("repo");
        fs::create_dir(&root).unwrap();
        // Git before 2.45 has no reftable backend.
        if checked(
            git(&root).args(["init", "-b", "main", "--ref-format=reftable"]),
            "init reftable repository",
        )
        .is_err()
        {
            return;
        }
        commit(&root, "initial");
        let repository = discover(&root).unwrap();
        assert_changes(&repository, "commit", || commit(&root, "second"));
        assert_changes(&repository, "switch -c", || {
            run(git(&root).args(["switch", "-c", "topic"]))
        });
        let linked = directory.path().join("linked");
        assert_changes(&repository, "worktree add", || {
            run(git(&root)
                .args(["worktree", "add", "-b", "linked", "--"])
                .arg(&linked))
        });
        assert_changes(&repository, "commit in a linked checkout", || {
            commit(&linked, "linked")
        });
        assert_changes(&repository, "detached linked checkout", || {
            run(git(&linked).args(["checkout", "--detach"]))
        });
    }

    #[test]
    fn listing_cache_lists_again_after_a_change_or_the_safety_interval() {
        let (directory, repository) = repository();
        let mut cache = ListingCache::default();
        let start = Instant::now();
        let just_before = |time: Instant| time + LISTING_REFRESH - Duration::from_millis(1);
        assert_eq!(
            cache.refresh_at(&repository, start).unwrap(),
            Some(list(&repository).unwrap())
        );
        assert_eq!(cache.refresh_at(&repository, start).unwrap(), None);
        assert_eq!(
            cache.refresh_at(&repository, just_before(start)).unwrap(),
            None
        );
        commit(&repository.root, "second");
        let listed = cache.refresh_at(&repository, start).unwrap();
        assert_eq!(listed, Some(list(&repository).unwrap()));
        assert_eq!(cache.refresh_at(&repository, start).unwrap(), None);
        // A new checkout is stamped with its own listing: no second listing follows.
        run(git(&repository.root)
            .args(["worktree", "add", "-b", "linked", "--"])
            .arg(directory.path().join("linked")));
        let later = start + Duration::from_secs(1);
        assert_eq!(
            cache.refresh_at(&repository, later).unwrap().unwrap().len(),
            2
        );
        assert_eq!(cache.refresh_at(&repository, later).unwrap(), None);
        assert_eq!(
            cache.refresh_at(&repository, just_before(later)).unwrap(),
            None
        );
        assert!(
            cache
                .refresh_at(&repository, later + LISTING_REFRESH)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn listing_cache_retries_a_failed_listing() {
        let (_directory, repository) = repository();
        let mut cache = ListingCache::default();
        assert!(cache.refresh(&repository).unwrap().is_some());
        let config = repository.common_dir.join("config");
        let valid = fs::read(&config).unwrap();
        fs::write(&config, "[broken").unwrap();
        assert!(cache.refresh(&repository).is_err());
        // Nothing changed since the failure, and Git is still asked.
        assert!(cache.refresh(&repository).is_err());
        fs::write(&config, valid).unwrap();
        assert!(cache.refresh(&repository).unwrap().is_some());
        assert_eq!(cache.refresh(&repository).unwrap(), None);
    }

    #[test]
    fn listing_cache_lists_every_time_past_the_walk_bound() {
        let (_directory, repository) = repository();
        let head = list(&repository).unwrap().remove(0).head;
        let bulk = repository.common_dir.join("refs/heads/bulk");
        fs::create_dir(&bulk).unwrap();
        for index in 0..FINGERPRINT_ENTRIES {
            fs::write(bulk.join(index.to_string()), format!("{head}\n")).unwrap();
        }
        assert_eq!(metadata_fingerprint(&repository.common_dir), None);
        let mut cache = ListingCache::default();
        assert!(cache.refresh(&repository).unwrap().is_some());
        assert!(cache.refresh(&repository).unwrap().is_some());
    }
}
