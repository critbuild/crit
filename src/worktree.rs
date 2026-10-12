use anyhow::{Context, Result, bail, ensure};
use sha2::{Digest, Sha256};
use std::collections::VecDeque;
use std::ffi::OsStr;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

#[derive(Clone, Debug)]
pub struct Repository {
    pub root: PathBuf,
    pub common_dir: PathBuf,
}

#[derive(Clone, Debug)]
pub struct Worktree {
    pub id: String,
    pub path: PathBuf,
    pub branch: String,
    pub head: String,
    pub is_main: bool,
    pub locked: bool,
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

fn digest(bytes: &[u8]) -> String {
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
fn canonical_path(path: &Path) -> Result<PathBuf> {
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
    let output = git(cwd)
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
    let output = checked(
        git(&repository.root).args(["worktree", "list", "--porcelain", "-z"]),
        "list Git worktrees",
    )?;
    parse_list(&output.stdout)
}

fn parse_list(bytes: &[u8]) -> Result<Vec<Worktree>> {
    let mut trees = Vec::new();
    let mut current: Option<Worktree> = None;
    for field in bytes.split(|byte| *byte == 0) {
        if let Some(path) = field.strip_prefix(b"worktree ") {
            if let Some(tree) = current.take() {
                trees.push(tree);
            }
            let path = canonical_path(Path::new(OsStr::from_bytes(path)))?;
            current = Some(Worktree {
                id: digest(path.as_os_str().as_bytes()),
                path,
                branch: String::new(),
                head: String::new(),
                is_main: trees.is_empty(),
                locked: false,
            });
        } else if let Some(tree) = current.as_mut() {
            if let Some(head) = field.strip_prefix(b"HEAD ") {
                tree.head = String::from_utf8(head.to_vec()).context("non-UTF-8 Git object id")?;
            } else if let Some(branch) = field.strip_prefix(b"branch refs/heads/") {
                tree.branch =
                    String::from_utf8(branch.to_vec()).context("non-UTF-8 Git branch name")?;
            } else if field == b"locked" || field.starts_with(b"locked ") {
                tree.locked = true;
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
        .status()
        .context("inspect local configuration ignore rule")?;
    ensure!(
        ignored.success() || ignored.code() == Some(1),
        "could not inspect local configuration ignore rule"
    );
    Ok(ignored.success())
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
    let exists = git(&repository.root)
        .args([
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ])
        .status()
        .context("check whether branch exists")?;
    ensure!(
        exists.success() || exists.code() == Some(1),
        "could not inspect branch {branch:?}"
    );
    let base_commit = if exists.success() && base.is_none() {
        None
    } else {
        let revision = format!("{}^{{commit}}", base.unwrap_or("HEAD"));
        let output = checked(
            git(&repository.root).args(["rev-parse", "--verify", "--end-of-options", &revision]),
            "resolve worktree base commit",
        )?;
        if exists.success() {
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
    // Git performs its own final cleanliness/lock checks. Never force, and never
    // delete the branch: a checkout and its branch have independent lifetimes.
    checked(
        git(&repository.root)
            .args(["worktree", "remove", "--"])
            .arg(&current.path),
        "remove Git worktree",
    )?;
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
        let trees = parse_list(&bytes).unwrap();
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
}
