//! Read-only introspection of MySQL/MariaDB databases via
//! `information_schema`.

use std::str::FromStr;

use sqlx::mysql::MySqlConnectOptions;
use sqlx::{ConnectOptions, Connection, Row};

use super::{IntrospectError, redact_url};
use crate::schema::{Column, Table, TableKind, TableOrigin};

/// A MySQL (or MariaDB) database reachable from the workspace.
pub struct MySqlDatabase {
    options: MySqlConnectOptions,
    /// The connection URL with any password redacted, safe for logs and
    /// error messages.
    display_url: String,
}

impl MySqlDatabase {
    /// Parses a `mysql://` or `mariadb://` connection URL. The URL must name
    /// a database: MySQL scopes relations to a database rather than a search
    /// path, so without one there is nothing to introspect.
    pub fn from_url(url: &str) -> Result<MySqlDatabase, IntrospectError> {
        if !url.starts_with("mysql://") && !url.starts_with("mariadb://") {
            return Err(IntrospectError::UnsupportedUrl {
                url: redact_url(url),
            });
        }
        let display_url = redact_url(url);
        let options =
            MySqlConnectOptions::from_str(url).map_err(|source| IntrospectError::MySqlUrl {
                url: display_url.clone(),
                source,
            })?;
        if options.get_database().is_none_or(str::is_empty) {
            return Err(IntrospectError::MySqlMissingDatabase { url: display_url });
        }
        Ok(MySqlDatabase {
            options,
            display_url,
        })
    }

    /// The connection URL with any password replaced by `***`.
    pub fn display_url(&self) -> &str {
        &self.display_url
    }

    /// Reads every table and view (with columns) of the URL's database from
    /// `information_schema`.
    pub async fn introspect(&self) -> Result<Vec<Table>, IntrospectError> {
        let query_error = |source| IntrospectError::MySqlQuery {
            url: self.display_url.clone(),
            source,
        };

        let mut connection = self.options.connect().await.map_err(query_error)?;
        // Server-enforced read-only for the whole session, so introspection
        // can never mutate the user's database.
        sqlx::query("SET SESSION TRANSACTION READ ONLY")
            .execute(&mut connection)
            .await
            .map_err(query_error)?;

        let relations = sqlx::query(
            "SELECT TABLE_NAME AS name, TABLE_TYPE AS kind \
             FROM information_schema.TABLES \
             WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME != '_sqlx_migrations' \
             ORDER BY TABLE_NAME",
        )
        .fetch_all(&mut connection)
        .await
        .map_err(query_error)?;

        let mut tables = Vec::with_capacity(relations.len());
        for relation in relations {
            let name: String = relation.try_get("name").map_err(query_error)?;
            let table_type: String = relation.try_get("kind").map_err(query_error)?;

            let column_rows = sqlx::query(
                "SELECT COLUMN_NAME AS name, COLUMN_TYPE AS data_type, \
                        IS_NULLABLE AS nullable, COLUMN_DEFAULT AS default_expr, \
                        COLUMN_KEY AS key_kind \
                 FROM information_schema.COLUMNS \
                 WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = ? \
                 ORDER BY ORDINAL_POSITION",
            )
            .bind(&name)
            .fetch_all(&mut connection)
            .await
            .map_err(query_error)?;

            let mut columns = Vec::with_capacity(column_rows.len());
            for row in column_rows {
                let column_name: String = row.try_get("name").map_err(query_error)?;
                let data_type: String = row.try_get("data_type").map_err(query_error)?;
                let nullable: String = row.try_get("nullable").map_err(query_error)?;
                let default: Option<String> = row.try_get("default_expr").map_err(query_error)?;
                let key_kind: String = row.try_get("key_kind").map_err(query_error)?;
                let primary_key = key_kind == "PRI";
                columns.push(Column {
                    name: column_name,
                    data_type: (!data_type.is_empty()).then_some(data_type),
                    not_null: nullable == "NO" || primary_key,
                    primary_key,
                    default,
                    location: None,
                });
            }

            tables.push(Table {
                name,
                kind: if table_type.contains("VIEW") {
                    TableKind::View
                } else {
                    TableKind::Table
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
    fn urls_require_a_database_and_are_redacted() {
        let database =
            MySqlDatabase::from_url("mysql://app:s3cret@db.example.com:3306/app").expect("valid");
        assert_eq!(
            database.display_url(),
            "mysql://app:***@db.example.com:3306/app"
        );

        assert!(matches!(
            MySqlDatabase::from_url("mysql://root@localhost"),
            Err(IntrospectError::MySqlMissingDatabase { .. })
        ));
        assert!(matches!(
            MySqlDatabase::from_url("postgres://app@localhost/app"),
            Err(IntrospectError::UnsupportedUrl { .. })
        ));

        // MariaDB URLs use the MySQL driver, matching sqlx's URL schemes.
        let database = MySqlDatabase::from_url("mariadb://app@db.example.com/app").expect("valid");
        assert_eq!(database.display_url(), "mariadb://app@db.example.com/app");
    }
}
