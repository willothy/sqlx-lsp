//! Live SQLite introspection tests against a real database file.

use std::path::{Path, PathBuf};

use sqlx::sqlite::SqliteConnectOptions;
use sqlx::{ConnectOptions, Connection};
use sqlx_lsp::db::DatabaseKind;
use sqlx_lsp::introspect::LiveDatabase;
use sqlx_lsp::schema::{Schema, TableKind, TableOrigin};

async fn database_with_fixture_schema(dir: &Path) -> PathBuf {
    let path = dir.join("app.db");
    let mut connection = SqliteConnectOptions::new()
        .filename(&path)
        .create_if_missing(true)
        .connect()
        .await
        .expect("create db");
    sqlx::raw_sql(
        "CREATE TABLE users (
             id INTEGER PRIMARY KEY,
             email TEXT NOT NULL,
             bio TEXT,
             created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
         );
         CREATE TABLE memberships (
             user_id INTEGER NOT NULL REFERENCES users(id),
             group_id INTEGER NOT NULL,
             PRIMARY KEY (user_id, group_id)
         );
         CREATE VIEW user_emails AS SELECT id, email FROM users;
         CREATE TABLE _sqlx_migrations (version BIGINT PRIMARY KEY);",
    )
    .execute(&mut connection)
    .await
    .expect("create fixture schema");
    connection.close().await.expect("close");

    path
}

#[tokio::test]
async fn introspects_tables_views_and_columns() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = database_with_fixture_schema(dir.path()).await;
    let url = format!("sqlite://{}", path.display());

    let database =
        LiveDatabase::from_url(&url, DatabaseKind::Sqlite, dir.path()).expect("valid url");
    let tables = database.introspect().await.expect("introspects");

    let users = tables
        .iter()
        .find(|table| table.name == "users")
        .expect("users table");
    assert_eq!(users.kind, TableKind::Table);
    assert_eq!(users.origin, TableOrigin::Database);

    let id = users.column("id").expect("id column");
    assert!(id.primary_key);
    assert!(id.not_null);
    assert_eq!(id.data_type.as_deref(), Some("INTEGER"));

    let email = users.column("email").expect("email column");
    assert!(email.not_null);
    assert!(!email.primary_key);
    assert_eq!(email.data_type.as_deref(), Some("TEXT"));

    let bio = users.column("bio").expect("bio column");
    assert!(!bio.not_null);

    let created_at = users.column("created_at").expect("created_at column");
    assert_eq!(created_at.default.as_deref(), Some("CURRENT_TIMESTAMP"));

    // Composite primary keys mark every member column.
    let memberships = tables
        .iter()
        .find(|table| table.name == "memberships")
        .expect("memberships table");
    assert!(memberships.column("user_id").expect("exists").primary_key);
    assert!(memberships.column("group_id").expect("exists").primary_key);

    let view = tables
        .iter()
        .find(|table| table.name == "user_emails")
        .expect("view");
    assert_eq!(view.kind, TableKind::View);
    assert_eq!(view.columns.len(), 2);

    // sqlx's bookkeeping table is not part of the user's schema.
    assert!(!tables.iter().any(|table| table.name == "_sqlx_migrations"));
}

#[tokio::test]
async fn read_only_session_cannot_mutate() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = database_with_fixture_schema(dir.path()).await;

    // The introspection connection opens the file read-only; verify the same
    // options reject writes.
    let mut connection = SqliteConnectOptions::new()
        .filename(&path)
        .read_only(true)
        .connect()
        .await
        .expect("connect");
    let error = sqlx::query("INSERT INTO users (email) VALUES ('nope@example.com')")
        .execute(&mut connection)
        .await
        .expect_err("write must be rejected");
    assert!(
        error.to_string().contains("readonly"),
        "unexpected error: {error}"
    );
}

#[tokio::test]
async fn live_database_merges_into_schema_index() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = database_with_fixture_schema(dir.path()).await;
    let url = format!("sqlite://{}", path.display());

    // Migrations define `users` (with a source location); the live database
    // contributes everything else without overriding it.
    let mut schema = Schema::default();
    schema.apply_sql(
        "CREATE TABLE users (id INTEGER PRIMARY KEY);",
        DatabaseKind::Sqlite,
        None,
    );

    let database =
        LiveDatabase::from_url(&url, DatabaseKind::Sqlite, dir.path()).expect("valid url");
    let tables = database.introspect().await.expect("introspects");
    schema.merge_database_tables(tables);

    let users = schema.table("users").expect("exists");
    assert_eq!(users.origin, TableOrigin::Migration);
    let memberships = schema.table("memberships").expect("merged in");
    assert_eq!(memberships.origin, TableOrigin::Database);
    assert!(schema.table("user_emails").is_some());
}
