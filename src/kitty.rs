//! Outer-Kitty key routing. Write lifecycle sequences to the real terminal,
//! never to a session PTY. F12 handling and the native-mode boolean live in the UI.
//!
//! Protocol: https://sw.kovidgoyal.net/kitty/mapping/#conditional-mappings-depending-on-the-state-of-the-focused-window
//! Key encoding: https://sw.kovidgoyal.net/kitty/keyboard-protocol/

use std::env;
use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::os::linux::net::SocketAddrExt;
use std::os::unix::net::{SocketAddr, UnixStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

const INTEGRATION: &[u8] = include_bytes!("../integrations/kitty.conf");
const MANAGED_HEADER: &[u8] = b"# Managed by Crit. Do not edit.\n";

/// Activate Crit shortcuts in this terminal, without changing desktop styling.
pub fn enter(writer: &mut impl Write) -> io::Result<()> {
    // Base64: "1" = MQ==, "crit" = Y3JpdA==.
    writer.write_all(
        b"\x1b]1337;SetUserVar=crit_active=MQ==\x07\x1b]1337;SetUserVar=crit_keys=Y3JpdA==\x07",
    )?;
    spacing(writer, false)?;
    writer.flush()
}

/// Persistently switch the outer window to native Kitty or Crit shortcuts.
/// The active marker is intentionally untouched, so F12 can always switch back.
pub fn set_native(writer: &mut impl Write, native: bool) -> io::Result<()> {
    writer.write_all(if native {
        // Base64: "native" = bmF0aXZl.
        b"\x1b]1337;SetUserVar=crit_keys=bmF0aXZl\x07"
    } else {
        b"\x1b]1337;SetUserVar=crit_keys=Y3JpdA==\x07"
    })?;
    writer.flush()
}

/// Restore configured spacing and normal Kitty routing.
pub fn leave(writer: &mut impl Write) -> io::Result<()> {
    writer.write_all(b"\x1b]1337;SetUserVar=crit_keys\x07\x1b]1337;SetUserVar=crit_active\x07")?;
    spacing(writer, true)?;
    writer.flush()
}

fn spacing(writer: &mut impl Write, restore: bool) -> io::Result<()> {
    // Per-window only. Never change Kitty's configured spacing or other windows.
    let value = if restore { "null" } else { "0" };
    write!(
        writer,
        "\x1bP@kitty-cmd{{\"cmd\":\"set-spacing\",\"version\":[0,26,0],\"no_response\":true,\"payload\":{{\"match_window\":\"state:self\",\"settings\":{{\"padding-top\":{value},\"padding-bottom\":{value},\"padding-left\":{value},\"padding-right\":{value},\"margin-top\":{value},\"margin-bottom\":{value},\"margin-left\":{value},\"margin-right\":{value}}}}}}}\x1b\\"
    )
}

/// Read this window's live default background without consuming terminal input.
/// Missing remote control means the color is unknown, not a fabricated RGB.
pub fn background() -> Option<[u8; 3]> {
    let socket = env::var("KITTY_LISTEN_ON").ok()?;
    let window = env::var("KITTY_WINDOW_ID").ok()?.parse::<u64>().ok()?;
    window_background(&socket, window).ok()
}

fn window_background(socket: &str, window: u64) -> Result<[u8; 3]> {
    let socket = socket
        .strip_prefix("unix:")
        .context("not a Unix Kitty socket")?;
    let address = if let Some(name) = socket.strip_prefix('@') {
        SocketAddr::from_abstract_name(name)?
    } else {
        SocketAddr::from_pathname(socket)?
    };
    let mut stream = UnixStream::connect_addr(&address)?;
    let deadline = Instant::now() + Duration::from_millis(150);
    stream.set_write_timeout(Some(Duration::from_millis(150)))?;
    write!(
        stream,
        "\x1bP@kitty-cmd{{\"cmd\":\"get-colors\",\"version\":[0,26,0],\"payload\":{{\"match\":\"id:{window}\"}}}}\x1b\\"
    )?;
    let mut reply = Vec::with_capacity(8192);
    let mut buffer = [0; 4096];
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .context("Kitty color query timed out")?;
        stream.set_read_timeout(Some(remaining))?;
        let count = stream.read(&mut buffer)?;
        anyhow::ensure!(count != 0, "incomplete Kitty color response");
        anyhow::ensure!(
            reply.len() + count <= 16 * 1024,
            "Kitty color response too large"
        );
        reply.extend_from_slice(&buffer[..count]);
        if reply.ends_with(b"\x1b\\") {
            break;
        }
    }
    let json = reply
        .strip_prefix(b"\x1bP@kitty-cmd")
        .and_then(|bytes| bytes.strip_suffix(b"\x1b\\"))
        .context("invalid Kitty color response")?;
    #[derive(serde::Deserialize)]
    struct Colors {
        data: String,
    }
    let colors: Colors = serde_json::from_slice(json)?;
    let value = colors
        .data
        .lines()
        .find_map(|line| {
            let mut fields = line.split_whitespace();
            (fields.next() == Some("background"))
                .then(|| fields.next())
                .flatten()
        })
        .context("Kitty did not report its background")?;
    let hex = value
        .strip_prefix('#')
        .filter(|hex| hex.len() == 6)
        .context("invalid Kitty background")?;
    let rgb = u32::from_str_radix(hex, 16)?;
    Ok([(rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8])
}

/// Install the managed include, preserving the user's other configuration.
/// This does not reload Kitty or enable remote control.
pub fn install() -> Result<PathBuf> {
    let directory = config_directory(
        env::var_os("KITTY_CONFIG_DIRECTORY").as_deref(),
        env::var_os("XDG_CONFIG_HOME").as_deref(),
        env::var_os("HOME").as_deref(),
    )?;
    install_in(&directory)
}

/// The directory Kitty reads. `KITTY_CONFIG_DIRECTORY` always wins; otherwise the first of
/// `$XDG_CONFIG_HOME/kitty` and `~/.config/kitty` holding a `kitty.conf`, so a fresh XDG
/// directory cannot shadow the user's real configuration. With neither, `$XDG_CONFIG_HOME/kitty`
/// if set, else `~/.config/kitty`.
fn config_directory(
    kitty_env: Option<&OsStr>,
    xdg: Option<&OsStr>,
    home: Option<&OsStr>,
) -> Result<PathBuf> {
    if let Some(directory) = kitty_env.filter(|value| !value.is_empty()) {
        return std::path::absolute(directory).with_context(|| {
            format!(
                "Resolving KITTY_CONFIG_DIRECTORY {}",
                Path::new(directory).display()
            )
        });
    }
    let xdg = xdg.filter(|value| !value.is_empty()).map(Path::new);
    let home = home.filter(|value| !value.is_empty()).map(Path::new);
    let existing = [
        xdg.filter(|root| root.is_absolute())
            .map(|root| root.join("kitty")),
        home.map(|home| home.join(".config/kitty")),
    ]
    .into_iter()
    .flatten()
    .find(|directory| directory.join("kitty.conf").exists());
    if let Some(directory) = existing {
        return Ok(directory);
    }
    if let Some(root) = xdg {
        if !root.is_absolute() {
            bail!("XDG_CONFIG_HOME must be an absolute path");
        }
        return Ok(root.join("kitty"));
    }
    let home =
        home.context("Neither XDG_CONFIG_HOME nor HOME is set; cannot locate Kitty configuration")?;
    Ok(home.join(".config/kitty"))
}

fn read_optional(path: &Path) -> Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("Reading {}", path.display())),
    }
}

