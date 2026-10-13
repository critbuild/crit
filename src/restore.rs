use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, BufWriter, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

pub const VERSION: u32 = 1;
pub const SPLIT_RATIO_SCALE: u16 = 10_000;

pub fn default_split_ratio() -> u16 {
    5_000
}

const MAX_SNAPSHOT_BYTES: u64 = 8 * 1024 * 1024;
const SNAPSHOT_FILE: &str = "workspace.json";
const BACKUP_FILE: &str = "workspace.json.bak";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    pub version: u32,
    pub projects: Vec<SavedProject>,
    pub active_project: usize,
    pub native_keys: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SavedProject {
    pub workspaces: Vec<SavedWorkspace>,
    pub active: usize,
    pub secondary: Option<usize>,
    /// Independent secondary selection, only when both panes share the active workspace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secondary_session: Option<usize>,
    pub focus_secondary: bool,
    pub layout: SavedLayout,
    #[serde(default = "default_split_ratio")]
    pub split_ratio: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SavedLayout {
    Full,
    Vertical,
    Horizontal,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SavedWorkspace {
    pub path: PathBuf,
    pub source: Option<PathBuf>,
    pub sessions: Vec<SavedSession>,
    pub selected: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SavedSession {
    pub kind: String,
    pub resume: Option<String>,
}

impl Snapshot {
    fn validate(&self) -> Result<()> {
        if self.version != VERSION {
            bail!(
                "unsupported workspace snapshot version {} (expected {VERSION})",
                self.version
            );
        }
        valid_selection(self.active_project, self.projects.len())
            .context("invalid active project index")?;
        for (project_index, project) in self.projects.iter().enumerate() {
            if project.split_ratio > SPLIT_RATIO_SCALE {
                bail!("project {project_index} has an out-of-range split ratio");
            }
            if project.workspaces.is_empty() {
                bail!("project {project_index} has no workspaces");
            }
            valid_selection(project.active, project.workspaces.len())
                .with_context(|| format!("invalid active workspace in project {project_index}"))?;
            if let Some(secondary) = project.secondary {
                valid_selection(secondary, project.workspaces.len()).with_context(|| {
                    format!("invalid secondary workspace in project {project_index}")
                })?;
                if secondary == project.active {
                    let workspace = &project.workspaces[secondary];
                    let Some(session) = project.secondary_session else {
                        bail!(
                            "project {project_index} has no secondary session for its shared workspace"
                        );
                    };
                    if session >= workspace.sessions.len() || session == workspace.selected {
                        bail!(
                            "project {project_index} has an invalid secondary session for its shared workspace"
                        );
                    }
                } else if project.secondary_session.is_some() {
                    bail!(
                        "project {project_index} has a secondary session for different workspaces"
                    );
                }
            }
            if project.secondary.is_none() && project.secondary_session.is_some() {
                bail!(
                    "project {project_index} has a secondary session without a secondary workspace"
                );
            }
            match project.layout {
                SavedLayout::Full
                    if project.secondary.is_some()
                        || project.secondary_session.is_some()
                        || project.focus_secondary =>
                {
                    bail!("project {project_index} has split state in a full layout");
                }
                SavedLayout::Vertical | SavedLayout::Horizontal if project.secondary.is_none() => {
                    bail!("project {project_index} has no secondary workspace for its split");
                }
                _ => {}
            }
            for (workspace_index, workspace) in project.workspaces.iter().enumerate() {
                let validate_workspace = || -> Result<()> {
                    valid_path(&workspace.path).context("invalid workspace path")?;
                    if let Some(source) = &workspace.source {
                        valid_path(source).context("invalid source checkout path")?;
                    }
                    valid_selection(workspace.selected, workspace.sessions.len())
                        .context("invalid selected session index")?;
                    for (session_index, session) in workspace.sessions.iter().enumerate() {
                        match session.kind.as_str() {
                            "omp" | "opencode" => {}
                            "shell" if session.resume.is_none() => {}
                            "shell" => bail!("shell session {session_index} has a resume identity"),
                            _ => bail!(
                                "session {session_index} has unknown kind {:?}",
                                session.kind
                            ),
                        }
                        if let Some(resume) = &session.resume {
                            if resume.trim().is_empty() || resume.contains('\0') {
                                bail!("session {session_index} has an invalid resume identity");
                            }
                        }
                    }
                    Ok(())
                };
                validate_workspace().with_context(|| {
                    format!("invalid workspace {workspace_index} in project {project_index}")
                })?;
            }
        }
        Ok(())
    }
}

fn valid_selection(selected: usize, count: usize) -> Result<()> {
    if (count == 0 && selected != 0) || (count != 0 && selected >= count) {
        bail!("index {selected} is out of range for {count} entries");
    }
    Ok(())
}

fn valid_path(path: &Path) -> Result<()> {
    if !path.is_absolute() || path.as_os_str().as_bytes().contains(&0) {
        bail!("path must be absolute and contain no NUL bytes");
    }
    Ok(())
}

/// Downcast an `open` error to this type to distinguish another running owner.
#[derive(Debug)]
pub struct Busy {
    directory: PathBuf,
}

impl fmt::Display for Busy {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "another Crit instance owns workspace state in {}",
            self.directory.display()
        )
    }
}

impl std::error::Error for Busy {}

/// Owns the advisory lock until dropped. The lock file is never removed: removing
/// it would let another process lock a different inode at the same pathname.
#[derive(Debug)]
pub struct Store {
    directory: PathBuf,
    directory_file: File,
    lock: File,
}

impl Drop for Store {
    fn drop(&mut self) {
        // A concurrently spawned PTY child can hold this open-file description
        // until exec closes its inherited descriptors. Release ownership now,
        // rather than waiting for every inherited copy of the descriptor.
        unsafe {
            libc::flock(self.lock.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

impl Store {
    pub fn open() -> Result<Self> {
        let base = match std::env::var_os("XDG_STATE_HOME").filter(|value| !value.is_empty()) {
            Some(value) => PathBuf::from(value),
            None => PathBuf::from(
                std::env::var_os("HOME")
                    .filter(|value| !value.is_empty())
                    .context("neither XDG_STATE_HOME nor HOME is set for workspace state")?,
            )
            .join(".local/state"),
        };
        if !base.is_absolute() {
            bail!("workspace state base directory must be absolute");
        }
        Self::open_in(base.join("crit"))
    }

    /// Uses this exact directory, without reading or changing global environment.
    pub fn open_in(directory: impl AsRef<Path>) -> Result<Self> {
        let directory = directory.as_ref();
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(directory)
            .with_context(|| {
                format!("creating workspace state directory {}", directory.display())
            })?;
        let directory_file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(directory)
            .with_context(|| {
                format!("opening workspace state directory {}", directory.display())
            })?;
        if directory_file.metadata()?.uid() != unsafe { libc::geteuid() } {
            bail!("workspace state directory is not owned by the current user");
        }
        directory_file
            .set_permissions(fs::Permissions::from_mode(0o700))
            .context("making workspace state directory private")?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
            .open(directory.join("workspace.lock"))
            .context("opening workspace state lock")?;
        private_regular_file(&lock).context("invalid workspace state lock")?;
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::WouldBlock {
                return Err(Busy {
                    directory: directory.to_path_buf(),
                }
                .into());
            }
            return Err(error).context("locking workspace state");
        }
        lock.set_permissions(fs::Permissions::from_mode(0o600))
            .context("making workspace state lock private")?;
        Ok(Self {
            directory: directory.to_path_buf(),
            directory_file,
            lock,
        })
    }

    /// A load error never modifies snapshot bytes. The caller must disable saving
    /// after failed recovery rather than overwrite the unreadable snapshot.
    pub fn load(&self) -> Result<Option<Snapshot>> {
        let path = self.directory.join(SNAPSHOT_FILE);
        let file = match OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
            .open(&path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error).with_context(|| format!("opening snapshot {}", path.display()));
            }
        };
        private_regular_file(&file).context("invalid workspace snapshot file")?;
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .context("making workspace snapshot private")?;
        let length = file.metadata()?.len();
        if length > MAX_SNAPSHOT_BYTES {
            bail!("workspace snapshot exceeds the {MAX_SNAPSHOT_BYTES}-byte limit");
        }
        let mut bytes = Vec::with_capacity(length as usize);
        file.take(MAX_SNAPSHOT_BYTES + 1)
            .read_to_end(&mut bytes)
            .context("reading workspace snapshot")?;
        if bytes.len() as u64 > MAX_SNAPSHOT_BYTES {
            bail!("workspace snapshot exceeds the {MAX_SNAPSHOT_BYTES}-byte limit");
        }
        let snapshot: Snapshot =
            serde_json::from_slice(&bytes).context("decoding workspace snapshot")?;
        snapshot
            .validate()
            .context("validating workspace snapshot")?;
        Ok(Some(snapshot))
    }

    pub fn save(&self, snapshot: &Snapshot) -> Result<()> {
        self.write(snapshot, SNAPSHOT_FILE)
    }

    /// Keeps `snapshot` as `workspace.json.bak` beside the live file, validated and
    /// written as privately and atomically as `save`, so a restore that leaves items
    /// out can replace `workspace.json` without losing them. Returns the backup's path.
    pub fn back_up(&self, snapshot: &Snapshot) -> Result<PathBuf> {
        self.write(snapshot, BACKUP_FILE)?;
        Ok(self.directory.join(BACKUP_FILE))
    }

    fn write(&self, snapshot: &Snapshot, name: &str) -> Result<()> {
        snapshot
            .validate()
            .context("validating workspace snapshot")?;
        let mut temporary = tempfile::Builder::new()
            .prefix(".workspace-")
            .tempfile_in(&self.directory)
            .context("creating temporary workspace snapshot")?;
        temporary
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))
            .context("making temporary workspace snapshot private")?;
        {
            let mut writer = BufWriter::new(temporary.as_file_mut());
            serde_json::to_writer(&mut writer, snapshot).context("encoding workspace snapshot")?;
            writer.flush().context("writing workspace snapshot")?;
        }
        if temporary.as_file().metadata()?.len() > MAX_SNAPSHOT_BYTES {
            bail!("workspace snapshot exceeds the {MAX_SNAPSHOT_BYTES}-byte limit");
        }
        temporary
            .as_file()
            .sync_all()
            .context("syncing temporary workspace snapshot")?;
        temporary
            .persist(self.directory.join(name))
            .context("replacing workspace snapshot")?;
        // Rename is the commit point. A directory-sync error means the complete
        // new snapshot is visible, but its survival across power loss is unknown.
        self.directory_file
            .sync_all()
            .context("snapshot replaced, but syncing its directory failed")?;
        Ok(())
    }
}

