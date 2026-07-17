//! Replaying DDL statements into the schema index.

use sqlparser::ast::{
    AlterTable, AlterTableOperation, ColumnDef, ColumnOption, CreateTable, CreateView, DataType,
    Expr, ObjectType, Query, RenameTableNameKind, SelectItem, SelectItemQualifiedWildcardKind,
    SetExpr, Statement, TableConstraint, TableFactor,
};
use sqlparser::tokenizer::Span as SqlSpan;
use tower_lsp_server::ls_types::Uri;

use crate::db::DatabaseKind;
use crate::document::Document;
use crate::parse::{ObjectNameExt, ParsedSql};
use crate::schema::{Column, Schema, SourceLocation, Table, TableKind, TableOrigin};

impl Schema {
    /// Applies every DDL statement in `sql` to the index. Statements that
    /// fail to parse are skipped; non-DDL statements are ignored. When `uri`
    /// is provided, definitions are recorded with source locations.
    pub fn apply_sql(&mut self, sql: &str, kind: DatabaseKind, uri: Option<&Uri>) {
        let parsed = ParsedSql::parse(kind.dialect(), sql);
        let document = Document::new(sql.to_owned());
        for statement in &parsed.statements {
            self.apply_statement(statement, uri, &document);
        }
    }

    fn apply_statement(&mut self, statement: &Statement, uri: Option<&Uri>, document: &Document) {
        match statement {
            Statement::CreateTable(create) => self.apply_create_table(create, uri, document),
            Statement::CreateView(create) => self.apply_create_view(create, uri, document),
            Statement::AlterTable(alter) => self.apply_alter_table(alter, uri, document),
            Statement::Drop {
                object_type: ObjectType::Table | ObjectType::View,
                names,
                ..
            } => {
                for name in names {
                    if let Some(ident) = name.simple_ident() {
                        self.tables.remove(&ident.value.to_ascii_lowercase());
                    }
                }
            }
            _ => {}
        }
    }

    fn source_location(
        uri: Option<&Uri>,
        document: &Document,
        span: SqlSpan,
    ) -> Option<SourceLocation> {
        let uri = uri?;
        let range = document.range_of(span)?;
        Some(SourceLocation {
            uri: uri.clone(),
            range,
        })
    }

    fn column_from_def(def: &ColumnDef, uri: Option<&Uri>, document: &Document) -> Column {
        let mut not_null = false;
        let mut primary_key = false;
        let mut default = None;
        for option in &def.options {
            match &option.option {
                ColumnOption::NotNull => not_null = true,
                ColumnOption::PrimaryKey(_) => {
                    primary_key = true;
                    not_null = true;
                }
                ColumnOption::Default(expr) => default = Some(expr.to_string()),
                _ => {}
            }
        }
        let data_type = match &def.data_type {
            DataType::Unspecified => None,
            other => Some(other.to_string()),
        };
        Column {
            name: def.name.value.clone(),
            data_type,
            not_null,
            primary_key,
            default,
            location: Self::source_location(uri, document, def.name.span),
        }
    }

    fn apply_create_table(&mut self, create: &CreateTable, uri: Option<&Uri>, document: &Document) {
        let Some(ident) = create.name.simple_ident() else {
            return;
        };
        if create.if_not_exists && !create.or_replace && self.table(&ident.value).is_some() {
            return;
        }

        let mut columns: Vec<Column> = create
            .columns
            .iter()
            .map(|def| Self::column_from_def(def, uri, document))
            .collect();
        for constraint in &create.constraints {
            if let TableConstraint::PrimaryKey(primary_key) = constraint {
                for index_column in &primary_key.columns {
                    if let Expr::Identifier(column_ident) = &index_column.column.expr
                        && let Some(column) = columns
                            .iter_mut()
                            .find(|column| column.name.eq_ignore_ascii_case(&column_ident.value))
                    {
                        column.primary_key = true;
                        column.not_null = true;
                    }
                }
            }
        }
        if columns.is_empty()
            && let Some(query) = &create.query
        {
            columns = self.derive_query_columns(query);
        }

        self.insert_table(Table {
            name: ident.value.clone(),
            kind: TableKind::Table,
            origin: TableOrigin::Migration,
            columns,
            location: Self::source_location(uri, document, ident.span),
        });
    }

