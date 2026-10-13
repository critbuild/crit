// Regression tests for the local emulator fixes listed in the crate docs.
use crate::parser::MAX_OSC_LEN;
use crate::{Callbacks, Color, Parser, Screen};
use std::time::{Duration, Instant};

/// The number of rows in the scrollback, leaving the view where it was.
fn history_rows(parser: &mut Parser) -> usize {
    let offset = parser.screen().scrollback();
    parser.screen_mut().set_scrollback(usize::MAX);
    let rows = parser.screen().scrollback();
    parser.screen_mut().set_scrollback(offset);
    rows
}

#[test]
fn widening_pads_scrollback_rows() {
    let mut parser = Parser::new(4, 10, 100);
    for line in 0..20 {
        parser.process(format!("line-{line:02}\r\n").as_bytes());
    }
    parser.screen_mut().set_size(4, 20);
    parser.screen_mut().set_scrollback(6);
    for row in 0..4 {
        for col in 0..20 {
            assert!(
                parser.screen().cell(row, col).is_some(),
                "cell({row}, {col})"
            );
        }
        assert!(parser.screen().cell(row, 20).is_none());
    }
    assert_eq!(
        parser.screen().contents(),
        "line-11\nline-12\nline-13\nline-14"
    );

    // history is only padded: narrowing must not cut it, so that widening
    // again shows the text again
    parser.screen_mut().set_size(4, 5);
    assert_eq!(parser.screen().contents(), "line-\nline-\nline-\nline-");
    parser.screen_mut().set_size(4, 10);
    assert_eq!(
        parser.screen().contents(),
        "line-11\nline-12\nline-13\nline-14"
    );

    // rows that a shrinking resize moves into history are padded as well
    let mut parser = Parser::new(6, 10, 100);
    parser.process(b"a\r\nb\r\nc\r\nd\r\ne\r\nf");
    parser.screen_mut().set_size(3, 20);
    parser.screen_mut().set_scrollback(3);
    assert_eq!(parser.screen().contents(), "a\nb\nc");
    for row in 0..3 {
        assert!(parser.screen().cell(row, 19).is_some());
    }
}

#[test]
fn shrinking_moves_the_top_rows_into_scrollback() {
    let mut parser = Parser::new(5, 20, 100);
    parser.process(b"out1\r\nout2\r\nout3\r\nERROR: build failed\r\n$ ");
    parser.screen_mut().set_size(4, 20);
    assert_eq!(
        parser.screen().contents(),
        "out2\nout3\nERROR: build failed\n$ "
    );
    assert_eq!(parser.screen().cursor_position(), (3, 2));
    assert_eq!(history_rows(&mut parser), 1);
    parser.screen_mut().set_scrollback(1);
    assert_eq!(
        parser.screen().contents(),
        "out1\nout2\nout3\nERROR: build failed"
    );

    let mut parser = Parser::new(6, 20, 100);
    parser.process(b"a\r\nb\r\nc\r\nd\r\ne\r\nf");
    parser.screen_mut().set_size(3, 20);
    assert_eq!(parser.screen().contents(), "d\ne\nf");
    assert_eq!(parser.screen().cursor_position(), (2, 1));
    assert_eq!(history_rows(&mut parser), 3);
    parser.screen_mut().set_scrollback(3);
    assert_eq!(parser.screen().contents(), "a\nb\nc");

    // the alternate screen has no scrollback: the rows are dropped
    let mut parser = Parser::new(4, 10, 100);
    parser.process(b"\x1b[?1049h\x1b[1;1Ha\x1b[2;1Hb\x1b[3;1Hc\x1b[4;1Hd");
    parser.screen_mut().set_size(2, 10);
    assert_eq!(parser.screen().contents(), "c\nd");
    assert_eq!(parser.screen().cursor_position(), (1, 1));
    assert_eq!(history_rows(&mut parser), 0);
}

#[test]
fn shrinking_keeps_a_scrolled_back_view_in_place() {
    let mut parser = Parser::new(4, 10, 100);
    for line in 0..10 {
        parser.process(format!("line-{line}\r\n").as_bytes());
    }
    parser.screen_mut().set_scrollback(2);
    assert_eq!(parser.screen().contents(), "line-5\nline-6\nline-7\nline-8");
    parser.screen_mut().set_size(3, 10);
    assert_eq!(parser.screen().scrollback(), 3);
    assert_eq!(parser.screen().contents(), "line-5\nline-6\nline-7");
}

fn filled(rows: u16, cols: u16) -> Parser {
    let mut parser = Parser::new(rows, cols, 0);
    let lines: Vec<String> = (0..rows)
        .map(|row| {
            format!("row-{row:02} {}", "x".repeat(usize::from(cols) - 7))
        })
        .collect();
    parser.process(lines.join("\r\n").as_bytes());
    parser
}

