//! Detection of the sqlx database backend configured for a workspace.
//!
//! The backend is determined from the features that Cargo actually resolves
//! for the `sqlx` package (which accounts for feature unification across the
//! workspace and transitive dependencies), not from the raw manifest text.

use std::collections::BTreeSet;
use std::fmt;
use std::path::Path;

use cargo_metadata::MetadataCommand;
use sqlparser::dialect::{Dialect, MySqlDialect, PostgreSqlDialect, SQLiteDialect};

static SQLITE_DIALECT: SQLiteDialect = SQLiteDialect {};
static POSTGRES_DIALECT: PostgreSqlDialect = PostgreSqlDialect {};
static MYSQL_DIALECT: MySqlDialect = MySqlDialect {};

/// A database backend supported by sqlx and this language server.
///
/// The ordering doubles as the selection priority when a workspace enables
/// more than one driver feature: `Postgres` and `MySql` are typically the
/// deployment target while `Sqlite` is commonly enabled as a dev/test extra,
/// so they win over it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DatabaseKind {
    /// PostgreSQL, enabled by the sqlx `postgres` feature.
    Postgres,
    /// MySQL/MariaDB, enabled by the sqlx `mysql` feature.
    MySql,
    /// SQLite, enabled by the sqlx `sqlite` feature.
    Sqlite,
}

impl DatabaseKind {
    /// All backends, in selection-priority order.
    pub const ALL: [DatabaseKind; 3] = [
        DatabaseKind::Postgres,
        DatabaseKind::MySql,
        DatabaseKind::Sqlite,
    ];

    /// The sqlx cargo feature that enables this backend's driver.
    pub fn feature_name(self) -> &'static str {
        match self {
            DatabaseKind::Postgres => "postgres",
            DatabaseKind::MySql => "mysql",
            DatabaseKind::Sqlite => "sqlite",
        }
    }

    /// The sqlparser dialect used to parse SQL written for this backend.
    pub fn dialect(self) -> &'static (dyn Dialect + Send + Sync) {
        match self {
            DatabaseKind::Postgres => &POSTGRES_DIALECT,
            DatabaseKind::MySql => &MYSQL_DIALECT,
            DatabaseKind::Sqlite => &SQLITE_DIALECT,
        }
    }
}

impl fmt::Display for DatabaseKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            DatabaseKind::Postgres => "PostgreSQL",
            DatabaseKind::MySql => "MySQL",
            DatabaseKind::Sqlite => "SQLite",
        };
        f.write_str(name)
    }
}

/// Failure to determine the database backend for a workspace.
#[derive(Debug, thiserror::Error)]
pub enum DetectError {
    /// `cargo metadata` could not be executed or produced invalid output.
    #[error("failed to query cargo metadata: {0}")]
    Metadata(#[from] cargo_metadata::Error),
    /// The workspace does not depend on `sqlx`.
    #[error("no `sqlx` dependency found in the workspace")]
    SqlxNotFound,
    /// `sqlx` is a dependency but none of its database driver features
    /// (`sqlite`, `postgres`, `mysql`) is enabled.
    #[error(
        "`sqlx` is a dependency but no database driver feature (sqlite, postgres, mysql) is enabled"
    )]
    NoDriverFeature,
}

/// The outcome of backend detection for a workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detection {
    /// The backend the server operates against.
    pub kind: DatabaseKind,
    /// Every backend whose driver feature is enabled. Contains more than one
    /// element when the workspace enables several drivers; [`Detection::kind`]
    /// is then the highest-priority member.
    pub enabled: BTreeSet<DatabaseKind>,
}

impl Detection {
    /// Detects the backend for the workspace containing `manifest_dir` by
    /// resolving the dependency graph and inspecting the features enabled on
    /// the `sqlx` package.
    pub fn detect(manifest_dir: &Path) -> Result<Detection, DetectError> {
        let metadata = MetadataCommand::new().current_dir(manifest_dir).exec()?;

        let sqlx_ids: BTreeSet<_> = metadata
            .packages
            .iter()
            .filter(|package| package.name.as_str() == "sqlx")
            .map(|package| &package.id)
            .collect();

        if sqlx_ids.is_empty() {
            return Err(DetectError::SqlxNotFound);
        }

        // Union the resolved features across every `sqlx` node. Duplicate
        // sqlx versions in one graph are pathological but shouldn't make
        // detection fail outright.
        let resolve = metadata.resolve.as_ref();
        let feature_names = resolve
            .into_iter()
            .flat_map(|resolve| resolve.nodes.iter())
            .filter(|node| sqlx_ids.contains(&node.id))
            .flat_map(|node| {
                node.features
                    .iter()
                    .map(|feature| -> &str { feature.as_ref() })
            });

        Detection::from_features(feature_names)
    }

    /// Builds a detection from the set of feature names enabled on the `sqlx`
    /// package, selecting the highest-priority backend when several drivers
    /// are enabled.
    pub fn from_features<'a>(
        features: impl IntoIterator<Item = &'a str>,
    ) -> Result<Detection, DetectError> {
        let features: BTreeSet<&str> = features.into_iter().collect();
        let enabled: BTreeSet<DatabaseKind> = DatabaseKind::ALL
            .into_iter()
            .filter(|kind| features.contains(kind.feature_name()))
            .collect();

        let kind = DatabaseKind::ALL
            .into_iter()
            .find(|kind| enabled.contains(kind))
            .ok_or(DetectError::NoDriverFeature)?;

        Ok(Detection { kind, enabled })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_driver_feature_selects_that_backend() {
        let detection =
            Detection::from_features(["sqlite", "macros", "runtime-tokio"]).expect("detects");
        assert_eq!(detection.kind, DatabaseKind::Sqlite);
        assert_eq!(detection.enabled.len(), 1);
    }

    #[test]
    fn multiple_driver_features_select_by_priority() {
        let detection = Detection::from_features(["sqlite", "postgres"]).expect("detects");
        assert_eq!(detection.kind, DatabaseKind::Postgres);
        assert!(detection.enabled.contains(&DatabaseKind::Sqlite));
        assert!(detection.enabled.contains(&DatabaseKind::Postgres));

        let detection = Detection::from_features(["mysql", "sqlite"]).expect("detects");
        assert_eq!(detection.kind, DatabaseKind::MySql);
    }

    #[test]
    fn no_driver_feature_is_an_error() {
        let error = Detection::from_features(["macros", "json"]).unwrap_err();
        assert!(matches!(error, DetectError::NoDriverFeature));
    }

    #[test]
    fn detects_drivers_for_this_workspace() {
        // This crate itself depends on sqlx with all three driver features,
        // which makes it a real end-to-end fixture for resolved-feature
        // detection: every driver is found and the priority order selects
        // PostgreSQL.
        let detection = Detection::detect(Path::new(env!("CARGO_MANIFEST_DIR"))).expect("detects");
        assert!(detection.enabled.contains(&DatabaseKind::Sqlite));
        assert!(detection.enabled.contains(&DatabaseKind::Postgres));
        assert!(detection.enabled.contains(&DatabaseKind::MySql));
        assert_eq!(detection.kind, DatabaseKind::Postgres);
    }
}
