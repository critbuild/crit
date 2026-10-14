use ratatui::layout::Rect;
use std::ops::Range;

/// A frozen visible pane, not a copy of the session's scrollback. Output keeps
/// running underneath it, so a repaint cannot change text during a drag.
pub struct Selection {
    pub session: u64,
    pub body: Rect,
    pub dragging: bool,
    pub moved: bool,
    start: (u16, u16),
    end: (u16, u16),
    cols: u16,
    rows: u16,
    cells: Vec<vt100::Cell>,
    wrapped: Vec<bool>,
}

impl Selection {
    pub fn new(session: u64, body: Rect, screen: &vt100::Screen, x: u16, y: u16) -> Self {
        let (rows, cols) = screen.size();
        let rows = rows.min(body.height);
        let cols = cols.min(body.width);
        let mut cells = Vec::with_capacity(usize::from(rows) * usize::from(cols));
        let mut wrapped = Vec::with_capacity(usize::from(rows));
        // vt100 gives every visible row at least the screen's width in cells.
        for row in screen.visible_rows().take(usize::from(rows)) {
            wrapped.push(row.wrapped());
            cells.extend(row.cells().take(usize::from(cols)).cloned());
        }
        let point = (
            y.saturating_sub(body.y).min(rows.saturating_sub(1)),
            x.saturating_sub(body.x).min(cols.saturating_sub(1)),
        );
        Self {
            session,
            body,
            dragging: true,
            moved: false,
            start: point,
            end: point,
            cols,
            rows,
            cells,
            wrapped,
        }
    }

    pub fn update(&mut self, x: u16, y: u16) {
        self.end = (
            y.saturating_sub(self.body.y)
                .min(self.rows.saturating_sub(1)),
            x.saturating_sub(self.body.x)
                .min(self.cols.saturating_sub(1)),
        );
        self.moved |= self.end != self.start;
    }

    fn cell(&self, row: u16, col: u16) -> Option<&vt100::Cell> {
        if row >= self.rows || col >= self.cols {
            return None;
        }
        self.cells
            .get(usize::from(row) * usize::from(self.cols) + usize::from(col))
    }

    fn range(&self) -> ((u16, u16), (u16, u16)) {
        let (mut start, mut end) = if self.start <= self.end {
            (self.start, self.end)
        } else {
            (self.end, self.start)
        };
        if self
            .cell(start.0, start.1)
            .is_some_and(vt100::Cell::is_wide_continuation)
        {
            start.1 = start.1.saturating_sub(1);
        }
        if self.cell(end.0, end.1).is_some_and(vt100::Cell::is_wide) {
            end.1 = end.1.saturating_add(1).min(self.cols.saturating_sub(1));
        }
        (start, end)
    }

    /// The frozen rows from the top, each with the columns the selection highlights on it
    /// (an empty range where it highlights none), so a frame draws them a row at a time.
    pub fn rows(&self) -> impl Iterator<Item = (&[vt100::Cell], Range<u16>)> {
        let ((top, left), (bottom, right)) = self.range();
        let (moved, cols) = (self.moved, self.cols);
        // `chunks` rejects a size of zero, and a snapshot without columns has no cells.
        self.cells
            .chunks(usize::from(cols).max(1))
            .zip(0..self.rows)
            .map(move |(cells, row)| {
                let highlight = if moved && (top..=bottom).contains(&row) {
                    let first = if row == top { left } else { 0 };
                    let end = if row == bottom { right + 1 } else { cols };
                    first..end
                } else {
                    0..0
                };
                (cells, highlight)
            })
    }

    pub fn text(&self) -> String {
        let (start, end) = self.range();
        let mut text = String::new();
        if self.rows == 0 || self.cols == 0 {
            return text;
        }
        for row in start.0..=end.0 {
            let first = if row == start.0 { start.1 } else { 0 };
            let last = if row == end.0 { end.1 } else { self.cols - 1 };
            for col in first..=last {
                let cell = self.cell(row, col).unwrap();
                if cell.is_wide_continuation() {
                    continue;
                }
                text.push_str(if cell.has_contents() {
                    cell.contents()
                } else {
                    " "
                });
            }
            if !self.wrapped[usize::from(row)] || row == end.0 {
                text.truncate(text.trim_end_matches(' ').len());
                if row != end.0 {
                    text.push('\n');
                }
            }
        }
        text
    }

