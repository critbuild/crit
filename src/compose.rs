use crate::worktree::Worktree;
use anyhow::{Context, Result, bail, ensure};
use serde::ser::{SerializeMap, SerializeSeq};
use serde::{Deserialize, Serialize, Serializer};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Environment {
    pub project: String,
    pub services: Vec<Service>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Service {
    pub name: String,
    pub state: String,
    pub health: String,
    #[serde(default)]
    pub exit_code: i64,
    pub ports: Vec<Port>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Port {
    pub host: u16,
    pub container: u16,
    pub protocol: String,
}

const COMPOSE_FILES: [&str; 4] = [
    "compose.yaml",
    "compose.yml",
    "docker-compose.yaml",
    "docker-compose.yml",
];

#[derive(Debug)]
struct Link {
    source: String,
    port: u16,
}

type Links = BTreeMap<String, BTreeMap<String, Link>>;

#[derive(Default)]
struct Settings {
    files: Option<Vec<PathBuf>>,
    links: Links,
    budget: Budget,
}

#[derive(Clone, Copy)]
struct Budget {
    cpus: f64,
    memory_mib: u64,
}

impl Default for Budget {
    fn default() -> Self {
        let cores = std::thread::available_parallelism().map_or(1, usize::from);
        let pages = unsafe { libc::sysconf(libc::_SC_PHYS_PAGES) };
        let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        let memory = if pages > 0 && page_size > 0 {
            (pages as u64).saturating_mul(page_size as u64) / (1024 * 1024)
        } else {
            4096
        };
        Self {
            cpus: (cores as f64 / 2.0).clamp(1.0, 4.0),
            memory_mib: (memory / 2).clamp(1024, 8192),
        }
    }
}

struct Manifest {
    project: String,
    path: PathBuf,
}

fn settings(worktree: &Worktree) -> Result<Settings> {
    let path = worktree.path.join("crit.toml");
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Settings::default());
        }
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
    let document: toml::Value =
        toml::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
    let mut budget = Budget::default();
    if let Some(checks) = document.get("checks") {
        let checks = checks
            .as_table()
            .context("crit.toml [checks] must be a table")?;
        if let Some(cpus) = checks.get("cpus") {
            budget.cpus = cpus
                .as_float()
                .or_else(|| cpus.as_integer().map(|n| n as f64))
                .context("checks.cpus must be a number")?;
            ensure!(
                budget.cpus.is_finite() && (0.1..=64.0).contains(&budget.cpus),
                "checks.cpus must be between 0.1 and 64"
            );
        }
        if let Some(memory) = checks.get("memory_mib") {
            let memory = memory
                .as_integer()
                .context("checks.memory_mib must be an integer")?;
            ensure!(
                (128..=262144).contains(&memory),
                "checks.memory_mib must be between 128 and 262144"
            );
            budget.memory_mib = memory as u64;
        }
    }
    let Some(compose) = document.get("compose") else {
        return Ok(Settings {
            files: None,
            links: Links::new(),
            budget,
        });
    };
    let compose = compose
        .as_table()
        .context("crit.toml [compose] must be a table")?;
    let files = compose
        .get("files")
        .map(|value| -> Result<Vec<PathBuf>> {
            let files = value
                .as_array()
                .context("crit.toml compose.files must be an array of file paths")?;
            ensure!(
                !files.is_empty(),
                "crit.toml compose.files must not be empty"
            );
            files
                .iter()
                .map(|file| {
                    let file = file
                        .as_str()
                        .context("crit.toml compose.files entries must be strings")?;
                    ensure!(
                        !file.is_empty(),
                        "crit.toml compose.files entries must not be empty"
                    );
                    Ok(worktree.path.join(file))
                })
                .collect()
        })
        .transpose()?;
    let mut links = Links::new();
    if let Some(value) = compose.get("links") {
        for (target, variables) in value
            .as_table()
            .context("crit.toml compose.links must be a table")?
        {
            let mut variables_out = BTreeMap::new();
            for (variable, reference) in variables
                .as_table()
                .with_context(|| format!("compose.links.{target} must be a table"))?
            {
                ensure!(
                    !variable.is_empty() && !variable.contains(['=', '\0']),
                    "invalid environment variable in compose.links.{target}: {variable:?}"
                );
                let reference = reference.as_str().with_context(|| {
                    format!("compose.links.{target}.{variable} must be a service:port string")
                })?;
                let (source, port) = reference.rsplit_once(':').with_context(|| {
                    format!("invalid browser link {reference:?}; expected service:port")
                })?;
                ensure!(
                    !source.is_empty(),
                    "browser link {reference:?} has no source service"
                );
                let port = port
                    .parse::<u16>()
                    .with_context(|| format!("invalid browser link port in {reference:?}"))?;
                ensure!(
                    port != 0,
                    "browser link {reference:?} has an invalid zero port"
                );
                variables_out.insert(
                    variable.clone(),
                    Link {
                        source: source.to_owned(),
                        port,
                    },
                );
            }
            links.insert(target.clone(), variables_out);
        }
    }
    Ok(Settings {
        files,
        links,
        budget,
    })
}

pub fn has_config(worktree: &Worktree) -> bool {
    if COMPOSE_FILES
        .iter()
        .any(|name| worktree.path.join(name).is_file())
    {
        return true;
    }
    match settings(worktree) {
        Ok(settings) => settings.files.is_some() || !settings.links.is_empty(),
        // Let the action expose malformed/unreadable configuration instead of
        // hiding it behind a misleading "no Compose configuration" state.
        Err(_) => true,
    }
}

fn compose_files(worktree: &Worktree, settings: &Settings) -> Result<Vec<PathBuf>> {
    let files = match &settings.files {
        Some(files) => files.clone(),
        None => {
            let primary = COMPOSE_FILES.iter()
                .map(|name| worktree.path.join(name))
                .find(|path| path.is_file())
                .context("no compose.yaml, compose.yml, docker-compose.yaml, docker-compose.yml, or crit.toml compose.files configuration")?;
            let mut files = vec![primary];
            if let Some(overrides) = [
                "compose.override.yaml",
                "compose.override.yml",
                "docker-compose.override.yaml",
                "docker-compose.override.yml",
            ]
            .iter()
            .map(|name| worktree.path.join(name))
            .find(|path| path.is_file())
            {
                files.push(overrides);
            }
            files
        }
    };
    files
        .into_iter()
        .map(|path| {
            ensure!(
                path.is_file(),
                "Compose configuration is not a file: {}",
                path.display()
            );
            // Relative contexts and binds use this checkout's supplied directory.
            // Preparation and runtime resolution reject symlinked input files.
            Ok(path)
        })
        .collect()
}

fn state_root() -> Result<PathBuf> {
    let state = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))
        .context("XDG_STATE_HOME or HOME is required to store checking manifests")?;
    let root = state.join("crit/check-compose");
    fs::create_dir_all(&root)?;
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
    Ok(root)
}

fn identity(repository: &Path) -> String {
    format!(
        "crit-project-{:x}",
        Sha256::digest(repository.as_os_str().as_encoded_bytes())
    )
}

fn manifest(worktree: &Worktree) -> Result<Manifest> {
    let root = state_root()?;
    if worktree.path.is_dir()
        && let Ok(repository) = crate::worktree::discover(&worktree.path)
    {
        let project = identity(&repository.common_dir);
        return Ok(Manifest {
            path: root.join(&project).join("compose.json"),
            project,
        });
    }
    // The checkout can disappear after startup. The private, durable manifest is
    // the authority for locating its runtime, never a legacy worktree id.
    let path = crate::worktree::canonical_path(&worktree.path)?;
    for entry in fs::read_dir(&root)? {
        let file = entry?.path().join("compose.json");
        if !file.is_file() {
            continue;
        }
        let config = load_manifest(&file)?;
        if config.pointer("/x-crit/worktree").and_then(Value::as_str) == path.to_str() {
            let repository = config
                .pointer("/x-crit/repository")
                .and_then(Value::as_str)
                .context("persisted checking manifest has no repository")?;
            let project = identity(Path::new(repository));
            ensure!(
                file == root.join(&project).join("compose.json"),
                "invalid manifest identity"
            );
            return Ok(Manifest {
                path: file,
                project,
            });
        }
    }
    bail!(
        "no persisted checking runtime for {}",
        worktree.path.display()
    )
}

pub fn project_id(worktree: &Worktree) -> Result<String> {
    Ok(manifest(worktree)?.project)
}

pub fn project_lock_path(worktree: &Worktree) -> Result<PathBuf> {
    let manifest = manifest(worktree)?;
    let directory = manifest
        .path
        .parent()
        .context("manifest directory missing")?;
    fs::create_dir_all(directory)?;
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(directory, fs::Permissions::from_mode(0o700))?;
    Ok(directory.join("project.lock"))
}

fn docker(worktree: &Worktree, project: &str) -> Command {
    let mut command = Command::new("docker");
    command
        .current_dir(&worktree.path)
        .env("COMPOSE_PROJECT_NAME", project);
    command
}

fn checked(command: &mut Command, operation: &str) -> Result<Output> {
    output_within(command, operation, Duration::from_secs(30))
}

fn succeeded(output: Output, operation: &str) -> Result<Output> {
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        bail!(
            "{operation}: {}{}{}",
            stderr.trim_end(),
            if stderr.is_empty() { "" } else { "\n" },
            stdout.trim_end()
        );
    }
    Ok(output)
}

