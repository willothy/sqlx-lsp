//! Semantic token highlighting for SQL documents.
//!
//! Classification happens in two layers. A lexical base layer types every
//! token the tokenizer produces (keywords, literals, comments, operators,
//! placeholders, and type-name keywords). An AST overlay then re-types the
//! identifiers the parser understood — table references, column references,
//! aliases, and function names — which also corrects identifiers whose names
//! collide with non-reserved keywords (`name`, `type`, ...). The overlay only
//! applies to statements that parse, so broken statements degrade to the
//! lexical layer instead of losing highlighting entirely.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::ops::ControlFlow;

use sqlparser::ast::{
    AlterTableOperation, Expr, ObjectName, ObjectNamePart, Statement, TableFactor, Visit, Visitor,
};
use sqlparser::keywords::Keyword;
use sqlparser::tokenizer::{Span, Token, Whitespace};
use tower_lsp::lsp_types::{SemanticToken, SemanticTokenType, SemanticTokensLegend};

use crate::db::DatabaseKind;
use crate::document::Document;
use crate::parse::ParsedSql;

/// The token types this server emits, in legend order. The discriminants of
/// [`TokenClass`] index into this slice.
const LEGEND_TYPES: &[SemanticTokenType] = &[
    SemanticTokenType::KEYWORD,
    SemanticTokenType::STRING,
    SemanticTokenType::NUMBER,
    SemanticTokenType::OPERATOR,
    SemanticTokenType::COMMENT,
    SemanticTokenType::FUNCTION,
    SemanticTokenType::PARAMETER,
    SemanticTokenType::TYPE,
    SemanticTokenType::CLASS,
    SemanticTokenType::PROPERTY,
];

/// The legend advertised in the server capabilities.
pub fn legend() -> SemanticTokensLegend {
    SemanticTokensLegend {
        token_types: LEGEND_TYPES.to_vec(),
        token_modifiers: Vec::new(),
    }
}

/// Semantic classification of one token. Discriminants index [`LEGEND_TYPES`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
enum TokenClass {
    Keyword = 0,
    StringLiteral = 1,
    Number = 2,
    Operator = 3,
    Comment = 4,
    Function = 5,
    Parameter = 6,
    Type = 7,
    Table = 8,
    Column = 9,
}

impl TokenClass {
    /// Precedence when the AST classifies one span multiple ways (a name can
    /// be visited both as a relation and inside an expression).
    fn priority(self) -> u8 {
        match self {
            TokenClass::Function => 3,
            TokenClass::Table => 2,
            TokenClass::Column => 1,
            _ => 0,
        }
    }
}

/// Keywords that name data types. Lexical fallback for type positions, since
/// `DataType` nodes carry no spans; the AST overlay wins wherever one of
/// these doubles as an identifier.
const TYPE_KEYWORDS: &[&str] = &[
    "BIGINT",
    "BIGSERIAL",
    "BINARY",
    "BIT",
    "BLOB",
    "BOOL",
    "BOOLEAN",
    "BYTEA",
    "CHAR",
    "CHARACTER",
    "CLOB",
    "DATE",
    "DATETIME",
    "DECIMAL",
    "DOUBLE",
    "ENUM",
    "FLOAT",
    "INT",
    "INT2",
    "INT4",
    "INT8",
    "INTEGER",
    "INTERVAL",
    "JSON",
    "JSONB",
    "MEDIUMINT",
    "NCHAR",
    "NUMERIC",
    "NVARCHAR",
    "PRECISION",
    "REAL",
    "SERIAL",
    "SMALLINT",
    "SMALLSERIAL",
    "TEXT",
    "TIME",
    "TIMESTAMP",
    "TIMESTAMPTZ",
    "TINYINT",
    "UNSIGNED",
    "UUID",
    "VARBINARY",
    "VARCHAR",
    "YEAR",
];

/// Collects identifier classifications from the AST, keyed by source span.
#[derive(Default)]
struct Overlay {
    spans: HashMap<Span, TokenClass>,
}

impl Overlay {
    fn record(&mut self, span: Span, class: TokenClass) {
        if span == Span::empty() {
            return;
        }
        match self.spans.entry(span) {
            Entry::Occupied(mut entry) => {
                if class.priority() > entry.get().priority() {
                    entry.insert(class);
                }
            }
            Entry::Vacant(entry) => {
                entry.insert(class);
            }
        }
    }

