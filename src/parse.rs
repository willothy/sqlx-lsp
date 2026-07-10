//! Best-effort SQL parsing shared by the schema loader and language features.

use sqlparser::ast::{Ident, ObjectName, ObjectNamePart, Statement};
use sqlparser::dialect::Dialect;
use sqlparser::parser::Parser;
use sqlparser::tokenizer::{Token, TokenWithSpan, Tokenizer};

/// A tokenized and (best-effort) parsed SQL source.
///
/// Editors hold syntactically incomplete SQL most of the time, so parsing
/// recovers at statement boundaries: a statement that fails to parse is
/// skipped up to the next semicolon while the statements around it still
/// produce AST nodes. The token stream is retained in full even when the
/// tokenizer stops early on malformed input.
#[derive(Debug)]
pub struct ParsedSql {
    /// All tokens, including whitespace and comments, with source spans.
    pub tokens: Vec<TokenWithSpan>,
    /// Every statement that parsed successfully.
    pub statements: Vec<Statement>,
}

impl ParsedSql {
    /// Tokenizes and parses `sql` under `dialect`, recovering from parse
    /// errors at statement boundaries.
    pub fn parse(dialect: &dyn Dialect, sql: &str) -> ParsedSql {
        let mut tokens = Vec::new();
        // On error the buffer keeps every token read before the failure.
        let _ = Tokenizer::new(dialect, sql).tokenize_with_location_into_buf(&mut tokens);

        let mut parser = Parser::new(dialect).with_tokens_with_locations(tokens.clone());
        let mut statements = Vec::new();
        loop {
            while parser.consume_token(&Token::SemiColon) {}
            if parser.peek_token().token == Token::EOF {
                break;
            }
            match parser.parse_statement() {
                Ok(statement) => statements.push(statement),
                Err(_) => loop {
                    let skipped = parser.next_token();
                    if matches!(skipped.token, Token::SemiColon | Token::EOF) {
                        break;
                    }
                },
            }
        }

        ParsedSql { tokens, statements }
    }
}

/// Extension methods for [`ObjectName`].
pub trait ObjectNameExt {
    /// The final identifier segment of the name (`users` in `main.users`),
    /// if that segment is a plain identifier.
    fn simple_ident(&self) -> Option<&Ident>;
}

impl ObjectNameExt for ObjectName {
    fn simple_ident(&self) -> Option<&Ident> {
        match self.0.last()? {
            ObjectNamePart::Identifier(ident) => Some(ident),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlparser::dialect::SQLiteDialect;

    #[test]
    fn recovers_after_a_broken_statement() {
        let sql = "SELECT 1; SELECT FROM WHERE; SELECT 2;";
        let parsed = ParsedSql::parse(&SQLiteDialect {}, sql);
        assert_eq!(parsed.statements.len(), 2);
    }

    #[test]
    fn keeps_tokens_when_tokenization_fails_midway() {
        // The unterminated string literal aborts the tokenizer, but the
        // tokens before it must survive for features that work off tokens.
        let sql = "SELECT name FROM users WHERE bio = 'unterminated";
        let parsed = ParsedSql::parse(&SQLiteDialect {}, sql);
        assert!(
            parsed
                .tokens
                .iter()
                .any(|token| matches!(&token.token, Token::Word(word) if word.value == "users"))
        );
    }

    #[test]
    fn parses_incomplete_trailing_statement_tokens() {
        let sql = "SELECT id, FROM users";
        let parsed = ParsedSql::parse(&SQLiteDialect {}, sql);
        // The statement may fail to parse, but tokens are all present.
        assert!(!parsed.tokens.is_empty());
    }
}
