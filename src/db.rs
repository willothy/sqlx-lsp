//! Detection of the sqlx database backend configured for a workspace.
//!
//! The backend is determined from the features that Cargo actually resolves
//! for the `sqlx` package (which accounts for feature unification across the
//! workspace and transitive dependencies), not from the raw manifest text.

use std::collections::BTreeSet;
use std::fmt;
use std::path::{Path, PathBuf};

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

    /// The backend a connection URL targets, judged by its scheme the same
    /// way the sqlx macros select a driver: `postgres`/`postgresql`,
    /// `mysql`/`mariadb`, and `sqlite`.
    pub fn from_url_scheme(url: &str) -> Option<DatabaseKind> {
        let (scheme, _) = url.split_once(':')?;
        match scheme.to_ascii_lowercase().as_str() {
            "postgres" | "postgresql" => Some(DatabaseKind::Postgres),
            "mysql" | "mariadb" => Some(DatabaseKind::MySql),
            "sqlite" => Some(DatabaseKind::Sqlite),
            _ => None,
        }
    }

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
    /// Directories of the cargo workspace's member crates, sorted and
    /// deduplicated. Editors often use a repository root as the LSP
    /// workspace root; migrations and `.env` files live next to crate
    /// manifests, so these are the places to look for them.
    pub member_roots: Vec<PathBuf>,
}

impl Detection {
    /// Detects the backend for the workspace containing (or below) `root` by
    /// resolving the dependency graph and inspecting the features enabled on
    /// the `sqlx` package.
    pub fn detect(root: &Path) -> Result<Detection, DetectError> {
        let manifest_dir = Self::find_manifest_dir(root);
        let metadata = MetadataCommand::new().current_dir(&manifest_dir).exec()?;

        let mut member_roots: Vec<PathBuf> = metadata
            .workspace_members
            .iter()
            .filter_map(|member| {
                metadata
                    .packages
                    .iter()
                    .find(|package| &package.id == member)
            })
            .filter_map(|package| package.manifest_path.parent())
            .map(|dir| dir.as_std_path().to_owned())
            .collect();
        member_roots.sort();
        member_roots.dedup();

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

        let mut detection = Detection::from_features(feature_names)?;
        detection.member_roots = member_roots;
        Ok(detection)
    }

    /// The directory whose manifest anchors `cargo metadata`: `root` itself
    /// when it contains a `Cargo.toml`, otherwise the first crate found by a
    /// shallow breadth-first scan. Editors frequently hand us a repository
    /// root that only *contains* the Rust workspace (monorepos), and cargo
    /// only searches upward on its own.
    fn find_manifest_dir(root: &Path) -> PathBuf {
        if root.join("Cargo.toml").is_file() {
            return root.to_owned();
        }

        let mut current_level = vec![root.to_owned()];
        for _depth in 0..3 {
            let mut next_level = Vec::new();
            for dir in current_level {
                let Ok(entries) = std::fs::read_dir(&dir) else {
                    continue;
                };
                let mut subdirs: Vec<PathBuf> = entries
                    .filter_map(|entry| entry.ok())
                    .map(|entry| entry.path())
                    .filter(|path| path.is_dir())
                    .filter(|path| {
                        !path
                            .file_name()
                            .and_then(|name| name.to_str())
                            .is_some_and(|name| {
                                name.starts_with('.') || name == "target" || name == "node_modules"
                            })
                    })
                    .collect();
                subdirs.sort();
                for subdir in subdirs {
                    if subdir.join("Cargo.toml").is_file() {
                        return subdir;
                    }
                    next_level.push(subdir);
                }
            }
            current_level = next_level;
        }
        root.to_owned()
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

        Ok(Detection {
            kind,
            enabled,
            member_roots: Vec::new(),
        })
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
    fn url_schemes_map_to_backends_like_sqlx_drivers() {
        let scheme = DatabaseKind::from_url_scheme;
        assert_eq!(scheme("postgres://h/db"), Some(DatabaseKind::Postgres));
        assert_eq!(scheme("postgresql://h/db"), Some(DatabaseKind::Postgres));
        assert_eq!(scheme("mysql://h/db"), Some(DatabaseKind::MySql));
        assert_eq!(scheme("mariadb://h/db"), Some(DatabaseKind::MySql));
        assert_eq!(scheme("sqlite:app.db"), Some(DatabaseKind::Sqlite));
        assert_eq!(scheme("sqlite://app.db"), Some(DatabaseKind::Sqlite));
        assert_eq!(scheme("redis://h"), None);
        assert_eq!(scheme("not a url"), None);
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
        assert!(
            detection
                .member_roots
                .contains(&PathBuf::from(env!("CARGO_MANIFEST_DIR")))
        );
    }

    #[test]
    fn finds_manifests_below_a_non_cargo_root() {
        // A repository root that merely contains the crate (a monorepo):
        // detection must scan downward to find the manifest. The fixture
        // crate has no sqlx dependency, so reaching `SqlxNotFound` (instead
        // of a metadata failure) proves the nested manifest was found and
        // resolved.
        let dir = tempfile::tempdir().expect("tempdir");
        let crate_dir = dir.path().join("services").join("backend");
        std::fs::create_dir_all(crate_dir.join("src")).expect("mkdir");
        std::fs::write(
            crate_dir.join("Cargo.toml"),
            "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .expect("write manifest");
        std::fs::write(crate_dir.join("src").join("lib.rs"), "").expect("write lib");

        let error = Detection::detect(dir.path()).unwrap_err();
        assert!(matches!(error, DetectError::SqlxNotFound), "{error}");
    }
}
