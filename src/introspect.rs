//! Live database introspection.
//!
//! When the workspace's `DATABASE_URL` points at a reachable database, the
//! database itself is the most authoritative source for the schema — it
//! reflects migrations that were actually applied, plus anything created
//! outside the migrations directory. Introspected relations carry no source
//! locations, so the schema index prefers migration-defined entries and uses
//! these to fill the gaps. SQLite, PostgreSQL, and MySQL are supported;
//! sessions are read-only so introspection can never mutate the user's
//! database.

use std::path::{Path, PathBuf};
use std::str::FromStr;

use sqlx::mysql::MySqlConnectOptions;
use sqlx::postgres::PgConnectOptions;
use sqlx::postgres::types::Oid;
use sqlx::sqlite::SqliteConnectOptions;
use sqlx::{ConnectOptions, Connection, Row};

use crate::db::DatabaseKind;
use crate::schema::{Column, Table, TableKind, TableOrigin};

/// Replaces the password portion of a connection URL's userinfo with `***`,
/// making the URL safe for logs and error messages.
fn redact_url(url: &str) -> String {
    let Some(scheme_end) = url.find("://") else {
        return url.to_owned();
    };
    let rest = &url[scheme_end + 3..];
    // Userinfo ends at the first '@' before the host.
    let Some(at) = rest.find('@') else {
        return url.to_owned();
    };
    let userinfo = &rest[..at];
    match userinfo.find(':') {
        Some(colon) => format!(
            "{}{}:***{}",
            &url[..scheme_end + 3],
            &userinfo[..colon],
            &rest[at..]
        ),
        None => url.to_owned(),
    }
}

/// Reads `DATABASE_URL` the way sqlx does: from the process environment
/// first, then from the first `.env` file found in `roots` (the workspace
/// root followed by its member crate directories).
pub fn discover_database_url<'a>(roots: impl IntoIterator<Item = &'a Path>) -> Option<String> {
    if let Ok(url) = std::env::var("DATABASE_URL")
        && !url.is_empty()
    {
        return Some(url);
    }
    for root in roots {
        if let Ok(env_file) = std::fs::read_to_string(root.join(".env"))
            && let Some(url) = EnvFile::new(&env_file).value_of("DATABASE_URL")
        {
            return Some(url);
        }
    }
    None
}

/// A parsed view over dotenv-style `KEY=VALUE` file contents.
struct EnvFile<'a> {
    text: &'a str,
}

impl<'a> EnvFile<'a> {
    fn new(text: &'a str) -> Self {
        EnvFile { text }
    }

    /// The value of `key`, with optional `export` prefix and surrounding
    /// single or double quotes handled.
    fn value_of(&self, key: &str) -> Option<String> {
        for line in self.text.lines() {
            let line = line.trim();
            let line = line
                .strip_prefix("export ")
                .map(str::trim_start)
                .unwrap_or(line);
            if line.starts_with('#') {
                continue;
            }
            let Some((name, value)) = line.split_once('=') else {
                continue;
            };
            if name.trim() != key {
                continue;
            }
            let value = value.trim();
            let unquoted = value
                .strip_prefix('"')
                .and_then(|rest| rest.strip_suffix('"'))
                .or_else(|| {
                    value
                        .strip_prefix('\'')
                        .and_then(|rest| rest.strip_suffix('\''))
                })
                .unwrap_or(value);
            return Some(unquoted.to_owned());
        }
        None
    }
}

