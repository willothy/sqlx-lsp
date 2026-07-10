//! In-memory text documents with position conversions.
//!
//! Three coordinate systems meet here: LSP positions are 0-based
//! `(line, UTF-16 code unit)` pairs, Rust string offsets are UTF-8 bytes, and
//! sqlparser locations are 1-based `(line, column)` pairs counted in Unicode
//! scalar values. [`Document`] owns the text of one open file and converts
//! between all three.

use sqlparser::tokenizer::{Location as SqlLocation, Span as SqlSpan};
use tower_lsp::lsp_types::{Position, Range};

/// The text and version of one open document.
#[derive(Debug, Clone)]
pub struct Document {
    text: String,
    version: i32,
    /// Byte offset of the first character of each line. Always non-empty;
    /// `line_starts[0]` is `0`.
    line_starts: Vec<usize>,
}

impl Document {
    /// Creates a document from its full text.
    pub fn new(text: String, version: i32) -> Self {
        let line_starts = Self::compute_line_starts(&text);
        Document {
            text,
            version,
            line_starts,
        }
    }

    /// Replaces the document contents (full-text synchronization).
    pub fn update(&mut self, text: String, version: i32) {
        self.line_starts = Self::compute_line_starts(&text);
        self.text = text;
        self.version = version;
    }

    /// The current document text.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// The version reported by the client for the current text.
    pub fn version(&self) -> i32 {
        self.version
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

        let mut utf16_offset = 0u32;
        for (byte_offset, ch) in line_text.char_indices() {
            if utf16_offset >= position.character {
                return line_start + byte_offset;
            }
            utf16_offset += ch.len_utf16() as u32;
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
        let mut utf16_offset = 0u32;
        for (chars_seen, ch) in line_text.chars().enumerate() {
            if chars_seen == target_chars {
                break;
            }
            utf16_offset += ch.len_utf16() as u32;
        }
        Some(Position {
            line: line as u32,
            character: utf16_offset,
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
        Document::new(text.to_owned(), 0)
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