    fn record_object_name(&mut self, name: &ObjectName, class: TokenClass) {
        for part in &name.0 {
            if let ObjectNamePart::Identifier(ident) = part {
                self.record(ident.span, class);
            }
        }
    }
}

impl Visitor for Overlay {
    type Break = ();

    fn pre_visit_relation(&mut self, relation: &ObjectName) -> ControlFlow<()> {
        self.record_object_name(relation, TokenClass::Table);
        ControlFlow::Continue(())
    }

    fn pre_visit_table_factor(&mut self, table_factor: &TableFactor) -> ControlFlow<()> {
        if let TableFactor::Table {
            alias: Some(alias), ..
        }
        | TableFactor::Derived {
            alias: Some(alias), ..
        } = table_factor
        {
            self.record(alias.name.span, TokenClass::Table);
        }
        ControlFlow::Continue(())
    }

    fn pre_visit_expr(&mut self, expr: &Expr) -> ControlFlow<()> {
        match expr {
            Expr::Identifier(ident) => self.record(ident.span, TokenClass::Column),
            Expr::CompoundIdentifier(parts) => {
                if let Some((column, qualifiers)) = parts.split_last() {
                    self.record(column.span, TokenClass::Column);
                    for qualifier in qualifiers {
                        self.record(qualifier.span, TokenClass::Table);
                    }
                }
            }
            Expr::Function(function) => {
                self.record_object_name(&function.name, TokenClass::Function);
            }
            _ => {}
        }
        ControlFlow::Continue(())
    }

    fn pre_visit_statement(&mut self, statement: &Statement) -> ControlFlow<()> {
        // Column definitions and column name lists are plain `Ident`s the
        // expression visitor never sees; classify them here.
        match statement {
            Statement::CreateTable(create) => {
                for def in &create.columns {
                    self.record(def.name.span, TokenClass::Column);
                }
            }
            Statement::CreateView(create) => {
                for def in &create.columns {
                    self.record(def.name.span, TokenClass::Column);
                }
            }
            Statement::AlterTable(alter) => {
                for operation in &alter.operations {
                    match operation {
                        AlterTableOperation::AddColumn { column_def, .. } => {
                            self.record(column_def.name.span, TokenClass::Column);
                        }
                        AlterTableOperation::DropColumn { column_names, .. } => {
                            for name in column_names {
                                self.record(name.span, TokenClass::Column);
                            }
                        }
                        AlterTableOperation::RenameColumn {
                            old_column_name,
                            new_column_name,
                        } => {
                            self.record(old_column_name.span, TokenClass::Column);
                            self.record(new_column_name.span, TokenClass::Column);
                        }
                        _ => {}
                    }
                }
            }
            Statement::Insert(insert) => {
                for column in &insert.columns {
                    self.record_object_name(column, TokenClass::Column);
                }
            }
            _ => {}
        }
        ControlFlow::Continue(())
    }
}

/// Lexical classification of a token, before the AST overlay.
fn base_class(token: &Token) -> Option<TokenClass> {
    match token {
        Token::Word(word) => {
            if word.quote_style.is_some() {
                None
            } else if TYPE_KEYWORDS
                .binary_search(&word.value.to_ascii_uppercase().as_str())
                .is_ok()
            {
                Some(TokenClass::Type)
            } else if word.keyword != Keyword::NoKeyword {
                Some(TokenClass::Keyword)
            } else {
                None
            }
        }
        Token::Number(..) => Some(TokenClass::Number),
        Token::SingleQuotedString(_)
        | Token::TripleSingleQuotedString(_)
        | Token::TripleDoubleQuotedString(_)
        | Token::DollarQuotedString(_)
        | Token::SingleQuotedByteStringLiteral(_)
        | Token::DoubleQuotedByteStringLiteral(_)
        | Token::TripleSingleQuotedByteStringLiteral(_)
        | Token::TripleDoubleQuotedByteStringLiteral(_)
        | Token::SingleQuotedRawStringLiteral(_)
        | Token::DoubleQuotedRawStringLiteral(_)
        | Token::TripleSingleQuotedRawStringLiteral(_)
        | Token::TripleDoubleQuotedRawStringLiteral(_)
        | Token::NationalStringLiteral(_)
        | Token::EscapedStringLiteral(_)
        | Token::UnicodeStringLiteral(_)
        | Token::HexStringLiteral(_) => Some(TokenClass::StringLiteral),
        Token::Placeholder(_) => Some(TokenClass::Parameter),
        Token::Whitespace(
            Whitespace::SingleLineComment { .. } | Whitespace::MultiLineComment(_),
        ) => Some(TokenClass::Comment),
        Token::DoubleEq
        | Token::Eq
        | Token::Neq
        | Token::Lt
        | Token::Gt
        | Token::LtEq
        | Token::GtEq
        | Token::Spaceship
        | Token::Plus
        | Token::Minus
        | Token::Mul
        | Token::Div
        | Token::Mod
        | Token::StringConcat
        | Token::DoubleColon
        | Token::Ampersand
        | Token::Pipe
        | Token::Caret
        | Token::ShiftLeft
        | Token::ShiftRight => Some(TokenClass::Operator),
        _ => None,
    }
}

