//! Per-crate `sqlx.toml` configuration.
//!
//! sqlx 0.9 reads a `sqlx.toml` next to each crate's manifest; it is the
//! upstream mechanism for multi-database workspaces. This module reads the
//! subset of that file the language server acts on, tolerating (and
//! ignoring) every other key so that valid upstream configs never fail to
//! load here.

use std::io;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// The default environment variable holding the connection URL.
pub const DEFAULT_DATABASE_URL_VAR: &str = "DATABASE_URL";

/// The default migrations directory, relative to the crate root.
pub const DEFAULT_MIGRATIONS_DIR: &str = "./migrations";

/// Failure to read a crate's `sqlx.toml`.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The file exists but could not be read.
    #[error("failed to read {path}: {source}")]
    Read {
        /// The config file path.
        path: PathBuf,
        /// The underlying I/O error.
        #[source]
        source: io::Error,
    },
    /// The file is not valid TOML (or has invalid types for known keys).
    #[error("failed to parse {path}: {source}")]
    Parse {
        /// The config file path.
        path: PathBuf,
        /// The underlying TOML error.
        #[source]
        source: toml::de::Error,
    },
}

/// The `[common]` section.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case", default)]
pub struct CommonConfig {
    /// The environment variable that holds this crate's database URL
    /// (`database-url-var`). Defaults to `DATABASE_URL`.
    pub database_url_var: Option<String>,
}

/// The `[migrate]` section.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case", default)]
pub struct MigrateConfig {
    /// The migrations directory (`migrations-dir`), relative to the crate
    /// root. Defaults to `./migrations`.
    pub migrations_dir: Option<String>,
}

/// The subset of `sqlx.toml` the language server consumes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case", default)]
pub struct SqlxConfig {
    /// Configuration shared between the macros and sqlx-cli.
    pub common: CommonConfig,
    /// Migration configuration.
    pub migrate: MigrateConfig,
}

impl SqlxConfig {
    /// Loads `<crate_dir>/sqlx.toml`. A missing file yields the default
    /// configuration, exactly as the sqlx macros treat it.
    pub fn load(crate_dir: &Path) -> Result<SqlxConfig, ConfigError> {
        let path = crate_dir.join("sqlx.toml");
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(SqlxConfig::default());
            }
            Err(source) => return Err(ConfigError::Read { path, source }),
        };
        toml::from_str(&text).map_err(|source| ConfigError::Parse { path, source })
    }

    /// The environment variable holding this crate's database URL.
    pub fn database_url_var(&self) -> &str {
        self.common
            .database_url_var
            .as_deref()
            .unwrap_or(DEFAULT_DATABASE_URL_VAR)
    }

    /// The migrations directory resolved against the crate root.
    pub fn migrations_dir(&self, crate_dir: &Path) -> PathBuf {
        crate_dir.join(
            self.migrate
                .migrations_dir
                .as_deref()
                .unwrap_or(DEFAULT_MIGRATIONS_DIR),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_yields_defaults() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = SqlxConfig::load(dir.path()).expect("loads");
        assert_eq!(config, SqlxConfig::default());
        assert_eq!(config.database_url_var(), "DATABASE_URL");
        assert_eq!(
            config.migrations_dir(dir.path()),
            dir.path().join("./migrations")
        );
    }

    #[test]
    fn reads_the_keys_the_server_acts_on() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("sqlx.toml"),
            r#"
[common]
database-url-var = "PG_DATABASE_URL"

[migrate]
migrations-dir = "db/migrations"
"#,
        )
        .expect("write config");

        let config = SqlxConfig::load(dir.path()).expect("loads");
        assert_eq!(config.database_url_var(), "PG_DATABASE_URL");
        assert_eq!(
            config.migrations_dir(dir.path()),
            dir.path().join("db/migrations")
        );
    }

    #[test]
    fn unknown_keys_are_tolerated() {
        // Upstream configs carry sections we do not consume; loading must
        // not reject them.
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("sqlx.toml"),
            r#"
[common]
database-url-var = "APP_DB"

[migrate]
table-name = "custom._sqlx_migrations"
ignored-chars = ["\r"]

[drivers.sqlite]
unsafe-load-extensions = ["uuid"]

[macros.preferred-crates]
date-time = "chrono"
"#,
        )
        .expect("write config");

        let config = SqlxConfig::load(dir.path()).expect("loads");
        assert_eq!(config.database_url_var(), "APP_DB");
        assert_eq!(config.migrate.migrations_dir, None);
    }

    #[test]
    fn invalid_toml_is_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("sqlx.toml"), "not [valid").expect("write config");
        assert!(matches!(
            SqlxConfig::load(dir.path()),
            Err(ConfigError::Parse { .. })
        ));
    }
}
