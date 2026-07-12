//! Hover information for schema objects referenced in SQL documents.

use tower_lsp::lsp_types::{Hover, HoverContents, MarkupContent, MarkupKind, Position};

use crate::analysis::resolve::{Resolved, resolve_at};
use crate::document::Document;
use crate::parse::ParsedSql;
use crate::schema::{Column, Schema, SourceLocation, Table, TableOrigin};

/// Builds hover content for the schema object referenced at `position`.
pub fn hover(
    document: &Document,
    parsed: &ParsedSql,
    position: Position,
    schema: &Schema,
) -> Option<Hover> {
    let resolved = resolve_at(document, parsed, position, schema)?;
    let (value, range) = match resolved {
        Resolved::Table { table, range } => (table_markdown(table), range),
        Resolved::Column {
            table,
            column,
            range,
        } => (column_markdown(table, column), range),
    };
    Some(Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value,
        }),
        range: Some(range),
    })
}

fn origin_line(origin: TableOrigin, location: Option<&SourceLocation>) -> String {
    match (origin, location) {
        (TableOrigin::Migration, Some(location)) => {
            let file = location
                .uri
                .path_segments()
                .and_then(|mut segments| segments.next_back())
                .unwrap_or("migration")
                .to_owned();
            format!("*defined in {file}*")
        }
        (TableOrigin::Migration, None) => "*defined in migrations*".to_owned(),
        (TableOrigin::Database, _) => "*from live database*".to_owned(),
    }
}

fn table_markdown(table: &Table) -> String {
    format!(
        "```sql\n{}\n```\n\n{}",
        table.ddl(),
        origin_line(table.origin, table.location.as_ref())
    )
}

fn column_markdown(table: &Table, column: &Column) -> String {
    format!(
        "```sql\n{}.{}\n```\n\n{}",
        table.name,
        column.signature(),
        origin_line(table.origin, column.location.as_ref())
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::DatabaseKind;

    fn hover_value(sql: &str, character: u32) -> Option<String> {
        let mut schema = Schema::default();
        schema.apply_sql(
            "CREATE TABLE users (id INTEGER PRIMARY KEY, email TEXT NOT NULL DEFAULT 'x');",
            DatabaseKind::Sqlite,
            None,
        );
        let document = Document::new(sql.to_owned());
        let parsed = ParsedSql::parse(DatabaseKind::Sqlite.dialect(), document.text());
        let hover = hover(&document, &parsed, Position::new(0, character), &schema)?;
        match hover.contents {
            HoverContents::Markup(markup) => Some(markup.value),
            _ => None,
        }
    }

    #[test]
    fn table_hover_shows_reconstructed_ddl() {
        let value = hover_value("SELECT id FROM users", 16).expect("hovers");
        assert!(value.contains("CREATE TABLE users ("));
        assert!(value.contains("id INTEGER PRIMARY KEY"));
        assert!(value.contains("email TEXT NOT NULL DEFAULT 'x'"));
        assert!(value.contains("*defined in migrations*"));
    }

    #[test]
    fn column_hover_shows_signature() {
        let value = hover_value("SELECT email FROM users", 8).expect("hovers");
        assert!(value.contains("users.email TEXT NOT NULL DEFAULT 'x'"));
    }

    #[test]
    fn no_hover_for_unknown_identifiers() {
        assert_eq!(hover_value("SELECT nope FROM missing", 8), None);
    }
}