/// Failure to introspect a live database.
#[derive(Debug, thiserror::Error)]
pub enum IntrospectError {
    /// The URL does not describe a file-backed SQLite database.
    #[error("DATABASE_URL is not a file-backed sqlite database: {url}")]
    UnsupportedUrl {
        /// The offending URL.
        url: String,
    },
    /// The database file does not exist (yet); common in fresh checkouts
    /// where migrations have never been run.
    #[error("sqlite database file {path} does not exist")]
    DatabaseMissing {
        /// The resolved database file path.
        path: PathBuf,
    },
    /// Connecting to or querying the database failed.
    #[error("failed to introspect sqlite database {path}: {source}")]
    Query {
        /// The resolved database file path.
        path: PathBuf,
        /// The underlying sqlx error.
        #[source]
        source: sqlx::Error,
    },
    /// The URL is not a valid PostgreSQL connection string.
    #[error("invalid postgres DATABASE_URL {url}: {source}")]
    PostgresUrl {
        /// The offending URL, with any password redacted.
        url: String,
        /// The underlying sqlx error.
        #[source]
        source: sqlx::Error,
    },
    /// Connecting to or querying the PostgreSQL database failed.
    #[error("failed to introspect postgres database {url}: {source}")]
    PostgresQuery {
        /// The database URL, with any password redacted.
        url: String,
        /// The underlying sqlx error.
        #[source]
        source: sqlx::Error,
    },
    /// The URL is not a valid MySQL connection string.
    #[error("invalid mysql DATABASE_URL {url}: {source}")]
    MySqlUrl {
        /// The offending URL, with any password redacted.
        url: String,
        /// The underlying sqlx error.
        #[source]
        source: sqlx::Error,
    },
    /// The MySQL URL names no database, so there is no schema to introspect.
    #[error("mysql DATABASE_URL must name a database: {url}")]
    MySqlMissingDatabase {
        /// The offending URL, with any password redacted.
        url: String,
    },
    /// Connecting to or querying the MySQL database failed.
    #[error("failed to introspect mysql database {url}: {source}")]
    MySqlQuery {
        /// The database URL, with any password redacted.
        url: String,
        /// The underlying sqlx error.
        #[source]
        source: sqlx::Error,
    },
}

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
        let unsupported = || IntrospectError::UnsupportedUrl {
            url: url.to_owned(),
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

    /// Reads every table and view (with columns) from the database.
    ///
    /// The connection is read-only, so introspection can never mutate or
    /// create the user's database.
    pub async fn introspect(&self) -> Result<Vec<Table>, IntrospectError> {
        if !self.path.is_file() {
            return Err(IntrospectError::DatabaseMissing {
                path: self.path.clone(),
            });
        }
        let query_error = |source| IntrospectError::Query {
            path: self.path.clone(),
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
             ORDER BY name",
        )
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

/// A PostgreSQL database reachable from the workspace.
pub struct PostgresDatabase {
    options: PgConnectOptions,
    /// The connection URL with any password redacted, safe for logs and
    /// error messages.
    display_url: String,
}

impl PostgresDatabase {
    /// Parses a `postgres://` / `postgresql://` connection URL.
    pub fn from_url(url: &str) -> Result<PostgresDatabase, IntrospectError> {
        if !url.starts_with("postgres://") && !url.starts_with("postgresql://") {
            return Err(IntrospectError::UnsupportedUrl {
                url: redact_url(url),
            });
        }
        let display_url = redact_url(url);
        let options = PgConnectOptions::from_str(url)
            .map_err(|source| IntrospectError::PostgresUrl {
                url: display_url.clone(),
                source,
            })?
            .application_name("sqlx-lsp")
            // Server-enforced read-only for the whole session, so
            // introspection can never mutate the user's database.
            .options([("default_transaction_read_only", "on")]);
        Ok(PostgresDatabase {
            options,
            display_url,
        })
    }

    /// The connection URL with any password replaced by `***`.
    pub fn display_url(&self) -> &str {
        &self.display_url
    }

    /// Reads every table, view, and materialized view visible on the
    /// connection's search path, with columns, from the system catalogs.
    pub async fn introspect(&self) -> Result<Vec<Table>, IntrospectError> {
        let query_error = |source| IntrospectError::PostgresQuery {
            url: self.display_url.clone(),
            source,
        };

        let mut connection = self.options.connect().await.map_err(query_error)?;

        let relations = sqlx::query(
            "SELECT c.oid, c.relname::text AS name, c.relkind::text AS kind \
             FROM pg_catalog.pg_class c \
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
             WHERE c.relkind IN ('r', 'p', 'v', 'm') \
               AND n.nspname NOT IN ('pg_catalog', 'information_schema') \
               AND pg_catalog.pg_table_is_visible(c.oid) \
             ORDER BY c.relname",
        )
        .fetch_all(&mut connection)
        .await
        .map_err(query_error)?;

        let mut tables = Vec::with_capacity(relations.len());
        for relation in relations {
            let oid: Oid = relation.try_get("oid").map_err(query_error)?;
            let name: String = relation.try_get("name").map_err(query_error)?;
            let relkind: String = relation.try_get("kind").map_err(query_error)?;

            let column_rows = sqlx::query(
                "SELECT a.attname::text AS name, \
                        pg_catalog.format_type(a.atttypid, a.atttypmod) AS data_type, \
                        a.attnotnull AS not_null, \
                        pg_catalog.pg_get_expr(d.adbin, d.adrelid) AS default_expr, \
                        COALESCE(a.attnum = ANY(pk.conkey), FALSE) AS primary_key \
                 FROM pg_catalog.pg_attribute a \
                 LEFT JOIN pg_catalog.pg_attrdef d \
                        ON d.adrelid = a.attrelid AND d.adnum = a.attnum \
                 LEFT JOIN pg_catalog.pg_constraint pk \
                        ON pk.conrelid = a.attrelid AND pk.contype = 'p' \
                 WHERE a.attrelid = $1 AND a.attnum > 0 AND NOT a.attisdropped \
                 ORDER BY a.attnum",
            )
            .bind(oid)
            .fetch_all(&mut connection)
            .await
            .map_err(query_error)?;

            let mut columns = Vec::with_capacity(column_rows.len());
            for row in column_rows {
                let column_name: String = row.try_get("name").map_err(query_error)?;
                let data_type: String = row.try_get("data_type").map_err(query_error)?;
                let not_null: bool = row.try_get("not_null").map_err(query_error)?;
                let default: Option<String> = row.try_get("default_expr").map_err(query_error)?;
                let primary_key: bool = row.try_get("primary_key").map_err(query_error)?;
                columns.push(Column {
                    name: column_name,
                    data_type: (!data_type.is_empty()).then_some(data_type),
                    not_null: not_null || primary_key,
                    primary_key,
                    default,
                    location: None,
                });
            }

            tables.push(Table {
                name,
                kind: match relkind.as_str() {
                    "v" | "m" => TableKind::View,
                    _ => TableKind::Table,
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

/// A MySQL (or MariaDB) database reachable from the workspace.
pub struct MySqlDatabase {
    options: MySqlConnectOptions,
    /// The connection URL with any password redacted, safe for logs and
    /// error messages.
    display_url: String,
}

impl MySqlDatabase {
    /// Parses a `mysql://` connection URL. The URL must name a database:
    /// MySQL scopes relations to a database rather than a search path, so
    /// without one there is nothing to introspect.
    pub fn from_url(url: &str) -> Result<MySqlDatabase, IntrospectError> {
        if !url.starts_with("mysql://") {
            return Err(IntrospectError::UnsupportedUrl {
                url: redact_url(url),
            });
        }
        let display_url = redact_url(url);
        let options =
            MySqlConnectOptions::from_str(url).map_err(|source| IntrospectError::MySqlUrl {
                url: display_url.clone(),
                source,
            })?;
        if options.get_database().is_none_or(str::is_empty) {
            return Err(IntrospectError::MySqlMissingDatabase { url: display_url });
        }
        Ok(MySqlDatabase {
            options,
            display_url,
        })
    }

    /// The connection URL with any password replaced by `***`.
    pub fn display_url(&self) -> &str {
        &self.display_url
    }

    /// Reads every table and view (with columns) of the URL's database from
    /// `information_schema`.
    pub async fn introspect(&self) -> Result<Vec<Table>, IntrospectError> {
        let query_error = |source| IntrospectError::MySqlQuery {
            url: self.display_url.clone(),
            source,
        };

        let mut connection = self.options.connect().await.map_err(query_error)?;
        // Server-enforced read-only for the whole session, so introspection
        // can never mutate the user's database.
        sqlx::query("SET SESSION TRANSACTION READ ONLY")
            .execute(&mut connection)
            .await
            .map_err(query_error)?;

        let relations = sqlx::query(
            "SELECT TABLE_NAME AS name, TABLE_TYPE AS kind \
             FROM information_schema.TABLES \
             WHERE TABLE_SCHEMA = DATABASE() \
             ORDER BY TABLE_NAME",
        )
        .fetch_all(&mut connection)
        .await
        .map_err(query_error)?;

        let mut tables = Vec::with_capacity(relations.len());
        for relation in relations {
            let name: String = relation.try_get("name").map_err(query_error)?;
            let table_type: String = relation.try_get("kind").map_err(query_error)?;

            let column_rows = sqlx::query(
                "SELECT COLUMN_NAME AS name, COLUMN_TYPE AS data_type, \
                        IS_NULLABLE AS nullable, COLUMN_DEFAULT AS default_expr, \
                        COLUMN_KEY AS key_kind \
                 FROM information_schema.COLUMNS \
                 WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = ? \
                 ORDER BY ORDINAL_POSITION",
            )
            .bind(&name)
            .fetch_all(&mut connection)
            .await
            .map_err(query_error)?;

            let mut columns = Vec::with_capacity(column_rows.len());
            for row in column_rows {
                let column_name: String = row.try_get("name").map_err(query_error)?;
                let data_type: String = row.try_get("data_type").map_err(query_error)?;
                let nullable: String = row.try_get("nullable").map_err(query_error)?;
                let default: Option<String> = row.try_get("default_expr").map_err(query_error)?;
                let key_kind: String = row.try_get("key_kind").map_err(query_error)?;
                let primary_key = key_kind == "PRI";
                columns.push(Column {
                    name: column_name,
                    data_type: (!data_type.is_empty()).then_some(data_type),
                    not_null: nullable == "NO" || primary_key,
                    primary_key,
                    default,
                    location: None,
                });
            }

            tables.push(Table {
                name,
                kind: if table_type.contains("VIEW") {
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

/// The live database for a workspace, dispatching on the detected backend.
pub enum LiveDatabase {
    /// A file-backed SQLite database.
    Sqlite(SqliteDatabase),
    /// A PostgreSQL server. Boxed: `PgConnectOptions` is an order of
    /// magnitude larger than the SQLite variant.
    Postgres(Box<PostgresDatabase>),
    /// A MySQL server. Boxed for the same reason as `Postgres`.
    MySql(Box<MySqlDatabase>),
}

impl LiveDatabase {
    /// Resolves `DATABASE_URL` for the detected backend. Relative SQLite
    /// paths are interpreted against the workspace `root`.
    pub fn from_url(
        url: &str,
        kind: DatabaseKind,
        root: &Path,
    ) -> Result<LiveDatabase, IntrospectError> {
        match kind {
            DatabaseKind::Sqlite => Ok(LiveDatabase::Sqlite(SqliteDatabase::from_url(url, root)?)),
            DatabaseKind::Postgres => Ok(LiveDatabase::Postgres(Box::new(
                PostgresDatabase::from_url(url)?,
            ))),
            DatabaseKind::MySql => Ok(LiveDatabase::MySql(Box::new(MySqlDatabase::from_url(url)?))),
        }
    }

    /// A password-free description of the database for logs.
    pub fn describe(&self) -> String {
        match self {
            LiveDatabase::Sqlite(database) => database.path().display().to_string(),
            LiveDatabase::Postgres(database) => database.display_url().to_owned(),
            LiveDatabase::MySql(database) => database.display_url().to_owned(),
        }
    }

    /// Reads every relation (with columns) from the database.
    pub async fn introspect(&self) -> Result<Vec<Table>, IntrospectError> {
        match self {
            LiveDatabase::Sqlite(database) => database.introspect().await,
            LiveDatabase::Postgres(database) => database.introspect().await,
            LiveDatabase::MySql(database) => database.introspect().await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_file_parsing_handles_quotes_export_and_comments() {
        let env = EnvFile::new(
            "# comment\n\
             export FOO='bar'\n\
             DATABASE_URL=\"sqlite://db/app.db\"\n\
             OTHER=x=y\n",
        );
        assert_eq!(env.value_of("FOO").as_deref(), Some("bar"));
        assert_eq!(
            env.value_of("DATABASE_URL").as_deref(),
            Some("sqlite://db/app.db")
        );
        assert_eq!(env.value_of("OTHER").as_deref(), Some("x=y"));
        assert_eq!(env.value_of("MISSING"), None);
    }

    #[test]
    fn database_url_is_discovered_across_search_roots() {
        // The ambient environment shadows .env files by design; the .env
        // path can only be asserted when the variable is unset.
        if std::env::var("DATABASE_URL").is_ok() {
            return;
        }
        let dir = tempfile::tempdir().expect("tempdir");
        let repo_root = dir.path().join("repo");
        let crate_root = repo_root.join("backend");
        std::fs::create_dir_all(&crate_root).expect("mkdir");
        std::fs::write(crate_root.join(".env"), "DATABASE_URL=sqlite://app.db\n")
            .expect("write .env");

        // Not at the first root, found at the member crate root.
        let url = discover_database_url([repo_root.as_path(), crate_root.as_path()]);
        assert_eq!(url.as_deref(), Some("sqlite://app.db"));
        assert_eq!(discover_database_url([repo_root.as_path()]), None);
    }

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
        let tables = database.introspect().await.expect("introspects");

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

    #[test]
    fn postgres_urls_are_redacted_in_display_and_errors() {
        let database = PostgresDatabase::from_url("postgres://app:s3cret@db.example.com:5432/app")
            .expect("valid url");
        assert_eq!(
            database.display_url(),
            "postgres://app:***@db.example.com:5432/app"
        );

        // No password → nothing to redact.
        let database =
            PostgresDatabase::from_url("postgresql://app@localhost/app").expect("valid url");
        assert_eq!(database.display_url(), "postgresql://app@localhost/app");

        assert!(matches!(
            PostgresDatabase::from_url("mysql://root@localhost/app"),
            Err(IntrospectError::UnsupportedUrl { .. })
        ));
    }

    #[test]
    fn mysql_urls_require_a_database_and_are_redacted() {
        let database =
            MySqlDatabase::from_url("mysql://app:s3cret@db.example.com:3306/app").expect("valid");
        assert_eq!(
            database.display_url(),
            "mysql://app:***@db.example.com:3306/app"
        );

        assert!(matches!(
            MySqlDatabase::from_url("mysql://root@localhost"),
            Err(IntrospectError::MySqlMissingDatabase { .. })
        ));
        assert!(matches!(
            MySqlDatabase::from_url("postgres://app@localhost/app"),
            Err(IntrospectError::UnsupportedUrl { .. })
        ));
    }

    #[test]
    fn live_database_dispatches_on_backend_kind() {
        let root = Path::new("/workspace");
        assert!(matches!(
            LiveDatabase::from_url("sqlite://app.db", DatabaseKind::Sqlite, root),
            Ok(LiveDatabase::Sqlite(_))
        ));
        assert!(matches!(
            LiveDatabase::from_url("postgres://app@localhost/app", DatabaseKind::Postgres, root),
            Ok(LiveDatabase::Postgres(_))
        ));
        assert!(matches!(
            LiveDatabase::from_url("mysql://root@localhost/app", DatabaseKind::MySql, root),
            Ok(LiveDatabase::MySql(_))
        ));
        // A URL that doesn't match the detected backend is rejected by the
        // backend-specific parser.
        assert!(matches!(
            LiveDatabase::from_url("postgres://app@localhost/app", DatabaseKind::Sqlite, root),
            Err(IntrospectError::UnsupportedUrl { .. })
        ));
    }

    #[tokio::test]
    async fn missing_database_file_is_reported() {
        let dir = tempfile::tempdir().expect("tempdir");
        let database = SqliteDatabase::from_url("sqlite://nope.db", dir.path()).expect("valid url");
        assert!(matches!(
            database.introspect().await,
            Err(IntrospectError::DatabaseMissing { .. })
        ));
    }
}
