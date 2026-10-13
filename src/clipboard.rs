//! Clipboard writes go to the outer terminal through OSC 52, including over SSH.
//! Reads use local Linux clipboard tools; remote sessions should use their
//! terminal's native paste shortcut instead. No clipboard queries are forwarded.

use anyhow::{Context, Result, bail};
use std::{
    env,
    io::{self, Read, Seek, SeekFrom, Write},
    os::unix::process::CommandExt,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
const PASTE_TIMEOUT: Duration = Duration::from_secs(2);

/// Copy a Crit selection to both the clipboard (`c`) and primary selection (`p`).
/// The terminal may ignore an unsupported selection or deny OSC 52 writes; the
/// protocol provides no acknowledgement, so success only confirms the write.
/// Keep the stdout lock through encoding and flushing to avoid interleaved frames.
pub fn copy(text: &str) -> Result<()> {
    write_copy(&mut io::stdout().lock(), text.as_bytes())
        .context("Copying selection with OSC 52; the outer terminal must allow clipboard writes")
}

/// Forward a child's OSC 52 write without allowing clipboard reads or escapes.
/// An empty selector uses the terminal default; an empty payload clears it.
/// Only the standard alphabet and canonical padded base64 are accepted.
pub fn copy_encoded(ty: &[u8], data: &[u8]) -> io::Result<()> {
    write_encoded(&mut io::stdout().lock(), ty, data)
}

fn write_copy(writer: &mut impl Write, bytes: &[u8]) -> io::Result<()> {
    // An empty OSC 52 payload clears the clipboard, so a blank selection writes nothing.
    if bytes.is_empty() {
        return Ok(());
    }
    writer.write_all(b"\x1b]52;cp;")?;
    // Each input chunk ends on a three-byte boundary, so padding occurs only
    // at the end. Encoding stays on the stack even for large selections.
    let mut encoded = [0u8; 4096];
    for chunk in bytes.chunks(3072) {
        let mut count = 0;
        for group in chunk.chunks(3) {
            let a = group[0];
            let b = group.get(1).copied().unwrap_or(0);
            let c = group.get(2).copied().unwrap_or(0);
            encoded[count] = BASE64[(a >> 2) as usize];
            encoded[count + 1] = BASE64[(((a & 3) << 4) | (b >> 4)) as usize];
            encoded[count + 2] = if group.len() > 1 {
                BASE64[(((b & 15) << 2) | (c >> 6)) as usize]
            } else {
                b'='
            };
            encoded[count + 3] = if group.len() > 2 {
                BASE64[(c & 63) as usize]
            } else {
                b'='
            };
            count += 4;
        }
        writer.write_all(&encoded[..count])?;
    }
    writer.write_all(b"\x07")?;
    writer.flush()
}

fn write_encoded(writer: &mut impl Write, ty: &[u8], data: &[u8]) -> io::Result<()> {
    // Validate everything before writing even the OSC prefix. In particular,
    // `?` must never reach the outer terminal and disclose its clipboard.
    if !ty
        .iter()
        .all(|byte| matches!(byte, b'c' | b'p' | b'q' | b's' | b'0'..=b'7'))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid OSC 52 selection",
        ));
    }
    if !valid_base64(data) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "OSC 52 requires base64 clipboard data; queries are not allowed",
        ));
    }
    writer.write_all(b"\x1b]52;")?;
    writer.write_all(ty)?;
    writer.write_all(b";")?;
    writer.write_all(data)?;
    writer.write_all(b"\x07")?;
    writer.flush()
}

