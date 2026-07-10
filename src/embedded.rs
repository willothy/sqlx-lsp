//! SQL embedded in Rust sources via sqlx's query macros.
//!
//! sqlx's compile-time-checked macros (`query!`, `query_as!`, ...) take
//! their SQL as a string literal. Tree-sitter locates those literals so the
//! SQL analyses can run inside Rust buffers. The literal text is used
//! verbatim — escape sequences are *not* decoded — which keeps the mapping
//! between host and embedded positions a pure line/column shift; raw strings
//! (`r#"..."#`), the idiomatic style for multi-line SQL, round-trip
//! losslessly, while a rare `\n` escape inside a plain string is simply seen
//! by the SQL parser as a backslash and an `n`.

use tower_lsp::lsp_types::{CompletionItem, Hover, Location, Position, Range, SemanticToken};
use tree_sitter::{Node, Parser};

use crate::analysis::semantic_tokens;
use crate::db::DatabaseKind;
use crate::document::Document;
use crate::schema::Schema;

/// sqlx macros whose first string-literal argument is SQL. `query_file!`
/// and friends reference `.sql` files, which are served as ordinary SQL
/// documents instead.
const SQL_MACROS: &[&str] = &[
    "query",
    "query_as",
    "query_as_unchecked",
    "query_scalar",
    "query_scalar_unchecked",
    "query_unchecked",
];

/// One SQL string found inside a Rust document.
#[derive(Debug, Clone, PartialEq)]
pub struct SqlRegion {
    /// The SQL text, verbatim from the Rust source (delimiters excluded).
    pub text: String,
    /// Where that text sits in the host document.
    pub range: Range,
}

impl SqlRegion {
    /// Whether `position` falls inside this region. The exclusive end is
    /// treated as inside so completion works at the very end of the string.
    pub fn contains(&self, position: Position) -> bool {
        self.range.start <= position && position <= self.range.end
    }

    /// Translates a host-document position (inside the region) to the
    /// coordinates of the embedded SQL text.
    pub fn to_embedded(&self, position: Position) -> Position {
        Position {
            line: position.line - self.range.start.line,
            character: if position.line == self.range.start.line {
                position.character - self.range.start.character
            } else {
                position.character
            },
        }
    }

    /// Translates a position in the embedded SQL text back to the host
    /// document.
    pub fn to_host(&self, position: Position) -> Position {
        Position {
            line: position.line + self.range.start.line,
            character: if position.line == 0 {
                position.character + self.range.start.character
            } else {
                position.character
            },
        }
    }

    /// Translates a range in the embedded SQL text back to the host
    /// document.
    pub fn to_host_range(&self, range: Range) -> Range {
        Range {
            start: self.to_host(range.start),
            end: self.to_host(range.end),
        }
    }
}

/// All SQL regions of one Rust document, ordered by position.
#[derive(Debug, Default)]
pub struct EmbeddedSql {
    /// The extracted regions, ordered by their start position.
    pub regions: Vec<SqlRegion>,
}

impl EmbeddedSql {
    /// Parses `document` as Rust and extracts the SQL string of every sqlx
    /// query macro invocation. Unparseable input yields whatever regions
    /// tree-sitter's error recovery still exposes.
    pub fn extract(document: &Document) -> EmbeddedSql {
        let mut parser = Parser::new();
        if parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .is_err()
        {
            // Only possible on an ABI mismatch between the linked grammar
            // and the tree-sitter runtime, which pinned versions rule out.
            tracing::error!("tree-sitter-rust grammar is incompatible with the runtime");
            return EmbeddedSql::default();
        }
        let Some(tree) = parser.parse(document.text(), None) else {
            return EmbeddedSql::default();
        };

        let source = document.text();
        let mut regions = Vec::new();
        let mut stack = vec![tree.root_node()];
        while let Some(node) = stack.pop() {
            if node.kind() == "macro_invocation" {
                if Self::is_sql_macro(node, source)
                    && let Some(region) = Self::region_from_macro(node, source, document)
                {
                    regions.push(region);
                }
                // Token trees contain only tokens; there is nothing further
                // to find below a macro invocation.
                continue;
            }
            let mut cursor = node.walk();
            stack.extend(node.children(&mut cursor));
        }
        regions.sort_by_key(|region| (region.range.start.line, region.range.start.character));

        EmbeddedSql { regions }
    }