/// `checked` for commands that must answer promptly: a wedged Docker daemon never
/// does. Past `deadline` the command and its whole process group (the Compose
/// plugin is a child of `docker`) are killed.
fn output_within(command: &mut Command, operation: &str, deadline: Duration) -> Result<Output> {
    use std::io::Read;
    use std::os::unix::process::CommandExt;
    use std::process::Stdio;
    ensure!(
        !deadline.is_zero(),
        "{operation}: Docker command deadline expired"
    );

    // Nonblocking drains keep the deadline valid even when a plugin forks a
    // descendant that inherits stdout. Never wait on unbounded reader threads.
    fn drain(pipe: &mut impl Read, bytes: &mut Vec<u8>) -> Result<bool> {
        let mut buffer = [0; 8192];
        loop {
            match pipe.read(&mut buffer) {
                Ok(0) => return Ok(true),
                Ok(count) => {
                    ensure!(
                        bytes.len() + count <= 16 * 1024 * 1024,
                        "Docker output exceeded the 16 MiB command limit"
                    );
                    bytes.extend_from_slice(&buffer[..count]);
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return Ok(false),
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error.into()),
            }
        }
    }

    #[cfg(not(test))]
    {
        // This supervisor must outlive a killed operation/worker/guardian so
        // detached Compose-plugin descendants cannot survive their owner.
        let parent = std::process::id();
        let mut supervised = Command::new(format!("/proc/{parent}/exe"));
        supervised.args(["__pane", "--parent", &parent.to_string(), "--"]);
        supervised
            .arg(command.get_program())
            .args(command.get_args());
        if let Some(directory) = command.get_current_dir() {
            supervised.current_dir(directory);
        }
        for (key, value) in command.get_envs() {
            if let Some(value) = value {
                supervised.env(key, value);
            } else {
                supervised.env_remove(key);
            }
        }
        *command = supervised;
    }
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .with_context(|| format!("{operation}: could not run Docker Compose"))?;
    let mut stdout = child.stdout.take().context("Docker stdout missing")?;
    let mut stderr = child.stderr.take().context("Docker stderr missing")?;
    let result = (|| -> Result<(Vec<u8>, Vec<u8>)> {
        for fd in [stdout.as_raw_fd(), stderr.as_raw_fd()] {
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
            ensure!(
                flags >= 0
                    && unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } >= 0,
                "could not make Docker output nonblocking"
            );
        }
        let mut out = Vec::new();
        let mut err = Vec::new();
        let started = Instant::now();
        loop {
            let out_done = drain(&mut stdout, &mut out)?;
            let err_done = drain(&mut stderr, &mut err)?;
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            let waited = unsafe {
                libc::waitid(
                    libc::P_PID,
                    child.id(),
                    &mut info,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            };
            if waited < 0 {
                return Err(std::io::Error::last_os_error()).context("wait for Docker command");
            }
            if unsafe { info.si_pid() } != 0 {
                // Keep the leader unreaped until descendants are signalled:
                // its process-group identity cannot be reused in this gap.
                unsafe { libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL) };
                if out_done && err_done {
                    return Ok((out, err));
                }
            }
            ensure!(
                started.elapsed() < deadline,
                "{operation}: Docker did not answer within {}s",
                deadline.as_secs_f64()
            );
            std::thread::sleep(Duration::from_millis(25));
        }
    })();
    #[cfg(not(test))]
    if result.is_err() {
        // Let __pane terminate and reap its separately grouped descendants
        // before escalating. Killing the supervisor first would orphan them.
        unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };
        let closing = Instant::now();
        loop {
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            let waited = unsafe {
                libc::waitid(
                    libc::P_PID,
                    child.id(),
                    &mut info,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            };
            if waited < 0
                || unsafe { info.si_pid() } != 0
                || closing.elapsed() >= Duration::from_secs(4)
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }
    unsafe { libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL) };
    let status = child
        .wait()
        .with_context(|| format!("{operation}: reap Docker command"))?;
    let (stdout, stderr) = result?;
    succeeded(
        Output {
            status,
            stdout,
            stderr,
        },
        operation,
    )
}

/// Prepare ignored Compose inputs using an explicit checkout of the same repository.
/// Preparation never starts a runtime; failure does not invalidate a created worktree.
/// Retrying is safe; existing regular target files are never overwritten.
pub fn prepare_local_files(source: &Worktree, target: &Worktree) -> Result<()> {
    let source_repository = crate::worktree::discover(&source.path)?;
    let target_repository = crate::worktree::discover(&target.path)?;
    ensure!(
        source_repository.common_dir == target_repository.common_dir,
        "local Compose configuration source belongs to a different repository"
    );
    ensure!(
        source.path != target.path,
        "configuration source must be another checkout"
    );
    copy_local_file(&source.path, &target.path, Path::new("crit.toml"))?;
    if !has_config(source) && !has_config(target) {
        return Ok(());
    }
    copy_local_file(&source.path, &target.path, Path::new(".env"))?;
    // Bootstrap only the manifests Crit actually selects, including a local override.
    // A tracked file absent in the target branch must remain absent.
    if has_config(source) {
        for file in compose_files(source, &settings(source)?)? {
            let relative = local_relative(&source.path, &file)?;
            copy_local_file(&source.path, &target.path, &relative)?;
        }
    }
    if !has_config(target) {
        return Ok(());
    }
    let settings = settings(target)?;
    if let Some(files) = &settings.files {
        for file in files {
            let relative = local_relative(&target.path, file)?;
            copy_local_file(&source.path, &target.path, &relative)?;
        }
    }
    let files = compose_files(target, &settings)?;
    let directory = files[0]
        .parent()
        .context("Compose file has no parent directory")?;
    for file in &files {
        let relative = local_relative(&target.path, file)?;
        // Also reject symlinked manifests: discovery must not read another checkout.
        secure_file(&target.path, &relative, false)?;
    }
    let project_env = local_relative(&target.path, &directory.join(".env"))?;
    copy_local_file(&source.path, &target.path, &project_env)?;
    let mut command = config_command(target, &files, "crit-local-inputs")?;
    command.arg("--no-env-resolution");
    let output = checked(&mut command, "discover local Compose env files")?;
    let config: Value = serde_json::from_slice(&output.stdout)
        .context("Docker Compose returned invalid config JSON")?;
    for relative in env_files(&config, &target.path, directory)? {
        copy_local_file(&source.path, &target.path, &relative)?;
    }
    Ok(())
}

fn local_relative(root: &Path, path: &Path) -> Result<PathBuf> {
    let path = if path.is_absolute() {
        path.strip_prefix(root)
            .context("local Compose input is outside the checkout")?
    } else {
        path
    };
    let mut relative = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(name) => relative.push(name),
            Component::CurDir => {}
            Component::ParentDir => {
                ensure!(relative.pop(), "local Compose input escapes the checkout");
            }
            _ => bail!("local Compose input escapes the checkout"),
        }
    }
    ensure!(
        !relative.as_os_str().is_empty(),
        "local Compose input is not a file"
    );
    Ok(relative)
}

fn env_files(config: &Value, root: &Path, directory: &Path) -> Result<BTreeSet<PathBuf>> {
    let mut files = BTreeSet::new();
    for service in services(config)?.values() {
        if let Some(values) = service.get("env_file") {
            for value in values
                .as_array()
                .context("Compose env_file must be an array")?
            {
                let path = value
                    .as_str()
                    .or_else(|| value.get("path").and_then(Value::as_str))
                    .context("Compose env_file has no path")?;
                files.insert(local_relative(root, &directory.join(path))?);
            }
        }
    }
    Ok(files)
}

// Walk directory descriptors, not canonicalized strings, so symlink substitution
// cannot redirect reads or writes between validation and copying.
fn secure_parent(
    root: &Path,
    relative: &Path,
    create: bool,
) -> Result<Option<(fs::File, std::ffi::CString)>> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let relative = local_relative(root, relative)?;
    let mut directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(root)?;
    let parts: Vec<_> = relative.components().collect();
    for (index, component) in parts.iter().enumerate() {
        let name = CString::new(component.as_os_str().as_bytes())?;
        if index + 1 == parts.len() {
            return Ok(Some((directory, name)));
        }
        let flags = libc::O_RDONLY | libc::O_DIRECTORY;
        let mut fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                name.as_ptr(),
                flags | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            )
        };
        if fd < 0
            && create
            && std::io::Error::last_os_error().kind() == std::io::ErrorKind::NotFound
        {
            let result = unsafe { libc::mkdirat(directory.as_raw_fd(), name.as_ptr(), 0o700) };
            if result < 0
                && std::io::Error::last_os_error().kind() != std::io::ErrorKind::AlreadyExists
            {
                return Err(std::io::Error::last_os_error())
                    .context("create local configuration directory");
            }
            fd = unsafe {
                libc::openat(
                    directory.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
        }
        if fd < 0 {
            let error = std::io::Error::last_os_error();
            if !create && error.kind() == std::io::ErrorKind::NotFound {
                return Ok(None);
            }
            return Err(error)
                .with_context(|| format!("open local configuration {}", relative.display()));
        }
        let file = unsafe { fs::File::from_raw_fd(fd) };
        ensure!(
            file.metadata()?.is_dir(),
            "local Compose input parent is not a directory"
        );
        directory = file;
    }
    unreachable!("validated nonempty relative file path")
}

fn secure_file(root: &Path, relative: &Path, create: bool) -> Result<Option<fs::File>> {
    let Some((directory, name)) = secure_parent(root, relative, create)? else {
        return Ok(None);
    };
    let flags = if create {
        libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL
    } else {
        libc::O_RDONLY | libc::O_NONBLOCK
    };
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )
    };
    if fd < 0 {
        let error = std::io::Error::last_os_error();
        if !create && error.kind() == std::io::ErrorKind::NotFound {
            return Ok(None);
        }
        return Err(error)
            .with_context(|| format!("open local configuration {}", relative.display()));
    }
    let file = unsafe { fs::File::from_raw_fd(fd) };
    ensure!(
        file.metadata()?.is_file(),
        "local Compose input is not a regular file: {}",
        relative.display()
    );
    Ok(Some(file))
}