/// One classified token at absolute document coordinates (UTF-16 units).
/// Never spans more than one line.
#[derive(Debug, Clone, Copy)]
pub struct TokenSegment {
    /// 0-based line.
    pub line: u32,
    /// 0-based UTF-16 start character within the line.
    pub start: u32,
    /// Length in UTF-16 units.
    pub length: u32,
    class: TokenClass,
}

/// Computes classified token segments for `document` at absolute positions.
/// Callers that embed SQL in a host document can shift the positions before
/// [`encode`]-ing; plain SQL documents use [`semantic_tokens`] directly.
pub fn segments(document: &Document, kind: DatabaseKind) -> Vec<TokenSegment> {
    let parsed = ParsedSql::parse(kind.dialect(), document.text());

    let mut overlay = Overlay::default();
    for statement in &parsed.statements {
        let _ = statement.visit(&mut overlay);
    }

    // One entry per line: clients only accept single-line tokens without the
    // multiline capability, so multi-line tokens (block comments) are split.
    let mut segments: Vec<TokenSegment> = Vec::new();
    for token in &parsed.tokens {
        let class = overlay
            .spans
            .get(&token.span)
            .copied()
            .or_else(|| base_class(&token.token));
        let Some(class) = class else {
            continue;
        };
        let Some(range) = document.range_of(token.span) else {
            continue;
        };

        for line in range.start.line..=range.end.line {
            let start = if line == range.start.line {
                range.start.character
            } else {
                0
            };
            let end = if line == range.end.line {
                range.end.character
            } else {
                document.line_utf16_len(line)
            };
            if end > start {
                segments.push(TokenSegment {
                    line,
                    start,
                    length: end - start,
                    class,
                });
            }
        }
    }
    segments
}

/// Sorts `segments` and delta-encodes them as the LSP wire format requires.
pub fn encode(mut segments: Vec<TokenSegment>) -> Vec<SemanticToken> {
    segments.sort_by_key(|segment| (segment.line, segment.start));

    let mut data = Vec::with_capacity(segments.len());
    let mut previous_line = 0u32;
    let mut previous_start = 0u32;
    for segment in segments {
        let delta_line = segment.line - previous_line;
        let delta_start = if delta_line == 0 {
            segment.start - previous_start
        } else {
            segment.start
        };
        data.push(SemanticToken {
            delta_line,
            delta_start,
            length: segment.length,
            token_type: segment.class as u32,
            token_modifiers_bitset: 0,
        });
        previous_line = segment.line;
        previous_start = segment.start;
    }
    data
}

