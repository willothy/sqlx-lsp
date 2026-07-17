//! Quick fixes for unresolved references: replacement candidates drawn from
//! the schema by edit distance.

use std::collections::HashMap;

use tower_lsp_server::ls_types::{
    CodeAction, CodeActionKind, Diagnostic, DiagnosticSeverity, Range, TextEdit, Uri, WorkspaceEdit,
};

use crate::analysis::resolve::{self, UnresolvedKind};
use crate::document::Document;
use crate::parse::ParsedSql;
use crate::schema::Schema;

/// How many replacement candidates one unresolved reference offers at most.
const MAX_SUGGESTIONS: usize = 3;

/// Quick-fix actions for the unresolved references overlapping `range`:
/// for each, up to [`MAX_SUGGESTIONS`] replacement candidates within a
/// length-scaled edit distance of the written name, closest first, the
/// closest marked preferred.
pub fn quick_fixes(
    document: &Document,
    parsed: &ParsedSql,
    schema: &Schema,
    uri: &Uri,
    range: Range,
) -> Vec<CodeAction> {
    let mut actions = Vec::new();
    for unresolved in resolve::unresolved_references(parsed, schema) {
        let Some(reference_range) = document.range_of(unresolved.span) else {
            continue;
        };
        if reference_range.end < range.start || range.end < reference_range.start {
            continue;
        }

        let (written, candidates): (&str, Vec<String>) = match &unresolved.kind {
            UnresolvedKind::Table { name } => {
                let mut names: Vec<String> =
                    schema.tables().map(|table| table.name.clone()).collect();
                names.extend(
                    resolve::query_local_tables(&parsed.statements, schema)
                        .into_iter()
                        .map(|table| table.name),
                );
                (name, names)
            }
            UnresolvedKind::Column { table, name } => (
                name,
                schema
                    .table(table)
                    .map(|table| {
                        table
                            .columns
                            .iter()
                            .map(|column| column.name.clone())
                            .collect()
                    })
                    .unwrap_or_default(),
            ),
        };

        // Tolerate more distance for longer names; a two-character name
        // within distance 2 of everything would suggest noise.
        let allowed = (written.len() / 4 + 1).min(3);
        let written_lower = written.to_ascii_lowercase();
        let mut scored: Vec<(usize, String)> = candidates
            .into_iter()
            .filter(|candidate| !candidate.eq_ignore_ascii_case(written))
            .filter_map(|candidate| {
                let distance = edit_distance(&written_lower, &candidate.to_ascii_lowercase());
                (distance <= allowed).then_some((distance, candidate))
            })
            .collect();
        scored.sort();
        scored.dedup_by(|a, b| a.1 == b.1);

        let diagnostic = Diagnostic {
            range: reference_range,
            severity: Some(DiagnosticSeverity::WARNING),
            source: Some("sqlx-lsp".to_owned()),
            message: unresolved.message.clone(),
            ..Diagnostic::default()
        };
        for (index, (_, candidate)) in scored.into_iter().take(MAX_SUGGESTIONS).enumerate() {
            actions.push(CodeAction {
                title: format!("Replace with `{candidate}`"),
                kind: Some(CodeActionKind::QUICKFIX),
                diagnostics: Some(vec![diagnostic.clone()]),
                edit: Some(WorkspaceEdit {
                    changes: Some(HashMap::from([(
                        uri.clone(),
                        vec![TextEdit {
                            range: reference_range,
                            new_text: candidate,
                        }],
                    )])),
                    ..WorkspaceEdit::default()
                }),
                is_preferred: (index == 0).then_some(true),
                ..CodeAction::default()
            });
        }
    }
    actions
}

/// The optimal-string-alignment distance between `a` and `b`: Levenshtein
/// extended with adjacent transpositions at cost 1, the dominant real-world
/// typo (`emial` is one edit from `email`).
fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut rows: Vec<Vec<usize>> = vec![(0..=b.len()).collect()];
    for (row, a_char) in a.iter().enumerate() {
        let mut current = vec![row + 1];
        for (column, b_char) in b.iter().enumerate() {
            let substitution = rows[row][column] + usize::from(a_char != b_char);
            let mut best = substitution
                .min(rows[row][column + 1] + 1)
                .min(current[column] + 1);
            if row > 0 && column > 0 && *a_char == b[column - 1] && a[row - 1] == *b_char {
                best = best.min(rows[row - 1][column - 1] + 1);
            }
            current.push(best);
        }
        rows.push(current);
    }
    rows[a.len()][b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::DatabaseKind;
    use tower_lsp_server::ls_types::Position;

    fn schema() -> Schema {
        let mut schema = Schema::default();
        schema.apply_sql(
            "CREATE TABLE users (id INTEGER PRIMARY KEY, email TEXT NOT NULL);
             CREATE TABLE posts (id INTEGER PRIMARY KEY, title TEXT);",
            DatabaseKind::Sqlite,
            None,
        );
        schema
    }

    fn fixes_for(sql: &str) -> Vec<CodeAction> {
        let document = Document::new(sql.to_owned());
        let parsed = ParsedSql::parse(DatabaseKind::Sqlite.dialect(), document.text());
        let uri = Uri::from_file_path("/tmp/q.sql").expect("uri");
        let end = document.position_at(sql.len());
        quick_fixes(
            &document,
            &parsed,
            &schema(),
            &uri,
            Range {
                start: Position::new(0, 0),
                end,
            },
        )
    }

    #[test]
    fn misspelled_tables_suggest_the_closest_name() {
        let actions = fixes_for("SELECT id FROM usrs");
        assert!(!actions.is_empty());
        assert_eq!(actions[0].title, "Replace with `users`");
        assert_eq!(actions[0].is_preferred, Some(true));

        let edit = actions[0].edit.as_ref().expect("edit");
        let changes = edit.changes.as_ref().expect("changes");
        let edits = changes.values().next().expect("edits");
        assert_eq!(edits[0].new_text, "users");
        assert_eq!(edits[0].range.start, Position::new(0, 15));
    }

    #[test]
    fn misspelled_columns_suggest_from_the_owning_table() {
        let actions = fixes_for("SELECT u.emial FROM users AS u");
        assert_eq!(actions.len(), 1, "{actions:?}");
        assert_eq!(actions[0].title, "Replace with `email`");
    }

    #[test]
    fn distant_names_offer_nothing() {
        assert!(fixes_for("SELECT id FROM zzzzzz").is_empty());
    }

    #[test]
    fn edit_distance_counts_single_edits() {
        assert_eq!(edit_distance("users", "users"), 0);
        assert_eq!(edit_distance("usrs", "users"), 1);
        // A transposition is one edit.
        assert_eq!(edit_distance("emial", "email"), 1);
        assert_eq!(edit_distance("", "abc"), 3);
    }
}
