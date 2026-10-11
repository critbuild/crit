use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::io::{self, Write};

pub const HELP: &str = "Crit shortcuts (same as Kitty's terminal/window actions)
  F12                    Toggle Crit / native Kitty shortcuts (persistent)
  Ctrl+Shift+T            New worktree tab
  Ctrl+Shift+Q            Close worktree tab; retain checkout
  Ctrl+Tab / Ctrl+Shift+Right  Next worktree
  Ctrl+Shift+Tab / Left   Previous worktree
  Ctrl+Shift+Enter        New OMP / OpenCode / shell session
  Ctrl+D                 From OMP / OpenCode, select or open a shell here
  Ctrl+Shift+W            Close focused session
  Ctrl+Shift+[ / ]        Previous / next session or split pane
  Ctrl+Shift+1..9 / 0     Focus session 1..10
  Ctrl+Shift+,            Split worktrees side by side
  Ctrl+Shift+.            Split worktrees top and bottom
  Ctrl+Shift+L            Cycle full / vertical / horizontal layout
  F1                     Ask Crit in natural language (Haiku 5.5)
  F1 then F2             Manual controls: Up/Down and Enter, no AI needed
  F10                    Exit Crit (confirmation required)

The agent keeps running when Ctrl+D selects a shell. In a shell, Ctrl+D remains
normal EOF; native shortcut mode always forwards Ctrl+D to the focused terminal.
Clipboard and font shortcuts remain native Kitty actions. Ctrl+C, Ctrl+Z,
Tab, Escape, and ordinary typing go to the focused terminal.
F12 switches routing, not a one-shot prefix. Setup: crit setup kitty
In Ask Crit: Enter sends, Esc cancels inference, Ctrl+U clears the input.
Answer follow-up questions in the same box; ordinary terminal input is unchanged.
";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    ToggleNative,
    Launcher,
    Quit,
    NewWorktree,
    CloseWorktree,
    NewSession,
    CloseSession,
    NextWorktree,
    PreviousWorktree,
    NextSession,
    PreviousSession,
    SplitVertical,
    SplitHorizontal,
    CycleLayout,
    FocusSession(usize),
}

pub fn action(key: KeyEvent) -> Option<Action> {
    use Action::*;
    use KeyCode::*;
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    if key.modifiers.intersects(
        KeyModifiers::ALT | KeyModifiers::SUPER | KeyModifiers::HYPER | KeyModifiers::META,
    ) {
        return None;
    }
    match key.code {
        F(12) if key.modifiers.is_empty() => Some(ToggleNative),
        F(1) if key.modifiers.is_empty() => Some(Action::Launcher),
        F(10) if key.modifiers.is_empty() => Some(Quit),
        Tab if ctrl && !shift => Some(NextWorktree),
        Tab | BackTab if ctrl && shift => Some(PreviousWorktree),
        Right if ctrl && shift => Some(NextWorktree),
        Left if ctrl && shift => Some(PreviousWorktree),
        Enter if ctrl && shift => Some(NewSession),
        Char(c) if ctrl && shift => match c.to_ascii_lowercase() {
            't' => Some(NewWorktree),
            'q' => Some(CloseWorktree),
            'w' => Some(CloseSession),
            '[' | '{' => Some(PreviousSession),
            ']' | '}' => Some(NextSession),
            ',' | '<' => Some(SplitVertical),
            '.' | '>' => Some(SplitHorizontal),
            'l' => Some(CycleLayout),
            '1'..='9' => Some(FocusSession(c as usize - '1' as usize)),
            '0' => Some(FocusSession(9)),
            _ => None,
        },
        _ => None,
    }
}

