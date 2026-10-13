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
    let Some(compose) = document.get("compose") else {
        return Ok(Settings::default());
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
    Ok(Settings { files, links })
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
            // Preserve the supplied file's directory even if the file is a symlink.
            // Relative contexts and binds belong to this checkout, not the link target.
            Ok(path)
        })
        .collect()
}

fn manifest(worktree: &Worktree) -> Result<Manifest> {
    ensure!(
        !worktree.id.is_empty()
            && worktree.id.len() <= 64
            && worktree
                .id
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-'),
        "invalid worktree environment id: {:?}",
        worktree.id
    );
    let state = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))
        .context("XDG_STATE_HOME or HOME is required to store Compose environments")?;
    Ok(Manifest {
        project: format!("crit-{}", worktree.id),
        path: state
            .join("crit/environments")
            .join(&worktree.id)
            .join("compose.json"),
    })
}

fn docker(worktree: &Worktree, project: &str) -> Command {
    let mut command = Command::new("docker");
    command
        .current_dir(&worktree.path)
        .env("COMPOSE_PROJECT_NAME", project);
    command
}

fn checked(command: &mut Command, operation: &str) -> Result<Output> {
    let output = command
        .output()
        .with_context(|| format!("{operation}: could not run Docker Compose"))?;
    succeeded(output, operation)
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

    fn drain(pipe: Option<impl Read + Send + 'static>) -> std::thread::JoinHandle<Vec<u8>> {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_end(&mut bytes);
            }
            bytes
        })
    }

    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .with_context(|| format!("{operation}: could not run Docker Compose"))?;
    let stdout = drain(child.stdout.take());
    let stderr = drain(child.stderr.take());
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child
            .try_wait()
            .with_context(|| format!("{operation}: could not wait for Docker Compose"))?
        {
            break Some(status);
        }
        if started.elapsed() >= deadline {
            unsafe { libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL) };
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(25));
    };
    let stdout = stdout.join().unwrap_or_default();
    let stderr = stderr.join().unwrap_or_default();
    let Some(status) = status else {
        bail!(
            "{operation}: Docker did not answer within {}s",
            deadline.as_secs_f64()
        );
    };
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
/// This is an environment operation: failure does not invalidate a created worktree.
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
    let directory = files[0]
        .parent()
        .context("Compose file has no parent directory")?;
    let mut command = config_command(worktree, &files, &manifest.project)?;
    let output = checked(&mut command, "resolve Compose configuration")?;
    let mut config: Value = serde_json::from_slice(&output.stdout)
        .context("Docker Compose returned invalid config JSON")?;
    isolate(&mut config, &manifest.project, directory)?;
    validate_links(&config, &settings.links)?;
    Ok(config)
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
            // Explicit build image names must not make two worktrees reuse or
            // overwrite one another's build outputs.
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
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn ensure_manifest(worktree: &Worktree) -> Result<Manifest> {
    let manifest = manifest(worktree)?;
    if !manifest.path.try_exists()? {
        let settings = settings(worktree)?;
        let config = resolved(worktree, &settings, &manifest)?;
        save(&manifest, &config)?;
    }
    Ok(manifest)
}

