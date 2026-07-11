//! Live MySQL introspection tests, backed by a throwaway container.
//!
//! Requires a running Docker daemon; testcontainers manages the container
//! lifecycle and tears it down when the test ends.

use sqlx::{Connection, MySqlConnection};
use sqlx_lsp::db::DatabaseKind;
use sqlx_lsp::introspect::{LiveDatabase, MySqlDatabase};
use sqlx_lsp::schema::{Schema, TableKind, TableOrigin};
use testcontainers_modules::mysql::Mysql;
use testcontainers_modules::testcontainers::runners::AsyncRunner;

async fn database_with_fixture_schema() -> (
    testcontainers_modules::testcontainers::ContainerAsync<Mysql>,
    String,
) {
    let container = Mysql::default().start().await.expect("start mysql");
    let port = container
        .get_host_port_ipv4(3306)
        .await
        .expect("mapped port");
    // The testcontainers image creates database `test` with a passwordless
    // root account.
    let url = format!("mysql://root@127.0.0.1:{port}/test");

    let mut connection = MySqlConnection::connect(&url).await.expect("connect");
    sqlx::raw_sql(
        "CREATE TABLE users (
             id BIGINT AUTO_INCREMENT PRIMARY KEY,
             email VARCHAR(255) NOT NULL,
             bio TEXT,
             created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
         );
         CREATE TABLE memberships (
             user_id BIGINT NOT NULL,
             group_id BIGINT NOT NULL,
             PRIMARY KEY (user_id, group_id)
         );
         CREATE VIEW user_emails AS SELECT id, email FROM users;",
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

    let database = MySqlDatabase::from_url(&url).expect("valid url");
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
    assert_eq!(id.data_type.as_deref(), Some("bigint"));

    let email = users.column("email").expect("email column");
    assert!(email.not_null);
    assert!(!email.primary_key);
    assert_eq!(email.data_type.as_deref(), Some("varchar(255)"));

    let bio = users.column("bio").expect("bio column");
    assert!(!bio.not_null);
    assert_eq!(bio.data_type.as_deref(), Some("text"));

    let created_at = users.column("created_at").expect("created_at column");
    assert!(
        created_at
            .default
            .as_deref()
            .is_some_and(|default| default.eq_ignore_ascii_case("CURRENT_TIMESTAMP")),
        "unexpected default: {:?}",
        created_at.default
    );

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
}

#[tokio::test]
async fn read_only_session_cannot_mutate() {
    let (_container, url) = database_with_fixture_schema().await;

    // The introspection session runs SET SESSION TRANSACTION READ ONLY;
    // verify the same statement makes writes fail at the server.
    let mut connection = MySqlConnection::connect(&url).await.expect("connect");
    sqlx::query("SET SESSION TRANSACTION READ ONLY")
        .execute(&mut connection)
        .await
        .expect("set read only");
    let error = sqlx::query("INSERT INTO users (email) VALUES ('nope@example.com')")
        .execute(&mut connection)
        .await
        .expect_err("write must be rejected");
    assert!(
        error.to_string().to_ascii_lowercase().contains("read only"),
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
        DatabaseKind::MySql,
        None,
    );

    let database = LiveDatabase::from_url(&url, DatabaseKind::MySql, std::path::Path::new("/"))
        .expect("valid url");
    let tables = database.introspect().await.expect("introspects");
    schema.merge_database_tables(tables);

    let users = schema.table("users").expect("exists");
    assert_eq!(users.origin, TableOrigin::Migration);
    let memberships = schema.table("memberships").expect("merged in");
    assert_eq!(memberships.origin, TableOrigin::Database);
    assert!(schema.table("user_emails").is_some());
}
