//! Hover information for schema objects referenced in SQL documents, with
//! curated fallback documentation for SQL keywords and common functions.

use sqlparser::keywords::Keyword;
use sqlparser::tokenizer::Token;
use tower_lsp_server::ls_types::{Hover, HoverContents, MarkupContent, MarkupKind, Position};

use crate::analysis::resolve::{Resolved, resolve_at};
use crate::document::Document;
use crate::parse::ParsedSql;
use crate::schema::{Column, Schema, SourceLocation, Table, TableOrigin};

/// Curated one-line documentation for SQL keywords, keyed by the uppercased
/// keyword token. Multi-word constructs are documented on each of their
/// leading tokens.
const KEYWORD_DOCS: &[(&str, &str)] = &[
    (
        "SELECT",
        "Retrieves rows, evaluating one expression per result column.",
    ),
    ("FROM", "Names the relations a query reads from."),
    ("WHERE", "Filters rows to those satisfying a predicate."),
    (
        "GROUP",
        "`GROUP BY` collapses rows sharing the listed values into one group per combination, for use with aggregate functions.",
    ),
    (
        "ORDER",
        "`ORDER BY` sorts the result rows by the listed expressions.",
    ),
    (
        "HAVING",
        "Filters groups produced by `GROUP BY`, after aggregation.",
    ),
    ("LIMIT", "Caps the number of rows returned."),
    ("OFFSET", "Skips a number of rows before returning results."),
    (
        "JOIN",
        "Combines rows of two relations on a join condition.",
    ),
    (
        "INNER",
        "`INNER JOIN` keeps only row pairs satisfying the join condition.",
    ),
    (
        "LEFT",
        "`LEFT JOIN` keeps every left-side row, padding missing right-side columns with NULL.",
    ),
    (
        "RIGHT",
        "`RIGHT JOIN` keeps every right-side row, padding missing left-side columns with NULL.",
    ),
    (
        "CROSS",
        "`CROSS JOIN` produces the cartesian product of both relations.",
    ),
    (
        "ON",
        "The join condition relating the two joined relations.",
    ),
    ("AS", "Names an output column or aliases a relation."),
    ("DISTINCT", "Removes duplicate rows from the result."),
    (
        "UNION",
        "Concatenates two query results, removing duplicates unless `ALL` is given.",
    ),
    (
        "BETWEEN",
        "True when a value lies within an inclusive range.",
    ),
    (
        "LIKE",
        "Pattern match; `%` matches any run of characters, `_` any single one.",
    ),
    ("EXISTS", "True when the subquery returns at least one row."),
    (
        "CASE",
        "Conditional expression: `CASE WHEN cond THEN value ... ELSE value END`.",
    ),
    (
        "INSERT",
        "`INSERT INTO` adds rows to a table, from a `VALUES` list or a query.",
    ),
    (
        "VALUES",
        "Literal row tuples for an insert or a table value constructor.",
    ),
    (
        "UPDATE",
        "Modifies column values of the rows matching the `WHERE` clause.",
    ),
    ("SET", "The column assignments of an `UPDATE`."),
    (
        "DELETE",
        "`DELETE FROM` removes the rows matching the `WHERE` clause.",
    ),
    (
        "WITH",
        "Defines common table expressions (CTEs): named subqueries usable in the following statement.",
    ),
    (
        "RETURNING",
        "Returns values computed from the rows an INSERT, UPDATE, or DELETE touched.",
    ),
    (
        "CREATE",
        "Defines a new schema object (`CREATE TABLE`, `CREATE VIEW`, ...).",
    ),
    (
        "ALTER",
        "`ALTER TABLE` changes an existing table's definition.",
    ),
    ("DROP", "Removes a schema object."),
    (
        "PRIMARY",
        "`PRIMARY KEY` declares the column(s) uniquely identifying each row.",
    ),
    (
        "REFERENCES",
        "Declares a foreign key: values must exist in the referenced column.",
    ),
    (
        "DEFAULT",
        "The value a column takes when an insert omits it.",
    ),
    (
        "UNIQUE",
        "Constrains a column (or set) to distinct values across rows.",
    ),
];

/// Curated one-line documentation for common SQL functions, keyed by the
/// lowercased function name.
const FUNCTION_DOCS: &[(&str, &str)] = &[
    ("abs", "`abs(x)` — the absolute value of `x`."),
    (
        "avg",
        "`avg(x)` — aggregate: the mean of non-NULL values of `x`.",
    ),
    (
        "coalesce",
        "`coalesce(a, b, ...)` — the first non-NULL argument.",
    ),
    (
        "count",
        "`count(*)` counts rows; `count(x)` counts non-NULL values of `x`.",
    ),
    (
        "ifnull",
        "`ifnull(a, b)` — `a` unless it is NULL, then `b`.",
    ),
    ("length", "`length(s)` — the character length of `s`."),
    ("lower", "`lower(s)` — `s` folded to lower case."),
    (
        "max",
        "`max(x)` — aggregate: the largest non-NULL value of `x`.",
    ),
    (
        "min",
        "`min(x)` — aggregate: the smallest non-NULL value of `x`.",
    ),
    (
        "round",
        "`round(x[, digits])` — `x` rounded to the given precision.",
    ),
    (
        "sum",
        "`sum(x)` — aggregate: the total of non-NULL values of `x`.",
    ),
    ("upper", "`upper(s)` — `s` folded to upper case."),
];

