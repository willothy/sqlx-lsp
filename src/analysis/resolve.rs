//! Resolution of the identifier under a cursor position to a schema object.
//!
//! This is the shared core of hover and goto-definition: both walk the AST
//! for identifier references (table names, aliases, qualified and unqualified
//! column references, column definitions), find the one containing the
//! request position, and resolve it against the schema index.

use std::collections::BTreeMap;
use std::ops::ControlFlow;

use sqlparser::ast::{
    AlterTableOperation, Expr, ObjectName, Statement, TableFactor, TableObject, Visit, Visitor,
};
use sqlparser::tokenizer::Span;
use tower_lsp::lsp_types::{Position, Range};

use crate::db::DatabaseKind;
use crate::document::Document;
use crate::parse::{ObjectNameExt, ParsedSql};
use crate::schema::{Column, Schema, Table};

/// A schema object resolved from a reference in a SQL document.
#[derive(Debug)]
pub enum Resolved<'a> {
    /// The reference names a table or view (directly or through an alias).
    Table {
        /// The resolved relation.
        table: &'a Table,
        /// The range of the reference in the document.
        range: Range,
    },
    /// The reference names a column.
    Column {
        /// The relation the column belongs to.
        table: &'a Table,
        /// The resolved column.
        column: &'a Column,
        /// The range of the reference in the document.
        range: Range,
    },
}

/// What an identifier reference in the document claims to name.
#[derive(Debug)]
enum CandidateKind {
    /// A relation reference: a table/view name or an alias for one.
    Table { name: String },
    /// A column reference, optionally qualified by a table name or alias.
    Column {
        qualifier: Option<String>,
        name: String,
    },
}

/// One identifier reference collected from the AST.
#[derive(Debug)]
struct Candidate {
    span: Span,
    kind: CandidateKind,
    statement: usize,
}

/// Collects identifier references and per-statement name scopes.
#[derive(Default)]
struct References {
    candidates: Vec<Candidate>,
    /// Per statement: alias or relation name (lowercased) mapped to the
    /// underlying relation name (lowercased).
    scopes: Vec<BTreeMap<String, String>>,
    current: usize,
}

impl References {
    fn collect(statements: &[Statement]) -> References {
        let mut references = References::default();
        for (index, statement) in statements.iter().enumerate() {
            references.current = index;
            references.scopes.push(BTreeMap::new());
            let _ = statement.visit(&mut references);
        }
        references
    }

    fn scope(&mut self) -> &mut BTreeMap<String, String> {
        &mut self.scopes[self.current]
    }

    fn record(&mut self, span: Span, kind: CandidateKind) {
        if span != Span::empty() {
            self.candidates.push(Candidate {
                span,
                kind,
                statement: self.current,
            });
        }
    }

    fn record_column_defs(&mut self, statement: &Statement) {
        let (table_name, column_idents): (Option<String>, Vec<_>) = match statement {
            Statement::CreateTable(create) => (
                create.name.simple_ident().map(|ident| ident.value.clone()),
                create.columns.iter().map(|def| &def.name).collect(),
            ),
            Statement::CreateView(create) => (
                create.name.simple_ident().map(|ident| ident.value.clone()),
                create.columns.iter().map(|def| &def.name).collect(),
            ),
            Statement::AlterTable(alter) => {
                let table = alter.name.simple_ident().map(|ident| ident.value.clone());
                let mut idents = Vec::new();
                for operation in &alter.operations {
                    match operation {
                        AlterTableOperation::AddColumn { column_def, .. } => {
                            idents.push(&column_def.name);
                        }
                        AlterTableOperation::DropColumn { column_names, .. } => {
                            idents.extend(column_names.iter());
                        }
                        AlterTableOperation::RenameColumn {
                            old_column_name, ..
                        } => {
                            idents.push(old_column_name);
                        }
                        _ => {}
                    }
                }
                (table, idents)
            }
            Statement::Insert(insert) => {
                let table = match &insert.table {
                    TableObject::TableName(name) => {
                        name.simple_ident().map(|ident| ident.value.clone())
                    }
                    _ => None,
                };
                let idents = insert
                    .columns
                    .iter()
                    .filter_map(|column| column.simple_ident())
                    .collect();
                (table, idents)
            }
            _ => return,
        };

        for ident in column_idents {
            self.record(
                ident.span,
                CandidateKind::Column {
                    qualifier: table_name.clone(),
                    name: ident.value.clone(),
                },
            );
        }
    }
}

impl Visitor for References {
    type Break = ();

    fn pre_visit_statement(&mut self, statement: &Statement) -> ControlFlow<()> {
        self.record_column_defs(statement);
        ControlFlow::Continue(())
    }

    fn pre_visit_relation(&mut self, relation: &ObjectName) -> ControlFlow<()> {
        if let Some(ident) = relation.simple_ident() {
            let name = ident.value.clone();
            let lowered = name.to_ascii_lowercase();
            self.scope().insert(lowered.clone(), lowered);
            self.record(ident.span, CandidateKind::Table { name });
        }
        ControlFlow::Continue(())
    }

    fn pre_visit_table_factor(&mut self, table_factor: &TableFactor) -> ControlFlow<()> {
        if let TableFactor::Table {
            name,
            alias: Some(alias),
            ..
        } = table_factor
            && let Some(ident) = name.simple_ident()
        {
            self.scope().insert(
                alias.name.value.to_ascii_lowercase(),
                ident.value.to_ascii_lowercase(),
            );
            self.record(
                alias.name.span,
                CandidateKind::Table {
                    name: alias.name.value.clone(),
                },
            );
        }
        ControlFlow::Continue(())
    }

