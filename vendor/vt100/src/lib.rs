//! This crate parses a terminal byte stream and provides an in-memory
//! representation of the rendered contents.
//!
//! This vendored version retains OSC 8 hyperlinks on individual cells.
//! [`Cell::hyperlink`] returns the URI, which may differ from the displayed
//! text. Links move with cells and are removed by overwrite or erase.
//!
//! It also carries local fixes on top of upstream 0.16.2, which must be kept
//! when re-vendoring (their tests are in `local_fix_tests.rs` and
//! `hyperlink_tests.rs`):
//!
//! - Resizing keeps the cursor row on screen by moving the rows above it into
//!   scrollback, and pads scrollback rows when the width grows, so that every
//!   visible cell exists even while scrolled back.
//! - `Row::resize` clears a wide character that a narrower width cuts in half.
//! - Huge ICH, IL and SD counts are clamped to the row or scroll region, and
//!   IL, DL and RI leave the rows alone while the cursor is outside the
//!   scroll region.
//! - Text wrapping and wide characters do not panic in one-row and
//!   one-column terminals.
//! - DECSTBM homes the cursor with `set_pos`, so DECOM is honoured, and
//!   `CSI 3 J` clears the scrollback.
//! - DECSC keeps one attribute slot per buffer and never saves the active
//!   OSC 8 link, and OSC 8 URIs that vte may have truncated are ignored.
//! - [`Parser::process`] drops OSC strings longer than 8 MiB instead of
//!   buffering them.
//! - `Row::new` builds its cells without a per-element clone, and scrolled
//!   rows are reused instead of reallocated.
//!
//! # Overview
//!
//! This is essentially the terminal parser component of a graphical terminal
//! emulator pulled out into a separate crate. Although you can use this crate
//! to build a graphical terminal emulator, it also contains functionality
//! necessary for implementing terminal applications that want to run other
//! terminal applications - programs like `screen` or `tmux` for example.
//!
//! # Synopsis
//!
//! ```
//! let mut parser = vt100::Parser::new(24, 80, 0);
//!
//! let screen = parser.screen().clone();
//! parser.process(b"this text is \x1b[31mRED\x1b[m");
//! assert_eq!(
//!     parser.screen().cell(0, 13).unwrap().fgcolor(),
//!     vt100::Color::Idx(1),
//! );
//!
//! let screen = parser.screen().clone();
//! parser.process(b"\x1b[3D\x1b[32mGREEN");
//! assert_eq!(
//!     parser.screen().contents_formatted(),
//!     &b"\x1b[?25h\x1b[m\x1b[H\x1b[Jthis text is \x1b[32mGREEN"[..],
//! );
//! assert_eq!(
//!     parser.screen().contents_diff(&screen),
//!     &b"\x1b[1;14H\x1b[32mGREEN"[..],
//! );
//! ```

#![warn(missing_docs)]
#![warn(clippy::cargo)]
#![warn(clippy::pedantic)]
#![warn(clippy::nursery)]
#![warn(clippy::as_conversions)]
#![warn(clippy::get_unwrap)]
#![allow(clippy::cognitive_complexity)]
#![allow(clippy::missing_const_for_fn)]
#![allow(clippy::similar_names)]
#![allow(clippy::struct_excessive_bools)]
#![allow(clippy::too_many_arguments)]
#![allow(clippy::too_many_lines)]
#![allow(clippy::type_complexity)]

mod attrs;
mod callbacks;
mod cell;
mod grid;
mod parser;
mod perform;
mod row;
mod screen;
mod term;

#[cfg(test)]
mod hyperlink_tests;
#[cfg(test)]
mod local_fix_tests;

pub use attrs::Color;
pub use callbacks::Callbacks;
pub use cell::Cell;
pub use parser::Parser;
pub use screen::{MouseProtocolEncoding, MouseProtocolMode, Screen};
