//! Read-only introspection of file-backed SQLite databases.

use std::path::{Path, PathBuf};

use sqlx::sqlite::SqliteConnectOptions;
use sqlx::{ConnectOptions, Connection, Row};

use super::{IntrospectError, redact_url};
use crate::db::DatabaseKind;
use crate::schema::{Column, Table, TableKind, TableOrigin};

/// A file-backed SQLite database reachable from the workspace.
pub struct SqliteDatabase {
    path: PathBuf,
}

impl SqliteDatabase {
    /// Resolves `url` (as found in `DATABASE_URL`) to a SQLite database file,
    /// interpreting relative paths against the workspace `root`.
    ///
    /// Accepted forms are `sqlite://<path>` and `sqlite:<path>`, optionally
    /// followed by `?<params>`. In-memory databases are rejected since there
    /// is nothing durable to introspect.
    pub fn from_url(url: &str, root: &Path) -> Result<SqliteDatabase, IntrospectError> {
        // The URL may be a network backend's (with credentials) handed to the
        // wrong parser; never echo it back unredacted.
        let unsupported = || IntrospectError::UnsupportedUrl {
            backend: DatabaseKind::Sqlite,
            url: redact_url(url),
        };
        let rest = url
            .strip_prefix("sqlite://")
            .or_else(|| url.strip_prefix("sqlite:"))
            .ok_or_else(unsupported)?;
        let rest = rest.split('?').next().unwrap_or(rest);
        if rest.is_empty() || rest == ":memory:" || rest.starts_with("file::memory:") {
            return Err(unsupported());
        }
        let path = Path::new(rest);
        let path = if path.is_absolute() {
            path.to_owned()
        } else {
            root.join(path)
        };
        Ok(SqliteDatabase { path })
    }

    /// The resolved database file path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Reads every table and view (with columns) from the database, except
    /// the `migrations_table` sqlx uses for bookkeeping.
    ///
    /// The connection is read-only, so introspection can never mutate or
    /// create the user's database.
    pub async fn introspect(&self, migrations_table: &str) -> Result<Vec<Table>, IntrospectError> {
        if !self.path.is_file() {
            return Err(IntrospectError::DatabaseMissing {
                path: self.path.clone(),
            });
        }
        let query_error = |source| IntrospectError::Query {
            backend: DatabaseKind::Sqlite,
            target: self.path.display().to_string(),
            source,
        };

        let mut connection = SqliteConnectOptions::new()
            .filename(&self.path)
            .read_only(true)
            .connect()
            .await
            .map_err(query_error)?;

        let relations = sqlx::query(
            "SELECT name, type FROM sqlite_master \
             WHERE type IN ('table', 'view') AND name NOT LIKE 'sqlite_%' \
               AND name != ? \
             ORDER BY name",
        )
        .bind(migrations_table)
        .fetch_all(&mut connection)
        .await
        .map_err(query_error)?;

        let mut tables = Vec::with_capacity(relations.len());
        for relation in relations {
            let name: String = relation.try_get("name").map_err(query_error)?;
            let relation_type: String = relation.try_get("type").map_err(query_error)?;

            let column_rows = sqlx::query(
                "SELECT name, type, \"notnull\", dflt_value, pk \
                 FROM pragma_table_info(?) ORDER BY cid",
            )
            .bind(&name)
            .fetch_all(&mut connection)
            .await
            .map_err(query_error)?;

            let mut columns = Vec::with_capacity(column_rows.len());
            for row in column_rows {
                let column_name: String = row.try_get("name").map_err(query_error)?;
                let data_type: String = row.try_get("type").map_err(query_error)?;
                let not_null: i64 = row.try_get("notnull").map_err(query_error)?;
                let default: Option<String> = row.try_get("dflt_value").map_err(query_error)?;
                let primary_key: i64 = row.try_get("pk").map_err(query_error)?;
                columns.push(Column {
                    name: column_name,
                    data_type: (!data_type.is_empty()).then_some(data_type),
                    not_null: not_null != 0 || primary_key != 0,
                    primary_key: primary_key != 0,
                    default,
                    location: None,
                });
            }

            tables.push(Table {
                name,
                kind: if relation_type == "view" {
                    TableKind::View
                } else {
                    TableKind::Table
                },
                origin: TableOrigin::Database,
                columns,
                location: None,
            });
        }

        connection.close().await.map_err(query_error)?;
        Ok(tables)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_resolution_handles_prefixes_and_relative_paths() {
        let root = Path::new("/workspace");
        let database = SqliteDatabase::from_url("sqlite://app.db?mode=rwc", root).expect("valid");
        assert_eq!(database.path(), Path::new("/workspace/app.db"));

        let database = SqliteDatabase::from_url("sqlite:/var/data/app.db", root).expect("valid");
        assert_eq!(database.path(), Path::new("/var/data/app.db"));

        assert!(matches!(
            SqliteDatabase::from_url("sqlite::memory:", root),
            Err(IntrospectError::UnsupportedUrl { .. })
        ));
        assert!(matches!(
            SqliteDatabase::from_url("postgres://localhost/app", root),
            Err(IntrospectError::UnsupportedUrl { .. })
        ));
    }

    #[tokio::test]
    async fn introspects_tables_views_and_columns() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("app.db");

        let mut connection = SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true)
            .connect()
            .await
            .expect("create db");
        sqlx::query(
            "CREATE TABLE users (
                id INTEGER PRIMARY KEY,
                email TEXT NOT NULL,
                bio TEXT DEFAULT 'hello'
            )",
        )
        .execute(&mut connection)
        .await
        .expect("create table");
        sqlx::query("CREATE VIEW user_emails AS SELECT id, email FROM users")
            .execute(&mut connection)
            .await
            .expect("create view");
        connection.close().await.expect("close");

        let url = format!("sqlite://{}", path.display());
        let database = SqliteDatabase::from_url(&url, dir.path()).expect("valid url");
        let tables = database
            .introspect("_sqlx_migrations")
            .await
            .expect("introspects");

        let users = tables
            .iter()
            .find(|table| table.name == "users")
            .expect("users table");
        assert_eq!(users.kind, TableKind::Table);
        assert_eq!(users.origin, TableOrigin::Database);
        let id = users.column("id").expect("id column");
        assert!(id.primary_key);
        assert_eq!(id.data_type.as_deref(), Some("INTEGER"));
        let email = users.column("email").expect("email column");
        assert!(email.not_null);
        let bio = users.column("bio").expect("bio column");
        assert_eq!(bio.default.as_deref(), Some("'hello'"));

        let view = tables
            .iter()
            .find(|table| table.name == "user_emails")
            .expect("view");
        assert_eq!(view.kind, TableKind::View);
        assert_eq!(view.columns.len(), 2);
    }

    #[tokio::test]
    async fn missing_database_file_is_reported() {
        let dir = tempfile::tempdir().expect("tempdir");
        let database = SqliteDatabase::from_url("sqlite://nope.db", dir.path()).expect("valid url");
        assert!(matches!(
            database.introspect("_sqlx_migrations").await,
            Err(IntrospectError::DatabaseMissing { .. })
        ));
    }
}
