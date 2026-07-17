//! Diagnostics for SQL documents: syntax problems the parser recovered
//! from, and references the schema cannot account for.

use sqlparser::tokenizer::Span;
use tower_lsp_server::ls_types::{Diagnostic, DiagnosticSeverity, Range};

use crate::analysis::resolve;
use crate::document::Document;
use crate::parse::ParsedSql;
use crate::schema::Schema;

/// Computes the diagnostics for one SQL document: parse failures as errors,
/// unresolved schema references as warnings, in source order.
pub fn diagnostics(document: &Document, parsed: &ParsedSql, schema: &Schema) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    for issue in &parsed.issues {
        diagnostics.push(Diagnostic {
            range: range_or_document_end(document, issue.span),
            severity: Some(DiagnosticSeverity::ERROR),
            source: Some("sqlx-lsp".to_owned()),
            message: issue.message.clone(),
            ..Diagnostic::default()
        });
    }
    for unresolved in resolve::unresolved_references(parsed, schema) {
        diagnostics.push(Diagnostic {
            range: range_or_document_end(document, unresolved.span),
            severity: Some(DiagnosticSeverity::WARNING),
            source: Some("sqlx-lsp".to_owned()),
            message: unresolved.message,
            ..Diagnostic::default()
        });
    }
    diagnostics.sort_by_key(|diagnostic| {
        (
            diagnostic.range.start.line,
            diagnostic.range.start.character,
        )
    });
    diagnostics
}

/// The LSP range of `span`, or a caret at the end of the document when the
/// span is empty (the input ended where more was expected).
fn range_or_document_end(document: &Document, span: Span) -> Range {
    document.range_of(span).unwrap_or_else(|| {
        let end = document.position_at(document.text().len());
        Range { start: end, end }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::DatabaseKind;
    use tower_lsp_server::ls_types::Position;

    fn schema() -> Schema {
        let mut schema = Schema::default();
        schema.apply_sql(
            "CREATE TABLE users (id INTEGER PRIMARY KEY, email TEXT NOT NULL);",
            DatabaseKind::Sqlite,
            None,
        );
        schema
    }

    fn diagnostics_for(sql: &str, schema: &Schema) -> Vec<Diagnostic> {
        let document = Document::new(sql.to_owned());
        let parsed = ParsedSql::parse(DatabaseKind::Sqlite.dialect(), document.text());
        diagnostics(&document, &parsed, schema)
    }

    #[test]
    fn unknown_tables_and_missing_columns_are_warnings() {
        let all = diagnostics_for("SELECT id FROM posts", &schema());
        assert_eq!(all.len(), 1, "{all:?}");
        assert_eq!(all[0].severity, Some(DiagnosticSeverity::WARNING));
        assert!(
            all[0].message.contains("unknown table"),
            "{}",
            all[0].message
        );
        assert_eq!(all[0].range.start, Position::new(0, 15));

        let all = diagnostics_for("SELECT u.nope FROM users AS u", &schema());
        assert_eq!(all.len(), 1, "{all:?}");
        assert!(
            all[0].message.contains("no column `nope`"),
            "{}",
            all[0].message
        );
    }

    #[test]
    fn resolvable_queries_are_clean() {
        assert!(diagnostics_for("SELECT u.email FROM users AS u", &schema()).is_empty());
        // CTEs and derived tables resolve locally.
        assert!(
            diagnostics_for(
                "WITH recent AS (SELECT id FROM users) SELECT recent.id FROM recent",
                &schema()
            )
            .is_empty()
        );
    }

    #[test]
    fn ddl_statements_are_exempt() {
        // A new table's own definition is not an unknown reference.
        assert!(diagnostics_for("CREATE TABLE fresh (id INTEGER)", &schema()).is_empty());
        assert!(diagnostics_for("DROP TABLE fresh", &schema()).is_empty());
    }

    #[test]
    fn syntax_problems_are_errors() {
        let all = diagnostics_for("SELECT FROM WHERE;", &schema());
        assert!(
            all.iter()
                .any(|diagnostic| diagnostic.severity == Some(DiagnosticSeverity::ERROR)),
            "{all:?}"
        );
    }

    #[test]
    fn an_empty_schema_reports_no_reference_warnings() {
        let empty = Schema::default();
        assert!(diagnostics_for("SELECT id FROM anything", &empty).is_empty());
    }
}