fn copy_local_file(source: &Path, target: &Path, relative: &Path) -> Result<()> {
    let relative = local_relative(target, relative)?;
    if let Some(existing) = secure_file(target, &relative, false)? {
        // An existing hard link to the source is not an independent local copy.
        if let Some(original) = secure_file(source, &relative, false)? {
            use std::os::unix::fs::MetadataExt;
            let a = existing.metadata()?;
            let b = original.metadata()?;
            ensure!(
                a.dev() != b.dev() || a.ino() != b.ino(),
                "target Compose input shares the source file"
            );
        }
        return Ok(());
    }
    if !crate::worktree::ignored_local_file(source, &relative)?
        || !crate::worktree::ignored_local_file(target, &relative)?
    {
        return Ok(());
    }
    let Some(mut input) = secure_file(source, &relative, false)? else {
        return Ok(());
    };
    let (directory, name) =
        secure_parent(target, &relative, true)?.context("create local Compose directory")?;
    static COPY_SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let temporary = std::ffi::CString::new(format!(
        ".crit-copy-{}-{}",
        std::process::id(),
        COPY_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ))?;
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            temporary.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )
    };
    ensure!(
        fd >= 0,
        "create temporary local Compose input: {}",
        std::io::Error::last_os_error()
    );
    let mut output = unsafe { fs::File::from_raw_fd(fd) };
    let result = (|| -> Result<()> {
        std::io::copy(&mut input, &mut output)?;
        output.sync_all()?;
        let result = unsafe {
            libc::renameat2(
                directory.as_raw_fd(),
                temporary.as_ptr(),
                directory.as_raw_fd(),
                name.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        if result < 0 {
            return Err(std::io::Error::last_os_error())
                .context("publish local Compose input without replacing existing file");
        }
        Ok(())
    })();
    // The directory descriptor keeps cleanup inside the validated checkout.
    unsafe {
        libc::unlinkat(directory.as_raw_fd(), temporary.as_ptr(), 0);
    }
    result.with_context(|| format!("copy local Compose input {}", relative.display()))?;
    Ok(())
}

fn config_command(worktree: &Worktree, files: &[PathBuf], project: &str) -> Result<Command> {
    let directory = files[0]
        .parent()
        .context("Compose file has no parent directory")?;
    let mut command = docker(worktree, project);
    command.args(["compose", "--project-name", project]);
    command.arg("--project-directory").arg(directory);
    for file in files {
        command.arg("--file").arg(file);
    }
    command.args(["config", "--format", "json"]);
    Ok(command)
}

fn resolved(worktree: &Worktree, settings: &Settings, manifest: &Manifest) -> Result<Value> {
    let files = compose_files(worktree, settings)?;
    for file in &files {
        let relative = local_relative(&worktree.path, file)?;
        ensure!(
            secure_file(&worktree.path, &relative, false)?.is_some(),
            "Compose input disappeared: {}",
            file.display()
        );
    }
    let directory = files[0]
        .parent()
        .context("Compose file has no parent directory")?;
    let mut command = config_command(worktree, &files, &manifest.project)?;
    let output = checked(&mut command, "resolve Compose configuration")?;
    let mut config: Value = serde_json::from_slice(&output.stdout)
        .context("Docker Compose returned invalid config JSON")?;
    isolate(&mut config, &manifest.project, directory)?;
    govern(&mut config, worktree, settings.budget)?;
    validate_links(&config, &settings.links)?;
    Ok(config)
}

fn memory_bytes(value: &Value) -> Result<u64> {
    if let Some(bytes) = value.as_u64() {
        return Ok(bytes);
    }
    let text = value
        .as_str()
        .context("memory limit must be bytes or a unit string")?
        .to_ascii_lowercase();
    let split = text
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(text.len());
    let bytes: f64 = text[..split].parse().context("invalid memory limit")?;
    let multiplier = match text[split..].trim() {
        "" | "b" => 1,
        "k" | "kb" | "kib" => 1024,
        "m" | "mb" | "mib" => 1024 * 1024,
        "g" | "gb" | "gib" => 1024 * 1024 * 1024,
        _ => bail!("unsupported memory limit {text:?}"),
    };
    let bytes = bytes * multiplier as f64;
    ensure!(
        bytes.is_finite() && bytes >= 0.0 && bytes < u64::MAX as f64,
        "memory limit overflow"
    );
    Ok(bytes.floor() as u64)
}

fn cpu_limit(value: &Value) -> Result<f64> {
    let cpus = value
        .as_f64()
        .or_else(|| value.as_str().and_then(|s| s.parse().ok()))
        .context("CPU limit must be a number")?;
    ensure!(cpus.is_finite() && cpus >= 0.0, "invalid CPU limit");
    Ok(cpus)
}

fn owner_context(value: &Value, checkout: &Path) -> Result<()> {
    let context = value.as_str().context("build context must be a string")?;
    if context.contains("://")
        || context.starts_with("git@")
        || context.starts_with("service:")
        || context.starts_with("docker-image:")
    {
        return Ok(());
    }
    owner_path(Path::new(context), checkout)
}

fn owner_path(path: &Path, checkout: &Path) -> Result<()> {
    let path = crate::worktree::canonical_path(path)?;
    ensure!(
        path.starts_with(checkout),
        "build input {} is outside the checking owner's checkout {}; use an input from this owner",
        path.display(),
        checkout.display()
    );
    Ok(())
}

/// Runtime budget is set by [checks] cpus (0.1..=64) and memory_mib
/// (128..=262144). Defaults: half the available CPUs clamped to 1..=4,
/// and half physical RAM clamped to 1024..=8192 MiB (RAM discovery
/// fallback: 4096 MiB, giving a 2048 MiB budget). One equal share per service, never above stricter
/// explicit service limits. This does not budget the Docker build daemon.
fn govern(config: &mut Value, worktree: &Worktree, budget: Budget) -> Result<()> {
    let discovered = crate::worktree::discover(&worktree.path)?;
    let registered = crate::worktree::list(&discovered)?;
    let repository = discovered.common_dir;
    let checkout = fs::canonicalize(&worktree.path)?;
    let labels = serde_json::json!({
        "io.crit.managed": "2",
        "io.crit.repository": repository.to_str().context("repository path is not UTF-8")?,
        "io.crit.worktree": checkout.to_str().context("checkout path is not UTF-8")?
    });
    config["x-crit"] = serde_json::json!({
        "repository": repository, "worktree": checkout,
        "cpus": budget.cpus, "memory_mib": budget.memory_mib
    });
    if config.get("networks").is_none() {
        config["networks"] = serde_json::json!({"default": {}});
    }
    for kind in ["networks", "volumes", "configs", "secrets"] {
        if let Some(resources) = config.get_mut(kind) {
            for (name, resource) in object_mut(resources, kind)? {
                if resource.is_null() {
                    *resource = serde_json::json!({});
                }
                let resource = object_mut(resource, kind)?;
                ensure!(
                    resource
                        .get("external")
                        .is_none_or(|v| v.is_null() || v == &Value::Bool(false)),
                    "external {kind}.{name} cannot be owned"
                );
                resource.remove("external");
                if kind == "networks" || kind == "volumes" {
                    let existing = resource
                        .entry("labels")
                        .or_insert_with(|| serde_json::json!({}));
                    object_mut(existing, "resource labels")?
                        .extend(labels.as_object().unwrap().clone());
                }
                // Data survives handoffs, isolated by checkout, never removed.
                if kind == "volumes" {
                    resource.insert(
                        "name".into(),
                        Value::String(format!(
                            "{}_wt-{:x}_{name}",
                            identity(&repository),
                            Sha256::digest(checkout.as_os_str().as_encoded_bytes())
                        )),
                    );
                }
            }
        }
    }
    let services = object_mut(
        config.get_mut("services").context("missing services")?,
        "services",
    )?;
    ensure!(!services.is_empty(), "checking runtime has no services");
    let cpu_share =
        (budget.cpus * 1_000_000_000.0 / services.len() as f64).floor() / 1_000_000_000.0;
    let memory_share = budget.memory_mib * 1024 * 1024 / services.len() as u64;
    ensure!(
        memory_share >= 6 * 1024 * 1024,
        "checking memory budget is too small for all services"
    );
    for (name, service) in services {
        let service = object_mut(service, name)?;
        service.remove("profiles");
        service.insert("restart".into(), Value::String("no".into()));
        service.insert("scale".into(), Value::from(1));
        if let Some(build) = service.get("build") {
            if build.is_string() {
                owner_context(build, &checkout)?;
            } else {
                if let Some(context) = build.get("context") {
                    owner_context(context, &checkout)?;
                }
                if let Some(dockerfile) = build.get("dockerfile").and_then(Value::as_str) {
                    if Path::new(dockerfile).is_absolute() {
                        owner_path(Path::new(dockerfile), &checkout)?;
                    } else if let Some(context) = build.get("context").and_then(Value::as_str)
                        && Path::new(context).is_absolute()
                    {
                        owner_path(&Path::new(context).join(dockerfile), &checkout)?;
                    }
                }
                if let Some(contexts) = build.get("additional_contexts").and_then(Value::as_object)
                {
                    for context in contexts.values() {
                        owner_context(context, &checkout)?;
                    }
                }
            }
        }
        if let Some(volumes) = service.get("volumes").and_then(Value::as_array) {
            for volume in volumes {
                if volume.get("type").and_then(Value::as_str) == Some("bind")
                    && let Some(source) = volume.get("source").and_then(Value::as_str)
                {
                    let source = crate::worktree::canonical_path(Path::new(source))?;
                    ensure!(
                        source.starts_with(&checkout)
                            || !registered.iter().any(|tree| source.starts_with(&tree.path)),
                        "service {name} binds another checkout at {}; use this owner's checkout {}",
                        source.display(),
                        checkout.display()
                    );
                }
            }
        }
        ensure!(
            service.get("privileged") != Some(&Value::Bool(true)),
            "privileged service {name} cannot be owned safely"
        );
        ensure!(
            service
                .get("external_links")
                .is_none_or(|v| v.as_array().is_some_and(Vec::is_empty)),
            "service {name} uses external container links"
        );
        if let Some(volumes) = service.get("volumes_from").and_then(Value::as_array) {
            ensure!(
                volumes
                    .iter()
                    .all(|v| v.as_str().is_some_and(|v| !v.starts_with("container:"))),
                "service {name} mounts volumes from an unowned container"
            );
        }
        if let Some(mode) = service.get("network_mode").and_then(Value::as_str) {
            ensure!(
                mode == "none" || mode.starts_with("service:"),
                "service {name} uses unowned network_mode {mode}"
            );
        }
        for key in ["pid", "ipc"] {
            if let Some(mode) = service.get(key).and_then(Value::as_str) {
                ensure!(
                    mode != "host" && !mode.starts_with("container:"),
                    "service {name} uses unowned {key}"
                );
            }
        }
        let mut cpus = cpu_share;
        let mut memory = memory_share;
        if let Some(value) = service.get("cpus") {
            let explicit = cpu_limit(value)?;
            if explicit > 0.0 {
                cpus = cpus.min(explicit);
            }
        }
        if let Some(quota) = service.get("cpu_quota").and_then(Value::as_i64)
            && quota > 0
        {
            let period = service
                .get("cpu_period")
                .and_then(Value::as_u64)
                .unwrap_or(100_000);
            ensure!(period > 0, "service {name} has a zero CPU period");
            cpus = cpus.min(quota as f64 / period as f64);
        }
        // Represent the stricter effective quota once: Compose rejects quota
        // and cpus fields when they describe different effective limits.
        service.remove("cpu_quota");
        service.remove("cpu_period");
        if let Some(value) = service.get("mem_limit") {
            let explicit = memory_bytes(value)?;
            if explicit > 0 {
                memory = memory.min(explicit);
            }
        }
        let deploy = service
            .entry("deploy")
            .or_insert_with(|| serde_json::json!({}));
        let deploy = object_mut(deploy, "deploy")?;
        deploy.insert("replicas".into(), Value::from(1));
        deploy.insert(
            "restart_policy".into(),
            serde_json::json!({"condition": "none"}),
        );
        let resources = deploy
            .entry("resources")
            .or_insert_with(|| serde_json::json!({}));
        let resources = object_mut(resources, "deploy.resources")?;
        let limits = resources
            .entry("limits")
            .or_insert_with(|| serde_json::json!({}));
        let limits = object_mut(limits, "deploy.resources.limits")?;
        if let Some(value) = limits.get("cpus") {
            let explicit = cpu_limit(value)?;
            if explicit > 0.0 {
                cpus = cpus.min(explicit);
            }
        }
        if let Some(value) = limits.get("memory") {
            let explicit = memory_bytes(value)?;
            if explicit > 0 {
                memory = memory.min(explicit);
            }
        }
        limits.insert("cpus".into(), Value::from(cpus));
        // The Compose input schema requires a byte-value string here, even
        // though `compose config --format json` normalizes memory to a number.
        limits.insert("memory".into(), Value::String(memory.to_string()));
        service.insert("cpus".into(), Value::from(cpus));
        service.insert("mem_limit".into(), Value::from(memory));
        let existing = service
            .entry("labels")
            .or_insert_with(|| serde_json::json!({}));
        object_mut(existing, "service labels")?.extend(labels.as_object().unwrap().clone());
    }
    Ok(())
}

fn object_mut<'a>(value: &'a mut Value, description: &str) -> Result<&'a mut Map<String, Value>> {
    value.as_object_mut().with_context(|| {
        format!("{description} must be an object in resolved Compose configuration")
    })
}

