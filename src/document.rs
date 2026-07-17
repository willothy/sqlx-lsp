//! In-memory text documents with position conversions.
//!
//! Three coordinate systems meet here: LSP positions are 0-based
//! `(line, character)` pairs counted in the session's negotiated encoding
//! (UTF-16 code units by default), Rust string offsets are UTF-8 bytes, and
//! sqlparser locations are 1-based `(line, column)` pairs counted in Unicode
//! scalar values. [`Document`] owns the text of one open file and converts
//! between all three.

use std::sync::atomic::{AtomicBool, Ordering};

use sqlparser::tokenizer::{Location as SqlLocation, Span as SqlSpan};
use tower_lsp_server::ls_types::{Position, Range};

/// How `Position.character` counts columns within a line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PositionEncoding {
    /// UTF-16 code units — the protocol's mandatory default.
    Utf16,
    /// UTF-8 bytes, negotiated with clients that prefer it.
    Utf8,
}

/// Whether positions count UTF-8 bytes. Session-wide: the process serves a
/// single client, and the encoding is fixed at `initialize`, before any
/// document conversion runs.
static UTF8_POSITIONS: AtomicBool = AtomicBool::new(false);

/// Fixes the session's position encoding. Called once during `initialize`.
pub fn set_position_encoding(encoding: PositionEncoding) {
    UTF8_POSITIONS.store(encoding == PositionEncoding::Utf8, Ordering::Relaxed);
}

/// The `Position.character` width of `ch` under the session encoding.
fn encoded_len(ch: char) -> u32 {
    if UTF8_POSITIONS.load(Ordering::Relaxed) {
        ch.len_utf8() as u32
    } else {
        ch.len_utf16() as u32
    }
}

/// The byte-level shape of one applied content change, in the coordinates
/// incremental reparsers (tree-sitter) consume: byte offsets plus 0-based
/// (row, byte column) points.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AppliedEdit {
    /// Byte offset where the change starts.
    pub start_byte: usize,
    /// Byte offset where the replaced text ended, in the old text.
    pub old_end_byte: usize,
    /// Byte offset where the inserted text ends, in the new text.
    pub new_end_byte: usize,
    /// (row, byte column) of the change start.
    pub start_point: (usize, usize),
    /// (row, byte column) where the replaced text ended, in the old text.
    pub old_end_point: (usize, usize),
    /// (row, byte column) where the inserted text ends, in the new text.
    pub new_end_point: (usize, usize),
}

/// The text of one open document.
#[derive(Debug, Clone)]
pub struct Document {
    text: String,
    /// Byte offset of the first character of each line. Always non-empty;
    /// `line_starts[0]` is `0`.
    line_starts: Vec<usize>,
}

impl Document {
    /// Creates a document from its full text.
    pub fn new(text: String) -> Self {
        let line_starts = Self::compute_line_starts(&text);
        Document { text, line_starts }
    }

    /// Replaces the document contents (full-text synchronization).
    pub fn update(&mut self, text: String) {
        self.line_starts = Self::compute_line_starts(&text);
        self.text = text;
    }

    /// Applies an incremental content change, replacing `range` with
    /// `new_text`. Out-of-range positions clamp the way [`Self::offset_at`]
    /// clamps. Returns the byte-level shape of the edit so incremental
    /// reparsers can shift their trees.
    pub fn apply_change(&mut self, range: Range, new_text: &str) -> AppliedEdit {
        let start_byte = self.offset_at(range.start);
        let old_end_byte = self.offset_at(range.end).max(start_byte);
        let start_point = self.byte_point(start_byte);
        let old_end_point = self.byte_point(old_end_byte);

        self.text.replace_range(start_byte..old_end_byte, new_text);
        self.line_starts = Self::compute_line_starts(&self.text);

        let new_end_byte = start_byte + new_text.len();
        AppliedEdit {
            start_byte,
            old_end_byte,
            new_end_byte,
            start_point,
            old_end_point,
            new_end_point: self.byte_point(new_end_byte),
        }
    }

    /// The 0-based (row, byte column) of a byte offset in the current text.
    fn byte_point(&self, offset: usize) -> (usize, usize) {
        // `line_starts[0]` is 0, so the partition point is at least 1.
        let row = self.line_starts.partition_point(|&start| start <= offset) - 1;
        (row, offset - self.line_starts[row])
    }

    /// The current document text.
    pub fn text(&self) -> &str {
        &self.text
    }

