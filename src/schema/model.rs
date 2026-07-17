//! The data model of the schema index: relations, columns, and where they
//! were defined.

use tower_lsp_server::ls_types::{Location, Range, Uri};

/// Where a schema object is defined in workspace sources.
#[derive(Debug, Clone, PartialEq)]
pub struct SourceLocation {
    /// URI of the defining file.
    pub uri: Uri,
    /// Range of the defining identifier within that file.
    pub range: Range,
}

impl From<SourceLocation> for Location {
    fn from(location: SourceLocation) -> Location {
        Location::new(location.uri, location.range)
    }
}

/// How a schema object became known to the index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableOrigin {
    /// Defined by a SQL migration in the workspace.
    Migration,
    /// Discovered by introspecting a live database.
    Database,
    /// Defined by the query being analyzed (a CTE or derived subquery);
    /// never stored in the schema index.
    Query,
}

/// Whether a relation is a table or a view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableKind {
    /// A base table.
    Table,
    /// A view.
    View,
}

/// One column of a table or view.
#[derive(Debug, Clone, PartialEq)]
pub struct Column {
    /// Name as written in the defining source.
    pub name: String,
    /// Declared data type; `None` for typeless (SQLite) or derived columns.
    pub data_type: Option<String>,
    /// Whether the column is declared `NOT NULL` (or is part of the primary
    /// key, which implies it).
    pub not_null: bool,
    /// Whether the column is part of the primary key.
    pub primary_key: bool,
    /// The default value expression, rendered as SQL.
    pub default: Option<String>,
    /// Location of the column definition, when defined in a migration.
    pub location: Option<SourceLocation>,
}

/// A table or view known to the schema index.
#[derive(Debug, Clone, PartialEq)]
pub struct Table {
    /// Name as written in the defining source.
    pub name: String,
    /// Whether this is a base table or a view.
    pub kind: TableKind,
    /// How the object became known to the index.
    pub origin: TableOrigin,
    /// The relation's columns, in definition order.
    pub columns: Vec<Column>,
    /// Location of the defining statement, when defined in a migration.
    pub location: Option<SourceLocation>,
}

impl Column {
    /// The column rendered as a SQL definition fragment,
    /// e.g. `email TEXT NOT NULL DEFAULT 'x'`.
    pub fn signature(&self) -> String {
        let mut signature = self.name.clone();
        if let Some(data_type) = &self.data_type {
            signature.push(' ');
            signature.push_str(data_type);
        }
        if self.primary_key {
            signature.push_str(" PRIMARY KEY");
        } else if self.not_null {
            signature.push_str(" NOT NULL");
        }
        if let Some(default) = &self.default {
            signature.push_str(" DEFAULT ");
            signature.push_str(default);
        }
        signature
    }
}

impl Table {
    /// Case-insensitive column lookup.
    pub fn column(&self, name: &str) -> Option<&Column> {
        self.columns
            .iter()
            .find(|column| column.name.eq_ignore_ascii_case(name))
    }

    /// The relation rendered as a `CREATE`-statement-shaped summary of what
    /// the index knows about it.
    pub fn ddl(&self) -> String {
        let keyword = match self.kind {
            TableKind::Table => "TABLE",
            TableKind::View => "VIEW",
        };
        if self.columns.is_empty() {
            return format!("CREATE {keyword} {}", self.name);
        }
        let columns: Vec<String> = self
            .columns
            .iter()
            .map(|column| format!("  {}", column.signature()))
            .collect();
        format!(
            "CREATE {keyword} {} (\n{}\n)",
            self.name,
            columns.join(",\n")
        )
    }
}
