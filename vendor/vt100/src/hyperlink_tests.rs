use crate::Parser;

const URI: &str = "https://example.com/report";
const START: &[u8] = b"\x1b]8;id=report;https://example.com/report\x1b\\";
const CLOSE: &[u8] = b"\x1b]8;;\x1b\\";

fn link(parser: &Parser, row: u16, col: u16) -> Option<&str> {
    parser.screen().cell(row, col).unwrap().hyperlink()
}

#[test]
fn labelled_link_closes_and_survives_sgr_reset() {
    let mut parser = Parser::new(3, 40, 4);
    parser.process(START);
    parser.process(b"Read\x1b[0m report");
    parser.process(CLOSE);
    parser.process(b" plain");
    assert_eq!(parser.screen().cell(0, 0).unwrap().contents(), "R");
    for col in 0..11 {
        assert_eq!(link(&parser, 0, col), Some(URI));
    }
    assert_eq!(link(&parser, 0, 11), None);
}

#[test]
fn links_follow_scrollback_and_are_evicted_with_cells() {
    let mut parser = Parser::new(2, 20, 2);
    parser.process(START);
    parser.process(b"label");
    parser.process(CLOSE);
    parser.process(b"\r\nsecond\r\nthird");
    assert_eq!(link(&parser, 0, 0), None);
    parser.screen_mut().set_scrollback(1);
    assert_eq!(link(&parser, 0, 0), Some(URI));
    parser.screen_mut().set_scrollback(0);
    parser.process(b"\r\nfourth\r\nfifth");
    parser.screen_mut().set_scrollback(2);
    assert_eq!(link(&parser, 0, 0), None);
}

#[test]
fn overwrites_and_erases_remove_links_even_while_link_is_active() {
    let mut parser = Parser::new(2, 20, 0);
    parser.process(START);
    parser.process(b"linked");
    parser.process(b"\r\x1b[2X");
    assert_eq!(link(&parser, 0, 0), None);
    assert_eq!(link(&parser, 0, 1), None);
    assert_eq!(link(&parser, 0, 2), Some(URI));
    parser.process(CLOSE);
    parser.process(b"XY");
    assert_eq!(link(&parser, 0, 0), None);
    assert_eq!(link(&parser, 0, 1), None);
    parser.process(b"\x1b[K");
    for col in 2..20 {
        assert_eq!(link(&parser, 0, col), None);
    }
    parser.process(START);
    parser.process(b"new\x1b[2J");
    for row in 0..2 {
        for col in 0..20 {
            assert_eq!(link(&parser, row, col), None);
        }
    }
}

#[test]
fn alternate_screen_retains_primary_cells_and_clears_reentered_grid() {
    let mut parser = Parser::new(2, 20, 0);
    parser.process(START);
    parser.process(b"primary");
    parser.process(CLOSE);
    parser.process(b"\x1b[?1049h\x1b[H\x1b]8;;https://alternate.example/\x1b\\alt\x1b]8;;\x1b\\");
    assert_eq!(link(&parser, 0, 0), Some("https://alternate.example/"));
    parser.process(b"\x1b[?1049l");
    assert_eq!(link(&parser, 0, 0), Some(URI));
    parser.process(b"\x1b[?1049h");
    assert_eq!(link(&parser, 0, 0), None);
}

#[test]
fn osc_sequences_work_when_split_at_every_byte() {
    let bytes = b"\x1b]8;;https://example.com/a;b\x1b\\label\x1b]8;;\x07plain";
    for split in 0..=bytes.len() {
        let mut parser = Parser::new(2, 40, 0);
        parser.process(&bytes[..split]);
        parser.process(&bytes[split..]);
        assert_eq!(link(&parser, 0, 0), Some("https://example.com/a;b"));
        assert_eq!(link(&parser, 0, 4), Some("https://example.com/a;b"));
        assert_eq!(link(&parser, 0, 5), None);
    }
    let mut parser = Parser::new(2, 40, 0);
    for byte in bytes {
        parser.process(std::slice::from_ref(byte));
    }
    assert_eq!(link(&parser, 0, 0), Some("https://example.com/a;b"));
    assert_eq!(link(&parser, 0, 5), None);
}

#[test]
fn wide_and_combining_cells_keep_links_and_clear_both_halves() {
    let mut parser = Parser::new(2, 20, 0);
    parser.process(START);
    parser.process("界e\u{301}".as_bytes());
    parser.process(CLOSE);
    assert_eq!(link(&parser, 0, 0), Some(URI));
    assert_eq!(link(&parser, 0, 1), Some(URI));
    assert!(parser.screen().cell(0, 1).unwrap().is_wide_continuation());
    assert_eq!(parser.screen().cell(0, 2).unwrap().contents(), "e\u{301}");
    assert_eq!(link(&parser, 0, 2), Some(URI));
    parser.process(b"\x1b[1;2Hx");
    assert_eq!(link(&parser, 0, 0), None);
    assert_eq!(link(&parser, 0, 1), None);
    assert_eq!(link(&parser, 0, 2), Some(URI));
}

#[test]
fn resize_keeps_retained_links_and_clears_truncated_wide_cells() {
    let mut parser = Parser::new(2, 8, 0);
    parser.process(START);
    parser.process("ab界".as_bytes());
    parser.process(CLOSE);
    parser.screen_mut().set_size(3, 12);
    assert_eq!(link(&parser, 0, 0), Some(URI));
    assert_eq!(link(&parser, 0, 2), Some(URI));
    assert_eq!(link(&parser, 0, 3), Some(URI));
    assert_eq!(link(&parser, 0, 8), None);
    parser.screen_mut().set_size(3, 3);
    assert_eq!(link(&parser, 0, 0), Some(URI));
    assert_eq!(link(&parser, 0, 2), None);
}

#[test]
fn malformed_osc8_does_not_leak_previous_link() {
    for malformed in [
        &b"\x1b]8;missing-uri\x1b\\"[..],
        &b"\x1b]8;;https://example.com/\xff\x1b\\"[..],
        &b"\x1b]8;;https://example.com/\xc2\x85\x1b\\"[..],
    ] {
        let mut parser = Parser::new(2, 20, 0);
        parser.process(START);
        parser.process(b"a");
        parser.process(malformed);
        parser.process(b"b");
        assert_eq!(link(&parser, 0, 0), Some(URI));
        assert_eq!(link(&parser, 0, 1), None);
    }
}

#[test]
fn formatted_contents_and_diffs_roundtrip_cell_links() {
    let mut parser = Parser::new(2, 20, 0);
    parser.process(START);
    parser.process(b"label");
    parser.process(CLOSE);
    parser.process(b" plain");
    let before = parser.screen().clone();
    let mut replay = Parser::new(2, 20, 0);
    replay.process(&before.contents_formatted());
    assert_eq!(link(&replay, 0, 0), Some(URI));
    assert_eq!(link(&replay, 0, 5), None);
    parser.process(b"\r\x1b[2X");
    replay.process(&parser.screen().contents_diff(&before));
    assert_eq!(link(&replay, 0, 0), None);
    assert_eq!(link(&replay, 0, 2), Some(URI));
}