fn number(value: &Value, description: &str) -> Result<u16> {
    let number = value
        .as_u64()
        .and_then(|number| u16::try_from(number).ok())
        .or_else(|| value.as_str().and_then(|number| number.parse::<u16>().ok()))
        .with_context(|| format!("{description} must be a port number between 1 and 65535"))?;
    ensure!(number != 0, "{description} must not be zero");
    Ok(number)
}

fn absolute_value(value: &mut Value, directory: &Path, description: &str) -> Result<()> {
    let path = value
        .as_str()
        .with_context(|| format!("{description} must be a path string"))?;
    if !Path::new(path).is_absolute() {
        *value = Value::String(
            directory
                .join(path)
                .to_str()
                .with_context(|| format!("{description} is not UTF-8"))?
                .to_owned(),
        );
    }
    Ok(())
}

fn build_context(value: &mut Value, directory: &Path) -> Result<()> {
    let context = value.as_str().context("build context must be a string")?;
    if !context.contains("://")
        && !context.starts_with("git@")
        && !context.starts_with("service:")
        && !context.starts_with("docker-image:")
    {
        absolute_value(value, directory, "build context")?;
    }
    Ok(())
}

fn isolate(config: &mut Value, project: &str, directory: &Path) -> Result<()> {
    let root = object_mut(config, "Compose configuration")?;
    root.insert("name".to_owned(), Value::String(project.to_owned()));
    for kind in ["networks", "volumes"] {
        if let Some(resources) = root.get_mut(kind) {
            for (name, resource) in object_mut(resources, kind)? {
                if resource.is_null() {
                    *resource = Value::Object(Map::new());
                }
                let resource = object_mut(resource, &format!("{kind}.{name}"))?;
                ensure!(
                    resource.get("external").is_none_or(
                        |external| external.is_null() || external == &Value::Bool(false)
                    ),
                    "external {kind} resource {name:?} cannot be isolated; use a project-owned resource"
                );
                resource.remove("external");
                resource.insert(
                    "name".to_owned(),
                    Value::String(format!("{project}_{name}")),
                );
            }
        }
    }
    for kind in ["configs", "secrets"] {
        if let Some(resources) = root.get_mut(kind) {
            for (name, resource) in object_mut(resources, kind)? {
                if let Some(file) = resource.get_mut("file") {
                    absolute_value(file, directory, &format!("{kind}.{name}.file"))?;
                }
            }
        }
    }
    let services = root
        .get_mut("services")
        .context("Compose configuration has no services")?;
    let services = object_mut(services, "services")?;
    ensure!(
        !services.is_empty(),
        "Compose configuration has no services"
    );
    let built_image = |name: &str| format!("{project}:build-{:x}", Sha256::digest(name.as_bytes()));
    // A service that reuses an image another service builds must run that
    // service's isolated build, not pull or reuse the shared name.
    let mut built = BTreeMap::new();
    for (name, service) in services.iter() {
        if service.get("build").is_some()
            && let Some(image) = service.get("image").and_then(Value::as_str)
        {
            built
                .entry(image.to_owned())
                .or_insert_with(|| built_image(name));
        }
    }
    for (name, service) in services {
        let service = object_mut(service, &format!("service {name}"))?;
        ensure!(
            service.get("network_mode").and_then(Value::as_str) != Some("host"),
            "service {name:?} uses host networking, which cannot be isolated"
        );
        service.remove("container_name");
        if let Some(ports) = service.get_mut("ports") {
            for port in ports
                .as_array_mut()
                .with_context(|| format!("service {name} ports must be an array"))?
            {
                let port = object_mut(port, &format!("service {name} port"))?;
                number(
                    port.get("target")
                        .context("resolved Compose port has no target")?,
                    "container port",
                )?;
                let protocol = port
                    .get("protocol")
                    .and_then(Value::as_str)
                    .unwrap_or("tcp");
                ensure!(
                    protocol == "tcp" || protocol == "udp",
                    "service {name:?} has unsupported port protocol {protocol:?}"
                );
                port.insert("published".to_owned(), Value::String("0".to_owned()));
                port.insert("host_ip".to_owned(), Value::String("127.0.0.1".to_owned()));
            }
        }
        if let Some(build) = service.get_mut("build") {
            if build.is_string() {
                build_context(build, directory)?;
            } else {
                let build = object_mut(build, &format!("service {name} build"))?;
                build.remove("tags");
                if let Some(context) = build.get_mut("context") {
                    build_context(context, directory)?;
                }
                if let Some(contexts) = build.get_mut("additional_contexts") {
                    for context in object_mut(contexts, "additional build contexts")?.values_mut() {
                        build_context(context, directory)?;
                    }
                }
            }
            // Keep owner builds off user-managed image tags. A complete build
            // before startup replaces these project-local tags on every handoff.
            let image = built_image(name);
            service.insert("image".to_owned(), Value::String(image));
        } else if let Some(image) = service
            .get("image")
            .and_then(Value::as_str)
            .and_then(|image| built.get(image))
        {
            service.insert("image".to_owned(), Value::String(image.clone()));
        }
        if let Some(volumes) = service.get_mut("volumes") {
            for volume in volumes
                .as_array_mut()
                .with_context(|| format!("service {name} volumes must be an array"))?
            {
                if volume.get("type").and_then(Value::as_str) == Some("bind") {
                    let source = volume
                        .get_mut("source")
                        .context("bind mount has no source path")?;
                    absolute_value(source, directory, "bind mount source")?;
                }
            }
        }
        if let Some(files) = service.get_mut("env_file") {
            let files = files
                .as_array_mut()
                .context("resolved env_file must be an array")?;
            for file in files {
                if file.is_string() {
                    absolute_value(file, directory, "env_file")?;
                } else if let Some(path) = file.get_mut("path") {
                    absolute_value(path, directory, "env_file path")?;
                }
            }
        }
    }
    Ok(())
}

fn services(config: &Value) -> Result<&Map<String, Value>> {
    config
        .get("services")
        .and_then(Value::as_object)
        .context("resolved Compose configuration has no services")
}

fn validate_links(config: &Value, links: &Links) -> Result<Vec<String>> {
    let services = services(config)?;
    for (target, variables) in links {
        ensure!(
            services.contains_key(target),
            "browser link target service {target:?} does not exist"
        );
        for (variable, link) in variables {
            let source = services.get(&link.source).with_context(|| {
                format!(
                    "browser link {target}.{variable} refers to missing source service {:?}",
                    link.source
                )
            })?;
            let found = source
                .get("ports")
                .and_then(Value::as_array)
                .is_some_and(|ports| {
                    ports.iter().any(|port| {
                        port.get("target")
                            .and_then(|target| number(target, "browser link port").ok())
                            == Some(link.port)
                            && port
                                .get("protocol")
                                .and_then(Value::as_str)
                                .unwrap_or("tcp")
                                == "tcp"
                    })
                });
            ensure!(
                found,
                "browser link {target}.{variable} refers to {}:{}, but the source does not publish that TCP port",
                link.source,
                link.port
            );
        }
    }
    // Include native Compose dependencies so starting a source cannot implicitly
    // start an as-yet-unwired target. Native health/completion conditions are still
    // enforced by Compose itself when the ordered services are started.
    let mut dependencies: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (name, service) in services {
        let mut required = BTreeSet::new();
        if let Some(depends_on) = service.get("depends_on") {
            if let Some(depends_on) = depends_on.as_object() {
                for (dependency, options) in depends_on {
                    if services.contains_key(dependency) {
                        required.insert(dependency.clone());
                    } else {
                        ensure!(
                            options.get("required").and_then(Value::as_bool) == Some(false),
                            "service {name:?} depends on missing service {dependency:?}"
                        );
                    }
                }
            } else if let Some(depends_on) = depends_on.as_array() {
                for dependency in depends_on {
                    required.insert(
                        dependency
                            .as_str()
                            .context("depends_on entry must be a service name")?
                            .to_owned(),
                    );
                }
            }
        }
        for key in ["network_mode", "ipc", "pid"] {
            if let Some(dependency) = service
                .get(key)
                .and_then(Value::as_str)
                .and_then(|mode| mode.strip_prefix("service:"))
            {
                required.insert(dependency.to_owned());
            }
        }
        if let Some(variables) = links.get(name) {
            required.extend(variables.values().map(|link| link.source.clone()));
        }
        dependencies.insert(name.clone(), required);
    }
    fn visit(
        name: &str,
        dependencies: &BTreeMap<String, BTreeSet<String>>,
        visiting: &mut BTreeSet<String>,
        visited: &mut BTreeSet<String>,
        order: &mut Vec<String>,
    ) -> Result<()> {
        if visited.contains(name) {
            return Ok(());
        }
        ensure!(
            visiting.insert(name.to_owned()),
            "browser link/Compose dependency cycle involving service {name:?}"
        );
        for dependency in dependencies
            .get(name)
            .with_context(|| format!("dependency service {name:?} does not exist"))?
        {
            visit(dependency, dependencies, visiting, visited, order)?;
        }
        visiting.remove(name);
        visited.insert(name.to_owned());
        order.push(name.to_owned());
        Ok(())
    }
    let mut order = Vec::new();
    let mut visiting = BTreeSet::new();
    let mut visited = BTreeSet::new();
    for target in links.keys() {
        visit(
            target,
            &dependencies,
            &mut visiting,
            &mut visited,
            &mut order,
        )?;
    }
    Ok(order)
}