    fn compute_line_starts(text: &str) -> Vec<usize> {
        let mut starts = vec![0];
        starts.extend(
            text.bytes()
                .enumerate()
                .filter(|&(_, byte)| byte == b'\n')
                .map(|(index, _)| index + 1),
        );
        starts
    }

    /// The byte range of a 0-based line, excluding its trailing newline.
    /// Returns `None` when `line` is past the end of the document.
    fn line_span(&self, line: usize) -> Option<(usize, usize)> {
        let start = *self.line_starts.get(line)?;
        let end = self
            .line_starts
            .get(line + 1)
            .map(|&next_start| next_start - 1)
            .unwrap_or(self.text.len());
        Some((start, end))
    }

    /// Converts an LSP position to a byte offset.
    ///
    /// Out-of-range positions are clamped as the LSP specification prescribes:
    /// a character past the end of its line maps to the end of that line, and
    /// a line past the end of the document maps to the end of the text.
    pub fn offset_at(&self, position: Position) -> usize {
        let Some((line_start, line_end)) = self.line_span(position.line as usize) else {
            return self.text.len();
        };
        let line_text = &self.text[line_start..line_end];

        let mut character_offset = 0u32;
        for (byte_offset, ch) in line_text.char_indices() {
            if character_offset >= position.character {
                return line_start + byte_offset;
            }
            character_offset += encoded_len(ch);
        }
        line_end
    }

    /// Converts a 1-based sqlparser location to an LSP position.
    ///
    /// Returns `None` for the empty location (line 0), which sqlparser uses
    /// when it has no source information. Columns are clamped to the end of
    /// the line; a column one past the final character is valid and denotes
    /// the exclusive end of a token.
    pub fn position_of(&self, location: SqlLocation) -> Option<Position> {
        if location.line == 0 || location.column == 0 {
            return None;
        }
        let line = (location.line - 1) as usize;
        let (line_start, line_end) = self.line_span(line)?;
        let line_text = &self.text[line_start..line_end];

        let target_chars = (location.column - 1) as usize;
        let mut character_offset = 0u32;
        for (chars_seen, ch) in line_text.chars().enumerate() {
            if chars_seen == target_chars {
                break;
            }
            character_offset += encoded_len(ch);
        }
        Some(Position {
            line: line as u32,
            character: character_offset,
        })
    }

    /// Converts a sqlparser span to an LSP range.
    ///
    /// sqlparser span ends point one past the final character of the token,
    /// which matches the LSP's exclusive range end directly.
    pub fn range_of(&self, span: SqlSpan) -> Option<Range> {
        let start = self.position_of(span.start)?;
        let end = self.position_of(span.end)?;
        Some(Range { start, end })
    }

    /// Converts a byte offset to an LSP position.
    ///
    /// Offsets past the end of the text clamp to the end; an offset inside a
    /// multi-byte character maps to the position of that character.
    pub fn position_at(&self, offset: usize) -> Position {
        let offset = offset.min(self.text.len());
        // `line_starts[0]` is 0, so the partition point is at least 1.
        let line = self.line_starts.partition_point(|&start| start <= offset) - 1;
        let line_start = self.line_starts[line];
        let mut character = 0u32;
        for (byte, ch) in self.text[line_start..].char_indices() {
            if line_start + byte >= offset {
                break;
            }
            character += encoded_len(ch);
        }
        Position {
            line: line as u32,
            character,
        }
    }

    /// The length of a 0-based line in the session's position encoding,
    /// excluding its trailing newline. Returns 0 for lines past the end of
    /// the document.
    pub fn line_len(&self, line: u32) -> u32 {
        let Some((start, end)) = self.line_span(line as usize) else {
            return 0;
        };
        self.text[start..end].chars().map(encoded_len).sum()
    }