fn command(worktree: &Worktree, manifest: &Manifest) -> Command {
    let mut command = docker(worktree, &manifest.project);
    command.args(["compose", "--project-name", &manifest.project]);
    command.arg("--project-directory").arg(&worktree.path);
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

pub fn up(worktree: &Worktree) -> Result<Environment> {
    let manifest = manifest(worktree)?;
    let settings = settings(worktree)?;
    let mut config = resolved(worktree, &settings, &manifest)?;
    let order = validate_links(&config, &settings.links)?;
    save(&manifest, &config)?;
    for name in order {
        if !settings.links.contains_key(&name)
            && !settings
                .links
                .values()
                .any(|variables| variables.values().any(|link| link.source == name))
        {
            // Native dependencies are started by Compose, including its health
            // and completion conditions, rather than as redundant explicit ups.
            continue;
        }
        if let Some(variables) = settings
            .links
            .get(&name)
            .filter(|variables| !variables.is_empty())
        {
            let environment = read_status(worktree, &manifest)?;
            wire(&mut config, &name, variables, &environment)?;
            save(&manifest, &config)?;
        }
        checked(
            command(worktree, &manifest).args(["up", "--detach", "--", &name]),
            &format!("start Compose service {name}"),
        )?;
    }
    checked(
        command(worktree, &manifest).args(["up", "--detach"]),
        "start Compose environment",
    )?;
    read_status(worktree, &manifest)
}

pub fn status(worktree: &Worktree) -> Result<Environment> {
    read_status(worktree, &ensure_manifest(worktree)?)
}

/// Status of an environment Crit has started, without running Docker for one it has not:
/// `up` saves the manifest before it starts any container, so no manifest means none exist.
pub fn existing_status(worktree: &Worktree) -> Result<Option<Environment>> {
    let manifest = manifest(worktree)?;
    if !manifest.path.try_exists()? {
        return Ok(None);
    }
    read_status(worktree, &manifest).map(Some)
}

pub fn down(worktree: &Worktree) -> Result<Environment> {
    let manifest = ensure_manifest(worktree)?;
    // No --volumes, --rmi, --remove-orphans, or prune: project data survives down.
    checked(
        command(worktree, &manifest).arg("down"),
        "stop Compose environment",
    )?;
    read_status(worktree, &manifest)
}

pub fn restart(worktree: &Worktree) -> Result<Environment> {
    let manifest = ensure_manifest(worktree)?;
    checked(
        command(worktree, &manifest).arg("restart"),
        "restart Compose environment",
    )?;
    // Restarted containers get new ephemeral host ports; `up` re-wires link targets.
    if !settings(worktree)?.links.is_empty() {
        return up(worktree);
    }
    read_status(worktree, &manifest)
}

pub fn logs_args(worktree: &Worktree) -> Result<Vec<String>> {
    let manifest = manifest(worktree)?;
    ensure!(
        manifest.path.try_exists()?,
        "This worktree's Docker environment has not been started yet; start or refresh it first."
    );
    Ok(vec![
        "compose".to_owned(),
        "--project-name".to_owned(),
        manifest.project,
        "--project-directory".to_owned(),
        worktree
            .path
            .to_str()
            .context("worktree path is not UTF-8")?
            .to_owned(),
        "--file".to_owned(),
        manifest
            .path
            .to_str()
            .context("generated manifest path is not UTF-8")?
            .to_owned(),
        "logs".to_owned(),
        "--follow".to_owned(),
        "--no-color".to_owned(),
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
    fn existing_status_without_manifest_skips_docker() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("compose.yaml"), "services: {}\n").unwrap();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        // A fresh id has no manifest in the state directory.
        let worktree = Worktree {
            id: format!("existing-status-{}-{nanos}", std::process::id()),
            path: directory.path().to_owned(),
            branch: "main".to_owned(),
            head: String::new(),
            is_main: false,
            locked: false,
            bare: false,
            prunable: false,
        };
        assert!(has_config(&worktree));
        // `status` would resolve the Compose file through Docker and write a manifest first.
        assert!(existing_status(&worktree).unwrap().is_none());
        assert!(!manifest(&worktree).unwrap().path.exists());
    }

    #[test]
    fn logs_need_a_started_environment() {
        let directory = tempfile::tempdir().unwrap();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        // A fresh id has no manifest in the state directory.
        let worktree = Worktree {
            id: format!("logs-need-start-{}-{nanos}", std::process::id()),
            path: directory.path().to_owned(),
            branch: "main".to_owned(),
            head: String::new(),
            is_main: false,
            locked: false,
            bare: false,
            prunable: false,
        };
        let error = logs_args(&worktree).unwrap_err().to_string();
        assert!(error.contains("not been started"), "{error}");
        assert!(!manifest(&worktree).unwrap().path.exists());
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
