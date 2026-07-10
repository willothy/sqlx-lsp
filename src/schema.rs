//! Schema index for the workspace's database objects.
//!
//! The index is built by replaying the project's sqlx migrations in version
//! order and can be augmented with objects introspected from a live database.
//! Objects defined in migrations carry source locations so goto-definition
//! can jump to the defining statement.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use sqlparser::ast::{
    AlterTable, AlterTableOperation, ColumnDef, ColumnOption, CreateTable, CreateView, DataType,
    Expr, ObjectType, Query, RenameTableNameKind, SelectItem, SelectItemQualifiedWildcardKind,
    SetExpr, Statement, TableConstraint, TableFactor,
};
use sqlparser::tokenizer::Span as SqlSpan;
use tower_lsp::lsp_types::{Location, Range, Url};

use crate::db::DatabaseKind;
use crate::document::Document;
use crate::parse::{ObjectNameExt, ParsedSql};

/// Where a schema object is defined in workspace sources.
#[derive(Debug, Clone, PartialEq)]
pub struct SourceLocation {
    /// URI of the defining file.
    pub uri: Url,
    /// Range of the defining identifier within that file.
    pub range: Range,
}

impl From<SourceLocation> for Location {
    fn from(location: SourceLocation) -> Location {
        Location::new(location.uri, location.range)
    }
}

/// How a schema object became known to the index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableOrigin {
    /// Defined by a SQL migration in the workspace.
    Migration,
    /// Discovered by introspecting a live database.
    Database,
}

/// Whether a relation is a table or a view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableKind {
    /// A base table.
    Table,
    /// A view.
    View,
}

/// One column of a table or view.
#[derive(Debug, Clone, PartialEq)]
pub struct Column {
    /// Name as written in the defining source.
    pub name: String,
    /// Declared data type; `None` for typeless (SQLite) or derived columns.
    pub data_type: Option<String>,
    /// Whether the column is declared `NOT NULL` (or is part of the primary
    /// key, which implies it).
    pub not_null: bool,
    /// Whether the column is part of the primary key.
    pub primary_key: bool,
    /// The default value expression, rendered as SQL.
    pub default: Option<String>,
    /// Location of the column definition, when defined in a migration.
    pub location: Option<SourceLocation>,
}

/// A table or view known to the schema index.
#[derive(Debug, Clone, PartialEq)]
pub struct Table {
    /// Name as written in the defining source.
    pub name: String,
    /// Whether this is a base table or a view.
    pub kind: TableKind,
    /// How the object became known to the index.
    pub origin: TableOrigin,
    /// The relation's columns, in definition order.
    pub columns: Vec<Column>,
    /// Location of the defining statement, when defined in a migration.
    pub location: Option<SourceLocation>,
}

impl Table {
    /// Case-insensitive column lookup.
    pub fn column(&self, name: &str) -> Option<&Column> {
        self.columns
            .iter()
            .find(|column| column.name.eq_ignore_ascii_case(name))
    }
}

/// Failure to build a schema from a migrations directory.
#[derive(Debug, thiserror::Error)]
pub enum SchemaError {
    /// The migrations directory could not be enumerated.
    #[error("failed to read migrations directory {path}: {source}")]
    ReadDir {
        /// The directory that failed to enumerate.
        path: PathBuf,
        /// The underlying I/O error.
        #[source]
        source: io::Error,
    },
    /// A migration file could not be read.
    #[error("failed to read migration file {path}: {source}")]
    ReadFile {
        /// The file that failed to read.
        path: PathBuf,
        /// The underlying I/O error.
        #[source]
        source: io::Error,
    },
    /// A migration path could not be converted to a file URI.
    #[error("migration path {path} is not representable as a file URI")]
    InvalidPath {
        /// The offending path.
        path: PathBuf,
    },
}

/// A migration file on disk, ordered by its sqlx version prefix.
struct MigrationFile {
    /// The integer version prefix of the filename
    /// (`20240101120000` in `20240101120000_create_users.sql`).
    version: Option<i64>,
    path: PathBuf,
}

impl MigrationFile {
    fn new(path: PathBuf) -> Self {
        let version = path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.split_once('_'))
            .and_then(|(prefix, _)| prefix.parse().ok());
        MigrationFile { version, path }
    }
}

/// All tables and views known for the workspace's database.
#[derive(Debug, Clone, Default)]
pub struct Schema {
    /// Relations keyed by ASCII-lowercased name, matching SQL's
    /// case-insensitive identifier resolution.
    tables: BTreeMap<String, Table>,
}

impl Schema {
    /// Case-insensitive table/view lookup.
    pub fn table(&self, name: &str) -> Option<&Table> {
        self.tables.get(&name.to_ascii_lowercase())
    }

    /// All known relations, ordered by name.
    pub fn tables(&self) -> impl Iterator<Item = &Table> {
        self.tables.values()
    }

    /// Inserts or replaces a relation.
    pub fn insert_table(&mut self, table: Table) {
        self.tables.insert(table.name.to_ascii_lowercase(), table);
    }

    /// Adds introspected relations for anything not already defined by
    /// migrations. Migration definitions win because they carry source
    /// locations; the database fills in objects created outside them.
    pub fn merge_database_tables(&mut self, tables: Vec<Table>) {
        for table in tables {
            if self.table(&table.name).is_none() {
                self.insert_table(table);
            }
        }
    }