#[test]
fn huge_ich_il_and_sd_counts_are_clamped() {
    // the counts past the row or the scroll region change nothing
    let run = |ops: &str| {
        let mut parser = filled(24, 80);
        parser.process(ops.as_bytes());
        parser.screen().contents_formatted()
    };
    for (huge, exact) in [
        ("\x1b[5;10H\x1b[65535@", "\x1b[5;10H\x1b[80@"),
        (
            "\x1b[3;20r\x1b[5;1H\x1b[65535L",
            "\x1b[3;20r\x1b[5;1H\x1b[24L",
        ),
        ("\x1b[3;20r\x1b[65535T", "\x1b[3;20r\x1b[24T"),
    ] {
        assert_eq!(run(huge), run(exact), "{huge:?}");
    }

    // unclamped, the ICH alone takes over a second
    let mut parser = filled(24, 80);
    let start = Instant::now();
    parser.process(b"\x1b[65535@\x1b[65535L\x1b[65535T");
    let elapsed = start.elapsed();
    assert!(elapsed < Duration::from_millis(500), "{elapsed:?}");
    assert_eq!(
        parser.screen().contents_formatted(),
        run("\x1b[80@\x1b[24L\x1b[24T")
    );
}

#[test]
fn one_row_and_one_column_terminals_do_not_panic() {
    for scrollback in [0, 100] {
        let mut parser = Parser::new(1, 10, scrollback);
        parser.process(b"12345678901");
        assert_eq!(parser.screen().contents(), "1");
    }

    // a wide character cannot be placed, the rest wraps normally
    let mut parser = Parser::new(5, 1, 0);
    parser.process("界abc".as_bytes());
    assert_eq!(parser.screen().contents(), "abc");

    let mut parser = Parser::new(24, 80, 100);
    parser.process(b"hello\r\n");
    parser.screen_mut().set_size(1, 80);
    parser.process(&[b'x'; 81]);
    assert_eq!(parser.screen().contents(), "x");
}

#[test]
fn il_dl_and_ri_outside_the_scroll_region_leave_the_rows_alone() {
    let run = |ops: &str| {
        let mut parser = Parser::new(6, 10, 0);
        parser.process(b"r0\r\nr1\r\nr2\r\nr3\r\nr4\r\nr5");
        parser.process(ops.as_bytes());
        parser.screen().contents()
    };
    let unchanged = "r0\nr1\nr2\nr3\nr4\nr5";
    // IL below the region, DL below the region and RI above the region
    assert_eq!(run("\x1b[2;3r\x1b[6;1H\x1b[L"), unchanged);
    assert_eq!(run("\x1b[2;3r\x1b[5;1H\x1b[M"), unchanged);
    assert_eq!(run("\x1b[4;6r\x1b[H\x1bM"), unchanged);

    // they still work inside of it
    assert_eq!(run("\x1b[2;3r\x1b[2;1H\x1b[L"), "r0\n\nr1\nr3\nr4\nr5");
    assert_eq!(run("\x1b[2;3r\x1b[2;1H\x1b[M"), "r0\nr2\n\nr3\nr4\nr5");
    assert_eq!(run("\x1b[2;3r\x1b[2;1H\x1bM"), "r0\n\nr1\nr3\nr4\nr5");
}

#[test]
fn decstbm_homes_the_cursor_like_cup() {
    let mut parser = Parser::new(10, 10, 0);
    parser.process(b"\x1b[3;3H\x1b[5;8r");
    assert_eq!(parser.screen().cursor_position(), (0, 0));

    // with origin mode on, home is the top of the new region
    let mut parser = Parser::new(10, 10, 0);
    parser.process(b"\x1b[3;3H\x1b[?6h\x1b[5;8r");
    assert_eq!(parser.screen().cursor_position(), (4, 0));
}

#[test]
fn decsc_attributes_are_saved_per_buffer() {
    let mut parser = Parser::new(5, 40, 0);
    parser
        .process(b"$ tui\r\n\x1b[?1049h\x1b[31;1m\x1b7\x1b[0mTUI\x1b[?1049l");
    parser.process(b"$ ls\r\nfile.txt\r\n");
    for row in [1, 2] {
        let cell = parser.screen().cell(row, 0).unwrap();
        assert_eq!(cell.fgcolor(), Color::Default);
        assert!(!cell.bold());
    }

    // inside the alternate screen DECSC and DECRC still pair up
    parser.process(b"\x1b[?1049h\x1b[H\x1b[32m\x1b7\x1b[0m\x1b8x");
    let cell = parser.screen().cell(0, 0).unwrap();
    assert_eq!(cell.contents(), "x");
    assert_eq!(cell.fgcolor(), Color::Idx(2));
}

#[test]
fn csi_3_j_clears_the_scrollback() {
    let fill = || {
        let mut parser = Parser::new(3, 20, 100);
        for line in 0..10 {
            parser.process(format!("secret-{line}\r\n").as_bytes());
        }
        parser
    };
    let mut parser = fill();
    assert!(history_rows(&mut parser) > 0);
    parser.process(b"\x1b[H\x1b[2J\x1b[3J");
    assert_eq!(history_rows(&mut parser), 0);
    parser.screen_mut().set_scrollback(usize::MAX);
    assert_eq!(parser.screen().contents(), "");

    // the view returns to the live screen, also for the DEC variant
    let mut parser = fill();
    parser.screen_mut().set_scrollback(2);
    parser.process(b"\x1b[?3J");
    assert_eq!(parser.screen().scrollback(), 0);
    assert_eq!(history_rows(&mut parser), 0);
}