// `config --format json` has already interpolated the original project. Escape
// literal dollars on serialization, without copying the complete manifest.
struct Escaped<'a>(&'a Value);

impl Serialize for Escaped<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        match self.0 {
            Value::String(text) if text.contains('$') => {
                serializer.serialize_str(&text.replace('$', "$$"))
            }
            Value::Array(values) => {
                let mut array = serializer.serialize_seq(Some(values.len()))?;
                for value in values {
                    array.serialize_element(&Escaped(value))?;
                }
                array.end()
            }
            Value::Object(values) => {
                let mut object = serializer.serialize_map(Some(values.len()))?;
                for (key, value) in values {
                    object.serialize_entry(key, &Escaped(value))?;
                }
                object.end()
            }
            value => value.serialize(serializer),
        }
    }
}

fn save(manifest: &Manifest, config: &Value) -> Result<()> {
    let directory = manifest
        .path
        .parent()
        .context("generated manifest has no parent")?;
    fs::create_dir_all(directory).with_context(|| format!("create {}", directory.display()))?;
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let temporary = directory.join(format!(
        ".compose-{}-{}.tmp",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)
            .with_context(|| format!("create {}", temporary.display()))?;
        serde_json::to_writer_pretty(&mut file, &Escaped(config))
            .context("serialize generated Compose configuration")?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        fs::rename(&temporary, &manifest.path)
            .with_context(|| format!("persist {}", manifest.path.display()))?;
        fs::File::open(directory)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn command(_worktree: &Worktree, manifest: &Manifest) -> Command {
    let mut command = Command::new("docker");
    let directory = manifest.path.parent().expect("manifest parent");
    command
        .current_dir(directory)
        .env_remove("COMPOSE_FILE")
        .env_remove("COMPOSE_PROFILES");
    command.args(["compose", "--project-name", &manifest.project]);
    command.arg("--project-directory").arg(directory);
    command.arg("--file").arg(&manifest.path);
    command
}

fn read_status(worktree: &Worktree, manifest: &Manifest) -> Result<Environment> {
    let output = output_within(
        command(worktree, manifest).args(["ps", "--all", "--format", "json"]),
        "read Compose environment status",
        Duration::from_secs(30),
    )?;
    parse_status(&manifest.project, &output.stdout)
}

fn parse_status(project: &str, bytes: &[u8]) -> Result<Environment> {
    let text = std::str::from_utf8(bytes)
        .context("Docker Compose status was not UTF-8")?
        .trim();
    let rows = if text.is_empty() {
        Vec::new()
    } else if let Ok(value) = serde_json::from_str::<Value>(text) {
        match value {
            Value::Array(rows) => rows,
            Value::Object(_) => vec![value],
            _ => bail!("Docker Compose status must contain JSON objects or an array"),
        }
    } else {
        text.lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| {
                serde_json::from_str::<Value>(line)
                    .context("Docker Compose returned invalid status JSON")
            })
            .collect::<Result<Vec<_>>>()?
    };
    let mut services: BTreeMap<String, Service> = BTreeMap::new();
    for row in rows {
        let name = row
            .get("Service")
            .and_then(Value::as_str)
            .context("Docker Compose status row has no Service name")?;
        let state = row.get("State").and_then(Value::as_str).unwrap_or("");
        let health = row.get("Health").and_then(Value::as_str).unwrap_or("");
        let exit_code = row.get("ExitCode").and_then(Value::as_i64).unwrap_or(0);
        let service = services.entry(name.to_owned()).or_insert_with(|| Service {
            name: name.to_owned(),
            state: state.to_owned(),
            health: health.to_owned(),
            exit_code,
            ports: Vec::new(),
        });
        if service.state != state {
            service.state = "mixed".to_owned();
        }
        if service.health != health {
            service.health = "mixed".to_owned();
        }
        if service.exit_code == 0 {
            service.exit_code = exit_code;
        }
        if let Some(publishers) = row.get("Publishers").filter(|value| !value.is_null()) {
            for publisher in publishers
                .as_array()
                .context("Docker Compose Publishers must be an array")?
            {
                // Compose can include exposed-but-unpublished ports with host 0.
                let Some(host) = publisher.get("PublishedPort") else {
                    continue;
                };
                if host == &Value::from(0) || host.as_str() == Some("0") {
                    continue;
                }
                let port = Port {
                    host: number(host, "published host port")?,
                    container: number(
                        publisher
                            .get("TargetPort")
                            .context("published port has no TargetPort")?,
                        "published container port",
                    )?,
                    protocol: publisher
                        .get("Protocol")
                        .and_then(Value::as_str)
                        .unwrap_or("tcp")
                        .to_owned(),
                };
                if !service.ports.contains(&port) {
                    service.ports.push(port);
                }
            }
        }
    }
    for service in services.values_mut() {
        service.ports.sort_by(|a, b| {
            (&a.container, &a.protocol, &a.host).cmp(&(&b.container, &b.protocol, &b.host))
        });
    }
    Ok(Environment {
        project: project.to_owned(),
        services: services.into_values().collect(),
    })
}

fn wire(
    config: &mut Value,
    target: &str,
    variables: &BTreeMap<String, Link>,
    environment: &Environment,
) -> Result<()> {
    let mut values = Map::new();
    for (variable, link) in variables {
        let source = environment
            .services
            .iter()
            .find(|service| service.name == link.source)
            .with_context(|| {
                format!(
                    "browser link {target}.{variable}: source service {:?} has no container",
                    link.source
                )
            })?;
        ensure!(
            source.state == "running",
            "browser link {target}.{variable}: source service {:?} is {}, not running",
            link.source,
            source.state
        );
        let ports: BTreeSet<u16> = source
            .ports
            .iter()
            .filter(|port| port.container == link.port && port.protocol == "tcp")
            .map(|port| port.host)
            .collect();
        ensure!(
            ports.len() == 1,
            "browser link {target}.{variable}: {}:{} needs exactly one published TCP host port, found {}",
            link.source,
            link.port,
            ports.len()
        );
        let host = ports.first().expect("exactly one published port");
        values.insert(
            variable.clone(),
            Value::String(format!("http://127.0.0.1:{host}")),
        );
    }
    let service = config
        .get_mut("services")
        .and_then(|services| services.get_mut(target))
        .with_context(|| format!("browser link target service {target:?} does not exist"))?;
    let service = object_mut(service, "browser link target service")?;
    let environment = service
        .entry("environment")
        .or_insert_with(|| Value::Object(Map::new()));
    if environment.is_null() {
        *environment = Value::Object(Map::new());
    }
    object_mut(environment, "resolved service environment")?.extend(values);
    Ok(())
}

fn inventory(kind: &str, filter: Option<&str>, deadline: Instant) -> Result<Vec<Value>> {
    let mut list = Command::new("docker");
    list.args([kind, "ls", "--quiet", "--no-trunc"]);
    if kind == "container" {
        list.arg("--all");
    }
    if let Some(filter) = filter {
        list.args(["--filter", filter]);
    }
    let remaining = || {
        deadline
            .saturating_duration_since(Instant::now())
            .min(Duration::from_secs(30))
    };
    let output = output_within(&mut list, "inventory Docker resources", remaining())?;
    let text = std::str::from_utf8(&output.stdout)?;
    let ids: Vec<_> = text.split_whitespace().collect();
    let mut resources = Vec::new();
    for batch in ids.chunks(64) {
        let output = output_within(
            Command::new("docker").args([kind, "inspect"]).args(batch),
            "inspect Docker ownership",
            remaining(),
        )?;
        resources.extend(serde_json::from_slice::<Vec<Value>>(&output.stdout)?);
    }
    Ok(resources)
}

fn labels(resource: &Value) -> Option<&Map<String, Value>> {
    resource
        .pointer("/Config/Labels")
        .or_else(|| resource.get("Labels"))
        .and_then(Value::as_object)
}

fn label<'a>(resource: &'a Value, key: &str) -> Option<&'a str> {
    labels(resource)?.get(key)?.as_str()
}

fn load_manifest(path: &Path) -> Result<Value> {
    let mut config: Value = serde_json::from_slice(
        &fs::read(path).with_context(|| format!("read checking manifest {}", path.display()))?,
    )?;
    // Metadata is read by Crit, not Compose. Undo only the interpolation escape
    // added by Escaped; the actual Compose fields must remain escaped on disk.
    for key in ["repository", "worktree"] {
        if let Some(value) = config.get_mut("x-crit").and_then(|meta| meta.get_mut(key))
            && let Some(text) = value.as_str()
        {
            *value = Value::String(text.replace("$$", "$"));
        }
    }
    Ok(config)
}

fn persisted(manifest: &Manifest) -> Result<Value> {
    let config = load_manifest(&manifest.path)?;
    let repository = config
        .pointer("/x-crit/repository")
        .and_then(Value::as_str)
        .context("manifest is not a new Crit managed runtime")?;
    ensure!(
        identity(Path::new(repository)) == manifest.project,
        "manifest repository identity mismatch"
    );
    ensure!(
        config
            .pointer("/x-crit/worktree")
            .and_then(Value::as_str)
            .is_some(),
        "manifest owner is missing"
    );
    Ok(config)
}

fn verified(resource: &Value, manifest: &Manifest, config: &Value) -> bool {
    label(resource, "io.crit.managed") == Some("2")
        && label(resource, "io.crit.repository")
            == config.pointer("/x-crit/repository").and_then(Value::as_str)
        && label(resource, "io.crit.worktree")
            == config.pointer("/x-crit/worktree").and_then(Value::as_str)
        && label(resource, "com.docker.compose.project") == Some(manifest.project.as_str())
}