/// Builds hover content for the schema object referenced at `position`, or
/// curated keyword/function documentation when the position holds no
/// resolvable reference.
pub fn hover(
    document: &Document,
    parsed: &ParsedSql,
    position: Position,
    schema: &Schema,
) -> Option<Hover> {
    let Some(resolved) = resolve_at(document, parsed, position, schema) else {
        return keyword_hover(document, parsed, position);
    };
    let (value, range) = match &resolved {
        Resolved::Table { table, range } => (table_markdown(table), *range),
        Resolved::Column {
            table,
            column,
            range,
        } => (column_markdown(table, column), *range),
    };
    Some(Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value,
        }),
        range: Some(range),
    })
}

/// Hover for the keyword or function-call name under `position`, from the
/// curated documentation tables. Function names only count when a
/// parenthesis follows — a bare word is an identifier, not a call.
fn keyword_hover(document: &Document, parsed: &ParsedSql, position: Position) -> Option<Hover> {
    let significant: Vec<_> = parsed
        .tokens
        .iter()
        .filter(|token| !matches!(token.token, Token::Whitespace(_) | Token::EOF))
        .collect();
    let index = significant
        .iter()
        .position(|token| document.position_in_span(position, token.span))?;
    let Token::Word(word) = &significant[index].token else {
        return None;
    };

    // Function names first: many (count, max, ...) double as sqlparser
    // keywords, and in call position the function reading is the right one.
    let follows_call = significant
        .get(index + 1)
        .is_some_and(|token| token.token == Token::LParen);
    let function_doc = follows_call
        .then(|| {
            let name = word.value.to_ascii_lowercase();
            FUNCTION_DOCS.iter().find(|(known, _)| *known == name)
        })
        .flatten();
    let doc = match function_doc {
        Some((_, doc)) => doc,
        None if word.keyword != Keyword::NoKeyword => {
            let name = word.value.to_ascii_uppercase();
            KEYWORD_DOCS.iter().find(|(known, _)| *known == name)?.1
        }
        None => return None,
    };

    Some(Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value: doc.to_owned(),
        }),
        range: document.range_of(significant[index].span),
    })
}

fn origin_line(origin: TableOrigin, location: Option<&SourceLocation>) -> String {
    match (origin, location) {
        (TableOrigin::Migration, Some(location)) => {
            let file = location
                .uri
                .to_file_path()
                .and_then(|path| {
                    path.file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                })
                .unwrap_or_else(|| "migration".to_owned());
            format!("*defined in {file}*")
        }
        (TableOrigin::Migration, None) => "*defined in migrations*".to_owned(),
        (TableOrigin::Database, _) => "*from live database*".to_owned(),
        (TableOrigin::Query, _) => "*defined in this query*".to_owned(),
    }
}

fn table_markdown(table: &Table) -> String {
    format!(
        "```sql\n{}\n```\n\n{}",
        table.ddl(),
        origin_line(table.origin, table.location.as_ref())
    )
}

fn column_markdown(table: &Table, column: &Column) -> String {
    format!(
        "```sql\n{}.{}\n```\n\n{}",
        table.name,
        column.signature(),
        origin_line(table.origin, column.location.as_ref())
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::DatabaseKind;

    fn hover_value(sql: &str, character: u32) -> Option<String> {
        let mut schema = Schema::default();
        schema.apply_sql(
            "CREATE TABLE users (id INTEGER PRIMARY KEY, email TEXT NOT NULL DEFAULT 'x');",
            DatabaseKind::Sqlite,
            None,
        );
        let document = Document::new(sql.to_owned());
        let parsed = ParsedSql::parse(DatabaseKind::Sqlite.dialect(), document.text());
        let hover = hover(&document, &parsed, Position::new(0, character), &schema)?;
        match hover.contents {
            HoverContents::Markup(markup) => Some(markup.value),
            _ => None,
        }
    }

    #[test]
    fn table_hover_shows_reconstructed_ddl() {
        let value = hover_value("SELECT id FROM users", 16).expect("hovers");
        assert!(value.contains("CREATE TABLE users ("));
        assert!(value.contains("id INTEGER PRIMARY KEY"));
        assert!(value.contains("email TEXT NOT NULL DEFAULT 'x'"));
        assert!(value.contains("*defined in migrations*"));
    }

    #[test]
    fn column_hover_shows_signature() {
        let value = hover_value("SELECT email FROM users", 8).expect("hovers");
        assert!(value.contains("users.email TEXT NOT NULL DEFAULT 'x'"));
    }

    #[test]
    fn no_hover_for_unknown_identifiers() {
        assert_eq!(hover_value("SELECT nope FROM missing", 8), None);
    }

    #[test]
    fn keywords_hover_with_curated_docs() {
        let value = hover_value("SELECT id FROM users", 2).expect("hovers");
        assert!(value.contains("Retrieves rows"), "{value}");

        // Multi-word constructs are documented on their leading token.
        let value = hover_value("SELECT id FROM users GROUP BY id", 23).expect("hovers");
        assert!(value.contains("GROUP BY"), "{value}");
    }

    #[test]
    fn function_calls_hover_with_curated_docs() {
        let value = hover_value("SELECT count(id) FROM users", 9).expect("hovers");
        assert!(value.contains("counts rows"), "{value}");
    }

    #[test]
    fn bare_words_matching_function_names_do_not_hover() {
        // `count` without a call is an identifier; with no schema match it
        // hovers nothing.
        assert_eq!(hover_value("SELECT count FROM missing", 9), None);
    }
}