    /// The region containing `position`, if any.
    pub fn region_at(&self, position: Position) -> Option<&SqlRegion> {
        self.regions.iter().find(|region| region.contains(position))
    }

    /// Whether the invoked macro is one of sqlx's query macros, either bare
    /// (`query_as!`) or path-qualified (`sqlx::query_as!`).
    fn is_sql_macro(node: Node<'_>, source: &str) -> bool {
        let Some(name_node) = node.child_by_field_name("macro") else {
            return false;
        };
        let name_node = match name_node.kind() {
            "scoped_identifier" => match name_node.child_by_field_name("name") {
                Some(name) => name,
                None => return false,
            },
            _ => name_node,
        };
        name_node
            .utf8_text(source.as_bytes())
            .is_ok_and(|name| SQL_MACROS.contains(&name))
    }

    /// The SQL region of one query macro invocation: the contents of the
    /// first (raw) string literal in its token tree.
    fn region_from_macro(node: Node<'_>, source: &str, document: &Document) -> Option<SqlRegion> {
        let mut cursor = node.walk();
        let token_tree = node
            .children(&mut cursor)
            .find(|child| child.kind() == "token_tree")?;

        let literal = token_tree
            .children(&mut cursor)
            .find(|child| matches!(child.kind(), "string_literal" | "raw_string_literal"))?;

        // The content is everything the grammar marks as string body:
        // `string_content` runs and (for plain strings) `escape_sequence`
        // nodes between them. An empty literal has neither and is skipped.
        let mut start = None;
        let mut end = None;
        for child in literal.children(&mut cursor) {
            if matches!(child.kind(), "string_content" | "escape_sequence") {
                start = Some(start.unwrap_or(child.start_byte()).min(child.start_byte()));
                end = Some(end.unwrap_or(child.end_byte()).max(child.end_byte()));
            }
        }
        let (start, end) = (start?, end?);
        let text = source.get(start..end)?.to_owned();

        Some(SqlRegion {
            text,
            range: Range {
                start: document.position_at(start),
                end: document.position_at(end),
            },
        })
    }
}

/// Completion items for the SQL region at `position`, if the position is
/// inside one.
pub fn completions(
    document: &Document,
    position: Position,
    schema: &Schema,
    kind: DatabaseKind,
) -> Vec<CompletionItem> {
    let embedded = EmbeddedSql::extract(document);
    let Some(region) = embedded.region_at(position) else {
        return Vec::new();
    };
    let sql_document = Document::new(region.text.clone(), 0);
    crate::analysis::completion::completions(
        &sql_document,
        region.to_embedded(position),
        schema,
        kind,
    )
}

/// Hover for the SQL region at `position`, with its highlight range mapped
/// back to host-document coordinates.
pub fn hover(
    document: &Document,
    position: Position,
    schema: &Schema,
    kind: DatabaseKind,
) -> Option<Hover> {
    let embedded = EmbeddedSql::extract(document);
    let region = embedded.region_at(position)?;
    let sql_document = Document::new(region.text.clone(), 0);
    let mut hover =
        crate::analysis::hover::hover(&sql_document, region.to_embedded(position), schema, kind)?;
    hover.range = hover.range.map(|range| region.to_host_range(range));
    Some(hover)
}

/// Goto definition for the SQL region at `position`. The result points into
/// a migration file, so no coordinate mapping is needed.
pub fn definition(
    document: &Document,
    position: Position,
    schema: &Schema,
    kind: DatabaseKind,
) -> Option<Location> {
    let embedded = EmbeddedSql::extract(document);
    let region = embedded.region_at(position)?;
    let sql_document = Document::new(region.text.clone(), 0);
    crate::analysis::definition::definition(
        &sql_document,
        region.to_embedded(position),
        schema,
        kind,
    )
}