/// Caller must hold project_lock_path exclusively through recovery and startup.
pub fn preflight(worktree: &Worktree) -> Result<()> {
    let repository = crate::worktree::discover(&worktree.path)?;
    let project = identity(&repository.common_dir);
    let trees = crate::worktree::list(&repository)?;
    let legacy: BTreeSet<_> = trees
        .iter()
        .map(|tree| format!("crit-{}", tree.id))
        .collect();
    let manifest = manifest(worktree)?;
    let config = if manifest.path.is_file() {
        Some(persisted(&manifest)?)
    } else {
        None
    };
    let mut conflicts = Vec::new();
    for resource in inventory("container", None, Instant::now() + Duration::from_secs(30))? {
        let compose = label(&resource, "com.docker.compose.project").unwrap_or("");
        let same_path = |text: &str| -> bool {
            let path = Path::new(text);
            trees.iter().any(|tree| path.starts_with(&tree.path))
                || crate::worktree::discover(path)
                    .is_ok_and(|repo| repo.common_dir == repository.common_dir)
        };
        let path_label = ["com.docker.compose.project.working_dir", "io.crit.worktree"]
            .iter()
            .any(|key| label(&resource, key).is_some_and(same_path));
        let files =
            label(&resource, "com.docker.compose.project.config_files").is_some_and(|files| {
                files.split(',').any(|file| {
                    same_path(
                        Path::new(file)
                            .parent()
                            .unwrap_or(Path::new(file))
                            .to_str()
                            .unwrap_or(""),
                    )
                })
            });
        let mounts = resource
            .get("Mounts")
            .and_then(Value::as_array)
            .is_some_and(|mounts| {
                mounts.iter().any(|mount| {
                    mount
                        .get("Source")
                        .and_then(Value::as_str)
                        .is_some_and(same_path)
                })
            });
        let same = compose == project
            || legacy.contains(compose)
            || path_label
            || files
            || mounts
            || label(&resource, "io.crit.repository") == repository.common_dir.to_str();
        if same
            && !config
                .as_ref()
                .is_some_and(|config| verified(&resource, &manifest, config))
        {
            conflicts.push(format!(
                "{} (project {compose:?}, id {})",
                resource.get("Name").and_then(Value::as_str).unwrap_or("?"),
                resource.get("Id").and_then(Value::as_str).unwrap_or("?")
            ));
        }
    }
    ensure!(
        conflicts.is_empty(),
        "same-repository legacy/unmanaged Docker stack blocks checking: {}. Stop/migrate these resources explicitly; Crit will not stop them.",
        conflicts.join(", ")
    );
    Ok(())
}

fn cleanup(manifest: &Manifest) -> Result<()> {
    let config = persisted(manifest)?;
    let deadline = Instant::now() + Duration::from_secs(120);
    let filter = format!("label=com.docker.compose.project={}", manifest.project);
    let mut unverified = Vec::new();
    for kind in ["container", "network"] {
        let resources = inventory(kind, Some(&filter), deadline)?;
        // Do not abandon authenticated containers merely because an unknown
        // resource shares their project name. Remove only the proven subset,
        // then keep the project blocked until the conflict is explicitly fixed.
        for resource in &resources {
            if !verified(resource, manifest, &config) {
                unverified.push(format!(
                    "{kind} {}",
                    resource.get("Id").and_then(Value::as_str).unwrap_or("?")
                ));
            }
        }
        for resource in resources
            .into_iter()
            .filter(|resource| verified(resource, manifest, &config))
        {
            let id = resource
                .get("Id")
                .and_then(Value::as_str)
                .context("resource has no id")?;
            if kind == "container" {
                output_within(
                    Command::new("docker").args(["container", "stop", "--time", "10", id]),
                    "stop verified checking container",
                    deadline
                        .saturating_duration_since(Instant::now())
                        .min(Duration::from_secs(30)),
                )?;
            }
            let mut command = Command::new("docker");
            command.args([kind, "rm"]);
            output_within(
                command.arg(id),
                "remove verified checking resource",
                deadline
                    .saturating_duration_since(Instant::now())
                    .min(Duration::from_secs(30)),
            )?;
        }
        ensure!(
            !inventory(kind, Some(&filter), deadline)?
                .iter()
                .any(|resource| verified(resource, manifest, &config)),
            "checking cleanup left managed {kind} resources for {}",
            manifest.project
        );
    }
    ensure!(
        unverified.is_empty(),
        "project {} contains unverified resources (not stopped): {}. Resolve explicitly before requesting another turn.",
        manifest.project,
        unverified.join(", ")
    );
    Ok(())
}

/// Removes only resources authenticated against the durable managed manifest.
/// Named and anonymous data volumes are intentionally never removed.
pub fn recover(worktree: &Worktree) -> Result<()> {
    let manifest = manifest(worktree)?;
    if manifest.path.is_file() {
        cleanup(&manifest)?;
    }
    Ok(())
}

pub fn up(worktree: &Worktree) -> Result<Environment> {
    preflight(worktree)?;
    let manifest = manifest(worktree)?;
    // Finish old ownership before replacing its manifest or building new code.
    if manifest.path.is_file() {
        cleanup(&manifest)?;
    }
    let settings = settings(worktree)?;
    let mut config = resolved(worktree, &settings, &manifest)?;
    let order = validate_links(&config, &settings.links)?;
    save(&manifest, &config)?;
    let started = Instant::now();
    let start = (|| -> Result<Environment> {
        let remaining = || Duration::from_secs(600).saturating_sub(started.elapsed());
        output_within(
            command(worktree, &manifest).arg("build"),
            "build checking images",
            remaining(),
        )?;
        for name in order {
            if !settings.links.contains_key(&name)
                && !settings
                    .links
                    .values()
                    .any(|variables| variables.values().any(|link| link.source == name))
            {
                continue;
            }
            if let Some(variables) = settings.links.get(&name).filter(|v| !v.is_empty()) {
                wire(
                    &mut config,
                    &name,
                    variables,
                    &read_status(worktree, &manifest)?,
                )?;
                save(&manifest, &config)?;
            }
            output_within(
                command(worktree, &manifest).args([
                    "up",
                    "--detach",
                    "--no-build",
                    "--no-recreate",
                    "--wait",
                    "--wait-timeout",
                    "120",
                    "--",
                    &name,
                ]),
                &format!("start checking service {name}"),
                remaining(),
            )?;
        }
        output_within(
            command(worktree, &manifest).args([
                "up",
                "--detach",
                "--no-build",
                "--wait",
                "--wait-timeout",
                "120",
            ]),
            "start healthy checking runtime",
            remaining(),
        )?;
        let environment = read_status(worktree, &manifest)?;
        let completed: BTreeSet<_> = services(&config)?
            .values()
            .filter_map(|service| service.get("depends_on").and_then(Value::as_object))
            .flat_map(|dependencies| dependencies.iter())
            .filter(|(_, dependency)| {
                dependency.get("condition").and_then(Value::as_str)
                    == Some("service_completed_successfully")
            })
            .map(|(name, _)| name.as_str())
            .collect();
        for name in services(&config)?.keys() {
            ensure!(
                environment
                    .services
                    .iter()
                    .any(|service| service.name == *name
                        && ((service.state == "running"
                            && (service.health.is_empty() || service.health == "healthy"))
                            || (completed.contains(name.as_str())
                                && service.state == "exited"
                                && service.exit_code == 0))),
                "checking service {name} did not remain running/healthy or finish its required one-shot successfully"
            );
        }
        Ok(environment)
    })();
    match start {
        Ok(environment) => Ok(environment),
        Err(error) => match cleanup(&manifest) {
            Ok(()) => Err(error),
            Err(cleanup) => Err(anyhow::anyhow!(
                "{error:#}; partial startup cleanup failed: {cleanup:#}"
            )),
        },
    }
}

pub fn status(worktree: &Worktree) -> Result<Environment> {
    let manifest = manifest(worktree)?;
    if !manifest.path.is_file() {
        return Ok(Environment {
            project: manifest.project,
            services: Vec::new(),
        });
    }
    persisted(&manifest)?;
    read_status(worktree, &manifest)
}

pub fn down(worktree: &Worktree) -> Result<Environment> {
    let manifest = manifest(worktree)?;
    if manifest.path.is_file() {
        cleanup(&manifest)?;
    }
    Ok(Environment {
        project: manifest.project,
        services: Vec::new(),
    })
}

