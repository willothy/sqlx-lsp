//! Live PostgreSQL introspection tests, backed by a throwaway container.
//!
//! Requires a running Docker daemon; testcontainers manages the container
//! lifecycle and tears it down when the test ends.

use sqlx::{Connection, PgConnection};
use sqlx_lsp::db::DatabaseKind;
use sqlx_lsp::introspect::{LiveDatabase, PostgresDatabase};
use sqlx_lsp::schema::{Schema, TableKind, TableOrigin};
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::runners::AsyncRunner;

async fn database_with_fixture_schema() -> (
    testcontainers_modules::testcontainers::ContainerAsync<Postgres>,
    String,
) {
    let container = Postgres::default().start().await.expect("start postgres");
    let port = container
        .get_host_port_ipv4(5432)
        .await
        .expect("mapped port");
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");

    let mut connection = PgConnection::connect(&url).await.expect("connect");
    sqlx::raw_sql(
        "CREATE TABLE users (
             id BIGSERIAL PRIMARY KEY,
             email TEXT NOT NULL,
             bio TEXT,
             created_at TIMESTAMPTZ NOT NULL DEFAULT now()
         );
         CREATE TABLE memberships (
             user_id BIGINT NOT NULL REFERENCES users(id),
             group_id BIGINT NOT NULL,
             PRIMARY KEY (user_id, group_id)
         );
         CREATE VIEW user_emails AS SELECT id, email FROM users;
         CREATE MATERIALIZED VIEW user_count AS SELECT count(*) AS total FROM users;",
    )
    .execute(&mut connection)
    .await
    .expect("create fixture schema");
    connection.close().await.expect("close");

    (container, url)
}

#[tokio::test]
async fn introspects_tables_views_and_columns() {
    let (_container, url) = database_with_fixture_schema().await;

    let database = PostgresDatabase::from_url(&url).expect("valid url");
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
    assert!(id.not_null);
    assert_eq!(id.data_type.as_deref(), Some("bigint"));
    // BIGSERIAL expands to a sequence default.
    assert!(id.default.as_deref().is_some_and(|d| d.contains("nextval")));

    let email = users.column("email").expect("email column");
    assert!(email.not_null);
    assert!(!email.primary_key);
    assert_eq!(email.data_type.as_deref(), Some("text"));

    let bio = users.column("bio").expect("bio column");
    assert!(!bio.not_null);

    let created_at = users.column("created_at").expect("created_at column");
    assert_eq!(
        created_at.data_type.as_deref(),
        Some("timestamp with time zone")
    );
    assert_eq!(created_at.default.as_deref(), Some("now()"));

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

    let materialized = tables
        .iter()
        .find(|table| table.name == "user_count")
        .expect("materialized view");
    assert_eq!(materialized.kind, TableKind::View);
    assert!(materialized.column("total").is_some());
}

#[tokio::test]
async fn read_only_session_cannot_mutate() {
    let (_container, url) = database_with_fixture_schema().await;

    // The introspection session sets default_transaction_read_only=on;
    // verify the same options reject writes at the server.
    use sqlx::ConnectOptions;
    use std::str::FromStr;
    let options = sqlx::postgres::PgConnectOptions::from_str(&url)
        .expect("parse url")
        .options([("default_transaction_read_only", "on")]);
    let mut connection = options.connect().await.expect("connect");
    let error = sqlx::query("INSERT INTO users (email) VALUES ('nope@example.com')")
        .execute(&mut connection)
        .await
        .expect_err("write must be rejected");
    assert!(
        error.to_string().contains("read-only"),
        "unexpected error: {error}"
    );
}

#[tokio::test]
async fn live_database_merges_into_schema_index() {
    let (_container, url) = database_with_fixture_schema().await;

    // Migrations define `users` (with a source location); the live database
    // contributes everything else without overriding it.
    let mut schema = Schema::default();
    schema.apply_sql(
        "CREATE TABLE users (id BIGINT PRIMARY KEY);",
        DatabaseKind::Postgres,
        None,
    );

    let database = LiveDatabase::from_url(&url, DatabaseKind::Postgres, std::path::Path::new("/"))
        .expect("valid url");
    let tables = database
        .introspect("_sqlx_migrations")
        .await
        .expect("introspects");
    schema.merge_database_tables(tables);

    let users = schema.table("users").expect("exists");
    assert_eq!(users.origin, TableOrigin::Migration);
    let memberships = schema.table("memberships").expect("merged in");
    assert_eq!(memberships.origin, TableOrigin::Database);
    assert!(schema.table("user_emails").is_some());
}
