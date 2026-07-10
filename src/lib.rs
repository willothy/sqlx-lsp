//! Language server for SQL files in projects using [sqlx](https://github.com/launchbadge/sqlx).
//!
//! The server determines the database backend (SQLite, PostgreSQL, or MySQL)
//! from the features enabled on the workspace's `sqlx` dependency and provides
//! completion, hover, goto definition, and semantic token highlighting for SQL
//! documents, backed by a schema index built from the project's migrations and
//! (for SQLite) live database introspection.

pub mod db;
pub mod document;