/// Semantic tokens for every SQL region in a Rust document, shifted to host
/// coordinates and merged into one delta-encoded stream. The surrounding
/// Rust code is untouched — its highlighting belongs to rust-analyzer.
pub fn embedded_semantic_tokens(document: &Document, kind: DatabaseKind) -> Vec<SemanticToken> {
    let embedded = EmbeddedSql::extract(document);
    let mut all = Vec::new();
    for region in &embedded.regions {
        let sql_document = Document::new(region.text.clone(), 0);
        for mut segment in semantic_tokens::segments(&sql_document, kind) {
            if segment.line == 0 {
                segment.start += region.range.start.character;
            }
            segment.line += region.range.start.line;
            all.push(segment);
        }
    }
    semantic_tokens::encode(all)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn extract(source: &str) -> Vec<SqlRegion> {
        EmbeddedSql::extract(&Document::new(source.to_owned(), 0)).regions
    }

    #[test]
    fn extracts_plain_query_macro_strings() {
        let regions = extract(
            r#"async fn get(pool: &sqlx::SqlitePool) {
    let row = sqlx::query!("SELECT id FROM users").fetch_one(pool).await;
}"#,
        );
        assert_eq!(regions.len(), 1);
        assert_eq!(regions[0].text, "SELECT id FROM users");
        assert_eq!(regions[0].range.start, Position::new(1, 28));
        assert_eq!(regions[0].range.end, Position::new(1, 48));
    }

    #[test]
    fn extracts_the_string_argument_of_query_as() {
        let regions = extract(
            r#"fn f() { sqlx::query_as!(User, "SELECT id FROM users WHERE id = ?", id); }"#,
        );
        assert_eq!(regions.len(), 1);
        assert_eq!(regions[0].text, "SELECT id FROM users WHERE id = ?");
    }

    #[test]
    fn extracts_bare_and_scoped_macro_names() {
        let regions = extract(
            r#"use sqlx::query;
fn f() {
    query!("SELECT 1");
    sqlx::query_scalar!("SELECT 2");
}"#,
        );
        assert_eq!(regions.len(), 2);
        assert_eq!(regions[0].text, "SELECT 1");
        assert_eq!(regions[1].text, "SELECT 2");
    }

    #[test]
    fn extracts_multi_line_raw_strings() {
        let source = "fn f() {\n    sqlx::query!(\n        r#\"\nSELECT id, email\nFROM users\n\"#,\n    );\n}";
        let regions = extract(source);
        assert_eq!(regions.len(), 1);
        // The grammar starts `string_content` after the newline that follows
        // the opening delimiter; the region tracks the content's actual
        // bytes, so text and range stay consistent with each other.
        assert_eq!(regions[0].text, "SELECT id, email\nFROM users\n");
        assert_eq!(regions[0].range.start, Position::new(3, 0));
        assert_eq!(regions[0].range.end, Position::new(5, 0));
    }

    #[test]
    fn keeps_escape_sequences_verbatim() {
        let regions = extract(r#"fn f() { sqlx::query!("SELECT '\n'"); }"#);
        assert_eq!(regions.len(), 1);
        assert_eq!(regions[0].text, r"SELECT '\n'");
    }

    #[test]
    fn ignores_other_macros_and_functions() {
        let regions = extract(
            r#"fn f() {
    println!("SELECT id FROM users");
    sqlx::query("SELECT id FROM users");
    sqlx::query_file!("queries/get.sql");
}"#,
        );
        assert!(regions.is_empty());
    }

    #[test]
    fn position_mapping_round_trips() {
        let source =
            "fn f() {\n    sqlx::query!(\n        r#\"SELECT id\nFROM users\"#,\n    );\n}";
        let regions = extract(source);
        let region = &regions[0];

        // First line of the region: character shift applies.
        let host = Position::new(2, 18); // on `id`
        let embedded = region.to_embedded(host);
        assert_eq!(embedded, Position::new(0, 7));
        assert_eq!(region.to_host(embedded), host);

        // Later lines: characters are unshifted.
        let host = Position::new(3, 5); // on `users`
        let embedded = region.to_embedded(host);
        assert_eq!(embedded, Position::new(1, 5));
        assert_eq!(region.to_host(embedded), host);

        assert!(region.contains(Position::new(2, 11)));
        assert!(!region.contains(Position::new(1, 0)));
    }

    #[test]
    fn region_at_finds_the_enclosing_region() {
        let source =
            "fn f() {\n    sqlx::query!(\"SELECT 1\");\n    sqlx::query!(\"SELECT 2\");\n}";
        let embedded = EmbeddedSql::extract(&Document::new(source.to_owned(), 0));
        assert_eq!(
            embedded
                .region_at(Position::new(1, 20))
                .map(|region| region.text.as_str()),
            Some("SELECT 1")
        );
        assert_eq!(
            embedded
                .region_at(Position::new(2, 20))
                .map(|region| region.text.as_str()),
            Some("SELECT 2")
        );
        assert_eq!(embedded.region_at(Position::new(0, 3)), None);
    }

    #[test]
    fn broken_rust_still_yields_recoverable_regions() {
        let regions = extract("fn f( { sqlx::query!(\"SELECT 1\") }");
        assert_eq!(regions.len(), 1);
    }

    fn schema() -> Schema {
        let mut schema = Schema::default();
        schema.apply_sql(
            "CREATE TABLE users (id INTEGER PRIMARY KEY, email TEXT NOT NULL);",
            DatabaseKind::Sqlite,
            None,
        );
        schema
    }

    #[test]
    fn completion_works_inside_a_macro_string() {
        let source = r#"fn f() { sqlx::query!("SELECT id FROM ").fetch_one(pool); }"#;
        let document = Document::new(source.to_owned(), 0);
        // Cursor right after `FROM `.
        let labels: Vec<String> = completions(
            &document,
            Position::new(0, 38),
            &schema(),
            DatabaseKind::Sqlite,
        )
        .into_iter()
        .map(|item| item.label)
        .collect();
        assert_eq!(labels, vec!["users".to_owned()]);
    }

    #[test]
    fn completion_outside_regions_offers_nothing() {
        let source = r#"fn f() { sqlx::query!("SELECT id FROM users"); }"#;
        let document = Document::new(source.to_owned(), 0);
        assert!(
            completions(
                &document,
                Position::new(0, 3),
                &schema(),
                DatabaseKind::Sqlite
            )
            .is_empty()
        );
    }

    #[test]
    fn hover_maps_its_range_to_host_coordinates() {
        let source = r#"fn f() { sqlx::query!("SELECT id FROM users"); }"#;
        let document = Document::new(source.to_owned(), 0);
        // `users` occupies host characters 38..43.
        let hover = hover(
            &document,
            Position::new(0, 40),
            &schema(),
            DatabaseKind::Sqlite,
        )
        .expect("hovers");
        let range = hover.range.expect("has range");
        assert_eq!(range.start, Position::new(0, 38));
        assert_eq!(range.end, Position::new(0, 43));
    }

    #[test]
    fn hover_maps_ranges_on_later_lines_of_raw_strings() {
        let source =
            "fn f() {\n    sqlx::query!(\n        r#\"SELECT id\nFROM users\"#,\n    );\n}";
        let document = Document::new(source.to_owned(), 0);
        let hover = hover(
            &document,
            Position::new(3, 7),
            &schema(),
            DatabaseKind::Sqlite,
        )
        .expect("hovers");
        let range = hover.range.expect("has range");
        // `users` on the second SQL line is unshifted horizontally.
        assert_eq!(range.start, Position::new(3, 5));
        assert_eq!(range.end, Position::new(3, 10));
    }

    #[test]
    fn semantic_tokens_are_shifted_into_the_host_document() {
        let source = r#"fn f() { sqlx::query!("SELECT id FROM users"); }"#;
        let document = Document::new(source.to_owned(), 0);
        let tokens = embedded_semantic_tokens(&document, DatabaseKind::Sqlite);
        assert!(!tokens.is_empty());
        // First token is SELECT at the string content start (char 23).
        assert_eq!(tokens[0].delta_line, 0);
        assert_eq!(tokens[0].delta_start, 23);
        assert_eq!(tokens[0].length, 6);
    }

    #[test]
    fn semantic_tokens_merge_across_multiple_regions() {
        let source =
            "fn f() {\n    sqlx::query!(\"SELECT 1\");\n    sqlx::query!(\"SELECT 2\");\n}";
        let document = Document::new(source.to_owned(), 0);
        let tokens = embedded_semantic_tokens(&document, DatabaseKind::Sqlite);
        // SELECT + number per region.
        assert_eq!(tokens.len(), 4);
        // Second region's first token is on the next line (delta encoded).
        assert_eq!(tokens[2].delta_line, 1);
    }

    #[test]
    fn definition_resolves_from_inside_a_macro() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("1_init.sql"),
            "CREATE TABLE users (id INTEGER PRIMARY KEY);",
        )
        .expect("write migration");
        let schema = Schema::load_migrations(dir.path(), DatabaseKind::Sqlite).expect("loads");

        let source = r#"fn f() { sqlx::query!("SELECT id FROM users"); }"#;
        let document = Document::new(source.to_owned(), 0);
        let location = definition(
            &document,
            Position::new(0, 40),
            &schema,
            DatabaseKind::Sqlite,
        )
        .expect("has definition");
        assert!(location.uri.path().ends_with("1_init.sql"));
    }
}