    pub fn link(&self) -> Option<String> {
        let (row, mut col) = self.start;
        let cell = self.cell(row, col)?;
        if cell.is_wide_continuation() {
            col = col.saturating_sub(1);
        }
        if let Some(uri) = self.cell(row, col)?.hyperlink() {
            return Some(uri.to_owned());
        }
        let mut first = row;
        while first > 0 && self.wrapped[usize::from(first - 1)] {
            first -= 1;
        }
        let mut last = row;
        while last + 1 < self.rows && self.wrapped[usize::from(last)] {
            last += 1;
        }
        let mut line = String::new();
        let mut position = 0;
        for r in first..=last {
            for c in 0..self.cols {
                let cell = self.cell(r, c)?;
                if (r, c) == (row, col) {
                    position = line.len();
                }
                if !cell.is_wide_continuation() {
                    line.push_str(if cell.has_contents() {
                        cell.contents()
                    } else {
                        " "
                    });
                }
            }
        }
        let separator = |c: char| c.is_whitespace() || matches!(c, '<' | '>' | '"' | '\'' | '`');
        let start = line[..position]
            .rfind(separator)
            .map_or(0, |i| i + line[i..].chars().next().unwrap().len_utf8());
        let end = line[position..]
            .find(separator)
            .map_or(line.len(), |i| position + i);
        let token = &line[start..end];
        let offset = ["https://", "http://", "mailto:", "file://"]
            .into_iter()
            .filter_map(|scheme| token.find(scheme))
            .min()?;
        let mut uri = token[offset..].trim_end_matches(['.', ',', ';', ':', '!']);
        for (open, close) in [('(', ')'), ('[', ']'), ('{', '}')] {
            while uri.ends_with(close) && uri.matches(close).count() > uri.matches(open).count() {
                uri = &uri[..uri.len() - 1];
            }
        }
        let link_start = start + offset;
        (position >= link_start && position < link_start + uri.len()).then(|| uri.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn selection_preserves_unicode_wraps_and_frozen_output() {
        let mut parser = vt100::Parser::new(4, 8, 4);
        parser.process("ab界e\u{301}fghijk\r\nnext".as_bytes());
        let mut selection = Selection::new(1, Rect::new(1, 3, 8, 4), parser.screen(), 3, 3);
        selection.update(4, 5);
        parser.process(b"\x1b[2Jreplaced");
        assert_eq!(selection.text(), "界e\u{301}fghijk\nnext");
    }
    #[test]
    fn links_join_wrapped_rows_and_exclude_surrounding_punctuation() {
        let mut parser = vt100::Parser::new(5, 14, 4);
        parser.process(b"See (https://example.org/a(b)).");
        let selection = Selection::new(1, Rect::new(0, 0, 14, 5), parser.screen(), 4, 1);
        assert_eq!(
            selection.link().as_deref(),
            Some("https://example.org/a(b)")
        );
    }
    #[test]
    fn selection_survives_widened_scrollback() {
        let mut parser = vt100::Parser::new(4, 40, 100);
        for line in 0..20 {
            parser.process(format!("line-{line:02}\r\n").as_bytes());
        }
        parser.screen_mut().set_size(4, 80);
        parser.screen_mut().set_scrollback(6);
        let mut selection = Selection::new(1, Rect::new(1, 3, 80, 4), parser.screen(), 1, 3);
        selection.update(80, 6);
        assert_eq!(selection.text(), "line-11\nline-12\nline-13\nline-14");
    }
    #[test]
    fn highlights_cover_whole_wide_characters_and_full_middle_rows() {
        let mut parser = vt100::Parser::new(4, 8, 0);
        parser.process("one\r\ntwo\r\nab界def\r\n界x".as_bytes());
        let body = Rect::new(2, 1, 8, 4);
        let highlights = |selection: &Selection| {
            selection
                .rows()
                .map(|(_, columns)| columns)
                .collect::<Vec<_>>()
        };
        // Nothing shows before the pointer moves; ending on the left half of 界 takes both.
        let mut selection = Selection::new(1, body, parser.screen(), 3, 1);
        assert_eq!(highlights(&selection), [0..0, 0..0, 0..0, 0..0]);
        selection.update(4, 3);
        assert_eq!(highlights(&selection), [1..8, 0..8, 0..4, 0..0]);
        assert_eq!(selection.text(), "ne\ntwo\nab界");
        // Starting on the right half of 界 takes both as well.
        let mut selection = Selection::new(1, body, parser.screen(), 5, 3);
        selection.update(4, 4);
        assert_eq!(highlights(&selection), [0..0, 0..0, 2..8, 0..3]);
        assert_eq!(selection.text(), "界def\n界x");
    }
}
