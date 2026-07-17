//! Best-effort SQL parsing shared by the schema loader and language features.

use sqlparser::ast::{Ident, ObjectName, ObjectNamePart, Statement};
use sqlparser::dialect::Dialect;
use sqlparser::parser::Parser;
use sqlparser::tokenizer::{Span, Token, TokenWithSpan, Tokenizer};

/// A syntax problem the parser recovered from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseIssue {
    /// Human-readable description, without the location suffix sqlparser
    /// embeds in its messages (the span carries the position).
    pub message: String,
    /// The source span the issue anchors to. Empty when the input ended
    /// where more was expected.
    pub span: Span,
}

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
    /// The syntax problems recovery skipped over, in source order.
    pub issues: Vec<ParseIssue>,
}

impl ParsedSql {
    /// Tokenizes and parses `sql` under `dialect`, recovering from parse
    /// errors at statement boundaries.
    pub fn parse(dialect: &dyn Dialect, sql: &str) -> ParsedSql {
        let mut issues = Vec::new();
        let mut tokens = Vec::new();
        // On error the buffer keeps every token read before the failure.
        if let Err(error) =
            Tokenizer::new(dialect, sql).tokenize_with_location_into_buf(&mut tokens)
        {
            issues.push(ParseIssue {
                message: strip_location_suffix(&error.message).to_owned(),
                span: Span::new(error.location, error.location),
            });
        }

        let mut parser = Parser::new(dialect).with_tokens_with_locations(tokens.clone());
        let mut statements = Vec::new();
        loop {
            while parser.consume_token(&Token::SemiColon) {}
            if parser.peek_token().token == Token::EOF {
                break;
            }
            match parser.parse_statement() {
                Ok(statement) => statements.push(statement),
                Err(error) => {
                    // The parser's cursor sits at the token it could not
                    // accept, which anchors the issue.
                    issues.push(ParseIssue {
                        message: strip_location_suffix(&error.to_string()).to_owned(),
                        span: parser.peek_token().span,
                    });
                    loop {
                        let skipped = parser.next_token();
                        if matches!(skipped.token, Token::SemiColon | Token::EOF) {
                            break;
                        }
                    }
                }
            }
        }

        ParsedSql {
            tokens,
            statements,
            issues,
        }
    }
}

/// Strips sqlparser's trailing ` at Line: N, Column: M` from an error
/// message, when present.
fn strip_location_suffix(message: &str) -> &str {
    let Some(index) = message.rfind(" at Line: ") else {
        return message;
    };
    let suffix = &message[index + " at Line: ".len()..];
    let Some((line, column)) = suffix.split_once(", Column: ") else {
        return message;
    };
    if line.parse::<u64>().is_ok() && column.parse::<u64>().is_ok() {
        &message[..index]
    } else {
        message
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

        // The skipped statement is reported with a position and without
        // sqlparser's location suffix.
        assert_eq!(parsed.issues.len(), 1);
        let issue = &parsed.issues[0];
        assert!(!issue.message.contains("at Line:"), "{}", issue.message);
        assert_eq!(issue.span.start.line, 1);
        assert!(issue.span.start.column >= 11);
    }

    #[test]
    fn clean_sql_reports_no_issues() {
        let parsed = ParsedSql::parse(&SQLiteDialect {}, "SELECT id FROM users;");
        assert!(parsed.issues.is_empty());
    }

    #[test]
    fn tokenizer_failures_are_reported_with_their_location() {
        let parsed = ParsedSql::parse(&SQLiteDialect {}, "SELECT 'unterminated");
        assert!(
            parsed
                .issues
                .iter()
                .any(|issue| issue.message.contains("Unterminated")),
            "{:?}",
            parsed.issues
        );
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