fn has_include(config: &[u8]) -> bool {
    let mut lines = config.split(|byte| *byte == b'\n').peekable();
    while let Some(line) = lines.next() {
        // Kitty joins a following line beginning with '\\' onto this line.
        // Such a physical line alone is not an include of crit.conf.
        if lines
            .peek()
            .is_some_and(|next| next.trim_ascii_start().starts_with(b"\\"))
        {
            continue;
        }
        let line = line.trim_ascii();
        if let Some(value) = line.strip_prefix(b"include") {
            if value.first().is_some_and(u8::is_ascii_whitespace)
                && matches!(value.trim_ascii(), b"crit.conf" | b"./crit.conf")
            {
                return true;
            }
        }
    }
    false
}

fn install_in(directory: &Path) -> Result<PathBuf> {
    let managed_path = directory.join("crit.conf");
    let config_path = directory.join("kitty.conf");

    // Refuse symlinks as well as non-managed files: installing Crit must never
    // replace or follow a user's unrelated crit.conf to some other location.
    match fs::symlink_metadata(&managed_path) {
        Ok(metadata) if !metadata.is_file() => {
            bail!(
                "Refusing to replace non-regular file {}",
                managed_path.display()
            );
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| format!("Inspecting {}", managed_path.display()));
        }
    }
    let previous = read_optional(&managed_path)?;
    if previous
        .as_deref()
        .is_some_and(|bytes| !bytes.starts_with(MANAGED_HEADER))
    {
        bail!("Refusing to replace non-managed {}", managed_path.display());
    }
    let config = read_optional(&config_path)?.unwrap_or_default();
    let needs_include = !has_include(&config);

    fs::create_dir_all(directory).with_context(|| format!("Creating {}", directory.display()))?;
    if previous.as_deref() != Some(INTEGRATION) {
        fs::write(&managed_path, INTEGRATION)
            .with_context(|| format!("Writing {}", managed_path.display()))?;
    }
    if needs_include {
        // Append, rather than rewriting, so existing bytes, permissions and
        // symlinks to the user's kitty.conf are preserved.
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&config_path)
            .with_context(|| format!("Opening {} for the Crit include", config_path.display()))?;
        let newline: &[u8] = if config.windows(2).any(|pair| pair == b"\r\n") {
            b"\r\n"
        } else {
            b"\n"
        };
        if !config.is_empty() && !config.ends_with(b"\n") {
            file.write_all(newline)?;
        }
        file.write_all(b"include crit.conf")?;
        file.write_all(newline)?;
        file.flush()?;
    }
    Ok(managed_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_location_honors_xdg_and_empty_fallback_without_mutating_environment() {
        let home = Some(OsStr::new("/home/tester"));
        assert_eq!(
            config_directory(None, Some(OsStr::new("/tmp/config")), home).unwrap(),
            Path::new("/tmp/config/kitty")
        );
        assert_eq!(
            config_directory(None, Some(OsStr::new("")), home).unwrap(),
            Path::new("/home/tester/.config/kitty")
        );
        assert!(config_directory(None, None, None).is_err());
        assert!(config_directory(None, Some(OsStr::new("relative")), home).is_err());
    }

    #[test]
    fn kitty_config_directory_always_wins_and_is_made_absolute() {
        let temporary = tempfile::tempdir().unwrap();
        let explicit = temporary.path().join("explicit");
        let home = temporary.path().join("home");
        let real = home.join(".config/kitty");
        fs::create_dir_all(&real).unwrap();
        fs::write(real.join("kitty.conf"), b"font_size 15\n").unwrap();
        assert_eq!(
            config_directory(
                Some(explicit.as_os_str()),
                Some(temporary.path().as_os_str()),
                Some(home.as_os_str()),
            )
            .unwrap(),
            explicit
        );
        assert_eq!(
            config_directory(Some(OsStr::new("relative/kitty")), None, None).unwrap(),
            env::current_dir().unwrap().join("relative/kitty")
        );
        // An empty value counts as unset.
        assert_eq!(
            config_directory(Some(OsStr::new("")), None, Some(home.as_os_str())).unwrap(),
            real
        );
    }

    #[test]
    fn existing_kitty_conf_is_not_shadowed_by_an_unused_xdg_directory() {
        let temporary = tempfile::tempdir().unwrap();
        let xdg = temporary.path().join("xdg");
        let home = temporary.path().join("home");
        let real = home.join(".config/kitty");
        fs::create_dir_all(&real).unwrap();
        fs::write(real.join("kitty.conf"), b"font_size 15\n").unwrap();
        let (xdg_env, home_env) = (Some(xdg.as_os_str()), Some(home.as_os_str()));
        // Kitty reads the first kitty.conf that exists, which is not in the unused XDG directory.
        assert_eq!(config_directory(None, xdg_env, home_env).unwrap(), real);
        // A relative XDG_CONFIG_HOME is invalid, so it cannot win either.
        assert_eq!(
            config_directory(None, Some(OsStr::new("relative")), home_env).unwrap(),
            real
        );
        // Once XDG holds a kitty.conf it keeps priority.
        let chosen = xdg.join("kitty");
        fs::create_dir_all(&chosen).unwrap();
        fs::write(chosen.join("kitty.conf"), b"").unwrap();
        assert_eq!(config_directory(None, xdg_env, home_env).unwrap(), chosen);
        // With no kitty.conf anywhere, XDG_CONFIG_HOME is still where one is created.
        fs::remove_file(chosen.join("kitty.conf")).unwrap();
        fs::remove_file(real.join("kitty.conf")).unwrap();
        assert_eq!(config_directory(None, xdg_env, home_env).unwrap(), chosen);
    }

    #[test]
    fn install_preserves_existing_bytes_and_is_idempotent() {
        let temporary = tempfile::tempdir().unwrap();
        let config_path = temporary.path().join("kitty.conf");
        let original =
            b"# user's config: \xff\r\nmap ctrl+shift+, launch --location=vsplit --cwd=current";
        fs::write(&config_path, original).unwrap();
        let installed = install_in(temporary.path()).unwrap();
        let mut expected = original.to_vec();
        expected.extend_from_slice(b"\r\ninclude crit.conf\r\n");
        assert_eq!(fs::read(&config_path).unwrap(), expected);
        assert!(fs::read(&installed).unwrap().starts_with(MANAGED_HEADER));
        assert_eq!(install_in(temporary.path()).unwrap(), installed);
        assert_eq!(fs::read(&config_path).unwrap(), expected);
    }

    #[test]
    fn unmanaged_file_refusal_preserves_both_files() {
        let temporary = tempfile::tempdir().unwrap();
        let managed = temporary.path().join("crit.conf");
        let config = temporary.path().join("kitty.conf");
        fs::write(&managed, b"# user's file\n").unwrap();
        fs::write(&config, b"font_size 13\n").unwrap();
        assert!(install_in(temporary.path()).is_err());
        assert_eq!(fs::read(&managed).unwrap(), b"# user's file\n");
        assert_eq!(fs::read(&config).unwrap(), b"font_size 13\n");
    }

    #[test]
    fn existing_include_recognition_obeys_kitty_comment_and_continuation_rules() {
        assert!(has_include(
            b"# comment\n  include\tcrit.conf \r\nfont_size 12\n"
        ));
        assert!(has_include(b"include ./crit.conf\n"));
        assert!(!has_include(b"# include crit.conf\n"));
        assert!(!has_include(b"include crit.conf.backup\n"));
        assert!(!has_include(
            b"include crit.conf # not an inline comment in Kitty\n"
        ));
        assert!(!has_include(b"include crit.conf\n\\.backup\n"));
        let temporary = tempfile::tempdir().unwrap();
        let config = temporary.path().join("kitty.conf");
        let original = b"font_size 12\n  include\tcrit.conf \n";
        fs::write(&config, original).unwrap();
        install_in(temporary.path()).unwrap();
        assert_eq!(fs::read(&config).unwrap(), original);
    }

    #[cfg(unix)]
    #[test]
    fn managed_symlink_is_refused_but_user_config_symlink_is_preserved() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().unwrap();
        let target = temporary.path().join("user.conf");
        fs::write(&target, b"font_size 12\n").unwrap();
        let directory = temporary.path().join("kitty");
        fs::create_dir(&directory).unwrap();
        let managed = directory.join("crit.conf");
        symlink(&target, &managed).unwrap();
        assert!(install_in(&directory).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"font_size 12\n");
        assert!(!directory.join("kitty.conf").exists());
        fs::remove_file(&managed).unwrap();
        let config = directory.join("kitty.conf");
        symlink(&target, &config).unwrap();
        install_in(&directory).unwrap();
        assert!(
            fs::symlink_metadata(&config)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            fs::read(&target).unwrap(),
            b"font_size 12\ninclude crit.conf\n"
        );
    }
}
