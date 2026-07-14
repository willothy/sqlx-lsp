//! Resolution of the identifier under a cursor position to a schema object.
//!
//! This is the shared core of hover and goto-definition: both walk the AST
//! for identifier references (table names, aliases, qualified and unqualified
//! column references, column definitions), find the one containing the
//! request position, and resolve it against the schema index.

use std::collections::BTreeMap;
use std::ops::ControlFlow;

use sqlparser::ast::{
    AlterTableOperation, Expr, Ident, ObjectName, Statement, TableFactor, TableObject, Visit,
    Visitor,
};
use sqlparser::tokenizer::Span;
use tower_lsp_server::ls_types::{Position, Range};

use crate::document::Document;
use crate::parse::{ObjectNameExt, ParsedSql};
use crate::schema::{Column, Schema, Table, TableKind, TableOrigin};

/// A schema object resolved from a reference in a SQL document. Owns its
/// data because the resolved relation may be query-local (a CTE or derived
/// subquery) rather than a schema entry.
#[derive(Debug)]
pub enum Resolved {
    /// The reference names a table or view (directly or through an alias).
    Table {
        /// The resolved relation.
        table: Table,
        /// The range of the reference in the document.
        range: Range,
    },
    /// The reference names a column.
    Column {
        /// The relation the column belongs to.
        table: Table,
        /// The resolved column.
        column: Column,
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
struct References<'s> {
    schema: &'s Schema,
    candidates: Vec<Candidate>,
    /// Per statement: alias or relation name (lowercased) mapped to the
    /// underlying relation name (lowercased).
    scopes: Vec<BTreeMap<String, String>>,
    /// Per statement: relations the query defines locally (CTEs and aliased
    /// derived subqueries), with columns derived through `schema`.
    locals: Vec<Vec<Table>>,
    current: usize,
}

impl<'s> References<'s> {
    fn collect(statements: &[Statement], schema: &'s Schema) -> References<'s> {
        let mut references = References {
            schema,
            candidates: Vec::new(),
            scopes: Vec::new(),
            locals: Vec::new(),
            current: 0,
        };
        for (index, statement) in statements.iter().enumerate() {
            references.current = index;
            references.scopes.push(BTreeMap::new());
            references.locals.push(Vec::new());
            references.record_ctes(statement);
            let _ = statement.visit(&mut references);
        }
        references
    }

    fn scope(&mut self) -> &mut BTreeMap<String, String> {
        &mut self.scopes[self.current]
    }

    /// Registers a query-local relation: it joins the statement scope, its
    /// name becomes a hoverable reference, and lookups see its columns.
    fn record_local(&mut self, name: &Ident, columns: Vec<Column>) {
        let lowered = name.value.to_ascii_lowercase();
        self.scope().insert(lowered.clone(), lowered);
        self.record(
            name.span,
            CandidateKind::Table {
                name: name.value.clone(),
            },
        );
        self.locals[self.current].push(Table {
            name: name.value.clone(),
            kind: TableKind::View,
            origin: TableOrigin::Query,
            columns,
            location: None,
        });
    }

    /// The common table expressions of a top-level query statement.
    fn record_ctes(&mut self, statement: &Statement) {
        let Statement::Query(query) = statement else {
            return;
        };
        let Some(with) = &query.with else {
            return;
        };
        for cte in &with.cte_tables {
            let columns = if cte.alias.columns.is_empty() {
                self.schema.derive_query_columns(&cte.query)
            } else {
                cte.alias
                    .columns
                    .iter()
                    .map(|def| Column {
                        name: def.name.value.clone(),
                        data_type: def.data_type.as_ref().map(ToString::to_string),
                        not_null: false,
                        primary_key: false,
                        default: None,
                        location: None,
                    })
                    .collect()
            };
            self.record_local(&cte.alias.name.clone(), columns);
        }
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

impl Visitor for References<'_> {
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
        match table_factor {
            TableFactor::Table {
                name,
                alias: Some(alias),
                ..
            } => {
                if let Some(ident) = name.simple_ident() {
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
            }
            TableFactor::Derived {
                subquery,
                alias: Some(alias),
                ..
            } => {
                let columns = self.schema.derive_query_columns(subquery);
                self.record_local(&alias.name.clone(), columns);
            }
            _ => {}
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
                // `table.column` or `schema.table.column`: the last part is
                // the column and the part before it the relation; anything
                // earlier is a schema/database qualifier the index doesn't
                // model.
                if let [.., qualifier, column] = parts.as_slice() {
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

/// The relations each statement in `statements` defines locally (CTEs and
/// aliased derived subqueries), flattened. Their columns resolve through
/// `schema` where they reference real tables.
pub fn query_local_tables(statements: &[Statement], schema: &Schema) -> Vec<Table> {
    References::collect(statements, schema)
        .locals
        .into_iter()
        .flatten()
        .collect()
}

/// Resolves the identifier at `position` in `document` to a schema object or
/// a query-local relation.
pub fn resolve_at(
    document: &Document,
    parsed: &ParsedSql,
    position: Position,
    schema: &Schema,
) -> Option<Resolved> {
    let references = References::collect(&parsed.statements, schema);

    let candidate = references
        .candidates
        .iter()
        .find(|candidate| document.position_in_span(position, candidate.span))?;
    let scope = &references.scopes[candidate.statement];
    let locals = &references.locals[candidate.statement];
    let range = document.range_of(candidate.span)?;

    // An alias or relation name resolves through the statement's own
    // relations first, then its scope, then a direct schema lookup.
    let table_via_scope = |name: &str| {
        locals
            .iter()
            .find(|table| table.name.eq_ignore_ascii_case(name))
            .or_else(|| {
                let lowered = name.to_ascii_lowercase();
                scope
                    .get(&lowered)
                    .and_then(|target| schema.table(target))
                    .or_else(|| schema.table(name))
            })
    };

    match &candidate.kind {
        CandidateKind::Table { name } => {
            let table = table_via_scope(name)?;
            Some(Resolved::Table {
                table: table.clone(),
                range,
            })
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
                    locals
                        .iter()
                        .find(|table| table.column(name).is_some())
                        .or_else(|| {
                            scope
                                .values()
                                .filter_map(|target| schema.table(target))
                                .find(|table| table.column(name).is_some())
                        })
                        .or_else(|| schema.tables().find(|table| table.column(name).is_some()))?
                }
            };
            let column = table.column(name)?.clone();
            Some(Resolved::Column {
                table: table.clone(),
                column,
                range,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::DatabaseKind;

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
        let document = Document::new(sql.to_owned());
        let schema = schema();
        let parsed = ParsedSql::parse(DatabaseKind::Sqlite.dialect(), document.text());
        resolve_at(&document, &parsed, Position::new(line, character), &schema).map(|resolved| {
            match resolved {
                Resolved::Table { table, .. } => format!("table:{}", table.name),
                Resolved::Column { table, column, .. } => {
                    format!("column:{}.{}", table.name, column.name)
                }
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
    fn resolves_ctes_as_query_local_relations() {
        let sql = "WITH recent AS (SELECT id, title FROM posts) SELECT recent.title FROM recent";
        // The reference in FROM and the qualifier both resolve to the CTE.
        assert_eq!(resolve(sql, 0, 71).as_deref(), Some("table:recent"));
        assert_eq!(resolve(sql, 0, 54).as_deref(), Some("table:recent"));
        // Its columns derive through the underlying schema table.
        assert_eq!(resolve(sql, 0, 60).as_deref(), Some("column:recent.title"));
        // The definition site itself is hoverable.
        assert_eq!(resolve(sql, 0, 6).as_deref(), Some("table:recent"));
    }

    #[test]
    fn resolves_derived_table_aliases() {
        let sql = "SELECT sub.name FROM (SELECT email AS name FROM users) sub";
        assert_eq!(resolve(sql, 0, 8).as_deref(), Some("table:sub"));
        assert_eq!(resolve(sql, 0, 12).as_deref(), Some("column:sub.name"));
        assert_eq!(resolve(sql, 0, 57).as_deref(), Some("table:sub"));
    }

    #[test]
    fn resolves_schema_qualified_references() {
        // `main.users.email`: the middle part is the relation.
        let sql = "SELECT main.users.email FROM main.users";
        assert_eq!(resolve(sql, 0, 12).as_deref(), Some("table:users"));
        assert_eq!(resolve(sql, 0, 18).as_deref(), Some("column:users.email"));
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