    fn apply_create_view(&mut self, create: &CreateView, uri: Option<&Uri>, document: &Document) {
        let Some(ident) = create.name.simple_ident() else {
            return;
        };
        if create.if_not_exists && !create.or_replace && self.table(&ident.value).is_some() {
            return;
        }

        let columns = if create.columns.is_empty() {
            self.derive_query_columns(&create.query)
        } else {
            create
                .columns
                .iter()
                .map(|def| Column {
                    name: def.name.value.clone(),
                    data_type: def.data_type.as_ref().map(ToString::to_string),
                    not_null: false,
                    primary_key: false,
                    default: None,
                    location: Self::source_location(uri, document, def.name.span),
                })
                .collect()
        };

        self.insert_table(Table {
            name: ident.value.clone(),
            kind: TableKind::View,
            origin: TableOrigin::Migration,
            columns,
            location: Self::source_location(uri, document, ident.span),
        });
    }

    fn apply_alter_table(&mut self, alter: &AlterTable, uri: Option<&Uri>, document: &Document) {
        let Some(ident) = alter.name.simple_ident() else {
            return;
        };
        // Take the table out so renames can re-key it on reinsertion.
        let Some(mut table) = self.tables.remove(&ident.value.to_ascii_lowercase()) else {
            return;
        };

        for operation in &alter.operations {
            match operation {
                AlterTableOperation::AddColumn {
                    column_def,
                    if_not_exists,
                    ..
                } => {
                    if *if_not_exists && table.column(&column_def.name.value).is_some() {
                        continue;
                    }
                    table
                        .columns
                        .push(Self::column_from_def(column_def, uri, document));
                }
                AlterTableOperation::DropColumn { column_names, .. } => {
                    table.columns.retain(|column| {
                        !column_names
                            .iter()
                            .any(|name| name.value.eq_ignore_ascii_case(&column.name))
                    });
                }
                AlterTableOperation::RenameColumn {
                    old_column_name,
                    new_column_name,
                } => {
                    if let Some(column) = table
                        .columns
                        .iter_mut()
                        .find(|column| column.name.eq_ignore_ascii_case(&old_column_name.value))
                    {
                        column.name = new_column_name.value.clone();
                        column.location =
                            Self::source_location(uri, document, new_column_name.span);
                    }
                }
                AlterTableOperation::RenameTable { table_name } => {
                    let (RenameTableNameKind::As(name) | RenameTableNameKind::To(name)) =
                        table_name;
                    if let Some(new_ident) = name.simple_ident() {
                        table.name = new_ident.value.clone();
                        table.location = Self::source_location(uri, document, new_ident.span);
                    }
                }
                _ => {}
            }
        }

        self.insert_table(table);
    }

    /// Best-effort column list for the `SELECT` defining a view, a
    /// `CREATE TABLE ... AS` result, or a query-local relation (CTE or
    /// derived subquery). Column references resolve through relations
    /// already known to the index (retaining their type and definition
    /// location); wildcards expand through them; expressions without an
    /// alias are skipped.
    pub(crate) fn derive_query_columns(&self, query: &Query) -> Vec<Column> {
        let SetExpr::Select(select) = query.body.as_ref() else {
            return Vec::new();
        };

        let from_tables: Vec<&Table> = select
            .from
            .iter()
            .flat_map(|table_with_joins| {
                std::iter::once(&table_with_joins.relation)
                    .chain(table_with_joins.joins.iter().map(|join| &join.relation))
            })
            .filter_map(|factor| match factor {
                TableFactor::Table { name, .. } => name
                    .simple_ident()
                    .and_then(|ident| self.table(&ident.value)),
                _ => None,
            })
            .collect();
        let resolve_column = |name: &str| {
            from_tables
                .iter()
                .find_map(|table| table.column(name))
                .cloned()
        };

        let mut columns = Vec::new();
        for item in &select.projection {
            match item {
                SelectItem::UnnamedExpr(Expr::Identifier(ident)) => {
                    columns.push(resolve_column(&ident.value).unwrap_or(Column {
                        name: ident.value.clone(),
                        data_type: None,
                        not_null: false,
                        primary_key: false,
                        default: None,
                        location: None,
                    }));
                }
                SelectItem::UnnamedExpr(Expr::CompoundIdentifier(parts)) => {
                    if let Some(last) = parts.last() {
                        columns.push(resolve_column(&last.value).unwrap_or(Column {
                            name: last.value.clone(),
                            data_type: None,
                            not_null: false,
                            primary_key: false,
                            default: None,
                            location: None,
                        }));
                    }
                }
                SelectItem::ExprWithAlias { alias, .. } => {
                    columns.push(Column {
                        name: alias.value.clone(),
                        data_type: None,
                        not_null: false,
                        primary_key: false,
                        default: None,
                        location: None,
                    });
                }
                SelectItem::Wildcard(_) => {
                    for table in &from_tables {
                        columns.extend(table.columns.iter().cloned());
                    }
                }
                SelectItem::QualifiedWildcard(
                    SelectItemQualifiedWildcardKind::ObjectName(name),
                    _,
                ) => {
                    if let Some(table) = name
                        .simple_ident()
                        .and_then(|ident| self.table(&ident.value))
                    {
                        columns.extend(table.columns.iter().cloned());
                    }
                }
                _ => {}
            }
        }
        columns
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sqlite_schema(sql: &str) -> Schema {
        let mut schema = Schema::default();
        schema.apply_sql(sql, DatabaseKind::Sqlite, None);
        schema
    }

    #[test]
    fn create_table_records_columns_and_constraints() {
        let schema = sqlite_schema(
            "CREATE TABLE users (\n\
             id INTEGER PRIMARY KEY,\n\
             email TEXT NOT NULL,\n\
             bio TEXT,\n\
             created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP\n\
             );",
        );
        let users = schema.table("users").expect("users exists");
        assert_eq!(users.kind, TableKind::Table);
        assert_eq!(users.columns.len(), 4);

        let id = users.column("id").expect("id exists");
        assert!(id.primary_key);
        assert!(id.not_null);
        assert_eq!(id.data_type.as_deref(), Some("INTEGER"));

        let email = users.column("EMAIL").expect("lookup is case-insensitive");
        assert!(email.not_null);
        assert!(!email.primary_key);

        let bio = users.column("bio").expect("bio exists");
        assert!(!bio.not_null);

        let created_at = users.column("created_at").expect("created_at exists");
        assert_eq!(created_at.default.as_deref(), Some("CURRENT_TIMESTAMP"));
    }

    #[test]
    fn table_level_primary_key_marks_columns() {
        let schema = sqlite_schema(
            "CREATE TABLE memberships (
                user_id INTEGER,
                group_id INTEGER,
                PRIMARY KEY (user_id, group_id)
            );",
        );
        let table = schema.table("memberships").expect("exists");
        assert!(table.column("user_id").expect("exists").primary_key);
        assert!(table.column("group_id").expect("exists").primary_key);
    }