#[test]
fn reused_rows_are_blank_and_match_the_current_width() {
    let mut parser = Parser::new(3, 12, 2);
    parser.process(b"\x1b]8;;https://example.com/\x1b\\\x1b[31;1m");
    for _ in 0..6 {
        parser.process(b"LINKLINKLINK\r\n");
    }
    parser.process(b"\x1b]8;;\x1b\\\x1b[0m");
    // the two rows left in history are 12 columns wide, and still styled
    // and linked, when two scrolls evict them and hand them to the screen
    parser.screen_mut().set_size(3, 6);
    parser.process(b"a\r\nb\r\n");
    assert_eq!(parser.screen().contents(), "a\nb");
    for row in 0..3 {
        for col in 0..6 {
            let cell = parser.screen().cell(row, col).unwrap();
            assert_eq!(cell.hyperlink(), None, "cell({row}, {col})");
            assert_eq!(cell.fgcolor(), Color::Default);
            assert!(!cell.bold());
        }
        assert!(parser.screen().cell(row, 6).is_none());
    }
}

#[derive(Default)]
struct Recorder {
    /// The type and the length of the data of every OSC 52 copy.
    copies: Vec<(Vec<u8>, usize)>,
    titles: Vec<Vec<u8>>,
}

impl Callbacks for Recorder {
    fn copy_to_clipboard(&mut self, _: &mut Screen, ty: &[u8], data: &[u8]) {
        self.copies.push((ty.to_vec(), data.len()));
    }

    fn set_window_title(&mut self, _: &mut Screen, title: &[u8]) {
        self.titles.push(title.to_vec());
    }
}

/// The start of an OSC 52 copy with `len` bytes of base64 data, unterminated.
fn osc52(introducer: &[u8], len: usize) -> Vec<u8> {
    let mut bytes = introducer.to_vec();
    bytes.extend_from_slice(b"52;c;");
    bytes.resize(bytes.len() + len, b'A');
    bytes
}

#[test]
fn oversized_osc_strings_are_dropped_without_dispatch() {
    // (introducer, terminator, size of the pieces the output is processed in)
    for (introducer, end, piece) in [
        (&b"\x1b]"[..], &b"\x07"[..], usize::MAX),
        (&b"\x1b]"[..], &b"\x1b\\"[..], 65_539),
        // vte stays in its escape state through DEL, and SUB ends the string
        (&b"\x1b\x7f]"[..], &b"\x1a"[..], 1 << 20),
    ] {
        let mut input = osc52(introducer, MAX_OSC_LEN);
        input.extend_from_slice(end);
        input.extend_from_slice(b"ok\x1b]2;t\x07");
        let mut parser =
            Parser::new_with_callbacks(3, 20, 0, Recorder::default());
        for chunk in input.chunks(piece) {
            parser.process(chunk);
        }
        assert_eq!(parser.callbacks().copies.len(), 0);
        assert_eq!(parser.screen().contents(), "ok");
        assert_eq!(parser.callbacks().titles, [b"t".to_vec()]);
    }
}

#[test]
fn osc_introducers_follow_vtes_escape_state() {
    // the cap tracks vte's escape state: these are the bytes after an ESC
    // that leave vte waiting for a final byte (so that `]` opens an OSC)
    for (prefix, opens) in [
        (&b"\x1b"[..], true),
        (b"\x1b\x1b", true),
        (b"\x1b\n", true),
        (b"\x1b\x7f", true),
        (b"\x1b\x80", true),
        ("\x1b\u{e9}".as_bytes(), true),
        (b"\x1b\x18", false),
        (b"\x1b\x1a", false),
        (b"\x1b(", false),
        (b"\x1b[", false),
    ] {
        let mut parser =
            Parser::new_with_callbacks(3, 20, 0, Recorder::default());
        parser.process(prefix);
        parser.process(b"]2;t\x07");
        let titles = parser.callbacks().titles.len();
        assert_eq!(titles, usize::from(opens), "{prefix:?}");
    }
}

#[test]
fn osc_strings_under_the_cap_are_dispatched() {
    let mut input = osc52(b"\x1b]", 1 << 20);
    input.push(7);
    let mut parser = Parser::new_with_callbacks(3, 20, 0, Recorder::default());
    for chunk in input.chunks(4093) {
        parser.process(chunk);
    }
    assert_eq!(parser.callbacks().copies, [(b"c".to_vec(), 1 << 20)]);

    // the introducer and the body may arrive one byte at a time
    for byte in b"\x1b]2;title\x1b\\" {
        parser.process(std::slice::from_ref(byte));
    }
    assert_eq!(parser.callbacks().titles, [b"title".to_vec()]);
}
