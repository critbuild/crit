use crate::sandbox::Scratch;
use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::ffi::{CString, OsStr, OsString};
use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentState {
    Unknown,
    Idle,
    Working,
    Blocked,
    Exited,
}

/// Unknown preserves the last identity; Clear is an authoritative no-chat selection.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", content = "identity", rename_all = "lowercase")]
pub enum ResumeUpdate {
    #[default]
    Unknown,
    Clear,
    Selected(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentUpdate {
    pub session: u64,
    pub state: AgentState,
    pub message: Option<String>,
    #[serde(default)]
    pub resume: ResumeUpdate,
}

pub struct IntegrationServer {
    pub socket_path: PathBuf,
    directory: PathBuf,
    stopping: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    /// Dup of the listener; `Drop` shuts it down to wake `accept` without the socket path.
    waker: Option<UnixListener>,
}

const MAX_FRAME: usize = 8192;
const READ_TIMEOUT: Duration = Duration::from_millis(250);
const ASSETS: &[(&str, &str)] = &[
    ("omp.ts", include_str!("../integrations/omp.ts")),
    ("opencode.js", include_str!("../integrations/opencode.js")),
    (
        "opencode-tui.js",
        include_str!("../integrations/opencode-tui.js"),
    ),
    ("reporter.js", include_str!("../integrations/reporter.js")),
    ("checks.js", include_str!("../integrations/checks.js")),
];

impl IntegrationServer {
    /// The window's private runtime folder (`crit-<pid>-<16 hex>`), which holds the socket and
    /// the agent CPU limit's start-up FIFOs, and whose name is unique to this run.
    pub fn directory(&self) -> &Path {
        &self.directory
    }
}

pub fn start(tx: SyncSender<crate::event::Event>) -> Result<IntegrationServer> {
    // An unusable XDG_RUNTIME_DIR (`su` without `-`, /run/user removed after logout) falls back
    // to the temp dir instead of ending Crit.
    let bases: Vec<PathBuf> = std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .into_iter()
        .chain([std::env::temp_dir()])
        .collect();
    start_in(tx, &bases)
}

/// The longest path a Unix socket can be bound at: `sun_path` holds 108 bytes, the last of
/// them the closing NUL.
const SOCKET_PATH_MAX: usize = 107;

/// Serve from the first base, in order, that can hold the private directory and its socket.
fn start_in(tx: SyncSender<crate::event::Event>, bases: &[PathBuf]) -> Result<IntegrationServer> {
    let mut random = [0u8; 8];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut random)?;
    let name = format!(
        "crit-{}-{:016x}",
        std::process::id(),
        u64::from_ne_bytes(random)
    );
    let mut created = None;
    for (index, base) in bases.iter().enumerate() {
        let directory = base.join(&name);
        let last = index + 1 == bases.len();
        // A base so deep that the socket's path would not fit could hold the directory, but
        // binding there would end Crit: the next base may be shorter.
        let socket = directory.join("agent.sock");
        if socket.as_os_str().len() > SOCKET_PATH_MAX {
            if last {
                bail!(
                    "the integration socket {} is longer than a Unix socket path can be ({SOCKET_PATH_MAX} bytes)",
                    socket.display()
                );
            }
            continue;
        }
        match DirBuilder::new().mode(0o700).create(&directory) {
            Ok(()) => {
                created = Some(directory);
                break;
            }
            Err(error) => {
                // An existing name is not ours; any other failure moves on to the next base.
                if error.kind() == std::io::ErrorKind::AlreadyExists || last {
                    return Err(error).with_context(|| {
                        format!(
                            "creating private integration directory {}",
                            directory.display()
                        )
                    });
                }
            }
        }
    }
    let directory = created.context("no directory for the integration socket")?;
    let socket_path = directory.join("agent.sock");
    let stopping = Arc::new(AtomicBool::new(false));
    let mut server = IntegrationServer {
        socket_path,
        directory,
        stopping,
        worker: None,
        waker: None,
    };
    fs::set_permissions(&server.directory, fs::Permissions::from_mode(0o700))?;
    let listener = UnixListener::bind(&server.socket_path).context("binding lifecycle socket")?;
    server.waker = Some(
        listener
            .try_clone()
            .context("duplicating lifecycle socket")?,
    );
    fs::set_permissions(&server.socket_path, fs::Permissions::from_mode(0o600))?;
    let stop = Arc::clone(&server.stopping);
    server.worker = Some(
        thread::Builder::new()
            .name("crit-integrations".into())
            .spawn(move || {
                let mut previous = HashMap::<u64, AgentUpdate>::new();
                while !stop.load(Ordering::Acquire) {
                    let mut stream = match listener.accept() {
                        Ok((stream, _)) => stream,
                        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                        // Descriptor or memory exhaustion is transient and accept reports it
                        // immediately, so back off rather than end the integration. Drop's unpark
                        // cuts the wait short and the loop condition then exits.
                        Err(error)
                            if matches!(
                                error.raw_os_error(),
                                Some(libc::EMFILE | libc::ENFILE | libc::ENOBUFS | libc::ENOMEM)
                            ) =>
                        {
                            thread::park_timeout(Duration::from_millis(100));
                            continue;
                        }
                        Err(_) => break,
                    };
                    if stop.load(Ordering::Acquire) {
                        break;
                    }
                    let Ok(update) = read_update(&mut stream) else {
                        continue;
                    };
                    let duplicate = previous.get(&update.session) == Some(&update);
                    if !duplicate {
                        let session = update.session;
                        let last = update.clone();
                        let mut event = crate::event::Event::Agent(update);
                        loop {
                            if stop.load(Ordering::Acquire) {
                                return;
                            }
                            match tx.try_send(event) {
                                Ok(()) => break,
                                Err(TrySendError::Disconnected(_)) => return,
                                Err(TrySendError::Full(value)) => {
                                    event = value;
                                    // Only channel backpressure wakes this wait; idle accept never polls.
                                    thread::park_timeout(Duration::from_millis(10));
                                }
                            }
                        }
                        // Bound deduplication memory even if a same-user client invents identities.
                        if previous.len() >= 4096 {
                            previous.clear();
                        }
                        previous.insert(session, last);
                    }
                    let _ = stream.set_write_timeout(Some(READ_TIMEOUT));
                    let _ = stream.write_all(b"\n");
                }
            })?,
    );
    Ok(server)
}

fn read_update(stream: &mut UnixStream) -> Result<AgentUpdate> {
    let deadline = Instant::now() + READ_TIMEOUT;
    let mut frame = [0u8; MAX_FRAME];
    let mut length = 0;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() || length == frame.len() {
            bail!("incomplete or oversized lifecycle frame");
        }
        stream.set_read_timeout(Some(remaining))?;
        let read = stream.read(&mut frame[length..])?;
        if read == 0 {
            bail!("lifecycle frame must end in a newline");
        }
        let start = length;
        length += read;
        if let Some(end) = frame[start..length].iter().position(|byte| *byte == b'\n') {
            let end = start + end;
            if end + 1 != length {
                bail!("one lifecycle frame is allowed per connection");
            }
            let update: AgentUpdate = serde_json::from_slice(&frame[..end])?;
            if update
                .message
                .as_ref()
                .is_some_and(|message| message.len() > 4096)
            {
                bail!("lifecycle message is too long");
            }
            if let ResumeUpdate::Selected(identity) = &update.resume
                && (identity.is_empty() || identity.len() > 4096 || identity.contains('\0'))
            {
                bail!("invalid resume identity");
            }
            return Ok(update);
        }
    }
}

impl Drop for IntegrationServer {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            worker.thread().unpark();
            // Wake our own blocking accept through the socket itself, since its path may be
            // gone; in-progress reads have a total deadline.
            if let Some(waker) = &self.waker {
                unsafe {
                    libc::shutdown(waker.as_raw_fd(), libc::SHUT_RDWR);
                }
            }
            let _ = worker.join();
        }
        let _ = fs::remove_file(&self.socket_path);
        let _ = fs::remove_dir(&self.directory);
    }
}

