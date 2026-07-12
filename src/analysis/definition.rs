//! Goto definition for schema objects referenced in SQL documents.

use tower_lsp::lsp_types::{Location, Position};

use crate::analysis::resolve::{Resolved, resolve_at};
use crate::db::DatabaseKind;
use crate::document::Document;
use crate::schema::Schema;

/// The definition location of the schema object referenced at `position`.
///
/// Only migration-defined objects have locations; a column without one falls
/// back to its table's defining statement. Objects known solely from live
/// database introspection resolve to `None`.
pub fn definition(
    document: &Document,
    position: Position,
    schema: &Schema,
    kind: DatabaseKind,
) -> Option<Location> {
    let resolved = resolve_at(document, position, schema, kind)?;
    let location = match resolved {
        Resolved::Table { table, .. } => table.location.clone(),
        Resolved::Column { table, column, .. } => {
            column.location.clone().or_else(|| table.location.clone())
        }
    };
    location.map(Location::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{Table, TableKind, TableOrigin};

    fn migration_schema(dir: &tempfile::TempDir) -> Schema {
        std::fs::write(
            dir.path().join("1_init.sql"),
            "CREATE TABLE users (\n  id INTEGER PRIMARY KEY,\n  email TEXT NOT NULL\n);",
        )
        .expect("write migration");
        Schema::load_migrations(dir.path(), DatabaseKind::Sqlite).expect("loads")
    }

    fn definition_at(schema: &Schema, sql: &str, character: u32) -> Option<Location> {
        let document = Document::new(sql.to_owned());
        definition(
            &document,
            Position::new(0, character),
            schema,
            DatabaseKind::Sqlite,
        )
    }

    #[test]
    fn table_reference_jumps_to_create_table() {
        let dir = tempfile::tempdir().expect("tempdir");
        let schema = migration_schema(&dir);
        let location = definition_at(&schema, "SELECT id FROM users", 17).expect("has definition");
        assert!(location.uri.path().ends_with("1_init.sql"));
        assert_eq!(location.range.start.line, 0);
        assert_eq!(location.range.start.character, 13);
    }

    #[test]
    fn column_reference_jumps_to_column_definition() {
        let dir = tempfile::tempdir().expect("tempdir");
        let schema = migration_schema(&dir);
        let location =
            definition_at(&schema, "SELECT email FROM users", 8).expect("has definition");
        assert!(location.uri.path().ends_with("1_init.sql"));
        assert_eq!(location.range.start.line, 2);
        assert_eq!(location.range.start.character, 2);
    }

    #[test]
    fn database_only_objects_have_no_definition() {
        let mut schema = Schema::default();
        schema.insert_table(Table {
            name: "sessions".to_owned(),
            kind: TableKind::Table,
            origin: TableOrigin::Database,
            columns: Vec::new(),
            location: None,
        });
        assert_eq!(definition_at(&schema, "SELECT * FROM sessions", 15), None);
    }
}