    /// Builds a schema by replaying the `.sql` migrations under `dir` in
    /// version order. Reversible down-migrations (`*.down.sql`) are skipped.
    ///
    /// A missing directory yields an empty schema; a workspace without
    /// migrations is not an error.
    pub fn load_migrations(dir: &Path, kind: DatabaseKind) -> Result<Schema, SchemaError> {
        let mut schema = Schema::default();
        if !dir.is_dir() {
            return Ok(schema);
        }

        let entries = std::fs::read_dir(dir).map_err(|source| SchemaError::ReadDir {
            path: dir.to_owned(),
            source,
        })?;
        let mut files = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|source| SchemaError::ReadDir {
                path: dir.to_owned(),
                source,
            })?;
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if !name.ends_with(".sql") || name.ends_with(".down.sql") {
                continue;
            }
            files.push(MigrationFile::new(path));
        }
        files.sort_by(|a, b| a.version.cmp(&b.version).then_with(|| a.path.cmp(&b.path)));

        for file in files {
            let text =
                std::fs::read_to_string(&file.path).map_err(|source| SchemaError::ReadFile {
                    path: file.path.clone(),
                    source,
                })?;
            let absolute =
                std::path::absolute(&file.path).map_err(|source| SchemaError::ReadFile {
                    path: file.path.clone(),
                    source,
                })?;
            let uri = Url::from_file_path(&absolute).map_err(|()| SchemaError::InvalidPath {
                path: file.path.clone(),
            })?;
            schema.apply_sql(&text, kind, Some(&uri));
        }
        Ok(schema)
    }

    /// Applies every DDL statement in `sql` to the index. Statements that
    /// fail to parse are skipped; non-DDL statements are ignored. When `uri`
    /// is provided, definitions are recorded with source locations.
    pub fn apply_sql(&mut self, sql: &str, kind: DatabaseKind, uri: Option<&Url>) {
        let parsed = ParsedSql::parse(kind.dialect(), sql);
        let document = Document::new(sql.to_owned(), 0);
        for statement in &parsed.statements {
            self.apply_statement(statement, uri, &document);
        }
    }

    fn apply_statement(&mut self, statement: &Statement, uri: Option<&Url>, document: &Document) {
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
        uri: Option<&Url>,
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

    fn column_from_def(def: &ColumnDef, uri: Option<&Url>, document: &Document) -> Column {
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

    fn apply_create_table(&mut self, create: &CreateTable, uri: Option<&Url>, document: &Document) {
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

    fn apply_create_view(&mut self, create: &CreateView, uri: Option<&Url>, document: &Document) {
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

    fn apply_alter_table(&mut self, alter: &AlterTable, uri: Option<&Url>, document: &Document) {
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

    /// Best-effort column list for the `SELECT` defining a view or a
    /// `CREATE TABLE ... AS` result. Column references resolve through
    /// relations already known to the index (retaining their type and
    /// definition location); wildcards expand through them; expressions
    /// without an alias are skipped.
    fn derive_query_columns(&self, query: &Query) -> Vec<Column> {
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

    #[test]
    fn migrations_load_in_version_order_and_skip_down_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let write = |name: &str, contents: &str| {
            std::fs::write(dir.path().join(name), contents).expect("write migration");
        };
        // Written out of order on purpose; version 2 renames what 1 created.
        write("2_rename.up.sql", "ALTER TABLE user RENAME TO users;");
        write(
            "1_init.up.sql",
            "CREATE TABLE user (id INTEGER PRIMARY KEY);",
        );
        write("2_rename.down.sql", "ALTER TABLE users RENAME TO user;");
        write("not-sql.txt", "ignore me");

        let schema = Schema::load_migrations(dir.path(), DatabaseKind::Sqlite).expect("loads");
        assert!(schema.table("users").is_some());
        assert!(schema.table("user").is_none());
    }

    #[test]
    fn migration_definitions_carry_source_locations() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("1_init.sql");
        std::fs::write(&path, "CREATE TABLE users (\n  id INTEGER PRIMARY KEY\n);")
            .expect("write migration");

        let schema = Schema::load_migrations(dir.path(), DatabaseKind::Sqlite).expect("loads");
        let users = schema.table("users").expect("exists");

        let table_location = users.location.as_ref().expect("has location");
        assert!(table_location.uri.path().ends_with("1_init.sql"));
        assert_eq!(table_location.range.start.line, 0);
        assert_eq!(table_location.range.start.character, 13);

        let id_location = users
            .column("id")
            .expect("exists")
            .location
            .as_ref()
            .expect("has location")
            .clone();
        assert_eq!(id_location.range.start.line, 1);
        assert_eq!(id_location.range.start.character, 2);
    }

    #[test]
    fn database_tables_fill_gaps_but_never_override_migrations() {
        let mut schema = sqlite_schema("CREATE TABLE users (id INTEGER PRIMARY KEY);");
        let database_table = |name: &str| Table {
            name: name.to_owned(),
            kind: TableKind::Table,
            origin: TableOrigin::Database,
            columns: Vec::new(),
            location: None,
        };
        schema.merge_database_tables(vec![database_table("users"), database_table("sessions")]);

        let users = schema.table("users").expect("exists");
        assert_eq!(users.origin, TableOrigin::Migration);
        assert!(users.column("id").is_some());
        let sessions = schema.table("sessions").expect("merged in");
        assert_eq!(sessions.origin, TableOrigin::Database);
    }

    #[test]
    fn missing_migrations_directory_yields_empty_schema() {
        let schema = Schema::load_migrations(
            Path::new("/nonexistent/definitely/missing"),
            DatabaseKind::Sqlite,
        )
        .expect("missing dir is not an error");
        assert_eq!(schema.tables().count(), 0);
    }
}