/// Computes the full semantic token stream for `document`, delta-encoded as
/// the LSP wire format requires.
pub fn semantic_tokens(document: &Document, kind: DatabaseKind) -> Vec<SemanticToken> {
    encode(segments(document, kind))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Decodes the delta encoding back to absolute
    /// (line, character, length, legend index) tuples.
    fn decode(tokens: &[SemanticToken]) -> Vec<(u32, u32, u32, u32)> {
        let mut absolute = Vec::new();
        let mut line = 0;
        let mut start = 0;
        for token in tokens {
            if token.delta_line > 0 {
                line += token.delta_line;
                start = token.delta_start;
            } else {
                start += token.delta_start;
            }
            absolute.push((line, start, token.length, token.token_type));
        }
        absolute
    }

    fn tokens_for(sql: &str) -> Vec<(u32, u32, u32, u32)> {
        let document = Document::new(sql.to_owned(), 0);
        decode(&semantic_tokens(&document, DatabaseKind::Sqlite))
    }

    #[test]
    fn type_keywords_are_sorted_for_binary_search() {
        let mut sorted = TYPE_KEYWORDS.to_vec();
        sorted.sort_unstable();
        assert_eq!(TYPE_KEYWORDS, sorted.as_slice());
    }

    #[test]
    fn classifies_query_tokens() {
        let tokens = tokens_for("SELECT id, 'x' FROM users WHERE age > 21 -- adults");
        let keyword = TokenClass::Keyword as u32;
        let string = TokenClass::StringLiteral as u32;
        let number = TokenClass::Number as u32;
        let operator = TokenClass::Operator as u32;
        let comment = TokenClass::Comment as u32;
        let table = TokenClass::Table as u32;
        let column = TokenClass::Column as u32;

        assert_eq!(
            tokens,
            vec![
                (0, 0, 6, keyword),  // SELECT
                (0, 7, 2, column),   // id
                (0, 11, 3, string),  // 'x'
                (0, 15, 4, keyword), // FROM
                (0, 20, 5, table),   // users
                (0, 26, 5, keyword), // WHERE
                (0, 32, 3, column),  // age
                (0, 36, 1, operator),
                (0, 38, 2, number),
                (0, 41, 9, comment),
            ]
        );
    }

    #[test]
    fn identifiers_shadowing_keywords_are_reclassified() {
        // `name` and `type` are in sqlparser's keyword list, but the AST
        // knows they are a column and a table here.
        let tokens = tokens_for("SELECT name FROM type");
        assert_eq!(tokens[1], (0, 7, 4, TokenClass::Column as u32));
        assert_eq!(tokens[3], (0, 17, 4, TokenClass::Table as u32));
    }

    #[test]
    fn create_table_classifies_definitions_and_types() {
        let tokens = tokens_for("CREATE TABLE users (id INTEGER PRIMARY KEY, email TEXT)");
        let column = TokenClass::Column as u32;
        let data_type = TokenClass::Type as u32;
        let table = TokenClass::Table as u32;
        assert!(tokens.contains(&(0, 13, 5, table))); // users
        assert!(tokens.contains(&(0, 20, 2, column))); // id
        assert!(tokens.contains(&(0, 23, 7, data_type))); // INTEGER
        assert!(tokens.contains(&(0, 44, 5, column))); // email
        assert!(tokens.contains(&(0, 50, 4, data_type))); // TEXT
    }

    #[test]
    fn placeholders_and_functions_are_classified() {
        let tokens = tokens_for("SELECT count(id) FROM users WHERE id = ?");
        assert!(tokens.contains(&(0, 7, 5, TokenClass::Function as u32)));
        assert!(tokens.contains(&(0, 39, 1, TokenClass::Parameter as u32)));
    }

    #[test]
    fn multi_line_comments_split_per_line() {
        let tokens = tokens_for("/* one\ntwo */ SELECT 1");
        let comment = TokenClass::Comment as u32;
        assert_eq!(tokens[0], (0, 0, 6, comment));
        assert_eq!(tokens[1], (1, 0, 6, comment));
    }

    #[test]
    fn qualified_references_split_table_and_column() {
        let tokens = tokens_for("SELECT u.email FROM users AS u");
        let table = TokenClass::Table as u32;
        let column = TokenClass::Column as u32;
        assert!(tokens.contains(&(0, 7, 1, table))); // u
        assert!(tokens.contains(&(0, 9, 5, column))); // email
        assert!(tokens.contains(&(0, 29, 1, table))); // alias u
    }

    #[test]
    fn broken_statements_keep_lexical_highlighting() {
        let tokens = tokens_for("SELECT FROM WHERE 'txt' 42");
        let keyword = TokenClass::Keyword as u32;
        assert!(tokens.contains(&(0, 0, 6, keyword)));
        assert!(tokens.contains(&(0, 18, 5, TokenClass::StringLiteral as u32)));
        assert!(tokens.contains(&(0, 24, 2, TokenClass::Number as u32)));
    }
}