    fn pre_visit_expr(&mut self, expr: &Expr) -> ControlFlow<()> {
        match expr {
            Expr::Identifier(ident) => self.record(
                ident.span,
                CandidateKind::Column {
                    qualifier: None,
                    name: ident.value.clone(),
                },
            ),
            Expr::CompoundIdentifier(parts) => {
                if let [qualifier, column] = parts.as_slice() {
                    self.record(
                        qualifier.span,
                        CandidateKind::Table {
                            name: qualifier.value.clone(),
                        },
                    );
                    self.record(
                        column.span,
                        CandidateKind::Column {
                            qualifier: Some(qualifier.value.clone()),
                            name: column.value.clone(),
                        },
                    );
                }
            }
            _ => {}
        }
        ControlFlow::Continue(())
    }
}

/// Resolves the identifier at `position` in `document` to a schema object.
pub fn resolve_at<'a>(
    document: &Document,
    position: Position,
    schema: &'a Schema,
    kind: DatabaseKind,
) -> Option<Resolved<'a>> {
    let parsed = ParsedSql::parse(kind.dialect(), document.text());
    let references = References::collect(&parsed.statements);

    let candidate = references
        .candidates
        .iter()
        .find(|candidate| document.position_in_span(position, candidate.span))?;
    let scope = &references.scopes[candidate.statement];
    let range = document.range_of(candidate.span)?;

    // An alias or relation name resolves through the statement scope first,
    // falling back to a direct schema lookup.
    let table_via_scope = |name: &str| {
        let lowered = name.to_ascii_lowercase();
        scope
            .get(&lowered)
            .and_then(|target| schema.table(target))
            .or_else(|| schema.table(name))
    };

    match &candidate.kind {
        CandidateKind::Table { name } => {
            let table = table_via_scope(name)?;
            Some(Resolved::Table { table, range })
        }
        CandidateKind::Column { qualifier, name } => {
            let table = match qualifier {
                Some(qualifier) => {
                    let table = table_via_scope(qualifier)?;
                    table.column(name)?;
                    table
                }
                None => {
                    // Prefer relations referenced by the enclosing statement;
                    // fall back to any relation with a matching column.
                    let in_scope = scope
                        .values()
                        .filter_map(|target| schema.table(target))
                        .find(|table| table.column(name).is_some());
                    in_scope
                        .or_else(|| schema.tables().find(|table| table.column(name).is_some()))?
                }
            };
            let column = table.column(name)?;
            Some(Resolved::Column {
                table,
                column,
                range,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schema() -> Schema {
        let mut schema = Schema::default();
        schema.apply_sql(
            "CREATE TABLE users (id INTEGER PRIMARY KEY, email TEXT NOT NULL);
             CREATE TABLE posts (id INTEGER PRIMARY KEY, author_id INTEGER, title TEXT);",
            DatabaseKind::Sqlite,
            None,
        );
        schema
    }

    fn resolve(sql: &str, line: u32, character: u32) -> Option<String> {
        let document = Document::new(sql.to_owned(), 0);
        let schema = schema();
        resolve_at(
            &document,
            Position::new(line, character),
            &schema,
            DatabaseKind::Sqlite,
        )
        .map(|resolved| match resolved {
            Resolved::Table { table, .. } => format!("table:{}", table.name),
            Resolved::Column { table, column, .. } => {
                format!("column:{}.{}", table.name, column.name)
            }
        })
    }

    #[test]
    fn resolves_table_reference() {
        assert_eq!(
            resolve("SELECT id FROM users", 0, 17).as_deref(),
            Some("table:users")
        );
    }

    #[test]
    fn resolves_unqualified_column_through_statement_scope() {
        // Both tables have an `id`; the FROM clause disambiguates.
        assert_eq!(
            resolve("SELECT title FROM posts", 0, 8).as_deref(),
            Some("column:posts.title")
        );
        assert_eq!(
            resolve("SELECT email FROM users", 0, 8).as_deref(),
            Some("column:users.email")
        );
    }

    #[test]
    fn resolves_alias_and_qualified_column() {
        let sql = "SELECT u.email FROM users AS u";
        // The qualifier `u` resolves to the aliased table.
        assert_eq!(resolve(sql, 0, 7).as_deref(), Some("table:users"));
        // The column resolves through the alias.
        assert_eq!(resolve(sql, 0, 9).as_deref(), Some("column:users.email"));
        // The alias definition itself resolves to the table.
        assert_eq!(resolve(sql, 0, 29).as_deref(), Some("table:users"));
    }

    #[test]
    fn resolves_columns_without_scope_by_searching_the_schema() {
        assert_eq!(
            resolve("SELECT author_id", 0, 8).as_deref(),
            Some("column:posts.author_id")
        );
    }

    #[test]
    fn resolves_insert_column_lists() {
        let sql = "INSERT INTO posts (author_id, title) VALUES (?, ?)";
        assert_eq!(resolve(sql, 0, 13).as_deref(), Some("table:posts"));
        assert_eq!(
            resolve(sql, 0, 20).as_deref(),
            Some("column:posts.author_id")
        );
        assert_eq!(resolve(sql, 0, 31).as_deref(), Some("column:posts.title"));
    }

    #[test]
    fn unknown_identifiers_do_not_resolve() {
        assert_eq!(resolve("SELECT nope FROM missing", 0, 8), None);
        assert_eq!(resolve("SELECT 1 + 2", 0, 8), None);
    }

    #[test]
    fn scopes_are_per_statement() {
        let sql = "SELECT id FROM users; SELECT id FROM posts;";
        assert_eq!(resolve(sql, 0, 7).as_deref(), Some("column:users.id"));
        assert_eq!(resolve(sql, 0, 29).as_deref(), Some("column:posts.id"));
    }
}