fn base64_value(byte: u8) -> Option<u8> {
    match byte {
        b'A'..=b'Z' => Some(byte - b'A'),
        b'a'..=b'z' => Some(byte - b'a' + 26),
        b'0'..=b'9' => Some(byte - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

fn valid_base64(data: &[u8]) -> bool {
    if !data.len().is_multiple_of(4) {
        return false;
    }
    let padding = data.iter().rev().take_while(|byte| **byte == b'=').count();
    if padding > 2 {
        return false;
    }
    let content = &data[..data.len() - padding];
    if !content.iter().all(|byte| base64_value(*byte).is_some()) {
        return false;
    }
    match padding {
        1 => content
            .last()
            .and_then(|byte| base64_value(*byte))
            .is_some_and(|value| value & 3 == 0),
        2 => content
            .last()
            .and_then(|byte| base64_value(*byte))
            .is_some_and(|value| value & 15 == 0),
        _ => true,
    }
}

/// Read the local clipboard without changing ordinary Ctrl+C/Ctrl+V routing.
/// Prefer Wayland, then available X11 tools, with one shared two-second deadline.
/// Clipboard text, including trailing newlines, is returned unchanged. Failures
/// explain the missing tool/display; native terminal paste also works over SSH.
pub fn paste() -> Result<String> {
    let present = |name| env::var_os(name).is_some_and(|value| !value.is_empty());
    let wayland = present("WAYLAND_DISPLAY");
    let x11 = present("DISPLAY");
    if !wayland && !x11 {
        bail!(
            "Cannot read clipboard: no WAYLAND_DISPLAY or DISPLAY. Use the outer terminal's native paste (Ctrl+Shift+V in Kitty), especially over SSH"
        );
    }
    let deadline = Instant::now() + PASTE_TIMEOUT;
    let mut failures = Vec::new();
    for (available, program, args) in [
        (wayland, "wl-paste", &["--no-newline"][..]),
        (x11, "xclip", &["-selection", "clipboard", "-o"][..]),
        (x11, "xsel", &["--clipboard", "--output"][..]),
    ] {
        if !available {
            continue;
        }
        match read_clipboard(program, args, deadline) {
            Ok(text) => return Ok(text),
            Err(error) => failures.push(format!("{program}: {error:#}")),
        }
        if Instant::now() >= deadline {
            break;
        }
    }
    bail!(
        "Cannot read clipboard: {}. Install wl-clipboard (Wayland) or xclip/xsel (X11), or use the outer terminal's native paste (Ctrl+Shift+V in Kitty)",
        failures.join("; ")
    )
}

// Temporary files, as in the updater, avoid pipe backpressure and detached reader
// threads. They are private and removed automatically; no clipboard file remains.
fn read_clipboard(program: &str, args: &[&str], deadline: Instant) -> Result<String> {
    let mut stdout = tempfile::tempfile().context("capture clipboard text")?;
    let mut stderr = tempfile::tempfile().context("capture clipboard diagnostic")?;
    let child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(stdout.try_clone()?)
        .stderr(stderr.try_clone()?)
        .process_group(0)
        .spawn()
        .with_context(|| format!("start {program}"))?;
    let mut process = PasteProcess {
        child,
        reaped: false,
    };
    let status = loop {
        if let Some(status) = process
            .child
            .try_wait()
            .context("wait for clipboard tool")?
        {
            process.reaped = true;
            break status;
        }
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            bail!("clipboard tool timed out after two seconds");
        };
        std::thread::sleep(remaining.min(Duration::from_millis(10)));
    };
    if !status.success() {
        stderr.seek(SeekFrom::Start(0))?;
        let mut bytes = Vec::new();
        stderr.take(4096).read_to_end(&mut bytes)?;
        let diagnostic: String = String::from_utf8_lossy(&bytes)
            .trim()
            .chars()
            .map(|ch| if ch.is_control() { ' ' } else { ch })
            .collect();
        bail!("{status}: {diagnostic}");
    }
    // Read a fixed snapshot rather than following a descendant still writing.
    let length = stdout.metadata()?.len();
    stdout.seek(SeekFrom::Start(0))?;
    let mut text = String::new();
    stdout
        .take(length)
        .read_to_string(&mut text)
        .context("clipboard text is not valid UTF-8 or could not be read")?;
    Ok(text)
}

struct PasteProcess {
    child: Child,
    reaped: bool,
}

impl Drop for PasteProcess {
    fn drop(&mut self) {
        if !self.reaped {
            // The unreaped child reserves its process-group ID until all group
            // signals are sent. Also stop the direct child if it changed groups.
            unsafe {
                libc::kill(-(self.child.id() as libc::pid_t), libc::SIGKILL);
            }
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_encoding_matches_base64_vectors_and_copies_both_selections() {
        for (text, base64) in [
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
            ("\0\x1b\x07", "ABsH"),
            ("\u{1f980}", "8J+mgA=="),
        ] {
            let mut output = Vec::new();
            write_copy(&mut output, text.as_bytes()).unwrap();
            assert_eq!(output, format!("\x1b]52;cp;{base64}\x07").as_bytes());
            assert!(valid_base64(base64.as_bytes()));
        }
    }

    #[test]
    fn empty_selection_writes_nothing() {
        let mut output = Vec::new();
        write_copy(&mut output, b"").unwrap();
        assert!(output.is_empty());
    }

    #[test]
    fn streaming_encoding_pads_only_the_final_chunk() {
        for (suffix, encoded_suffix) in [("", ""), ("f", "Zg=="), ("fo", "Zm8=")] {
            let text = format!("{}{suffix}", "foo".repeat(2048));
            let mut output = Vec::new();
            write_copy(&mut output, text.as_bytes()).unwrap();
            assert_eq!(
                output,
                format!("\x1b]52;cp;{}{encoded_suffix}\x07", "Zm9v".repeat(2048)).as_bytes()
            );
        }
    }

    #[test]
    fn encoded_writes_preserve_valid_selectors_and_allow_clear() {
        for selector in [b"".as_slice(), b"c", b"cp", b"pqsc01234567"] {
            for data in [b"".as_slice(), b"Zg==", b"Zm8=", b"Zm9v", b"+/8="] {
                let mut output = Vec::new();
                write_encoded(&mut output, selector, data).unwrap();
                assert_eq!(
                    output,
                    [b"\x1b]52;".as_slice(), selector, b";", data, b"\x07"].concat()
                );
            }
        }
    }

    #[test]
    fn unsafe_selectors_never_write_a_partial_escape() {
        for selector in [
            b"?".as_slice(),
            b"c;p",
            b"c\x07",
            b"c\x1b\\",
            b"c\n",
            b"8",
            b"C",
            b"\xff",
        ] {
            let mut output = Vec::new();
            let error = write_encoded(&mut output, selector, b"Zm9v").unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
            assert!(output.is_empty());
        }
    }

    #[test]
    fn queries_escapes_and_malformed_base64_never_write_anything() {
        for data in [
            b"?".as_slice(),
            b"????",
            b"Zm9v\x07\x1b]52;c;?",
            b"Zm9v\x1b\\",
            b"Zm9v\n",
            b"Zm9v;",
            b"Zm9v?===",
            b"\xffAAA",
            b"____",
            b"Z",
            b"Zg",
            b"Zg=",
            b"Zg===",
            b"=m9v",
            b"Zg==Zm9v",
            b"====",
            b"Zh==",
            b"Zm9=",
        ] {
            let mut output = Vec::new();
            let error = write_encoded(&mut output, b"c", data).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
            assert!(output.is_empty(), "unsafe payload: {data:?}");
        }
    }
}
