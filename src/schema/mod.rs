//! Schema index for the workspace's database objects.
//!
//! The index is built by replaying the project's sqlx migrations in version
//! order and can be augmented with objects introspected from a live database.
//! Objects defined in migrations carry source locations so goto-definition
//! can jump to the defining statement.

mod ddl;
mod migrations;
mod model;

pub use migrations::SchemaError;
pub use model::{Column, SourceLocation, Table, TableKind, TableOrigin};

use std::collections::BTreeMap;

/// All tables and views known for the workspace's database.
#[derive(Debug, Clone, Default)]
pub struct Schema {
    /// Relations keyed by ASCII-lowercased name, matching SQL's
    /// case-insensitive identifier resolution.
    tables: BTreeMap<String, Table>,
}

impl Schema {
    /// Case-insensitive table/view lookup.
    pub fn table(&self, name: &str) -> Option<&Table> {
        self.tables.get(&name.to_ascii_lowercase())
    }

    /// All known relations, ordered by name.
    pub fn tables(&self) -> impl Iterator<Item = &Table> {
        self.tables.values()
    }

    /// Inserts or replaces a relation.
    pub fn insert_table(&mut self, table: Table) {
        self.tables.insert(table.name.to_ascii_lowercase(), table);
    }

    /// Adds introspected relations for anything not already defined by
    /// migrations. Migration definitions win because they carry source
    /// locations; the database fills in objects created outside them.
    pub fn merge_database_tables(&mut self, tables: Vec<Table>) {
        for table in tables {
            if self.table(&table.name).is_none() {
                self.insert_table(table);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::DatabaseKind;

    #[test]
    fn database_tables_fill_gaps_but_never_override_migrations() {
        let mut schema = Schema::default();
        schema.apply_sql(
            "CREATE TABLE users (id INTEGER PRIMARY KEY);",
            DatabaseKind::Sqlite,
            None,
        );
        let database_table = |name: &str| Table {
            name: name.to_owned(),
            kind: TableKind::Table,
            origin: TableOrigin::Database,
            columns: Vec::new(),
            location: None,
        };
        schema.merge_database_tables(vec![database_table("users"), database_table("sessions")]);

        let users = schema.table("users").expect("exists");
        assert_eq!(users.origin, TableOrigin::Migration);
        assert!(users.column("id").is_some());
        let sessions = schema.table("sessions").expect("merged in");
        assert_eq!(sessions.origin, TableOrigin::Database);
    }
}