pub fn logs_args(worktree: &Worktree) -> Result<Vec<String>> {
    let manifest = manifest(worktree)?;
    ensure!(
        manifest.path.is_file(),
        "checking runtime has not been started"
    );
    persisted(&manifest)?;
    Ok(vec![
        "compose".into(),
        "--project-name".into(),
        manifest.project,
        "--project-directory".into(),
        manifest
            .path
            .parent()
            .context("manifest directory missing")?
            .to_str()
            .context("manifest path is not UTF-8")?
            .into(),
        "--file".into(),
        manifest
            .path
            .to_str()
            .context("manifest path is not UTF-8")?
            .into(),
        "logs".into(),
        "--follow".into(),
        "--no-color".into(),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn local_fixture() -> (tempfile::TempDir, Worktree, Worktree) {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("source");
        fs::create_dir(&root).unwrap();
        let run = |arguments: &[&str]| {
            assert!(
                Command::new("git")
                    .arg("-C")
                    .arg(&root)
                    .args(arguments)
                    .status()
                    .unwrap()
                    .success()
            );
        };
        run(&["init", "-b", "main"]);
        fs::write(
            root.join(".gitignore"),
            ".env\ncrit.toml\n*.override.yml\n/env/\n",
        )
        .unwrap();
        run(&["add", ".gitignore"]);
        run(&[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "-m",
            "fixture",
        ]);
        let repository = crate::worktree::discover(&root).unwrap();
        let source = crate::worktree::list(&repository).unwrap().remove(0);
        let target = crate::worktree::create(&repository, "copy-test", None).unwrap();
        (directory, source, target)
    }

    #[test]
    fn local_env_copies_are_private_independent_and_preserve_existing_files() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let (_directory, source, target) = local_fixture();
        fs::create_dir(source.path.join("env")).unwrap();
        fs::write(source.path.join("env/keycloak.env"), "SECRET=source\n").unwrap();
        fs::write(source.path.join("env/database.db"), "not an env input").unwrap();
        let config = json!({"services": {"auth": {"env_file": [
            {"path": target.path.join("env/keycloak.env"), "required": true}
        ]}}});
        for path in env_files(&config, &target.path, &target.path).unwrap() {
            copy_local_file(&source.path, &target.path, &path).unwrap();
        }
        let copied = target.path.join("env/keycloak.env");
        assert_eq!(fs::read_to_string(&copied).unwrap(), "SECRET=source\n");
        assert_eq!(
            fs::metadata(&copied).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_ne!(
            fs::metadata(&copied).unwrap().ino(),
            fs::metadata(source.path.join("env/keycloak.env"))
                .unwrap()
                .ino()
        );
        assert!(!target.path.join("env/database.db").exists());
        fs::write(&copied, "SECRET=target\n").unwrap();
        copy_local_file(&source.path, &target.path, Path::new("env/keycloak.env")).unwrap();
        assert_eq!(fs::read_to_string(&copied).unwrap(), "SECRET=target\n");
        assert_eq!(
            fs::read_to_string(source.path.join("env/keycloak.env")).unwrap(),
            "SECRET=source\n"
        );
    }

    #[test]
    fn local_copy_rejects_escapes_symlinks_and_shared_hardlinks() {
        use std::os::unix::fs::symlink;
        let (directory, source, target) = local_fixture();
        let outside = directory.path().join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("secret.env"), "outside").unwrap();
        assert!(local_relative(&target.path, Path::new("../outside/secret.env")).is_err());
        assert!(
            env_files(
                &json!({"services": {"auth": {"env_file": [outside.join("secret.env")]}}}),
                &target.path,
                &target.path
            )
            .is_err()
        );
        symlink(&outside, source.path.join("env")).unwrap();
        assert!(copy_local_file(&source.path, &target.path, Path::new("env/secret.env")).is_err());
        fs::remove_file(source.path.join("env")).unwrap();
        fs::create_dir(source.path.join("env")).unwrap();
        fs::write(source.path.join("env/secret.env"), "source").unwrap();
        symlink(&outside, target.path.join("env")).unwrap();
        assert!(copy_local_file(&source.path, &target.path, Path::new("env/secret.env")).is_err());
        fs::remove_file(target.path.join("env")).unwrap();
        fs::create_dir(target.path.join("env")).unwrap();
        symlink(
            source.path.join("env/secret.env"),
            target.path.join("env/secret.env"),
        )
        .unwrap();
        assert!(copy_local_file(&source.path, &target.path, Path::new("env/secret.env")).is_err());
        fs::remove_file(target.path.join("env/secret.env")).unwrap();
        fs::hard_link(
            source.path.join("env/secret.env"),
            target.path.join("env/secret.env"),
        )
        .unwrap();
        assert!(copy_local_file(&source.path, &target.path, Path::new("env/secret.env")).is_err());
        assert_eq!(
            fs::read_to_string(outside.join("secret.env")).unwrap(),
            "outside"
        );
    }

    #[test]
    fn local_copy_does_not_restore_tracked_files_or_unignored_inputs() {
        let (_directory, source, target) = local_fixture();
        fs::write(source.path.join("plain.env"), "not ignored").unwrap();
        copy_local_file(&source.path, &target.path, Path::new("plain.env")).unwrap();
        assert!(!target.path.join("plain.env").exists());
        fs::write(source.path.join(".env"), "tracked on source").unwrap();
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(&source.path)
                .args(["add", "--force", ".env"])
                .status()
                .unwrap()
                .success()
        );
        copy_local_file(&source.path, &target.path, Path::new(".env")).unwrap();
        assert!(!target.path.join(".env").exists());
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(&source.path)
                .args(["reset", "--", ".env"])
                .status()
                .unwrap()
                .success()
        );
        fs::write(target.path.join(".env"), "tracked on target").unwrap();
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(&target.path)
                .args(["add", "--force", ".env"])
                .status()
                .unwrap()
                .success()
        );
        fs::remove_file(target.path.join(".env")).unwrap();
        copy_local_file(&source.path, &target.path, Path::new(".env")).unwrap();
        assert!(!target.path.join(".env").exists());
    }

    #[test]
    fn preparation_rejects_unrelated_source_without_copying() {
        let (_one, source, _unused) = local_fixture();
        let (_two, _unused_source, target) = local_fixture();
        fs::write(source.path.join(".env"), "SECRET=unrelated").unwrap();
        assert!(
            prepare_local_files(&source, &target)
                .unwrap_err()
                .to_string()
                .contains("different repository")
        );
        assert!(!target.path.join(".env").exists());
    }

    #[test]
    fn discovery_keeps_env_paths_in_target_and_uses_no_env_resolution() {
        let (_directory, source, target) = local_fixture();
        let files = vec![target.path.join("docker-compose.yml")];
        let mut command = config_command(&target, &files, "crit-local-inputs").unwrap();
        command.arg("--no-env-resolution");
        let arguments: Vec<_> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert!(arguments.contains(&"--no-env-resolution".to_owned()));
        assert_eq!(command.get_current_dir(), Some(target.path.as_path()));
        let config = json!({"services": {"auth": {"env_file": ["env/keycloak.env", {"path": "./env/web.env", "required": false}]}}});
        assert_eq!(
            env_files(&config, &target.path, &target.path).unwrap(),
            BTreeSet::from([
                PathBuf::from("env/keycloak.env"),
                PathBuf::from("env/web.env")
            ])
        );
        assert!(
            !arguments
                .iter()
                .any(|arg| arg.contains(source.path.to_str().unwrap()))
        );
    }

    #[test]
    fn preparation_uses_explicit_nonmain_checkout_for_local_configuration() {
        let (_directory, main, target) = local_fixture();
        let repository = crate::worktree::discover(&main.path).unwrap();
        let source = crate::worktree::create(&repository, "source-branch", None).unwrap();
        fs::write(main.path.join("crit.toml"), "# main config\n").unwrap();
        fs::write(source.path.join("crit.toml"), "# split source config\n").unwrap();
        // No Compose manifest: this exercises source identity without running Docker.
        prepare_local_files(&source, &target).unwrap();
        assert_eq!(
            fs::read_to_string(target.path.join("crit.toml")).unwrap(),
            "# split source config\n"
        );
    }

    #[test]
    fn isolation_names_resources_and_allocates_loopback_tcp_and_udp() {
        let mut config = json!({
            "name": "user-project",
            "services": {"api": {
                "container_name": "shared-api", "image": "user/shared-image",
                "build": {"context": "./api", "tags": ["user/shared-tag"]},
                "volumes": [{"type": "bind", "source": "./src", "target": "/app"}],
                "ports": [{"target": 8000, "published": "8000", "host_ip": "0.0.0.0"}, {"target": 8001, "published": "8001", "protocol": "udp"}]
            }},
            "volumes": {"data": {"name": "shared-data"}},
            "networks": {"default": {"name": "shared-network"}}
        });
        isolate(&mut config, "crit-tree-one", Path::new("/checkouts/one")).unwrap();
        assert_eq!(config["volumes"]["data"]["name"], "crit-tree-one_data");
        assert_eq!(
            config["networks"]["default"]["name"],
            "crit-tree-one_default"
        );
        let api = &config["services"]["api"];
        assert!(api.get("container_name").is_none());
        assert!(
            api["image"]
                .as_str()
                .unwrap()
                .starts_with("crit-tree-one:build-")
        );
        assert!(api["build"].get("tags").is_none());
        assert_eq!(api["build"]["context"], "/checkouts/one/./api");
        assert_eq!(api["volumes"][0]["source"], "/checkouts/one/./src");
        for port in api["ports"].as_array().unwrap() {
            assert_eq!(port["published"], "0");
            assert_eq!(port["host_ip"], "127.0.0.1");
        }
        assert_eq!(api["ports"][1]["protocol"], "udp");
    }

    #[test]
    fn isolation_rewrites_images_built_by_another_service() {
        let mut config = json!({"services": {
            "web": {"build": {"context": "/repo"}, "image": "myapp"},
            "worker": {"image": "myapp"},
            "db": {"image": "postgres:17"}
        }});
        isolate(&mut config, "crit-tree", Path::new("/repo")).unwrap();
        let image = |name: &str| config["services"][name]["image"].as_str().unwrap();
        assert!(image("web").starts_with("crit-tree:build-"));
        assert_eq!(image("worker"), image("web"));
        assert_eq!(image("db"), "postgres:17");
    }

    #[test]
    fn shared_external_resources_and_host_networking_are_rejected() {
        for kind in ["networks", "volumes"] {
            for external in [json!(true), json!({"name": "shared"})] {
                let mut config = json!({"services": {"api": {"image": "fixture"}}});
                config[kind] = json!({"data": {"external": external}});
                assert!(
                    isolate(&mut config, "crit-one", Path::new("/repo"))
                        .unwrap_err()
                        .to_string()
                        .contains("external")
                );
            }
        }
        let mut config = json!({"services": {"api": {"network_mode": "host"}}});
        assert!(
            isolate(&mut config, "crit-one", Path::new("/repo"))
                .unwrap_err()
                .to_string()
                .contains("host networking")
        );
    }

    #[test]
    fn status_accepts_arrays_object_lines_and_exposed_ports() {
        let first = json!({"Service": "api", "State": "running", "Health": "healthy", "Publishers": [
            {"TargetPort": 8000, "PublishedPort": 32100, "Protocol": "tcp"},
            {"TargetPort": 9000, "PublishedPort": 0, "Protocol": "tcp"}
        ]});
        let second = json!({"Service": "web", "State": "exited", "Health": "", "ExitCode": 3, "Publishers": null});
        let array = serde_json::to_vec(&json!([first, second])).unwrap();
        let lines = format!("{first}\n{second}\n");
        let from_array = parse_status("crit-one", &array).unwrap();
        let from_lines = parse_status("crit-one", lines.as_bytes()).unwrap();
        assert_eq!(
            serde_json::to_value(&from_array).unwrap(),
            serde_json::to_value(&from_lines).unwrap()
        );
        assert_eq!(
            from_array.services[0].ports,
            vec![Port {
                host: 32100,
                container: 8000,
                protocol: "tcp".to_owned()
            }]
        );
        assert_eq!(from_array.services[1].state, "exited");
        assert_eq!(from_array.services[1].exit_code, 3);
        assert!(parse_status("crit-one", b"not json").is_err());
    }

    fn linked_config() -> (Value, Links) {
        let config = json!({"services": {
            "db": {"image": "db"},
            "api": {"depends_on": {"db": {"condition": "service_healthy"}}, "ports": [{"target": 8000}]},
            "web": {"environment": {"KEEP": "value"}, "ports": [{"target": 3000}]}
        }});
        let links = BTreeMap::from([(
            "web".to_owned(),
            BTreeMap::from([(
                "VITE_API_URL".to_owned(),
                Link {
                    source: "api".to_owned(),
                    port: 8000,
                },
            )]),
        )]);
        (config, links)
    }

    #[test]
    fn explicit_browser_links_order_dependencies_and_only_set_named_variables() {
        let (mut config, links) = linked_config();
        assert_eq!(
            validate_links(&config, &links).unwrap(),
            vec!["db", "api", "web"]
        );
        let environment = Environment {
            project: "crit-one".to_owned(),
            services: vec![Service {
                name: "api".to_owned(),
                state: "running".to_owned(),
                health: String::new(),
                exit_code: 0,
                ports: vec![Port {
                    host: 32123,
                    container: 8000,
                    protocol: "tcp".to_owned(),
                }],
            }],
        };
        wire(&mut config, "web", &links["web"], &environment).unwrap();
        assert_eq!(
            config["services"]["web"]["environment"],
            json!({"KEEP": "value", "VITE_API_URL": "http://127.0.0.1:32123"})
        );
        assert!(wire(&mut config, "web", &links["web"], &Environment::default()).is_err());
    }

    #[test]
    fn browser_links_reject_missing_ports_sources_and_combined_cycles() {
        let (mut config, mut links) = linked_config();
        links
            .get_mut("web")
            .unwrap()
            .get_mut("VITE_API_URL")
            .unwrap()
            .port = 1234;
        assert!(
            validate_links(&config, &links)
                .unwrap_err()
                .to_string()
                .contains("publish")
        );
        links
            .get_mut("web")
            .unwrap()
            .get_mut("VITE_API_URL")
            .unwrap()
            .source = "missing".to_owned();
        assert!(
            validate_links(&config, &links)
                .unwrap_err()
                .to_string()
                .contains("missing source")
        );
        let (_, links) = linked_config();
        config["services"]["api"]["depends_on"] = json!({"web": {}});
        assert!(
            validate_links(&config, &links)
                .unwrap_err()
                .to_string()
                .contains("cycle")
        );
    }

    #[test]
    fn generated_manifest_preserves_literal_dollars() {
        let source = json!({"services": {"api": {"environment": {"PASSWORD": "a$b${c}"}, "command": ["echo", "$HOME"]}}});
        let config = serde_json::to_value(Escaped(&source)).unwrap();
        assert_eq!(
            config["services"]["api"]["environment"]["PASSWORD"],
            "a$$b$${c}"
        );
        assert_eq!(config["services"]["api"]["command"][1], "$$HOME");
    }

    #[test]
    fn configured_files_and_links_preserve_the_worktree_base_directory() {
        let directory = tempfile::tempdir().unwrap();
        let tree = directory.path().join("checkout");
        fs::create_dir_all(tree.join("docker")).unwrap();
        fs::write(directory.path().join("shared.yaml"), "services: {}").unwrap();
        std::os::unix::fs::symlink(
            directory.path().join("shared.yaml"),
            tree.join("docker/compose.yaml"),
        )
        .unwrap();
        fs::write(tree.join("docker/override.yaml"), "services: {}").unwrap();
        fs::write(tree.join("crit.toml"), "[compose]\nfiles = ['docker/compose.yaml', 'docker/override.yaml']\n[compose.links.web]\nVITE_API_URL = 'api:8000'\n").unwrap();
        let worktree = Worktree {
            id: "fixture".to_owned(),
            path: tree.clone(),
            branch: "main".to_owned(),
            head: String::new(),
            is_main: true,
            locked: false,
            bare: false,
            prunable: false,
        };
        let settings = settings(&worktree).unwrap();
        assert!(has_config(&worktree));
        assert_eq!(
            compose_files(&worktree, &settings).unwrap(),
            vec![
                tree.join("docker/compose.yaml"),
                tree.join("docker/override.yaml")
            ]
        );
        assert_eq!(settings.links["web"]["VITE_API_URL"].source, "api");
        assert_eq!(settings.links["web"]["VITE_API_URL"].port, 8000);
    }

    #[test]
    fn persisted_manifest_is_private_and_does_not_mutate_resolved_values() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let manifest = Manifest {
            project: "crit-fixture".to_owned(),
            path: directory.path().join("state/compose.json"),
        };
        let config = json!({"services": {"api": {"image": "fixture", "environment": {"PASSWORD": "$secret"}}}});
        save(&manifest, &config).unwrap();
        let generated: Value = serde_json::from_slice(&fs::read(&manifest.path).unwrap()).unwrap();
        assert_eq!(
            generated["services"]["api"]["environment"]["PASSWORD"],
            "$$secret"
        );
        assert_eq!(
            config["services"]["api"]["environment"]["PASSWORD"],
            "$secret"
        );
        assert_eq!(
            fs::metadata(&manifest.path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn repository_identity_and_stopped_status_are_shared_without_docker() {
        let (_directory, source, target) = local_fixture();
        assert_eq!(project_id(&source).unwrap(), project_id(&target).unwrap());
        assert_eq!(
            project_lock_path(&source).unwrap(),
            project_lock_path(&target).unwrap()
        );
        assert!(status(&source).unwrap().services.is_empty());
        assert!(status(&target).unwrap().services.is_empty());
        assert!(!manifest(&source).unwrap().path.exists());
        assert!(
            logs_args(&target)
                .unwrap_err()
                .to_string()
                .contains("not been started")
        );
    }

    #[test]
    fn ownership_budget_and_volume_identity_follow_complete_checkout() {
        let (_directory, source, target) = local_fixture();
        let original = json!({"services": {
            "api": {"image": "api", "restart": "always", "cpus": 0.25,
                "deploy": {"replicas": 10, "resources": {"limits": {"memory": "64m"}}}},
            "web": {"image": "web", "profiles": ["optional"]}
        }, "volumes": {"data": {}}});
        let mut first = original.clone();
        govern(
            &mut first,
            &source,
            Budget {
                cpus: 2.0,
                memory_mib: 512,
            },
        )
        .unwrap();
        assert_eq!(first["services"]["api"]["cpus"], 0.25);
        assert_eq!(first["services"]["web"]["cpus"], 1.0);
        assert_eq!(first["services"]["api"]["mem_limit"], 64 * 1024 * 1024);
        assert_eq!(first["services"]["web"]["mem_limit"], 256 * 1024 * 1024);
        assert_eq!(
            first["services"]["api"]["deploy"]["resources"]["limits"]["memory"],
            "67108864"
        );
        assert_eq!(
            first["services"]["web"]["deploy"]["resources"]["limits"]["memory"],
            "268435456"
        );
        assert_eq!(first["services"]["api"]["restart"], "no");
        assert_eq!(first["services"]["api"]["deploy"]["replicas"], 1);
        assert!(first["services"]["web"].get("profiles").is_none());
        assert_eq!(first["services"]["api"]["labels"]["io.crit.managed"], "2");
        let mut second = original;
        govern(
            &mut second,
            &target,
            Budget {
                cpus: 2.0,
                memory_mib: 512,
            },
        )
        .unwrap();
        assert_eq!(
            first["x-crit"]["repository"],
            second["x-crit"]["repository"]
        );
        assert_ne!(first["x-crit"]["worktree"], second["x-crit"]["worktree"]);
        assert_ne!(
            first["volumes"]["data"]["name"],
            second["volumes"]["data"]["name"]
        );
    }

    #[test]
    fn runtime_rejects_code_from_another_checkout() {
        let (_directory, source, target) = local_fixture();
        assert!(owner_context(&json!(target.path), &target.path).is_ok());
        assert!(owner_context(&json!(source.path), &target.path).is_err());
        let mut build = json!({"services": {"api": {"build": {"context": source.path}}}});
        assert!(govern(&mut build, &target, Budget::default()).is_err());
        let mut bind = json!({"services": {"api": {"image": "api", "volumes": [
            {"type": "bind", "source": source.path, "target": "/app"}
        ]}}});
        assert!(govern(&mut bind, &target, Budget::default()).is_err());
    }

    #[test]
    fn persisted_teardown_metadata_does_not_need_checkout_or_interpolation() {
        let (directory, source, target) = local_fixture();
        let repository = crate::worktree::discover(&source.path).unwrap().common_dir;
        let manifest = Manifest {
            project: identity(&repository),
            path: directory.path().join("durable/compose.json"),
        };
        let config = json!({"x-crit": {
            "repository": repository, "worktree": target.path
        }, "services": {"api": {"environment": {"LITERAL": "$value"}}}});
        save(&manifest, &config).unwrap();
        fs::remove_dir_all(&target.path).unwrap();
        assert_eq!(persisted(&manifest).unwrap()["x-crit"], config["x-crit"]);
        assert_eq!(
            command(&target, &manifest).get_current_dir(),
            manifest.path.parent()
        );
        let dollars = Manifest {
            project: identity(Path::new("/repo/$name/.git")),
            path: directory.path().join("dollars/compose.json"),
        };
        save(
            &dollars,
            &json!({"x-crit": {"repository": "/repo/$name/.git", "worktree": "/repo/$$owner"}}),
        )
        .unwrap();
        assert_eq!(
            persisted(&dollars).unwrap()["x-crit"]["worktree"],
            "/repo/$$owner"
        );
    }

    #[test]
    fn teardown_requires_all_ownership_labels_not_just_project_name() {
        let manifest = Manifest {
            project: "fixture".into(),
            path: PathBuf::new(),
        };
        let config = json!({"x-crit": {"repository": "/repo/.git", "worktree": "/repo/owner"}});
        let mut resource = json!({"Config": {"Labels": {
            "com.docker.compose.project": "fixture", "io.crit.managed": "2",
            "io.crit.repository": "/repo/.git", "io.crit.worktree": "/repo/owner"
        }}});
        assert!(verified(&resource, &manifest, &config));
        resource["Config"]["Labels"]["io.crit.worktree"] = json!("/repo/other");
        assert!(!verified(&resource, &manifest, &config));
        resource["Config"]["Labels"]
            .as_object_mut()
            .unwrap()
            .remove("io.crit.managed");
        assert!(!verified(&resource, &manifest, &config));
    }

    #[test]
    fn checking_budget_configuration_is_bounded_and_independent_of_compose_table() {
        let defaults = Budget::default();
        assert!((1.0..=4.0).contains(&defaults.cpus));
        assert!((1024..=8192).contains(&defaults.memory_mib));
        let (_directory, source, _target) = local_fixture();
        fs::write(
            source.path.join("crit.toml"),
            "[checks]\ncpus = 1.5\nmemory_mib = 768\n",
        )
        .unwrap();
        let configured = settings(&source).unwrap();
        assert_eq!(configured.budget.cpus, 1.5);
        assert_eq!(configured.budget.memory_mib, 768);
        fs::write(source.path.join("crit.toml"), "[checks]\ncpus = 65\n").unwrap();
        assert!(settings(&source).is_err());
        fs::write(source.path.join("crit.toml"), "[checks]\nmemory_mib = 1\n").unwrap();
        assert!(settings(&source).is_err());
    }

    #[test]
    fn output_within_kills_hung_commands() {
        let started = Instant::now();
        let error = output_within(
            Command::new("sh").args(["-c", "sleep 5"]),
            "hung command",
            Duration::from_millis(100),
        )
        .unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(error.to_string().contains("did not answer"), "{error}");
        let output = output_within(
            Command::new("sh").args(["-c", "echo ok; echo warn >&2"]),
            "quick command",
            Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(output.stdout, b"ok\n");
        assert_eq!(output.stderr, b"warn\n");
    }
}
