/// The longest OSC string body that is passed on to vte. vte buffers an OSC
/// string until it is terminated, without any limit, so a stray `ESC ]`
/// followed by a long stream of output would otherwise grow without bound
/// (and swallow all of that output).
pub const MAX_OSC_LEN: usize = 8 << 20;

const ESC: u8 = 0x1b;

/// Whether `byte` ends an OSC string (BEL, CAN and SUB) or aborts it (ESC).
fn ends_osc(byte: u8) -> bool {
    matches!(byte, 0x07 | 0x18 | 0x1a | ESC)
}

/// The part of vte's state machine that decides whether an OSC string is
/// open, tracked alongside vte by [`Parser::process`] to cap the length of
/// OSC strings.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum OscState {
    /// Outside of an escape sequence, or inside one that cannot start an OSC
    /// string.
    Ground,
    /// Directly after an ESC.
    Escape,
    /// Inside an OSC string, with this many bytes of its body seen so far.
    Osc(usize),
    /// Skipping the rest of an OSC string that is longer than `MAX_OSC_LEN`.
    Discard,
}

/// A parser for terminal output which produces an in-memory representation of
/// the terminal contents.
#[allow(clippy::struct_field_names)]
pub struct Parser<CB: crate::callbacks::Callbacks = ()> {
    parser: vte::Parser,
    osc: OscState,
    screen: crate::perform::WrappedScreen<CB>,
}

impl Parser {
    /// Creates a new terminal parser of the given size and with the given
    /// amount of scrollback.
    #[must_use]
    pub fn new(rows: u16, cols: u16, scrollback_len: usize) -> Self {
        Self {
            parser: vte::Parser::new(),
            osc: OscState::Ground,
            screen: crate::perform::WrappedScreen::new(
                rows,
                cols,
                scrollback_len,
            ),
        }
    }
}

impl<CB: crate::callbacks::Callbacks> Parser<CB> {
    /// Creates a new terminal parser of the given size and with the given
    /// amount of scrollback. Terminal events will be reported via method
    /// calls on the provided [`Callbacks`](crate::callbacks::Callbacks)
    /// implementation.
    pub fn new_with_callbacks(
        rows: u16,
        cols: u16,
        scrollback_len: usize,
        callbacks: CB,
    ) -> Self {
        Self {
            parser: vte::Parser::new(),
            osc: OscState::Ground,
            screen: crate::perform::WrappedScreen::new_with_callbacks(
                rows,
                cols,
                scrollback_len,
                callbacks,
            ),
        }
    }

    /// Processes the contents of the given byte string, and updates the
    /// in-memory terminal state.
    ///
    /// An OSC string longer than 8 MiB is dropped without being dispatched.
    pub fn process(&mut self, bytes: &[u8]) {
        // bytes[fed..] has not been passed to vte yet: everything goes
        // through vte unchanged except for the tail of an oversized OSC string
        let mut fed = 0;
        let mut i = 0;
        while i < bytes.len() {
            let rest = &bytes[i..];
            match self.osc {
                OscState::Ground => {
                    let Some(esc) = rest.iter().position(|&b| b == ESC) else {
                        break;
                    };
                    self.osc = OscState::Escape;
                    i += esc + 1;
                }
                OscState::Escape => {
                    self.osc = match rest[0] {
                        b']' => OscState::Osc(0),
                        // CAN and SUB abort the sequence
                        0x18 | 0x1a => OscState::Ground,
                        // vte keeps waiting for the final byte through other
                        // controls, and ignores DEL and non-ASCII bytes
                        0x00..=0x1f | 0x7f..=0xff => OscState::Escape,
                        _ => OscState::Ground,
                    };
                    i += 1;
                }
                OscState::Osc(len) => {
                    let body = rest
                        .iter()
                        .position(|&b| ends_osc(b))
                        .unwrap_or(rest.len());
                    let room = MAX_OSC_LEN - len;
                    if body > room {
                        // vte has buffered the body up to the cap by now:
                        // drop that without dispatching it, and skip the rest
                        // of the string
                        self.parser
                            .advance(&mut self.screen, &bytes[fed..i + room]);
                        self.parser = vte::Parser::new();
                        self.osc = OscState::Discard;
                        i += room;
                        fed = i;
                    } else if body == rest.len() {
                        self.osc = OscState::Osc(len + body);
                        i = bytes.len();
                    } else {
                        self.osc = if rest[body] == ESC {
                            OscState::Escape
                        } else {
                            OscState::Ground
                        };
                        i += body + 1;
                    }
                }
                OscState::Discard => {
                    if let Some(end) = rest.iter().position(|&b| ends_osc(b)) {
                        i += end;
                        if bytes[i] == ESC {
                            // vte starts over at this ESC
                            self.osc = OscState::Escape;
                            fed = i;
                        } else {
                            self.osc = OscState::Ground;
                            fed = i + 1;
                        }
                        i += 1;
                    } else {
                        i = bytes.len();
                        fed = i;
                    }
                }
            }
        }
        if fed < bytes.len() {
            self.parser.advance(&mut self.screen, &bytes[fed..]);
        }
    }

    /// Returns a reference to a [`Screen`](crate::Screen) object containing
    /// the terminal state.
    #[must_use]
    pub fn screen(&self) -> &crate::Screen {
        &self.screen.screen
    }

    /// Returns a mutable reference to a [`Screen`](crate::Screen) object
    /// containing the terminal state.
    #[must_use]
    pub fn screen_mut(&mut self) -> &mut crate::Screen {
        &mut self.screen.screen
    }

    /// Returns a reference to the [`Callbacks`](crate::callbacks::Callbacks)
    /// state object passed into the constructor.
    pub fn callbacks(&self) -> &CB {
        &self.screen.callbacks
    }

    /// Returns a mutable reference to the
    /// [`Callbacks`](crate::callbacks::Callbacks) state object passed into
    /// the constructor.
    pub fn callbacks_mut(&mut self) -> &mut CB {
        &mut self.screen.callbacks
    }
}

impl Default for Parser {
    /// Returns a parser with dimensions 80x24 and no scrollback.
    fn default() -> Self {
        Self::new(24, 80, 0)
    }
}

impl std::io::Write for Parser {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.process(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
