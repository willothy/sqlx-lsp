//! Read-only introspection of PostgreSQL databases via the system catalogs.

use std::str::FromStr;

use sqlx::postgres::PgConnectOptions;
use sqlx::postgres::types::Oid;
use sqlx::{ConnectOptions, Connection, Row};

use super::{IntrospectError, redact_url};
use crate::db::DatabaseKind;
use crate::schema::{Column, Table, TableKind, TableOrigin};

/// A PostgreSQL database reachable from the workspace.
pub struct PostgresDatabase {
    options: PgConnectOptions,
    /// The connection URL with any password redacted, safe for logs and
    /// error messages.
    display_url: String,
}

impl PostgresDatabase {
    /// Parses a `postgres://` / `postgresql://` connection URL.
    pub fn from_url(url: &str) -> Result<PostgresDatabase, IntrospectError> {
        if !url.starts_with("postgres://") && !url.starts_with("postgresql://") {
            return Err(IntrospectError::UnsupportedUrl {
                backend: DatabaseKind::Postgres,
                url: redact_url(url),
            });
        }
        let display_url = redact_url(url);
        let options = PgConnectOptions::from_str(url)
            .map_err(|source| IntrospectError::InvalidUrl {
                backend: DatabaseKind::Postgres,
                url: display_url.clone(),
                source,
            })?
            .application_name("sqlx-lsp")
            // Server-enforced read-only for the whole session, so
            // introspection can never mutate the user's database.
            .options([("default_transaction_read_only", "on")]);
        Ok(PostgresDatabase {
            options,
            display_url,
        })
    }

    /// The connection URL with any password replaced by `***`.
    pub fn display_url(&self) -> &str {
        &self.display_url
    }

    /// Reads every table, view, and materialized view visible on the
    /// connection's search path, with columns, from the system catalogs —
    /// except the `migrations_table` sqlx uses for bookkeeping.
    pub async fn introspect(&self, migrations_table: &str) -> Result<Vec<Table>, IntrospectError> {
        let query_error = |source| IntrospectError::Query {
            backend: DatabaseKind::Postgres,
            target: self.display_url.clone(),
            source,
        };

        let mut connection = self.options.connect().await.map_err(query_error)?;

        let relations = sqlx::query(
            "SELECT c.oid, c.relname::text AS name, c.relkind::text AS kind \
             FROM pg_catalog.pg_class c \
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
             WHERE c.relkind IN ('r', 'p', 'v', 'm') \
               AND n.nspname NOT IN ('pg_catalog', 'information_schema') \
               AND c.relname != $1 \
               AND pg_catalog.pg_table_is_visible(c.oid) \
             ORDER BY c.relname",
        )
        .bind(migrations_table)
        .fetch_all(&mut connection)
        .await
        .map_err(query_error)?;

        let mut tables = Vec::with_capacity(relations.len());
        for relation in relations {
            let oid: Oid = relation.try_get("oid").map_err(query_error)?;
            let name: String = relation.try_get("name").map_err(query_error)?;
            let relkind: String = relation.try_get("kind").map_err(query_error)?;

            let column_rows = sqlx::query(
                "SELECT a.attname::text AS name, \
                        pg_catalog.format_type(a.atttypid, a.atttypmod) AS data_type, \
                        a.attnotnull AS not_null, \
                        pg_catalog.pg_get_expr(d.adbin, d.adrelid) AS default_expr, \
                        COALESCE(a.attnum = ANY(pk.conkey), FALSE) AS primary_key \
                 FROM pg_catalog.pg_attribute a \
                 LEFT JOIN pg_catalog.pg_attrdef d \
                        ON d.adrelid = a.attrelid AND d.adnum = a.attnum \
                 LEFT JOIN pg_catalog.pg_constraint pk \
                        ON pk.conrelid = a.attrelid AND pk.contype = 'p' \
                 WHERE a.attrelid = $1 AND a.attnum > 0 AND NOT a.attisdropped \
                 ORDER BY a.attnum",
            )
            .bind(oid)
            .fetch_all(&mut connection)
            .await
            .map_err(query_error)?;

            let mut columns = Vec::with_capacity(column_rows.len());
            for row in column_rows {
                let column_name: String = row.try_get("name").map_err(query_error)?;
                let data_type: String = row.try_get("data_type").map_err(query_error)?;
                let not_null: bool = row.try_get("not_null").map_err(query_error)?;
                let default: Option<String> = row.try_get("default_expr").map_err(query_error)?;
                let primary_key: bool = row.try_get("primary_key").map_err(query_error)?;
                columns.push(Column {
                    name: column_name,
                    data_type: (!data_type.is_empty()).then_some(data_type),
                    not_null: not_null || primary_key,
                    primary_key,
                    default,
                    location: None,
                });
            }

            tables.push(Table {
                name,
                kind: match relkind.as_str() {
                    "v" | "m" => TableKind::View,
                    _ => TableKind::Table,
                },
                origin: TableOrigin::Database,
                columns,
                location: None,
            });
        }

        connection.close().await.map_err(query_error)?;
        Ok(tables)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_are_redacted_in_display_and_errors() {
        let database = PostgresDatabase::from_url("postgres://app:s3cret@db.example.com:5432/app")
            .expect("valid url");
        assert_eq!(
            database.display_url(),
            "postgres://app:***@db.example.com:5432/app"
        );

        // No password → nothing to redact.
        let database =
            PostgresDatabase::from_url("postgresql://app@localhost/app").expect("valid url");
        assert_eq!(database.display_url(), "postgresql://app@localhost/app");

        assert!(matches!(
            PostgresDatabase::from_url("mysql://root@localhost/app"),
            Err(IntrospectError::UnsupportedUrl { .. })
        ));
    }
}