fn private_regular_file(file: &File) -> Result<()> {
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.uid() != unsafe { libc::geteuid() } || metadata.nlink() != 1
    {
        bail!("state must be a regular, singly linked file owned by the current user");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    fn snapshot() -> Snapshot {
        Snapshot {
            version: VERSION,
            projects: vec![
                SavedProject {
                    workspaces: vec![
                        SavedWorkspace {
                            path: "/missing/project/first".into(),
                            source: Some("/missing/project/source".into()),
                            sessions: vec![
                                SavedSession {
                                    kind: "omp".into(),
                                    resume: Some("/missing/chats/first.jsonl".into()),
                                },
                                SavedSession {
                                    kind: "omp".into(),
                                    resume: Some("/missing/chats/second.jsonl".into()),
                                },
                                SavedSession {
                                    kind: "shell".into(),
                                    resume: None,
                                },
                            ],
                            selected: 1,
                        },
                        SavedWorkspace {
                            path: "/missing/project/second".into(),
                            source: None,
                            sessions: vec![
                                SavedSession {
                                    kind: "opencode".into(),
                                    resume: Some("ses_distinct_chat".into()),
                                },
                                SavedSession {
                                    kind: "omp".into(),
                                    resume: None,
                                },
                            ],
                            selected: 0,
                        },
                    ],
                    active: 0,
                    secondary: Some(1),
                    secondary_session: None,
                    focus_secondary: true,
                    layout: SavedLayout::Vertical,
                    split_ratio: default_split_ratio(),
                },
                SavedProject {
                    workspaces: vec![SavedWorkspace {
                        path: "/missing/other-project".into(),
                        source: None,
                        sessions: vec![],
                        selected: 0,
                    }],
                    active: 0,
                    secondary: None,
                    secondary_session: None,
                    focus_secondary: false,
                    layout: SavedLayout::Full,
                    split_ratio: default_split_ratio(),
                },
            ],
            active_project: 1,
            native_keys: true,
        }
    }

    #[test]
    fn roundtrip_preserves_chat_identity_order_and_empty_workspaces() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open_in(directory.path()).unwrap();
        assert_eq!(store.load().unwrap(), None);
        let expected = snapshot();
        store.save(&expected).unwrap();
        drop(store);
        let reopened = Store::open_in(directory.path()).unwrap();
        assert_eq!(reopened.load().unwrap(), Some(expected));
        let empty = Snapshot {
            version: VERSION,
            projects: vec![],
            active_project: 0,
            native_keys: false,
        };
        reopened.save(&empty).unwrap();
        assert_eq!(reopened.load().unwrap(), Some(empty));
    }

    #[test]
    fn legacy_snapshots_load_with_equal_split_ratios() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open_in(directory.path()).unwrap();
        let expected = snapshot();
        let mut legacy = serde_json::to_value(&expected).unwrap();
        assert!(
            legacy["projects"]
                .as_array()
                .unwrap()
                .iter()
                .all(|project| project.get("secondary_session").is_none())
        );
        for project in legacy["projects"].as_array_mut().unwrap() {
            project.as_object_mut().unwrap().remove("split_ratio");
            project.as_object_mut().unwrap().remove("secondary_session");
        }
        fs::write(
            directory.path().join(SNAPSHOT_FILE),
            serde_json::to_vec(&legacy).unwrap(),
        )
        .unwrap();
        let loaded = store.load().unwrap().unwrap();
        assert!(
            loaded
                .projects
                .iter()
                .all(|project| project.split_ratio == 5_000)
        );
        assert_eq!(loaded, expected);
    }

    #[test]
    fn shared_workspace_splits_roundtrip_independent_selections_focus_and_ratio() {
        let directory = tempfile::tempdir().unwrap();
        for layout in [SavedLayout::Vertical, SavedLayout::Horizontal] {
            for focus_secondary in [false, true] {
                for (primary, secondary) in [(1, 2), (2, 0)] {
                    let store = Store::open_in(directory.path()).unwrap();
                    let mut expected = snapshot();
                    let project = &mut expected.projects[0];
                    project.secondary = Some(project.active);
                    project.secondary_session = Some(secondary);
                    project.workspaces[project.active].selected = primary;
                    project.focus_secondary = focus_secondary;
                    project.layout = layout;
                    project.split_ratio = 3_750;
                    expected.active_project = 0;
                    store.save(&expected).unwrap();
                    let serialized: serde_json::Value = serde_json::from_slice(
                        &fs::read(directory.path().join(SNAPSHOT_FILE)).unwrap(),
                    )
                    .unwrap();
                    assert_eq!(
                        serialized["projects"][0]["secondary_session"],
                        serde_json::json!(secondary)
                    );
                    drop(store);
                    let reopened = Store::open_in(directory.path()).unwrap();
                    assert_eq!(reopened.load().unwrap(), Some(expected));
                }
            }
        }
    }

    #[test]
    fn invalid_shared_workspace_sessions_are_rejected_without_overwriting() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open_in(directory.path()).unwrap();
        let path = directory.path().join(SNAPSHOT_FILE);
        let mut valid = snapshot();
        valid.projects[0].secondary = Some(0);
        valid.projects[0].secondary_session = Some(2);
        let mutations: &[fn(&mut SavedProject)] = &[
            |p| p.secondary_session = None,
            |p| p.secondary_session = Some(p.workspaces[p.active].selected),
            |p| p.secondary_session = Some(p.workspaces[p.active].sessions.len()),
            |p| {
                p.workspaces[p.active].sessions.clear();
                p.workspaces[p.active].selected = 0;
                p.secondary_session = Some(0);
            },
            |p| p.secondary = Some(1),
            |p| p.secondary = None,
            |p| p.layout = SavedLayout::Full,
            |p| {
                p.secondary = None;
                p.focus_secondary = false;
                p.layout = SavedLayout::Full;
            },
        ];
        for mutate in mutations {
            store.save(&valid).unwrap();
            let before = fs::read(&path).unwrap();
            let mut invalid = valid.clone();
            mutate(&mut invalid.projects[0]);
            assert!(store.save(&invalid).is_err(), "accepted {invalid:?}");
            assert_eq!(fs::read(&path).unwrap(), before);
            assert_eq!(store.load().unwrap(), Some(valid.clone()));
            let invalid_bytes = serde_json::to_vec(&invalid).unwrap();
            fs::write(&path, &invalid_bytes).unwrap();
            assert!(store.load().is_err(), "loaded {invalid:?}");
            assert_eq!(fs::read(&path).unwrap(), invalid_bytes);
        }
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 2);
    }

    #[test]
    fn split_ratios_roundtrip_including_runtime_clamped_endpoints() {
        let directory = tempfile::tempdir().unwrap();
        for ratio in [3_750, 0, SPLIT_RATIO_SCALE] {
            let store = Store::open_in(directory.path()).unwrap();
            let mut expected = snapshot();
            expected.projects[0].split_ratio = ratio;
            store.save(&expected).unwrap();
            drop(store);
            let reopened = Store::open_in(directory.path()).unwrap();
            assert_eq!(reopened.load().unwrap(), Some(expected));
        }
    }

    #[test]
    fn replacement_is_atomic_and_files_are_private() {
        let directory = tempfile::tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o755)).unwrap();
        let store = Store::open_in(directory.path()).unwrap();
        let path = directory.path().join(SNAPSHOT_FILE);
        store.save(&snapshot()).unwrap();
        let before = fs::read(&path).unwrap();
        let mut original = File::open(&path).unwrap();
        let original_inode = original.metadata().unwrap().ino();
        let mut replacement = snapshot();
        replacement.projects[0].layout = SavedLayout::Horizontal;
        replacement.projects[0].workspaces[0].selected = 2;
        store.save(&replacement).unwrap();
        let mut original_bytes = Vec::new();
        original.read_to_end(&mut original_bytes).unwrap();
        assert_eq!(original_bytes, before);
        assert_ne!(fs::metadata(&path).unwrap().ino(), original_inode);
        assert_eq!(store.load().unwrap(), Some(replacement));
        for (path, mode) in [
            (directory.path().to_path_buf(), 0o700),
            (path, 0o600),
            (directory.path().join("workspace.lock"), 0o600),
        ] {
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                mode
            );
        }
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 2);
    }

    #[test]
    fn backup_keeps_a_private_copy_beside_the_snapshot() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open_in(directory.path()).unwrap();
        let expected = snapshot();
        let path = store.back_up(&expected).unwrap();
        assert_eq!(path, directory.path().join("workspace.json.bak"));
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let decoded: Snapshot = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(decoded, expected);
        // The live snapshot is untouched: there is still none to load.
        assert_eq!(store.load().unwrap(), None);
        let mut invalid = snapshot();
        invalid.version += 1;
        assert!(store.back_up(&invalid).is_err());
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 2);
    }

    #[test]
    fn lock_excludes_second_owner_and_releases_on_drop_without_changing_state() {
        let directory = tempfile::tempdir().unwrap();
        let owner = Store::open_in(directory.path()).unwrap();
        owner.save(&snapshot()).unwrap();
        let path = directory.path().join(SNAPSHOT_FILE);
        let before = fs::read(&path).unwrap();
        let error = Store::open_in(directory.path()).unwrap_err();
        assert!(error.downcast_ref::<Busy>().is_some(), "{error:#}");
        assert_eq!(fs::read(&path).unwrap(), before);
        let inherited = owner.lock.try_clone().unwrap();
        drop(owner);
        let next = Store::open_in(directory.path()).unwrap();
        assert_eq!(next.load().unwrap(), Some(snapshot()));
        drop(inherited);
        assert!(
            Store::open_in(directory.path())
                .unwrap_err()
                .downcast_ref::<Busy>()
                .is_some()
        );
        assert_eq!(fs::read(path).unwrap(), before);
    }

    #[test]
    fn malformed_unsupported_and_unknown_schema_bytes_survive_open_and_load() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(SNAPSHOT_FILE);
        let mut unsupported = serde_json::to_value(snapshot()).unwrap();
        unsupported["version"] = serde_json::json!(VERSION + 1);
        let mut unknown = serde_json::to_value(snapshot()).unwrap();
        unknown["projects"][0]["workspaces"][0]["chat_text"] = serde_json::json!("not metadata");
        let mut wrong_type = serde_json::to_value(snapshot()).unwrap();
        wrong_type["native_keys"] = serde_json::json!("yes");
        for bytes in [
            b"{broken".to_vec(),
            serde_json::to_vec(&unsupported).unwrap(),
            serde_json::to_vec(&unknown).unwrap(),
            serde_json::to_vec(&wrong_type).unwrap(),
        ] {
            fs::write(&path, &bytes).unwrap();
            let store = Store::open_in(directory.path()).unwrap();
            assert_eq!(fs::read(&path).unwrap(), bytes);
            assert!(store.load().is_err());
            assert_eq!(fs::read(&path).unwrap(), bytes);
        }
    }

    #[test]
    fn invalid_indices_layout_ratios_paths_and_sessions_are_rejected_without_overwriting() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open_in(directory.path()).unwrap();
        let path = directory.path().join(SNAPSHOT_FILE);
        let mutations: &[fn(&mut Snapshot)] = &[
            |s| s.version += 1,
            |s| s.active_project = s.projects.len(),
            |s| {
                s.projects.clear();
                s.active_project = 1;
            },
            |s| s.projects[0].workspaces.clear(),
            |s| s.projects[0].active = 2,
            |s| s.projects[0].secondary = Some(2),
            |s| s.projects[0].secondary = Some(0),
            |s| s.projects[0].secondary = None,
            |s| s.projects[0].layout = SavedLayout::Full,
            |s| s.projects[1].focus_secondary = true,
            |s| s.projects[0].split_ratio = SPLIT_RATIO_SCALE + 1,
            |s| s.projects[0].workspaces[0].selected = 3,
            |s| s.projects[1].workspaces[0].selected = 1,
            |s| s.projects[0].workspaces[0].path = "relative".into(),
            |s| s.projects[0].workspaces[0].source = Some("relative".into()),
            |s| s.projects[0].workspaces[0].path = "/nul\0path".into(),
            |s| s.projects[0].workspaces[0].sessions[0].kind = "other".into(),
            |s| s.projects[0].workspaces[0].sessions[2].resume = Some("not-a-shell-chat".into()),
            |s| s.projects[0].workspaces[0].sessions[0].resume = Some(" ".into()),
            |s| s.projects[0].workspaces[0].sessions[0].resume = Some("nul\0identity".into()),
        ];
        for mutate in mutations {
            store.save(&snapshot()).unwrap();
            let before = fs::read(&path).unwrap();
            let mut invalid = snapshot();
            mutate(&mut invalid);
            assert!(store.save(&invalid).is_err(), "accepted {invalid:?}");
            assert_eq!(fs::read(&path).unwrap(), before);
            let invalid_bytes = serde_json::to_vec(&invalid).unwrap();
            fs::write(&path, &invalid_bytes).unwrap();
            assert!(store.load().is_err(), "loaded {invalid:?}");
            assert_eq!(fs::read(&path).unwrap(), invalid_bytes);
        }
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 2);
    }

    #[test]
    fn serialization_and_size_errors_leave_previous_snapshot_and_remove_temporaries() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open_in(directory.path()).unwrap();
        let path = directory.path().join(SNAPSHOT_FILE);
        store.save(&snapshot()).unwrap();
        let before = fs::read(&path).unwrap();
        let mut invalid = snapshot();
        invalid.projects[0].workspaces[0].path =
            PathBuf::from(OsString::from_vec(b"/invalid-utf8-\xff".to_vec()));
        assert!(store.save(&invalid).is_err());
        assert_eq!(fs::read(&path).unwrap(), before);
        let mut oversized = snapshot();
        oversized.projects[0].workspaces[0].sessions[0].resume =
            Some("a".repeat(MAX_SNAPSHOT_BYTES as usize));
        assert!(store.save(&oversized).is_err());
        assert_eq!(fs::read(&path).unwrap(), before);
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 2);

        let file = OpenOptions::new().write(true).open(&path).unwrap();
        file.set_len(MAX_SNAPSHOT_BYTES + 1).unwrap();
        let oversized_bytes = fs::read(&path).unwrap();
        assert!(store.load().is_err());
        assert_eq!(fs::read(&path).unwrap(), oversized_bytes);
    }
}
