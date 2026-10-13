use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
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
];

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

/// Serve from the first base, in order, that can hold the private directory.
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
        match DirBuilder::new().mode(0o700).create(&directory) {
            Ok(()) => {
                created = Some(directory);
                break;
            }
            Err(error) => {
                // An existing name is not ours; any other failure moves on to the next base.
                if error.kind() == std::io::ErrorKind::AlreadyExists || index + 1 == bases.len() {
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
                if let Some(sound) = value.as_str() {
                    if !sound.is_empty()
                        && !Path::new(sound).is_absolute()
                        && !sound.starts_with("~/")
                    {
                        *value = Value::String(base.join(sound).to_string_lossy().into_owned());
                    }
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
        if let Some(spec) = spec {
            if let Some(text) = spec.as_str() {
                if text.starts_with('.') {
                    *spec = Value::String(file_url(&base.join(text))?);
                }
            }
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

/// Return launch-scoped injection; inherited auth, profiles and plugin discovery stay intact.
/// `resume` is passed through unchecked: callers validate it first (`validate_resume`, plus
/// `opencode_chats` for OpenCode), because restore checks every chat up front.
pub fn command(
    kind: &str,
    session: u64,
    socket: &Path,
    resume: Option<Resume<'_>>,
) -> Result<(String, Vec<String>, Vec<(String, String)>)> {
    if !matches!(kind, "omp" | "opencode") {
        bail!("unsupported agent {kind:?}; expected omp or opencode");
    }
    install()?;
    let directory = assets_directory()?;
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
    ];
    if let Some(Resume::Chat(identity)) = resume {
        env.push(("CRIT_RESUME_ID".into(), identity.into()));
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
        match resume {
            Some(Resume::Chat(identity)) => args.extend(["--resume".into(), identity.into()]),
            Some(Resume::Continue) => args.push("--continue".into()),
            None => {}
        }
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
    Ok((kind.into(), args, env))
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

/// What a restore needs from OpenCode's database. One `opencode db` call answers all of it:
/// the CLI takes about 0.6 s to start, so one call per chat would freeze a restore for seconds.
#[derive(Debug, Default)]
pub struct OpencodeChats {
    /// The saved chat identities that still exist.
    pub existing: HashSet<String>,
    /// Folder → its newest top-level, unarchived chat that no saved identity names.
    pub latest: HashMap<String, String>,
}

/// Looks up which saved chats `ids` exist, and the newest chat of each of `folders`, whose
/// panes remember none. A value that cannot be an identity or folder is never asked about.
pub fn opencode_chats(ids: &[&str], folders: &[&str]) -> Result<OpencodeChats> {
    let mut chats = OpencodeChats::default();
    let Some(query) = opencode_chat_query(ids, folders) else {
        return Ok(chats);
    };
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
        // Saved chats come back without a folder.
        match row["folder"].as_str() {
            Some(folder) => {
                chats.latest.insert(folder.to_owned(), id.to_owned());
            }
            None => {
                chats.existing.insert(id.to_owned());
            }
        }
    }
    Ok(chats)
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
    use std::sync::mpsc;

    fn send(socket: &Path, frame: &[u8]) -> bool {
        let mut client = UnixStream::connect(socket).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        client.write_all(frame).unwrap();
        let mut ack = [0];
        client.read_exact(&mut ack).is_ok() && ack == [b'\n']
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