    #[test]
    fn alter_table_add_drop_rename() {
        let schema = sqlite_schema(
            "CREATE TABLE users (id INTEGER PRIMARY KEY, nickname TEXT, legacy TEXT);
             ALTER TABLE users ADD COLUMN email TEXT NOT NULL;
             ALTER TABLE users DROP COLUMN legacy;
             ALTER TABLE users RENAME COLUMN nickname TO display_name;",
        );
        let users = schema.table("users").expect("exists");
        assert!(users.column("email").expect("added").not_null);
        assert!(users.column("legacy").is_none());
        assert!(users.column("nickname").is_none());
        assert!(users.column("display_name").is_some());
    }

    #[test]
    fn rename_table_rekeys_the_index() {
        let schema = sqlite_schema(
            "CREATE TABLE user (id INTEGER PRIMARY KEY);
             ALTER TABLE user RENAME TO users;",
        );
        assert!(schema.table("user").is_none());
        assert!(schema.table("users").is_some());
    }

    #[test]
    fn drop_table_removes_it() {
        let schema = sqlite_schema(
            "CREATE TABLE temp_stuff (id INTEGER);
             DROP TABLE temp_stuff;",
        );
        assert!(schema.table("temp_stuff").is_none());
    }

    #[test]
    fn create_if_not_exists_keeps_the_existing_definition() {
        let schema = sqlite_schema(
            "CREATE TABLE users (id INTEGER PRIMARY KEY);
             CREATE TABLE IF NOT EXISTS users (other TEXT);",
        );
        let users = schema.table("users").expect("exists");
        assert!(users.column("id").is_some());
        assert!(users.column("other").is_none());
    }

    #[test]
    fn view_columns_resolve_through_known_tables() {
        let schema = sqlite_schema(
            "CREATE TABLE users (id INTEGER PRIMARY KEY, email TEXT NOT NULL, bio TEXT);
             CREATE VIEW user_emails AS SELECT id, email AS address FROM users;
             CREATE VIEW all_users AS SELECT * FROM users;",
        );

        let view = schema.table("user_emails").expect("view exists");
        assert_eq!(view.kind, TableKind::View);
        // `id` resolved through `users`, keeping its type.
        assert_eq!(
            view.column("id").expect("exists").data_type.as_deref(),
            Some("INTEGER")
        );
        assert!(view.column("address").is_some());
        assert!(view.column("email").is_none());

        let all = schema.table("all_users").expect("view exists");
        assert_eq!(all.columns.len(), 3);
    }
}