pub fn write_key(
    writer: &mut impl Write,
    key: KeyEvent,
    application_cursor: bool,
    kitty_keyboard: bool,
) -> io::Result<()> {
    use KeyCode::*;
    let modifiers = key.modifiers;
    let modifier_number = 1
        + u8::from(modifiers.contains(KeyModifiers::SHIFT))
        + 2 * u8::from(modifiers.contains(KeyModifiers::ALT))
        + 4 * u8::from(modifiers.contains(KeyModifiers::CONTROL))
        + 8 * u8::from(modifiers.contains(KeyModifiers::SUPER));
    let printable =
        matches!(key.code, Char(_)) && modifiers.difference(KeyModifiers::SHIFT).is_empty();
    if kitty_keyboard && !printable && (modifier_number > 1 || key.code == Esc) {
        let code = match key.code {
            Char(c) => Some(if modifiers.contains(KeyModifiers::SHIFT) {
                c.to_ascii_lowercase()
            } else {
                c
            } as u32),
            Enter => Some(13),
            Tab | BackTab => Some(9),
            Esc => Some(27),
            Backspace => Some(127),
            _ => None,
        };
        if let Some(code) = code {
            return write!(writer, "\x1b[{code};{modifier_number}u");
        }
    }
    if modifiers.contains(KeyModifiers::ALT)
        && matches!(key.code, Char(_) | Enter | Backspace | Tab | Esc)
    {
        writer.write_all(b"\x1b")?;
    }
    match key.code {
        Char(c) => {
            if modifiers.contains(KeyModifiers::CONTROL) {
                let c = c.to_ascii_lowercase();
                let byte = match c {
                    'a'..='z' => c as u8 - b'a' + 1,
                    '@' | ' ' | '2' => 0,
                    '[' | '3' => 27,
                    '\\' | '4' => 28,
                    ']' | '5' => 29,
                    '^' | '6' => 30,
                    '_' | '7' => 31,
                    '?' | '8' => 127,
                    _ => return Ok(()),
                };
                writer.write_all(&[byte])
            } else {
                let mut bytes = [0; 4];
                writer.write_all(c.encode_utf8(&mut bytes).as_bytes())
            }
        }
        Enter => writer.write_all(b"\r"),
        Backspace => writer.write_all(b"\x7f"),
        Tab if !modifiers.contains(KeyModifiers::SHIFT) => writer.write_all(b"\t"),
        Tab | BackTab => writer.write_all(b"\x1b[Z"),
        Esc => writer.write_all(b"\x1b"),
        Up | Down | Right | Left | Home | End => {
            let suffix = match key.code {
                Up => 'A',
                Down => 'B',
                Right => 'C',
                Left => 'D',
                Home => 'H',
                _ => 'F',
            };
            if modifier_number > 1 {
                write!(writer, "\x1b[1;{modifier_number}{suffix}")
            } else if application_cursor {
                write!(writer, "\x1bO{suffix}")
            } else {
                write!(writer, "\x1b[{suffix}")
            }
        }
        Insert | Delete | PageUp | PageDown | F(5..=12) => {
            let number = match key.code {
                Insert => 2,
                Delete => 3,
                PageUp => 5,
                PageDown => 6,
                F(5) => 15,
                F(6) => 17,
                F(7) => 18,
                F(8) => 19,
                F(9) => 20,
                F(10) => 21,
                F(11) => 23,
                _ => 24,
            };
            if modifier_number > 1 {
                write!(writer, "\x1b[{number};{modifier_number}~")
            } else {
                write!(writer, "\x1b[{number}~")
            }
        }
        F(number @ 1..=4) => {
            let suffix = char::from(b'P' + number - 1);
            if modifier_number > 1 {
                write!(writer, "\x1b[1;{modifier_number}{suffix}")
            } else {
                write!(writer, "\x1bO{suffix}")
            }
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn shell_controls_are_not_app_actions() {
        for c in ['c', 'd', 'z', 't', 'q', 'w'] {
            let key = KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL);
            assert_eq!(action(key), None);
        }
        assert_eq!(
            action(KeyEvent::new(
                KeyCode::Char('T'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT
            )),
            Some(Action::NewWorktree)
        );
    }
    #[test]
    fn ctrl_d_remains_terminal_input_when_not_switching_from_an_agent() {
        let key = KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL);
        assert_eq!(action(key), None);
        let mut bytes = Vec::new();
        write_key(&mut bytes, key, false, false).unwrap();
        assert_eq!(bytes, b"\x04");
        bytes.clear();
        write_key(&mut bytes, key, false, true).unwrap();
        assert_eq!(bytes, b"\x1b[100;5u");
    }
    #[test]
    fn modifier_and_unicode_input_survives_terminal_encoding() {
        let mut bytes = Vec::new();
        write_key(
            &mut bytes,
            KeyEvent::new(KeyCode::Char('界'), KeyModifiers::NONE),
            false,
            false,
        )
        .unwrap();
        write_key(
            &mut bytes,
            KeyEvent::new(KeyCode::Left, KeyModifiers::CONTROL),
            false,
            false,
        )
        .unwrap();
        write_key(
            &mut bytes,
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            false,
            false,
        )
        .unwrap();
        assert_eq!(bytes, "界\x1b[1;5D\x03".as_bytes());
        bytes.clear();
        write_key(
            &mut bytes,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT),
            false,
            true,
        )
        .unwrap();
        assert_eq!(bytes, b"\x1b[13;2u");
    }
    #[test]
    fn shifted_text_stays_utf8_when_keyboard_disambiguation_is_enabled() {
        let mut bytes = Vec::new();
        for (c, modifiers) in [
            ('A', KeyModifiers::SHIFT),
            ('z', KeyModifiers::NONE),
            ('!', KeyModifiers::SHIFT),
            ('?', KeyModifiers::SHIFT),
            ('Ä', KeyModifiers::SHIFT),
            ('界', KeyModifiers::NONE),
        ] {
            write_key(
                &mut bytes,
                KeyEvent::new(KeyCode::Char(c), modifiers),
                false,
                true,
            )
            .unwrap();
        }
        assert_eq!(bytes, "Az!?Ä界".as_bytes());
        bytes.clear();
        write_key(
            &mut bytes,
            KeyEvent::new(
                KeyCode::Char('A'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            ),
            false,
            true,
        )
        .unwrap();
        assert_eq!(bytes, b"\x1b[97;6u");
    }
}
