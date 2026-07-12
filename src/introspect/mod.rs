//! Live database introspection.
//!
//! When a crate's `DATABASE_URL` points at a reachable database, the
//! database itself is the most authoritative source for the schema — it
//! reflects migrations that were actually applied, plus anything created
//! outside the migrations directory. Introspected relations carry no source
//! locations, so the schema index prefers migration-defined entries and uses
//! these to fill the gaps. SQLite, PostgreSQL, and MySQL are supported;
//! sessions are read-only so introspection can never mutate the user's
//! database.

mod mysql;
mod postgres;
mod sqlite;

use std::path::{Path, PathBuf};

use crate::db::DatabaseKind;
use crate::schema::Table;

pub use mysql::MySqlDatabase;
pub use postgres::PostgresDatabase;
pub use sqlite::SqliteDatabase;

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

/// The macro-relevant environment discovered for one crate.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MacroEnv {
    /// The crate's database connection URL, when one is configured.
    pub database_url: Option<String>,
    /// Whether `SQLX_OFFLINE` requests that no database be contacted.
    pub offline: bool,
}

/// Discovers the environment for the crate rooted at `crate_dir`, matching
/// sqlx-macros' `load_env`: `.env` files are read (via dotenvy, the parser
/// sqlx itself uses) from every ancestor of the crate directory, with
/// entries from outer ancestors overwriting inner ones, and the process
/// environment overriding them all. An empty URL counts as unset.
///
/// `database_url_var` is the crate's URL variable name from its `sqlx.toml`
/// (`DATABASE_URL` by default). Malformed `.env` lines are skipped rather
/// than failing the whole discovery; the macros error out there, but an
/// editor session should degrade instead.
pub fn discover_macro_env(crate_dir: &Path, database_url_var: &str) -> MacroEnv {
    let mut database_url = None;
    let mut offline = None;

    for dir in crate_dir.ancestors() {
        let path = dir.join(".env");
        let Ok(entries) = dotenvy::from_path_iter(&path) else {
            continue;
        };
        for (name, value) in entries.flatten() {
            if name == database_url_var {
                database_url = Some(value);
            } else if name == "SQLX_OFFLINE" {
                offline = Some(value);
            }
        }
    }

    let database_url = std::env::var(database_url_var)
        .ok()
        .or(database_url)
        .filter(|url| !url.is_empty());
    let offline = std::env::var("SQLX_OFFLINE")
        .ok()
        .or(offline)
        .is_some_and(|value| value.eq_ignore_ascii_case("true") || value == "1");

    MacroEnv {
        database_url,
        offline,
    }
}

/// Failure to introspect a live database. URLs in these errors always have
/// their password redacted.
#[derive(Debug, thiserror::Error)]
pub enum IntrospectError {
    /// The URL does not describe a database the expected backend can open
    /// (wrong scheme, or an in-memory SQLite database).
    #[error("cannot introspect {backend} from DATABASE_URL {url}")]
    UnsupportedUrl {
        /// The backend that rejected the URL.
        backend: DatabaseKind,
        /// The offending URL.
        url: String,
    },
    /// The SQLite database file does not exist (yet); common in fresh
    /// checkouts where migrations have never been run.
    #[error("sqlite database file {path} does not exist")]
    DatabaseMissing {
        /// The resolved database file path.
        path: PathBuf,
    },
    /// The URL is not a valid connection string for its backend.
    #[error("invalid {backend} DATABASE_URL {url}: {source}")]
    InvalidUrl {
        /// The backend that failed to parse the URL.
        backend: DatabaseKind,
        /// The offending URL.
        url: String,
        /// The underlying sqlx error.
        #[source]
        source: sqlx::Error,
    },
    /// The URL names no database, so there is no schema to introspect
    /// (MySQL scopes relations to a database rather than a search path).
    #[error("DATABASE_URL must name a database: {url}")]
    MissingDatabase {
        /// The offending URL.
        url: String,
    },
    /// Connecting to or querying the database failed.
    #[error("failed to introspect {backend} database {target}: {source}")]
    Query {
        /// The backend that failed.
        backend: DatabaseKind,
        /// The database file path or connection URL.
        target: String,
        /// The underlying sqlx error.
        #[source]
        source: sqlx::Error,
    },
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

    /// A var name no ambient environment plausibly sets, so the `.env` paths
    /// are testable regardless of the developer's shell.
    const TEST_VAR: &str = "SQLX_LSP_TEST_DATABASE_URL";

    #[test]
    fn macro_env_walks_ancestors_with_outer_wins_precedence() {
        // sqlx-macros' load_env visits every ancestor's .env and later
        // (outer) files overwrite earlier ones; the crate's own .env loses
        // to the workspace root's. Surprising, but matched deliberately.
        let dir = tempfile::tempdir().expect("tempdir");
        let crate_dir = dir.path().join("repo").join("backend");
        std::fs::create_dir_all(&crate_dir).expect("mkdir");
        std::fs::write(
            crate_dir.join(".env"),
            format!("{TEST_VAR}=sqlite://inner.db\n"),
        )
        .expect("write inner .env");

        let env = discover_macro_env(&crate_dir, TEST_VAR);
        assert_eq!(env.database_url.as_deref(), Some("sqlite://inner.db"));

        std::fs::write(
            dir.path().join("repo").join(".env"),
            format!("{TEST_VAR}=sqlite://outer.db\n"),
        )
        .expect("write outer .env");
        let env = discover_macro_env(&crate_dir, TEST_VAR);
        assert_eq!(env.database_url.as_deref(), Some("sqlite://outer.db"));
    }

    #[test]
    fn macro_env_treats_empty_urls_as_unset_and_reads_offline() {
        if std::env::var("SQLX_OFFLINE").is_ok() {
            return;
        }
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join(".env"),
            format!("{TEST_VAR}=''\nSQLX_OFFLINE=true\n"),
        )
        .expect("write .env");

        let env = discover_macro_env(dir.path(), TEST_VAR);
        assert_eq!(env.database_url, None);
        assert!(env.offline);
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
}
