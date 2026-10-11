use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{Read, Write};
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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentUpdate {
    pub session: u64,
    pub state: AgentState,
    pub message: Option<String>,
}

pub struct IntegrationServer {
    pub socket_path: PathBuf,
    directory: PathBuf,
    stopping: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
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
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let mut random = [0u8; 8];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut random)?;
    let directory = base.join(format!(
        "crit-{}-{:016x}",
        std::process::id(),
        u64::from_ne_bytes(random)
    ));
    DirBuilder::new()
        .mode(0o700)
        .create(&directory)
        .with_context(|| {
            format!(
                "creating private integration directory {}",
                directory.display()
            )
        })?;
    let socket_path = directory.join("agent.sock");
    let stopping = Arc::new(AtomicBool::new(false));
    let mut server = IntegrationServer {
        socket_path,
        directory,
        stopping,
        worker: None,
    };
    fs::set_permissions(&server.directory, fs::Permissions::from_mode(0o700))?;
    let listener = UnixListener::bind(&server.socket_path).context("binding lifecycle socket")?;
    fs::set_permissions(&server.socket_path, fs::Permissions::from_mode(0o600))?;
    let stop = Arc::clone(&server.stopping);
    server.worker = Some(
        thread::Builder::new()
            .name("crit-integrations".into())
            .spawn(move || {
                let mut previous = HashMap::<u64, (AgentState, Option<String>)>::new();
                while !stop.load(Ordering::Acquire) {
                    let mut stream = match listener.accept() {
                        Ok((stream, _)) => stream,
                        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(_) => break,
                    };
                    if stop.load(Ordering::Acquire) {
                        break;
                    }
                    let Ok(update) = read_update(&mut stream) else {
                        continue;
                    };
                    let duplicate = previous
                        .get(&update.session)
                        .is_some_and(|last| last.0 == update.state && last.1 == update.message);
                    if !duplicate {
                        let session = update.session;
                        let last = (update.state, update.message.clone());
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
            return Ok(update);
        }
    }
}

impl Drop for IntegrationServer {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            worker.thread().unpark();
            // Wake our own blocking accept; in-progress reads have a total deadline.
            let _ = UnixStream::connect(&self.socket_path);
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

/// Return launch-scoped injection; inherited auth, profiles and plugin discovery stay intact.
pub fn command(
    kind: &str,
    session: u64,
    socket: &Path,
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
    let mut args = Vec::new();
    if kind == "omp" {
        args.push("--extension".into());
        args.push(
            directory
                .join("omp.ts")
                .to_str()
                .context("extension path is not UTF-8")?
                .into(),
        );
    } else {
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
