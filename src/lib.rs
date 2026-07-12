//! Language server for SQL in projects using [sqlx](https://github.com/launchbadge/sqlx).
//!
//! The server mirrors how sqlx's compile-time machinery resolves everything
//! relative to the invoking crate: each sqlx-dependent workspace member gets
//! its own database context — backend (SQLite, PostgreSQL, or MySQL, decided
//! by its connection URL scheme gated on the enabled driver features),
//! migrations, and live introspection. Against those contexts it provides
//! completion, hover, goto definition, and semantic token highlighting for
//! `.sql` documents and for the SQL embedded in sqlx query macros in Rust
//! documents.

pub mod analysis;
pub mod config;
pub mod db;
pub mod document;
pub mod embedded;
pub mod introspect;
pub mod parse;
pub mod schema;
pub mod server;
pub mod workspace;