fn assets_directory() -> Result<PathBuf> {
    let base = match std::env::var_os("XDG_DATA_HOME").filter(|value| !value.is_empty()) {
        Some(value) => PathBuf::from(value),
        None => {
            PathBuf::from(std::env::var_os("HOME").context("HOME or XDG_DATA_HOME is required")?)
                .join(".local/share")
        }
    };
    if !base.is_absolute() {
        bail!("XDG_DATA_HOME must be absolute");
    }
    Ok(base.join("crit/integrations"))
}

fn managed_write(path: &Path, content: &[u8]) -> Result<()> {
    static NEXT_FILE: AtomicU64 = AtomicU64::new(0);
    if fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_file())
        && fs::read(path).is_ok_and(|existing| existing == content)
    {
        return Ok(());
    }
    let temporary = path.with_extension(format!(
        "tmp-{}-{}",
        std::process::id(),
        NEXT_FILE.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&temporary)
            .with_context(|| format!("writing managed integration {}", path.display()))?;
        file.write_all(content)?;
        // Concurrent pane launches must never observe a partially written hook/config.
        fs::rename(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

/// Install only Crit-owned assets. Never register hooks in user agent configuration.
pub fn install() -> Result<()> {
    let directory = assets_directory()?;
    fs::create_dir_all(directory.parent().context("integration parent")?)?;
    match DirBuilder::new().mode(0o700).create(&directory) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    let metadata = fs::symlink_metadata(&directory)?;
    if !metadata.is_dir() || metadata.uid() != unsafe { libc::geteuid() } {
        bail!("managed integration directory is not owned by this user");
    }
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
    for (name, content) in ASSETS {
        let path = directory.join(name);
        managed_write(&path, content.as_bytes())?;
    }
    Ok(())
}

fn parse_config(text: &str, source: &str) -> Result<Value> {
    let value = jsonc_parser::parse_to_serde_value(text, &Default::default())
        .with_context(|| format!("parsing {source}"))?
        .unwrap_or_else(|| json!({}));
    if !value.is_object() {
        bail!("{source} must contain a JSON object");
    }
    Ok(value)
}

fn add_plugin(config: &mut Value, plugin: &str) -> Result<()> {
    let plugins = config
        .as_object_mut()
        .context("plugin config object")?
        .entry("plugin")
        .or_insert_with(|| json!([]));
    let plugins = plugins
        .as_array_mut()
        .context("OpenCode plugin must be an array")?;
    if !plugins.iter().any(|entry| entry.as_str() == Some(plugin)) {
        plugins.push(Value::String(plugin.to_owned()));
    }
    Ok(())
}

fn file_url(path: &Path) -> Result<String> {
    let text = path.to_str().context("OpenCode plugin path is not UTF-8")?;
    let mut result = String::from("file://");
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"/-._~".contains(&byte) {
            result.push(char::from(byte));
        } else {
            use std::fmt::Write;
            write!(result, "%{byte:02X}")?;
        }
    }
    Ok(result)
}

fn relocate_tui_config(config: &mut Value, original: &Path) -> Result<()> {
    let base = original.parent().context("TUI config parent")?;
    // OpenCode resolves file plugin specs relative to the declaring config.
    for object in [config.pointer_mut("/tui")].into_iter().flatten() {
        if let Some(plugins) = object.get_mut("plugin").and_then(Value::as_array_mut) {
            relocate_plugins(plugins, base)?;
        }
    }
    if let Some(plugins) = config.get_mut("plugin").and_then(Value::as_array_mut) {
        relocate_plugins(plugins, base)?;
    }
    for pointer in ["/attention/sounds", "/tui/attention/sounds"] {
        if let Some(sounds) = config.pointer_mut(pointer).and_then(Value::as_object_mut) {
            for value in sounds.values_mut() {
                if let Some(sound) = value.as_str()
                    && !sound.is_empty()
                    && !Path::new(sound).is_absolute()
                    && !sound.starts_with("~/")
                {
                    *value = Value::String(base.join(sound).to_string_lossy().into_owned());
                }
            }
        }
    }
    Ok(())
}

fn relocate_plugins(plugins: &mut [Value], base: &Path) -> Result<()> {
    for plugin in plugins {
        let spec = if plugin.is_array() {
            plugin.get_mut(0)
        } else {
            Some(plugin)
        };
        if let Some(spec) = spec
            && let Some(text) = spec.as_str()
            && text.starts_with('.')
        {
            *spec = Value::String(file_url(&base.join(text))?);
        }
    }
    Ok(())
}

/// The chat a launched agent opens; no `Resume` starts a new one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resume<'a> {
    /// This exact chat: an OMP session file or an OpenCode session ID.
    Chat(&'a str),
    /// The agent's own `--continue`. OMP's picks the newest chat of the working folder;
    /// OpenCode's searches the whole project, so restore names that folder's chat instead.
    Continue,
}

/// What `command` launches: the program, its arguments, and the environment it adds.
pub type Launch = (String, Vec<String>, Vec<(String, String)>);

/// Return launch-scoped injection; inherited auth, profiles and plugin discovery stay intact.
/// `resume` is passed through unchecked: callers validate it first (`validate_resume`, plus
/// `opencode_chats` for OpenCode), because restore checks every chat up front. `scratch` is
/// the pane's directory in the sandbox, whose note the agent gets in its system prompt.
pub fn command(
    kind: &str,
    session: u64,
    socket: &Path,
    resume: Option<Resume<'_>>,
    scratch: Option<&Scratch>,
) -> Result<Launch> {
    if !matches!(kind, "omp" | "opencode") {
        bail!("unsupported agent {kind:?}; expected omp or opencode");
    }
    install()?;
    launch(kind, session, socket, resume, scratch, &assets_directory()?)
}

/// `command` for the assets installed in `directory`.
fn launch(
    kind: &str,
    session: u64,
    socket: &Path,
    resume: Option<Resume<'_>>,
    scratch: Option<&Scratch>,
    directory: &Path,
) -> Result<Launch> {
    let mut env = vec![
        ("CRIT_ENV".into(), "1".into()),
        ("CRIT_SESSION".into(), session.to_string()),
        (
            "CRIT_SOCKET".into(),
            socket
                .to_str()
                .context("socket path is not UTF-8")?
                .to_owned(),
        ),
        ("CRIT_AGENT_PID".into(), String::new()),
        ("CRIT_WINDOW_PID".into(), std::process::id().to_string()),
        (
            "CRIT_BIN".into(),
            std::env::current_exe()?
                .to_str()
                .context("Crit executable path is not UTF-8")?
                .to_owned(),
        ),
    ];
    if let Some(Resume::Chat(identity)) = resume {
        env.push(("CRIT_RESUME_ID".into(), identity.into()));
    }
    // OpenCode has no flag for it: Crit's plugin adds it to every request's system prompt.
    if let Some(scratch) = scratch {
        env.push(("CRIT_SANDBOX_NOTE".into(), scratch.note.clone()));
    }
    let mut args = Vec::new();
    if kind == "omp" {
        // Crit has already chosen the workspace; do not relocate home to /tmp.
        args.push("--allow-home".into());
        args.push("--extension".into());
        args.push(
            directory
                .join("omp.ts")
                .to_str()
                .context("extension path is not UTF-8")?
                .into(),
        );
        if let Some(scratch) = scratch {
            args.extend(["--append-system-prompt".into(), scratch.note.clone()]);
        }
        match resume {
            Some(Resume::Chat(identity)) => args.extend(["--resume".into(), identity.into()]),
            Some(Resume::Continue) => args.push("--continue".into()),
            None => {}
        }
        // Written before `PI_CONFIG_FILES` names it: OMP refuses to start without the file.
        managed_write(&directory.join(OMP_OVERLAY_FILE), OMP_OVERLAY.as_bytes())?;
    } else {
        match resume {
            Some(Resume::Chat(identity)) => args.extend(["--session".into(), identity.into()]),
            Some(Resume::Continue) => args.push("--continue".into()),
            None => {}
        }
        let mut config = parse_config(
            &std::env::var("OPENCODE_CONFIG_CONTENT").unwrap_or_else(|_| "{}".into()),
            "OPENCODE_CONFIG_CONTENT",
        )?;
        add_plugin(&mut config, &file_url(&directory.join("opencode.js"))?)?;
        env.push((
            "OPENCODE_CONFIG_CONTENT".into(),
            serde_json::to_string(&config)?,
        ));
        let mut tui = if let Some(path) =
            std::env::var_os("OPENCODE_TUI_CONFIG").filter(|value| !value.is_empty())
        {
            let path = std::path::absolute(path)?;
            let text = fs::read_to_string(&path)
                .with_context(|| format!("reading inherited TUI config {}", path.display()))?;
            let text = substitute_tui_config(&text, path.parent().context("TUI config parent")?)?;
            let mut value = parse_config(&text, "OPENCODE_TUI_CONFIG")?;
            relocate_tui_config(&mut value, &path)?;
            value
        } else {
            json!({})
        };
        // Flatten exactly as OpenCode does before appending our plugin, preserving nested plugin entries.
        if let Some(Value::Object(nested)) = tui
            .as_object_mut()
            .context("TUI config object")?
            .remove("tui")
        {
            for (key, value) in nested {
                tui.as_object_mut().unwrap().entry(key).or_insert(value);
            }
        }
        add_plugin(&mut tui, &file_url(&directory.join("opencode-tui.js"))?)?;
        // The source was already expanded once; protect literal tokens in resolved
        // values from a second substitution when OpenCode reads the managed file.
        let text = serde_json::to_string(&tui)?
            .replace("{env:", "\\u007benv:")
            .replace("{file:", "\\u007bfile:")
            .into_bytes();
        let hash = Sha256::digest(&text);
        let path = directory.join(format!("tui-{:x}.json", hash));
        managed_write(&path, &text)?;
        env.push((
            "OPENCODE_TUI_CONFIG".into(),
            path.to_str()
                .context("TUI config path is not UTF-8")?
                .into(),
        ));
    }
    env.extend(runtime_env(
        kind,
        directory,
        |name| std::env::var_os(name),
        gc_markers_supported,
    ));
    Ok((kind.into(), args, env))
}

/// The settings overlay that OMP panes get through `PI_CONFIG_FILES`, in the assets directory.
const OMP_OVERLAY_FILE: &str = "omp-overlay.yml";
/// Turns off what OMP sends that Crit's panes never show: the run-state spinner it animates in
/// the terminal title while it works, OSC 9;4 progress, and OSC 7501 program status (Crit
/// learns an agent's state from its own extension).
const OMP_OVERLAY: &str = "\
# Crit's OMP panes read this through PI_CONFIG_FILES: Crit never shows the terminal
# title, OSC 9;4 progress or OSC 7501 status, so OMP need not send them.
tui:
  titleState: false
terminal:
  showProgress: false
  programStatus: false
";
/// Bun forces a garbage collection every `BUN_GC_TIMER_INTERVAL` ms (1000 by default) while
/// the heap changes. JavaScriptCore's own timers still collect as memory fills.
const GC_TIMER: (&str, &str) = ("BUN_GC_TIMER_INTERVAL", "10000");
/// JavaScriptCore marks the heap on a thread per core, up to a cap, and those `HeapHelper`
/// threads wake more often than any other thread of a working agent.
const GC_MARKERS: (&str, &str) = ("BUN_JSC_numberOfGCMarkers", "2");
/// How an executable holds the `GC_MARKERS` option's name: JavaScriptCore's option table keeps
/// each name as a C string.
const GC_MARKERS_NAME: &[u8] = b"\0JSC_numberOfGCMarkers\0";
/// Bytes `contains` reads at a time.
const SCAN_CHUNK: usize = 256 * 1024;

/// What agent pane `kind` adds to its environment so that its runtime wakes the CPU less: a
/// longer Bun GC timer and fewer GC markers (an idle OMP woke 195 times a second; 33 with the
/// timer alone, 40 with the markers alone, 15 with both), and for OMP the overlay in
/// `directory` that turns off output Crit never shows. `inherited` looks a variable up in
/// Crit's own environment: a value set there is the user's, which the pane inherits as it is,
/// and overlays listed there stay after Crit's so that they win. `ships_gc_markers` says
/// whether the executable `kind` names knows the marker option; it is only asked when the
/// option would be added.
fn runtime_env(
    kind: &str,
    directory: &Path,
    inherited: impl Fn(&str) -> Option<OsString>,
    ships_gc_markers: impl FnOnce(&str) -> bool,
) -> Vec<(String, String)> {
    let mut env = Vec::new();
    if inherited(GC_TIMER.0).is_none() {
        env.push((GC_TIMER.0.to_owned(), GC_TIMER.1.to_owned()));
    }
    // Bun exits at startup ("invalid JSC environment variable") on a `BUN_JSC_` option its
    // JavaScriptCore does not know, and the options change between releases, so only an
    // executable that ships this one gets it. Everything the agent starts inherits it too, a
    // `bun test` say, which exits the same way if its own Bun lacks the option.
    if inherited(GC_MARKERS.0).is_none() && ships_gc_markers(kind) {
        env.push((GC_MARKERS.0.to_owned(), GC_MARKERS.1.to_owned()));
    }
    if kind == "omp" {
        // OMP applies the overlays in `PI_CONFIG_FILES` in order, each over those before. A
        // path with the list's `:` in it would split into files that do not exist, which OMP
        // refuses to start with, and a list that is not UTF-8 cannot be extended.
        let overlay = directory.join(OMP_OVERLAY_FILE);
        if let Some(overlay) = overlay.to_str().filter(|path| !path.contains(':')) {
            let files = match inherited("PI_CONFIG_FILES") {
                Some(theirs) if !theirs.is_empty() => theirs
                    .into_string()
                    .ok()
                    .map(|theirs| format!("{overlay}:{theirs}")),
                _ => Some(overlay.to_owned()),
            };
            env.extend(files.map(|files| ("PI_CONFIG_FILES".to_owned(), files)));
        }
    }
    env
}

/// Whether the `program` a pane spawns, found in Crit's `PATH`, ships the GC-marker option.
fn gc_markers_supported(program: &str) -> bool {
    std::env::var_os("PATH")
        .and_then(|path| find_in_path(program, &path))
        .is_some_and(|executable| knows_gc_markers(&executable))
}

/// The file a pane's spawn runs for `program`: the first executable of that name in the folders
/// of `path`. `None` without one, and when a relative folder comes first: the spawn resolves
/// that from the pane's own folder, which is not known here.
fn find_in_path(program: &str, path: &OsStr) -> Option<PathBuf> {
    for folder in std::env::split_paths(path) {
        if !folder.is_absolute() {
            return None;
        }
        let candidate = folder.join(program);
        let runnable = !candidate.is_dir()
            && CString::new(candidate.as_os_str().as_bytes())
                .is_ok_and(|name| unsafe { libc::access(name.as_ptr(), libc::X_OK) } == 0);
        if runnable {
            return Some(candidate);
        }
    }
    None
}

/// Whether the executable at `path` names the GC-marker option. Opening it follows symlinks,
/// such as OpenCode's `opencode` to `opencode.exe`. The answer is kept for the life of Crit per
/// file, by device, inode, size and modification time, so only the first launch of a binary
/// reads it, and a binary that an update replaces or rewrites is read again.
fn knows_gc_markers(path: &Path) -> bool {
    type Identity = (u64, u64, u64, i64, i64);
    static KNOWN: parking_lot::Mutex<Vec<(Identity, bool)>> = parking_lot::Mutex::new(Vec::new());
    // Non-blocking, so that a FIFO in its place cannot hang the launch; only a file is read.
    let Ok(mut file) = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
    else {
        return false;
    };
    let Ok(metadata) = file.metadata() else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    let identity = (
        metadata.dev(),
        metadata.ino(),
        metadata.size(),
        metadata.mtime(),
        metadata.mtime_nsec(),
    );
    let mut known = KNOWN.lock();
    if let Some(&(_, answer)) = known.iter().find(|(seen, _)| *seen == identity) {
        return answer;
    }
    // A read that fails is not remembered: the next launch tries again.
    let Ok(answer) = contains(&mut file, GC_MARKERS_NAME) else {
        return false;
    };
    known.push((identity, answer));
    answer
}

/// Whether `needle` occurs in what `reader` yields, read up to `SCAN_CHUNK` bytes at a time.
/// Each read is searched together with the last `needle.len() - 1` bytes before it, so that a
/// match across two reads is found too. JavaScriptCore's option names sit about a quarter
/// megabyte into Bun's executables, so a scan for one ends within the first two reads.
fn contains(mut reader: impl Read, needle: &[u8]) -> std::io::Result<bool> {
    let finder = memchr::memmem::Finder::new(needle);
    let overlap = needle.len().saturating_sub(1);
    let mut buffer = vec![0; SCAN_CHUNK];
    let mut filled = 0;
    loop {
        match reader.read(&mut buffer[filled..]) {
            Ok(0) => return Ok(false),
            Ok(read) => filled += read,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
        if finder.find(&buffer[..filled]).is_some() {
            return Ok(true);
        }
        let kept = filled.min(overlap);
        buffer.copy_within(filled - kept..filled, 0);
        filled = kept;
    }
}

/// Refuses a saved chat identity that cannot be resumed. For OMP it also reads the chat
/// file's header. OpenCode keeps chats in its database, which `opencode_chats` checks for
/// all of them at once.
pub fn validate_resume(kind: &str, identity: &str) -> Result<()> {
    use std::io::BufRead;
    if !valid_identity(identity) {
        bail!("invalid {kind} resume identity");
    }
    if kind == "omp" {
        let path = Path::new(identity);
        if !path.is_absolute() {
            bail!("OMP resume target must be an absolute session file path");
        }
        let file =
            fs::File::open(path).with_context(|| format!("opening saved OMP chat {identity}"))?;
        if !file.metadata()?.is_file() {
            bail!("saved OMP chat is not a regular file: {identity}");
        }
        // OMP treats missing/empty/invalid-header files as new chats. Refuse that
        // fallback; inspect only the header, never copy the conversation.
        let mut reader = std::io::BufReader::new(file).take(1024 * 1024);
        let mut line = String::new();
        reader.read_line(&mut line)?;
        let mut header: Value = serde_json::from_str(&line)
            .with_context(|| format!("reading saved OMP chat header {identity}"))?;
        // Current OMP puts a fixed title slot before the header; legacy files
        // start directly with the header. Match its title-slot discriminator.
        if header["type"] == "title"
            && header["v"] == 1
            && header["title"].is_string()
            && header["updatedAt"].is_string()
            && header["pad"].is_string()
            && (header.get("source").is_none()
                || matches!(header["source"].as_str(), Some("auto" | "user")))
        {
            line.clear();
            reader.read_line(&mut line)?;
            header = serde_json::from_str(&line)
                .with_context(|| format!("reading saved OMP chat header {identity}"))?;
        }
        // OMP's header predicate requires only type/id; cwd/version were optional
        // in older journals and are filled by its migration layer.
        if header["type"] != "session" || !header["id"].is_string() {
            bail!("saved OMP chat has no valid session header: {identity}");
        }
    }
    Ok(())
}

fn valid_identity(identity: &str) -> bool {
    !identity.is_empty() && identity.len() <= 4096 && !identity.contains('\0')
}

/// What a restore needs from OpenCode's database. One query answers all of it: the `opencode db`
/// CLI takes about 0.6 s to start, so one call per chat would freeze a restore for seconds.
#[derive(Debug, Default)]
pub struct OpencodeChats {
    /// The saved chat identities that still exist.
    pub existing: HashSet<String>,
    /// Folder → its newest top-level, unarchived chat that no saved identity names.
    pub latest: HashMap<String, String>,
}

impl OpencodeChats {
    /// Records one row of the query: saved chats come back without a folder.
    fn record(&mut self, id: String, folder: Option<String>) {
        match folder {
            Some(folder) => {
                self.latest.insert(folder, id);
            }
            None => {
                self.existing.insert(id);
            }
        }
    }
}

/// Looks up which saved chats `ids` exist, and the newest chat of each of `folders`, whose
/// panes remember none. A value that cannot be an identity or folder is never asked about.
///
/// The query runs against OpenCode's SQLite file directly: the `opencode db` CLI boots the
/// whole app (0.6 s warm) to run a query that takes 0.3 ms, and a restore draws nothing until
/// this returns. The CLI stays as the fallback for whatever the direct read cannot answer: no
/// database at the expected path, one that is locked, malformed or lacks what the query
/// names, or one that `OPENCODE_DB` moves.
pub fn opencode_chats(ids: &[&str], folders: &[&str]) -> Result<OpencodeChats> {
    let mut chats = OpencodeChats::default();
    let Some(query) = opencode_chat_query(ids, folders) else {
        return Ok(chats);
    };
    if let Some(found) =
        opencode_database().and_then(|database| opencode_chats_sqlite(&database, &query))
    {
        return Ok(found);
    }
    // OpenCode's missing-session TUI route falls back to Home, so a missing chat must be
    // caught here. The query returns only the IDs that exist.
    let output = std::process::Command::new("opencode")
        .args(["db", "--format", "json", &query])
        .output()
        .context("checking saved OpenCode chats")?;
    if !output.status.success() {
        bail!("could not check saved OpenCode chats");
    }
    let rows: Value =
        serde_json::from_slice(&output.stdout).context("reading OpenCode chat existence result")?;
    for row in rows
        .as_array()
        .context("OpenCode chat existence result is not a list")?
    {
        let Some(id) = row["id"].as_str() else {
            continue;
        };
        chats.record(id.to_owned(), row["folder"].as_str().map(str::to_owned));
    }
    Ok(chats)
}

/// OpenCode's database: `opencode/opencode.db` under `$XDG_DATA_HOME`, else `~/.local/share`.
/// `None` when `OPENCODE_DB` moves it or there is no `$HOME`: only the CLI resolves those.
fn opencode_database() -> Option<PathBuf> {
    let set = |name: &str| std::env::var_os(name).filter(|value| !value.is_empty());
    if set("OPENCODE_DB").is_some() {
        return None;
    }
    let data = match set("XDG_DATA_HOME") {
        Some(data) => PathBuf::from(data),
        None => PathBuf::from(set("HOME")?).join(".local/share"),
    };
    Some(data.join("opencode/opencode.db"))
}

/// `query` run against the SQLite file `database`, or `None` for any reason it cannot be: no
/// file, a lock held past 250 ms, not a database, or a table or column the query names missing.
/// OpenCode keeps the file in WAL mode and writes it while crit restores, so it is opened
/// read-only but not `immutable`, which would ignore the WAL and miss its newest chats.
fn opencode_chats_sqlite(database: &Path, query: &str) -> Option<OpencodeChats> {
    let connection = Connection::open_with_flags(
        database,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()?;
    connection.busy_timeout(Duration::from_millis(250)).ok()?;
    let mut statement = connection.prepare(query).ok()?;
    let mut rows = statement.query([]).ok()?;
    let mut chats = OpencodeChats::default();
    while let Some(row) = rows.next().ok()? {
        chats.record(row.get(0).ok()?, row.get(1).ok()?);
    }
    Some(chats)
}

/// `None` when nothing qualifies. Saved `ids` come back with a NULL folder, then each folder
/// with its newest top-level, unarchived chat other than those `ids`, which their own panes
/// reopen. Each valid value appears once, quoted for SQL.
fn opencode_chat_query(ids: &[&str], folders: &[&str]) -> Option<String> {
    let ids = sql_list(ids);
    let folders = sql_list(folders);
    let saved = (!ids.is_empty())
        .then(|| format!("SELECT id, NULL AS folder FROM session WHERE id IN ({ids})"));
    if folders.is_empty() {
        return saved;
    }
    let unclaimed = if ids.is_empty() {
        String::new()
    } else {
        format!(" AND id NOT IN ({ids})")
    };
    let latest = format!(
        "SELECT id, directory AS folder FROM (SELECT id, directory, row_number() OVER \
         (PARTITION BY directory ORDER BY time_updated DESC) AS position FROM session \
         WHERE directory IN ({folders}) AND parent_id IS NULL AND time_archived IS NULL\
         {unclaimed}) WHERE position = 1"
    );
    Some(match saved {
        Some(saved) => format!("{saved} UNION ALL {latest}"),
        None => latest,
    })
}

/// The valid `values`, each once, as a quoted SQL list.
fn sql_list(values: &[&str]) -> String {
    let mut seen = HashSet::new();
    values
        .iter()
        .copied()
        .filter(|value| valid_identity(value) && seen.insert(*value))
        .map(|value| format!("'{}'", value.replace('\'', "''")))
        .collect::<Vec<_>>()
        .join(",")
}

/// OpenCode's subcommands; with one, `opencode` is not running its interactive TUI.
const OPENCODE_COMMANDS: &[&str] = &[
    "acp",
    "agent",
    "attach",
    "auth",
    "completion",
    "db",
    "debug",
    "export",
    "github",
    "import",
    "mcp",
    "models",
    "plug",
    "plugin",
    "pr",
    "providers",
    "run",
    "serve",
    "session",
    "stats",
    "uninstall",
    "upgrade",
    "web",
];

/// The agent whose interactive TUI runs in process group `group`, so the shell pane running
/// it can reopen as that agent. Every member counts, because launchers such as npm's
/// `opencode` wrapper run the real binary as their child.
pub fn adopted_agent(group: libc::pid_t) -> Option<&'static str> {
    let mut cmdline = Vec::new();
    fs::read_dir("/proc").ok()?.flatten().find_map(|entry| {
        let pid: libc::pid_t = entry.file_name().to_str()?.parse().ok()?;
        if unsafe { libc::getpgid(pid) } != group {
            return None;
        }
        cmdline.clear();
        fs::File::open(entry.path().join("cmdline"))
            .and_then(|mut file| file.read_to_end(&mut cmdline))
            .ok()?;
        let argv = cmdline
            .strip_suffix(&[0])
            .unwrap_or(&cmdline)
            .split(|byte| *byte == 0)
            .map(std::str::from_utf8)
            .collect::<Result<Vec<_>, _>>()
            .ok()?;
        agent_tui(&argv)
    })
}

/// `"opencode"` or `"omp"` when `argv` starts that agent's interactive TUI: OpenCode without a
/// subcommand or `--mini`, OMP without positional arguments such as a prompt.
fn agent_tui(argv: &[&str]) -> Option<&'static str> {
    let (program, args) = argv.split_first()?;
    match program.rsplit('/').next() {
        Some("opencode")
            if !args
                .iter()
                .any(|arg| *arg == "--mini" || OPENCODE_COMMANDS.contains(arg)) =>
        {
            Some("opencode")
        }
        Some("omp") if args.iter().all(|arg| arg.starts_with('-')) => Some("omp"),
        _ => None,
    }
}

fn substitute_tui_config(text: &str, base: &Path) -> Result<String> {
    // Match OpenCode's pre-JSONC expansion while retaining the original config's
    // directory. Serializing afterwards safely embeds the resolved TUI values.
    let mut expanded = String::with_capacity(text.len());
    let mut remaining = text;
    while let Some(start) = remaining.find("{env:") {
        let token = &remaining[start + 5..];
        let Some(end) = token.find('}') else { break };
        expanded.push_str(&remaining[..start]);
        expanded.push_str(&std::env::var(&token[..end]).unwrap_or_default());
        remaining = &token[end + 1..];
    }
    expanded.push_str(remaining);
    let mut result = String::with_capacity(expanded.len());
    let mut offset = 0;
    while let Some(relative) = expanded[offset..].find("{file:") {
        let start = offset + relative;
        result.push_str(&expanded[offset..start]);
        let Some(end) = expanded[start..].find('}').map(|end| start + end) else {
            result.push_str(&expanded[start..]);
            return Ok(result);
        };
        let line_start = expanded[..start].rfind('\n').map_or(0, |index| index + 1);
        if expanded[line_start..start].trim_start().starts_with("//") {
            result.push_str(&expanded[start..=end]);
        } else {
            let name = &expanded[start + 6..end];
            let path = if let Some(name) = name.strip_prefix("~/") {
                PathBuf::from(
                    std::env::var_os("HOME").context("HOME is required by TUI file reference")?,
                )
                .join(name)
            } else {
                base.join(name)
            };
            // OpenCode's TUI config loader treats unavailable file references as empty.
            let content = fs::read_to_string(path).unwrap_or_default();
            let escaped = serde_json::to_string(content.trim())?;
            result.push_str(&escaped[1..escaped.len() - 1]);
        }
        offset = end + 1;
    }
    result.push_str(&expanded[offset..]);
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::ffi::OsStringExt;
    use std::sync::mpsc;

    fn send(socket: &Path, frame: &[u8]) -> bool {
        let mut client = UnixStream::connect(socket).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        client.write_all(frame).unwrap();
        let mut ack = [0];
        client.read_exact(&mut ack).is_ok() && ack == *b"\n"
    }

    #[test]
    fn agents_are_told_where_their_scratch_goes() {
        // `launch` is `command` without installing the assets in the real data folder.
        let assets = tempfile::tempdir().unwrap();
        let socket = assets.path().join("agent.sock");
        let scratch = Scratch {
            dir: PathBuf::from("/var/tmp/crit-u/app-0123456789abcdef01234567"),
            note: "Scratch space: under $TMPDIR (/var/tmp/crit-u/app-0123456789abcdef01234567)."
                .into(),
        };
        let note = ("CRIT_SANDBOX_NOTE".to_owned(), scratch.note.clone());
        let (program, args, env) =
            launch("omp", 7, &socket, None, Some(&scratch), assets.path()).unwrap();
        assert_eq!(program, "omp");
        let flag = args
            .iter()
            .position(|arg| arg == "--append-system-prompt")
            .expect("OMP gets the note as a system-prompt addition");
        assert_eq!(args[flag + 1], scratch.note);
        assert!(env.contains(&note), "{env:?}");

        let (program, args, env) =
            launch("opencode", 7, &socket, None, Some(&scratch), assets.path()).unwrap();
        assert_eq!(program, "opencode");
        let flagged = |args: &[String]| args.iter().any(|arg| arg == "--append-system-prompt");
        assert!(!flagged(&args), "{args:?}");
        assert!(env.contains(&note), "{env:?}");

        // Without a sandbox there is nothing to say.
        let (_, args, env) = launch("omp", 7, &socket, None, None, assets.path()).unwrap();
        let noted = env.iter().any(|(name, _)| name == "CRIT_SANDBOX_NOTE");
        assert!(!flagged(&args) && !noted, "{args:?} {env:?}");
    }

    /// `runtime_env` for agent `kind` with its assets in `directory`, given Crit's environment
    /// as `pairs` and whether the agent's executable ships the GC-marker option.
    fn runtime(
        kind: &str,
        directory: &str,
        pairs: &[(&str, &str)],
        ships: bool,
    ) -> HashMap<String, String> {
        let inherited = |name: &str| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| OsString::from(value))
        };
        runtime_env(kind, Path::new(directory), inherited, |_| ships)
            .into_iter()
            .collect()
    }

    #[test]
    fn agents_get_runtime_settings_unless_crits_environment_has_them() {
        let assets = "/data/crit/integrations";
        for kind in ["omp", "opencode"] {
            let env = runtime(kind, assets, &[], true);
            assert_eq!(env["BUN_GC_TIMER_INTERVAL"], "10000");
            assert_eq!(env["BUN_JSC_numberOfGCMarkers"], "2");
            // Bun exits on a JavaScriptCore option it lacks: only one that ships it gets it.
            let env = runtime(kind, assets, &[], false);
            assert_eq!(env["BUN_GC_TIMER_INTERVAL"], "10000");
            assert!(!env.contains_key("BUN_JSC_numberOfGCMarkers"), "{env:?}");
            // The executable asked about is the agent's own...
            runtime_env(
                kind,
                Path::new(assets),
                |_| None,
                |program| {
                    assert_eq!(program, kind);
                    true
                },
            );
            // ...and it is not even read when the user set the option: their values stand.
            let user = |name: &str| name.starts_with("BUN_").then(|| OsString::from("1"));
            let env = runtime_env(kind, Path::new(assets), user, |_| panic!("asked"));
            assert!(
                env.iter().all(|(name, _)| !name.starts_with("BUN_")),
                "{env:?}"
            );
        }
        // Only OMP gets the overlay. Overlays the user lists come after Crit's, so theirs win.
        let overlay = "/data/crit/integrations/omp-overlay.yml";
        let files = |kind: &str, pairs: &[(&str, &str)]| {
            runtime(kind, assets, pairs, false).remove("PI_CONFIG_FILES")
        };
        assert_eq!(files("omp", &[]).as_deref(), Some(overlay));
        assert_eq!(files("opencode", &[]), None);
        let theirs = [("PI_CONFIG_FILES", "/home/u/a.yml:b.yml")];
        assert_eq!(
            files("omp", &theirs),
            Some(format!("{overlay}:/home/u/a.yml:b.yml"))
        );
        assert_eq!(files("opencode", &theirs), None);
        assert_eq!(
            files("omp", &[("PI_CONFIG_FILES", "")]).as_deref(),
            Some(overlay)
        );
        // A list Crit cannot extend stays the user's, and a path the list cannot hold is left
        // out.
        let unreadable = |name: &str| {
            (name == "PI_CONFIG_FILES").then(|| OsString::from_vec(b"/\xff.yml".to_vec()))
        };
        let env = runtime_env("omp", Path::new(assets), unreadable, |_| false);
        assert!(
            env.iter().all(|(name, _)| name != "PI_CONFIG_FILES"),
            "{env:?}"
        );
        assert!(!runtime("omp", "/data:crit", &[], false).contains_key("PI_CONFIG_FILES"));
    }

    #[test]
    fn omp_panes_get_an_overlay_that_turns_off_what_crit_never_shows() {
        let assets = tempfile::tempdir().unwrap();
        let socket = assets.path().join("agent.sock");
        let overlay = assets.path().join("omp-overlay.yml");
        let listed = |env: &[(String, String)]| {
            env.iter()
                .find(|(name, _)| name == "PI_CONFIG_FILES")
                .map(|(_, files)| files.clone())
        };
        let (_, _, env) = launch("opencode", 7, &socket, None, None, assets.path()).unwrap();
        assert_eq!(listed(&env), None);
        assert!(!overlay.exists());
        let (_, _, env) = launch("omp", 7, &socket, None, None, assets.path()).unwrap();
        // Crit's own environment may list overlays as well, inside an OMP pane say: they follow.
        let files = listed(&env).expect("OMP gets the overlay");
        assert_eq!(files.split(':').next(), overlay.to_str());
        assert!(fs::symlink_metadata(&overlay).unwrap().is_file());
    }

    #[test]
    fn agents_are_found_in_path_as_their_spawn_finds_them() {
        let temp = tempfile::tempdir().unwrap();
        let folders = ["plain", "folder", "runnable"].map(|name| temp.path().join(name));
        for folder in &folders {
            fs::create_dir(folder).unwrap();
        }
        let [plain, folder, runnable] = &folders;
        // A file that may not run, or a folder of the name, is not what a spawn runs.
        fs::write(plain.join("omp"), "").unwrap();
        fs::create_dir(folder.join("omp")).unwrap();
        fs::write(runnable.join("omp"), "").unwrap();
        fs::set_permissions(runnable.join("omp"), fs::Permissions::from_mode(0o755)).unwrap();
        let path = std::env::join_paths(&folders).unwrap();
        assert_eq!(find_in_path("omp", &path), Some(runnable.join("omp")));
        assert_eq!(find_in_path("opencode", &path), None);
        // A relative folder resolves from the pane's own folder, which is not known here.
        let path = std::env::join_paths([Path::new("bin"), runnable.as_path()]).unwrap();
        assert_eq!(find_in_path("omp", &path), None);
        let path = std::env::join_paths([runnable.as_path(), Path::new("bin")]).unwrap();
        assert_eq!(find_in_path("omp", &path), Some(runnable.join("omp")));
    }

    #[test]
    fn executables_are_read_for_the_gc_marker_option_until_they_change() {
        let temp = tempfile::tempdir().unwrap();
        let agent = temp.path().join("agent");
        // Across the end of the first read, the name is only found with the bytes kept from it.
        let start = SCAN_CHUNK - 10;
        let mut bytes = vec![b'x'; SCAN_CHUNK + 64];
        bytes[start..start + GC_MARKERS_NAME.len()].copy_from_slice(GC_MARKERS_NAME);
        fs::write(&agent, &bytes).unwrap();
        assert!(knows_gc_markers(&agent));
        // The answer stands while the file keeps its device, inode, size and modification
        // time...
        let modified = fs::metadata(&agent).unwrap().modified().unwrap();
        bytes[start] = b'x';
        fs::write(&agent, &bytes).unwrap();
        let file = OpenOptions::new().write(true).open(&agent).unwrap();
        file.set_modified(modified).unwrap();
        assert!(
            knows_gc_markers(&agent),
            "an unchanged file is not read again"
        );
        // ...and a file that changed, if only by a nanosecond, is read again.
        file.set_modified(modified + Duration::from_nanos(1))
            .unwrap();
        assert!(!knows_gc_markers(&agent));
        // Another option, or the name without its NULs, is not the option.
        fs::write(
            &agent,
            b"\0JSC_numberOfGCMarkersMax\0JSC_numberOfGCMarkers numberOfGCMarkers\0",
        )
        .unwrap();
        assert!(!knows_gc_markers(&agent));
        // Neither a missing file nor a FIFO, which must not block the launch, is read.
        assert!(!knows_gc_markers(&temp.path().join("missing")));
        let fifo = temp.path().join("fifo");
        let name = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o700) }, 0);
        assert!(!knows_gc_markers(&fifo));
    }

    #[test]
    fn a_name_split_across_reads_is_found() {
        /// Yields at most five bytes a read, so that every match spans reads.
        struct Trickle<'a>(&'a [u8]);
        impl Read for Trickle<'_> {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                let count = buffer.len().min(self.0.len()).min(5);
                buffer[..count].copy_from_slice(&self.0[..count]);
                self.0 = &self.0[count..];
                Ok(count)
            }
        }
        for offset in 0..10 {
            let mut bytes = vec![b'x'; offset];
            bytes.extend_from_slice(GC_MARKERS_NAME);
            assert!(
                contains(Trickle(&bytes), GC_MARKERS_NAME).unwrap(),
                "{offset}"
            );
            bytes.pop();
            assert!(
                !contains(Trickle(&bytes), GC_MARKERS_NAME).unwrap(),
                "{offset}"
            );
        }
    }

    #[test]
    fn identity_only_updates_and_authoritative_clear_are_not_duplicates() {
        let (tx, rx) = mpsc::sync_channel(4);
        let server = start(tx).unwrap();
        for resume in [
            json!({"status": "selected", "identity": "/sessions/first.jsonl"}),
            json!({"status": "selected", "identity": "/sessions/second.jsonl"}),
            json!({"status": "clear"}),
            json!({"status": "unknown"}),
        ] {
            let mut frame = serde_json::to_vec(&json!({
                "session": 7, "state": "idle", "message": null, "resume": resume,
            }))
            .unwrap();
            frame.push(b'\n');
            assert!(send(&server.socket_path, &frame));
            let crate::event::Event::Agent(update) =
                rx.recv_timeout(Duration::from_secs(1)).unwrap()
            else {
                panic!("expected identity update");
            };
            assert_eq!(serde_json::to_value(update.resume).unwrap(), resume);
            assert!(send(&server.socket_path, &frame));
            assert!(rx.try_recv().is_err());
        }
    }

    #[test]
    fn omp_restore_refuses_missing_empty_and_invalid_session_files() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("exact chat ' name.jsonl");
        let identity = file.to_str().unwrap();
        assert!(validate_resume("omp", identity).is_err());
        fs::write(&file, "").unwrap();
        assert!(validate_resume("omp", identity).is_err());
        fs::write(&file, "{\"type\":\"message\"}\n").unwrap();
        assert!(validate_resume("omp", identity).is_err());
        fs::write(
            &file,
            "{\"type\":\"session\",\"version\":3,\"id\":\"exact\",\"cwd\":\"/tmp\"}\n",
        )
        .unwrap();
        assert!(validate_resume("omp", identity).is_ok());
        fs::write(&file, "{\"type\":\"title\",\"v\":1,\"title\":\"Chat\",\"updatedAt\":\"2026-10-11\",\"pad\":\"\"}\n{\"type\":\"session\",\"id\":\"legacy\"}\n").unwrap();
        assert!(validate_resume("omp", identity).is_ok());
        assert!(validate_resume("omp", "relative.jsonl").is_err());
    }

    #[test]
    fn opencode_batch_query_escapes_and_dedupes() {
        assert_eq!(
            opencode_chat_query(&["ses_a", "it's", "ses_a", "", "nul\0", "ses_b"], &[]).as_deref(),
            Some("SELECT id, NULL AS folder FROM session WHERE id IN ('ses_a','it''s','ses_b')")
        );
        // Folders without a remembered chat ask for their newest chat no saved pane names.
        assert_eq!(
            opencode_chat_query(&["ses_a"], &["/w/it's", "/w/b", "/w/it's", ""]).as_deref(),
            Some(
                "SELECT id, NULL AS folder FROM session WHERE id IN ('ses_a') UNION ALL \
                 SELECT id, directory AS folder FROM (SELECT id, directory, row_number() OVER \
                 (PARTITION BY directory ORDER BY time_updated DESC) AS position FROM session \
                 WHERE directory IN ('/w/it''s','/w/b') AND parent_id IS NULL AND \
                 time_archived IS NULL AND id NOT IN ('ses_a')) WHERE position = 1"
            )
        );
        assert_eq!(
            opencode_chat_query(&[], &["/w/b"]).as_deref(),
            Some(
                "SELECT id, directory AS folder FROM (SELECT id, directory, row_number() OVER \
                 (PARTITION BY directory ORDER BY time_updated DESC) AS position FROM session \
                 WHERE directory IN ('/w/b') AND parent_id IS NULL AND time_archived IS NULL) \
                 WHERE position = 1"
            )
        );
        assert_eq!(opencode_chat_query(&[], &[]), None);
        assert_eq!(
            opencode_chat_query(&["", &"x".repeat(4097)], &["nul\0"]),
            None
        );
        // Nothing to ask means no `opencode` call at all.
        let chats = opencode_chats(&[], &[]).unwrap();
        assert!(chats.existing.is_empty() && chats.latest.is_empty());
        // Resume validation no longer asks OpenCode; only the identity is checked here.
        assert!(validate_resume("opencode", "ses_a").is_ok());
        assert!(validate_resume("opencode", "").is_err());
    }

    #[test]
    fn opencode_chats_sqlite_reads_a_session_table() {
        let temp = tempfile::tempdir().unwrap();
        let database = temp.path().join("opencode.db");
        // OpenCode keeps the file in WAL mode and commits while crit restores. This writer
        // stays open, so its rows sit in the WAL, which an `immutable` read would not see.
        let writer = Connection::open(&database).unwrap();
        writer
            .execute_batch(
                "PRAGMA journal_mode = WAL; \
                 CREATE TABLE session (id TEXT PRIMARY KEY, directory TEXT, parent_id TEXT, \
                 time_archived INTEGER, time_updated INTEGER); \
                 INSERT INTO session VALUES \
                 ('ses_saved', '/w/a', NULL, NULL, 300), \
                 ('ses_old', '/w/a', NULL, NULL, 100), \
                 ('ses_new', '/w/a', NULL, NULL, 200), \
                 ('ses_archived', '/w/a', NULL, 1, 400), \
                 ('ses_child', '/w/a', 'ses_new', NULL, 500), \
                 ('ses_elsewhere', '/w/b', NULL, NULL, 600)",
            )
            .unwrap();
        let query = opencode_chat_query(&["ses_saved", "ses_gone"], &["/w/a"]).unwrap();
        let chats = opencode_chats_sqlite(&database, &query).unwrap();
        // Only the saved chat that exists is reported. The folder's newest chat is neither
        // that one (its own pane reopens it) nor an archived, child or other folder's chat.
        assert_eq!(chats.existing, HashSet::from(["ses_saved".to_owned()]));
        assert_eq!(
            chats.latest,
            HashMap::from([("/w/a".to_owned(), "ses_new".to_owned())])
        );
        writer
            .execute_batch("INSERT INTO session VALUES ('ses_next', '/w/a', NULL, NULL, 700)")
            .unwrap();
        let chats = opencode_chats_sqlite(&database, &query).unwrap();
        assert_eq!(chats.latest["/w/a"], "ses_next");
        // After a restart OpenCode is not running: no writer, no WAL beside the file.
        drop(writer);
        let chats = opencode_chats_sqlite(&database, &query).unwrap();
        assert_eq!(chats.latest["/w/a"], "ses_next");
    }

    #[test]
    fn opencode_chats_falls_back_when_the_database_is_unusable() {
        let temp = tempfile::tempdir().unwrap();
        let query = opencode_chat_query(&["ses_a"], &["/w/a"]).unwrap();
        let read = |name: &str| opencode_chats_sqlite(&temp.path().join(name), &query);
        assert!(read("missing.db").is_none());
        // Looking for the file must not create it.
        assert!(!temp.path().join("missing.db").exists());
        fs::write(
            temp.path().join("garbage.db"),
            "not a database ".repeat(1000),
        )
        .unwrap();
        assert!(read("garbage.db").is_none());
        // A database that is not OpenCode's, and one whose `session` lacks a column the query
        // names.
        Connection::open(temp.path().join("other.db"))
            .unwrap()
            .execute_batch("CREATE TABLE project (id TEXT)")
            .unwrap();
        assert!(read("other.db").is_none());
        Connection::open(temp.path().join("old.db"))
            .unwrap()
            .execute_batch("CREATE TABLE session (id TEXT, directory TEXT)")
            .unwrap();
        assert!(read("old.db").is_none());
    }

    #[test]
    fn only_interactive_agent_tuis_are_adopted() {
        assert_eq!(agent_tui(&["opencode"]), Some("opencode"));
        assert_eq!(
            agent_tui(&["/bin/opencode", "-s", "ses_a", "-m", "p/m", "."]),
            Some("opencode")
        );
        assert_eq!(agent_tui(&["opencode", "run", "hi"]), None);
        assert_eq!(
            agent_tui(&["opencode", "--log-level", "INFO", "serve"]),
            None
        );
        assert_eq!(agent_tui(&["opencode", "--mini"]), None);
        // npm's launcher is not the agent; its child, the real binary, is.
        assert_eq!(
            agent_tui(&["node", "/lib/node_modules/opencode-ai/bin/opencode"]),
            None
        );
        assert_eq!(agent_tui(&["omp"]), Some("omp"));
        assert_eq!(agent_tui(&["/usr/bin/omp", "--continue"]), Some("omp"));
        assert_eq!(agent_tui(&["omp", "update"]), None);
        assert_eq!(agent_tui(&["omp", "-p", "a prompt"]), None);
        assert_eq!(agent_tui(&["fish"]), None);
        assert_eq!(agent_tui(&[]), None);
    }

    #[test]
    fn any_member_of_the_foreground_group_can_be_the_agent() {
        use std::os::unix::process::CommandExt;
        let temp = tempfile::tempdir().unwrap();
        let agent = temp.path().join("opencode");
        std::os::unix::fs::symlink("/bin/sleep", &agent).unwrap();
        // A launcher whose child is the agent, as npm's wrapper is.
        let mut launcher = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(format!("'{}' 60 & wait", agent.display()))
            .process_group(0)
            .spawn()
            .unwrap();
        let mut plain = std::process::Command::new("/bin/sleep")
            .arg("60")
            .process_group(0)
            .spawn()
            .unwrap();
        let group = launcher.id() as libc::pid_t;
        let deadline = Instant::now() + Duration::from_secs(5);
        while adopted_agent(group).is_none() {
            assert!(
                Instant::now() < deadline,
                "the launched agent was never found"
            );
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(adopted_agent(group), Some("opencode"));
        assert_eq!(adopted_agent(plain.id() as libc::pid_t), None);
        unsafe { libc::kill(-group, libc::SIGKILL) };
        let _ = plain.kill();
        let _ = launcher.wait();
        let _ = plain.wait();
    }

    #[test]
    fn malformed_clients_and_duplicates_cannot_replace_another_session() {
        let (tx, rx) = mpsc::sync_channel(4);
        let server = start(tx).unwrap();
        let path = server.socket_path.clone();
        let directory = server.directory.clone();
        assert_eq!(
            fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(!send(&path, b"{\"session\":7,\"state\":\"bogus\"}\n"));
        assert!(!send(&path, b"{\"session\":\"7\",\"state\":\"idle\"}\n"));
        assert!(rx.try_recv().is_err());
        let first = b"{\"session\":18446744073709551615,\"state\":\"working\",\"message\":null}\n";
        assert!(send(&path, first));
        let crate::event::Event::Agent(update) = rx.recv_timeout(Duration::from_secs(1)).unwrap()
        else {
            panic!("expected lifecycle update");
        };
        assert_eq!(update.session, u64::MAX);
        assert_eq!(update.state, AgentState::Working);
        assert!(send(&path, first));
        assert!(rx.try_recv().is_err());
        assert!(send(
            &path,
            b"{\"session\":7,\"state\":\"blocked\",\"message\":\"Approval\"}\n"
        ));
        let crate::event::Event::Agent(update) = rx.recv_timeout(Duration::from_secs(1)).unwrap()
        else {
            panic!("expected lifecycle update");
        };
        assert_eq!(update.session, 7);
        assert_eq!(update.state, AgentState::Blocked);
        drop(server);
        assert!(!path.exists());
        assert!(!directory.exists());
    }

    #[test]
    fn unterminated_frame_is_bounded_and_drop_wakes_backpressure() {
        let (mut client, mut server) = UnixStream::pair().unwrap();
        client
            .write_all(b"{\"session\":1,\"state\":\"idle\"}")
            .unwrap();
        client.shutdown(std::net::Shutdown::Write).unwrap();
        assert!(read_update(&mut server).is_err());
        let (tx, _rx) = mpsc::sync_channel(0);
        let integration = start(tx).unwrap();
        let mut client = UnixStream::connect(&integration.socket_path).unwrap();
        client
            .write_all(b"{\"session\":1,\"state\":\"idle\"}\n")
            .unwrap();
        drop(integration);
    }

    #[test]
    fn drop_returns_after_socket_path_is_removed() {
        let (tx, _rx) = mpsc::sync_channel(1);
        let server = start(tx).unwrap();
        // An acked round trip proves the worker is back in its blocking accept.
        assert!(send(
            &server.socket_path,
            b"{\"session\":1,\"state\":\"idle\"}\n"
        ));
        let directory = server.directory.clone();
        fs::remove_file(&server.socket_path).unwrap();
        let (done, finished) = mpsc::channel();
        thread::spawn(move || {
            drop(server);
            let _ = done.send(());
        });
        finished
            .recv_timeout(Duration::from_secs(1))
            .expect("drop must not hang once the socket path is gone");
        assert!(!directory.exists());
    }

    #[test]
    fn start_in_skips_bases_that_cannot_hold_the_directory() {
        let temp = tempfile::tempdir().unwrap();
        let missing = PathBuf::from("/proc/crit-nonexistent");
        let (tx, _rx) = mpsc::sync_channel(1);
        let server = start_in(tx, &[missing.clone(), temp.path().to_path_buf()]).unwrap();
        assert!(server.socket_path.starts_with(temp.path()));
        assert!(UnixStream::connect(&server.socket_path).is_ok());
        let (tx, _rx) = mpsc::sync_channel(1);
        let error = start_in(tx, &[missing]).err().unwrap();
        assert!(format!("{error:#}").contains("/proc/crit-nonexistent"));
    }

    #[test]
    fn start_in_skips_bases_too_deep_for_the_socket_path() {
        let temp = tempfile::tempdir().unwrap();
        // Deep enough that `<base>/crit-<pid>-<16 hex>/agent.sock` passes 107 bytes.
        let deep = temp.path().join("d".repeat(SOCKET_PATH_MAX));
        fs::create_dir(&deep).unwrap();
        let (tx, _rx) = mpsc::sync_channel(1);
        let server = start_in(tx, &[deep.clone(), temp.path().to_path_buf()]).unwrap();
        assert!(server.socket_path.starts_with(temp.path()));
        assert!(!server.socket_path.starts_with(&deep));
        assert!(UnixStream::connect(&server.socket_path).is_ok());
        assert_eq!(
            fs::read_dir(&deep).unwrap().count(),
            0,
            "nothing is left in the deep base"
        );
        let (tx, _rx) = mpsc::sync_channel(1);
        let error = start_in(tx, &[deep]).err().unwrap();
        assert!(format!("{error:#}").contains("longer than a Unix socket path"));
    }

    #[test]
    fn explicit_tui_overlay_preserves_jsonc_plugins_and_relative_paths() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("tui.jsonc");
        let mut config = parse_config(
            r#"{
            // The user plugin and options must survive injection.
            "plugin": [["./mine.js", {"token":"unchanged"}], "package-name",],
            "attention": {"sounds": {"done": "sounds/done.wav"}},
            "theme": "custom",
        }"#,
            "test",
        )
        .unwrap();
        relocate_tui_config(&mut config, &source).unwrap();
        add_plugin(&mut config, "file:///managed/crit.js").unwrap();
        assert_eq!(config["plugin"][0][1]["token"], "unchanged");
        assert_eq!(config["plugin"][1], "package-name");
        assert_eq!(config["plugin"][2], "file:///managed/crit.js");
        assert_eq!(config["theme"], "custom");
        assert_eq!(
            config["attention"]["sounds"]["done"],
            temp.path().join("sounds/done.wav").to_str().unwrap()
        );
    }
}