    /// Whether the LSP `position` falls inside the sqlparser `span`
    /// (start inclusive, end exclusive).
    pub fn position_in_span(&self, position: Position, span: SqlSpan) -> bool {
        let Some(range) = self.range_of(span) else {
            return false;
        };
        (position >= range.start) && (position < range.end)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlparser::dialect::SQLiteDialect;
    use sqlparser::tokenizer::Tokenizer;

    fn doc(text: &str) -> Document {
        Document::new(text.to_owned())
    }

    #[test]
    fn offset_at_ascii() {
        let d = doc("SELECT 1;\nSELECT 2;");
        assert_eq!(d.offset_at(Position::new(0, 0)), 0);
        assert_eq!(d.offset_at(Position::new(0, 7)), 7);
        assert_eq!(d.offset_at(Position::new(1, 0)), 10);
        assert_eq!(d.offset_at(Position::new(1, 7)), 17);
    }

    #[test]
    fn offset_at_clamps_out_of_range() {
        let d = doc("ab\ncd");
        // Past end of line clamps to the line end (before the newline).
        assert_eq!(d.offset_at(Position::new(0, 99)), 2);
        // Past end of document clamps to the text length.
        assert_eq!(d.offset_at(Position::new(9, 0)), 5);
    }

    #[test]
    fn offset_at_multibyte() {
        // '😀' is 4 UTF-8 bytes and 2 UTF-16 code units.
        let d = doc("😀 x");
        assert_eq!(d.offset_at(Position::new(0, 0)), 0);
        assert_eq!(d.offset_at(Position::new(0, 2)), 4);
        assert_eq!(d.offset_at(Position::new(0, 3)), 5);
    }

    #[test]
    fn apply_change_replaces_ranges_and_reports_edit_shape() {
        let mut d = doc("SELECT id\nFROM users");
        // Replace `id` with `email`.
        let edit = d.apply_change(
            Range::new(Position::new(0, 7), Position::new(0, 9)),
            "email",
        );
        assert_eq!(d.text(), "SELECT email\nFROM users");
        assert_eq!(edit.start_byte, 7);
        assert_eq!(edit.old_end_byte, 9);
        assert_eq!(edit.new_end_byte, 12);
        assert_eq!(edit.start_point, (0, 7));
        assert_eq!(edit.old_end_point, (0, 9));
        assert_eq!(edit.new_end_point, (0, 12));

        // A multi-line insertion moves the end point to a later row.
        let edit = d.apply_change(
            Range::new(Position::new(1, 10), Position::new(1, 10)),
            "\nWHERE id = 1",
        );
        assert_eq!(d.text(), "SELECT email\nFROM users\nWHERE id = 1");
        assert_eq!(edit.start_point, (1, 10));
        assert_eq!(edit.new_end_point, (2, 12));

        // Deleting across lines shrinks the text.
        let edit = d.apply_change(Range::new(Position::new(0, 12), Position::new(2, 0)), "");
        assert_eq!(d.text(), "SELECT emailWHERE id = 1");
        assert_eq!(edit.new_end_byte, edit.start_byte);
    }

    #[test]
    fn apply_change_handles_multibyte_positions() {
        // '😀' is 4 UTF-8 bytes, 2 UTF-16 units.
        let mut d = doc("😀 x");
        d.apply_change(Range::new(Position::new(0, 3), Position::new(0, 4)), "yz");
        assert_eq!(d.text(), "😀 yz");
    }

    #[test]
    fn position_of_counts_chars_not_bytes() {
        // sqlparser columns count scalar values: 'é' is 1 column but the
        // LSP character offset for what follows is still 1 (BMP char).
        let d = doc("é😀x");
        assert_eq!(
            d.position_of(SqlLocation { line: 1, column: 1 }),
            Some(Position::new(0, 0))
        );
        assert_eq!(
            d.position_of(SqlLocation { line: 1, column: 2 }),
            Some(Position::new(0, 1))
        );
        // '😀' occupies 1 sqlparser column but 2 UTF-16 units.
        assert_eq!(
            d.position_of(SqlLocation { line: 1, column: 3 }),
            Some(Position::new(0, 3))
        );
    }

    #[test]
    fn position_of_rejects_empty_location() {
        let d = doc("SELECT 1;");
        assert_eq!(d.position_of(SqlLocation { line: 0, column: 0 }), None);
    }

    #[test]
    fn tokenizer_span_ends_are_exclusive() {
        // Guards the assumption `range_of` is built on: sqlparser reports a
        // token's end as one column past its final character.
        let d = doc("SELECT name");
        let dialect = SQLiteDialect {};
        let tokens = Tokenizer::new(&dialect, d.text())
            .tokenize_with_location()
            .expect("tokenizes");
        let select = &tokens[0];
        let range = d.range_of(select.span).expect("in range");
        assert_eq!(range.start, Position::new(0, 0));
        assert_eq!(range.end, Position::new(0, 6));

        assert!(d.position_in_span(Position::new(0, 0), select.span));
        assert!(d.position_in_span(Position::new(0, 5), select.span));
        assert!(!d.position_in_span(Position::new(0, 6), select.span));
    }
}
