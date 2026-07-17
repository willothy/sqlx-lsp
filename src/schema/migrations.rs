//! Building the schema index from sqlx migration files on disk.

use std::io;
use std::path::{Path, PathBuf};

use tower_lsp_server::ls_types::Uri;

use crate::db::DatabaseKind;
use crate::schema::Schema;

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

impl Schema {
    /// Builds a schema by replaying the `.sql` migrations under `dir` in
    /// version order.
    pub fn load_migrations(dir: &Path, kind: DatabaseKind) -> Result<Schema, SchemaError> {
        let mut schema = Schema::default();
        schema.apply_migrations(dir, kind)?;
        Ok(schema)
    }

    /// Replays the `.sql` migrations under `dir` in version order into this
    /// schema. Reversible down-migrations (`*.down.sql`) are skipped.
    /// Workspaces can hold several migration directories (one per member
    /// crate); applying each into one schema indexes them all.
    ///
    /// A missing directory applies nothing; a crate without migrations is
    /// not an error.
    pub fn apply_migrations(&mut self, dir: &Path, kind: DatabaseKind) -> Result<(), SchemaError> {
        if !dir.is_dir() {
            return Ok(());
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
            let uri = Uri::from_file_path(&absolute).ok_or_else(|| SchemaError::InvalidPath {
                path: file.path.clone(),
            })?;
            self.apply_sql(&text, kind, Some(&uri));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(table_location.uri.path().as_str().ends_with("1_init.sql"));
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
    fn migrations_from_several_directories_share_one_schema() {
        let dir = tempfile::tempdir().expect("tempdir");
        let users_dir = dir.path().join("users-svc");
        let posts_dir = dir.path().join("posts-svc");
        std::fs::create_dir_all(&users_dir).expect("mkdir");
        std::fs::create_dir_all(&posts_dir).expect("mkdir");
        std::fs::write(
            users_dir.join("1_init.sql"),
            "CREATE TABLE users (id INTEGER PRIMARY KEY);",
        )
        .expect("write migration");
        std::fs::write(
            posts_dir.join("1_init.sql"),
            "CREATE TABLE posts (id INTEGER PRIMARY KEY);",
        )
        .expect("write migration");

        let mut schema = Schema::default();
        schema
            .apply_migrations(&users_dir, DatabaseKind::Sqlite)
            .expect("applies");
        schema
            .apply_migrations(&posts_dir, DatabaseKind::Sqlite)
            .expect("applies");
        assert!(schema.table("users").is_some());
        assert!(schema.table("posts").is_some());
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
