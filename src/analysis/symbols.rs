//! Document outline for SQL files: the relations a document defines.

use sqlparser::ast::{Spanned, Statement};
use tower_lsp_server::ls_types::{DocumentSymbol, Range, SymbolKind};

use crate::document::Document;
use crate::parse::{ObjectNameExt, ParsedSql};

/// The outline of `document`: one symbol per `CREATE TABLE`/`CREATE VIEW`
/// statement, with the defined columns as children.
pub fn document_symbols(document: &Document, parsed: &ParsedSql) -> Vec<DocumentSymbol> {
    let mut symbols = Vec::new();
    for statement in &parsed.statements {
        let (name_ident, kind, column_idents) = match statement {
            Statement::CreateTable(create) => (
                create.name.simple_ident(),
                SymbolKind::CLASS,
                create
                    .columns
                    .iter()
                    .map(|def| &def.name)
                    .collect::<Vec<_>>(),
            ),
            Statement::CreateView(create) => (
                create.name.simple_ident(),
                SymbolKind::INTERFACE,
                create.columns.iter().map(|def| &def.name).collect(),
            ),
            _ => continue,
        };
        let Some(ident) = name_ident else {
            continue;
        };
        let Some(selection_range) = document.range_of(ident.span) else {
            continue;
        };
        // The statement's full extent; the defining identifier must sit
        // inside it, which a recovered partial parse can violate.
        let range = document
            .range_of(statement.span())
            .filter(|range| {
                range.start <= selection_range.start && selection_range.end <= range.end
            })
            .unwrap_or(selection_range);

        let children = column_idents
            .into_iter()
            .filter_map(|column| {
                let range = document.range_of(column.span)?;
                Some(symbol(
                    column.value.clone(),
                    SymbolKind::FIELD,
                    range,
                    range,
                    Vec::new(),
                ))
            })
            .collect();

        symbols.push(symbol(
            ident.value.clone(),
            kind,
            range,
            selection_range,
            children,
        ));
    }
    symbols
}

/// A [`DocumentSymbol`] with the fields this outline uses.
// `DocumentSymbol` carries a deprecated-but-present field.
#[allow(deprecated)]
fn symbol(
    name: String,
    kind: SymbolKind,
    range: Range,
    selection_range: Range,
    children: Vec<DocumentSymbol>,
) -> DocumentSymbol {
    DocumentSymbol {
        name,
        detail: None,
        kind,
        tags: None,
        deprecated: None,
        range,
        selection_range,
        children: if children.is_empty() {
            None
        } else {
            Some(children)
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::DatabaseKind;
    use tower_lsp_server::ls_types::Position;

    fn symbols_for(sql: &str) -> Vec<DocumentSymbol> {
        let document = Document::new(sql.to_owned());
        let parsed = ParsedSql::parse(DatabaseKind::Sqlite.dialect(), document.text());
        document_symbols(&document, &parsed)
    }

    #[test]
    fn tables_outline_with_column_children() {
        let symbols = symbols_for(
            "CREATE TABLE users (\n  id INTEGER PRIMARY KEY,\n  email TEXT NOT NULL\n);",
        );
        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].name, "users");
        assert_eq!(symbols[0].kind, SymbolKind::CLASS);
        assert_eq!(symbols[0].selection_range.start, Position::new(0, 13));

        let children = symbols[0].children.as_ref().expect("columns");
        // The table's range covers every column child.
        assert!(
            children
                .iter()
                .all(|child| child.range.end <= symbols[0].range.end),
            "{symbols:?}"
        );
        let names: Vec<_> = children.iter().map(|child| child.name.as_str()).collect();
        assert_eq!(names, vec!["id", "email"]);
        assert_eq!(children[0].kind, SymbolKind::FIELD);
    }

    #[test]
    fn views_and_multiple_statements_each_get_a_symbol() {
        let symbols = symbols_for(
            "CREATE TABLE users (id INTEGER);\n\
             CREATE VIEW names AS SELECT id FROM users;",
        );
        assert_eq!(symbols.len(), 2);
        assert_eq!(symbols[1].name, "names");
        assert_eq!(symbols[1].kind, SymbolKind::INTERFACE);
    }

    #[test]
    fn non_ddl_statements_produce_no_symbols() {
        assert!(symbols_for("SELECT id FROM users").is_empty());
    }
}
