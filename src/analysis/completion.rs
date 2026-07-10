//! Context-aware completion for SQL documents.
//!
//! Completion works from the token stream rather than the AST: the statement
//! under the cursor is usually syntactically incomplete while the user types,
//! so the context (table position, qualified column position, general
//! expression) and the in-scope tables are recovered from tokens alone.

use std::collections::BTreeMap;

use sqlparser::keywords::Keyword;
use sqlparser::tokenizer::Token;
use tower_lsp::lsp_types::{
    CompletionItem, CompletionItemKind, Documentation, MarkupContent, MarkupKind, Position, Range,
};

use crate::db::DatabaseKind;
use crate::document::Document;
use crate::parse::ParsedSql;
use crate::schema::{Column, Schema, Table, TableKind};

/// Common SQL keywords offered in general contexts.
const KEYWORDS: &[&str] = &[
    "SELECT",
    "FROM",
    "WHERE",
    "GROUP BY",
    "ORDER BY",
    "HAVING",
    "LIMIT",
    "OFFSET",
    "JOIN",
    "INNER JOIN",
    "LEFT JOIN",
    "RIGHT JOIN",
    "CROSS JOIN",
    "ON",
    "AS",
    "AND",
    "OR",
    "NOT",
    "IN",
    "IS",
    "NULL",
    "LIKE",
    "BETWEEN",
    "EXISTS",
    "CASE",
    "WHEN",
    "THEN",
    "ELSE",
    "END",
    "UNION",
    "ALL",
    "DISTINCT",
    "INSERT INTO",
    "VALUES",
    "UPDATE",
    "SET",
    "DELETE FROM",
    "RETURNING",
    "WITH",
    "CREATE TABLE",
    "CREATE VIEW",
    "ALTER TABLE",
    "DROP TABLE",
    "PRIMARY KEY",
    "REFERENCES",
    "NOT NULL",
    "DEFAULT",
    "UNIQUE",
    "ASC",
    "DESC",
];

/// Common SQL functions offered in general contexts.
const FUNCTIONS: &[&str] = &[
    "abs", "avg", "coalesce", "count", "ifnull", "length", "lower", "max", "min", "round", "sum",
    "upper",
];

/// What kind of completions the cursor position calls for.
#[derive(Debug, PartialEq, Eq)]
enum Context {
    /// Right after `FROM`, `JOIN`, `INTO`, `UPDATE`, or `TABLE`: relations.
    TableName,
    /// Right after `<qualifier>.`: columns of the qualified relation.
    QualifiedColumn { qualifier: String },
    /// Anywhere else: columns in scope, relations, keywords, functions.
    General,
}

/// The token neighborhood of the cursor within its statement.
struct Cursor<'a> {
    /// Significant (non-whitespace, non-comment) tokens of the statement
    /// containing the cursor, paired with their document ranges.
    tokens: Vec<(&'a Token, Range)>,
    /// Index into `tokens` of the last token that starts before the cursor.
    last_before: Option<usize>,
}

impl<'a> Cursor<'a> {
    fn locate(parsed: &'a ParsedSql, document: &Document, position: Position) -> Cursor<'a> {
        // Statement boundaries are semicolons; track only the statement
        // containing the cursor so scope scanning stays local.
        let mut tokens: Vec<(&Token, Range)> = Vec::new();
        let mut last_before = None;
        let mut cursor_statement_done = false;

        for token in &parsed.tokens {
            let Some(range) = document.range_of(token.span) else {
                continue;
            };
            if matches!(token.token, Token::Whitespace(_) | Token::EOF) {
                continue;
            }
            if token.token == Token::SemiColon {
                if range.start >= position {
                    // The statement holding the cursor has ended.
                    cursor_statement_done = true;
                    continue;
                }
                // The cursor is in a later statement; restart.
                tokens.clear();
                last_before = None;
                continue;
            }
            if cursor_statement_done {
                break;
            }
            if range.start < position {
                last_before = Some(tokens.len());
            }
            tokens.push((&token.token, range));
        }

        Cursor {
            tokens,
            last_before,
        }
    }

    fn context(&self, position: Position) -> Context {
        let Some(mut index) = self.last_before else {
            return Context::General;
        };
        // A word the cursor sits inside (or at the end of) is the partially
        // typed text the client will filter with, not completed context;
        // the context comes from the token before it.
        let (token, range) = &self.tokens[index];
        if matches!(token, Token::Word(_)) && position <= range.end {
            match index.checked_sub(1) {
                Some(previous) => index = previous,
                None => return Context::General,
            }
        }

        match self.tokens[index].0 {
            Token::Period => {
                if let Some(before_period) = index.checked_sub(1)
                    && let Token::Word(word) = self.tokens[before_period].0
                {
                    return Context::QualifiedColumn {
                        qualifier: word.value.clone(),
                    };
                }
                Context::General
            }
            Token::Word(word)
                if matches!(
                    word.keyword,
                    Keyword::FROM
                        | Keyword::JOIN
                        | Keyword::INTO
                        | Keyword::UPDATE
                        | Keyword::TABLE
                ) =>
            {
                Context::TableName
            }
            _ => Context::General,
        }
    }

    /// The relations referenced by this statement, scanned from tokens:
    /// every `FROM`/`JOIN`/`INTO`/`UPDATE` target with its optional alias,
    /// including comma-separated `FROM` lists. Keys and values are
    /// lowercased; aliases map to their table, tables map to themselves.
    fn scope(&self) -> BTreeMap<String, String> {
        let mut scope = BTreeMap::new();
        let word_at = |index: usize| match self.tokens.get(index) {
            Some((Token::Word(word), _)) => Some(word),
            _ => None,
        };

        let mut index = 0;
        while index < self.tokens.len() {
            let Some(word) = word_at(index) else {
                index += 1;
                continue;
            };
            if !matches!(
                word.keyword,
                Keyword::FROM | Keyword::JOIN | Keyword::INTO | Keyword::UPDATE
            ) {
                index += 1;
                continue;
            }

            // Consume `table [AS] [alias]` groups, continuing over commas
            // for `FROM a, b` lists.
            let mut next = index + 1;
            loop {
                let Some(table) = word_at(next).filter(|word| word.keyword == Keyword::NoKeyword)
                else {
                    break;
                };
                let table_name = table.value.to_ascii_lowercase();
                scope.insert(table_name.clone(), table_name.clone());
                next += 1;

                if word_at(next).is_some_and(|word| word.keyword == Keyword::AS) {
                    next += 1;
                }
                if let Some(alias) = word_at(next).filter(|word| word.keyword == Keyword::NoKeyword)
                {
                    scope.insert(alias.value.to_ascii_lowercase(), table_name);
                    next += 1;
                }

                if self
                    .tokens
                    .get(next)
                    .is_some_and(|(token, _)| **token == Token::Comma)
                {
                    next += 1;
                } else {
                    break;
                }
            }
            index = next;
        }
        scope
    }
}

fn table_item(table: &Table, sort_group: char) -> CompletionItem {
    CompletionItem {
        label: table.name.clone(),
        kind: Some(match table.kind {
            TableKind::Table => CompletionItemKind::CLASS,
            TableKind::View => CompletionItemKind::INTERFACE,
        }),
        detail: Some(
            match table.kind {
                TableKind::Table => "table",
                TableKind::View => "view",
            }
            .to_owned(),
        ),
        documentation: Some(Documentation::MarkupContent(MarkupContent {
            kind: MarkupKind::Markdown,
            value: format!("```sql\n{}\n```", table.ddl()),
        })),
        sort_text: Some(format!("{sort_group}{}", table.name)),
        ..CompletionItem::default()
    }
}

fn column_item(table: &Table, column: &Column, sort_group: char) -> CompletionItem {
    CompletionItem {
        label: column.name.clone(),
        kind: Some(CompletionItemKind::FIELD),
        detail: Some(column.signature()),
        documentation: Some(Documentation::MarkupContent(MarkupContent {
            kind: MarkupKind::Markdown,
            value: format!("column of `{}`", table.name),
        })),
        sort_text: Some(format!("{sort_group}{}", column.name)),
        ..CompletionItem::default()
    }
}

fn keyword_items(items: &mut Vec<CompletionItem>) {
    items.extend(KEYWORDS.iter().map(|keyword| CompletionItem {
        label: (*keyword).to_owned(),
        kind: Some(CompletionItemKind::KEYWORD),
        sort_text: Some(format!("3{keyword}")),
        ..CompletionItem::default()
    }));
    items.extend(FUNCTIONS.iter().map(|function| CompletionItem {
        label: (*function).to_owned(),
        kind: Some(CompletionItemKind::FUNCTION),
        sort_text: Some(format!("4{function}")),
        ..CompletionItem::default()
    }));
}

/// Computes completion items for `position` in `document`.
pub fn completions(
    document: &Document,
    position: Position,
    schema: &Schema,
    kind: DatabaseKind,
) -> Vec<CompletionItem> {
    let parsed = ParsedSql::parse(kind.dialect(), document.text());
    let cursor = Cursor::locate(&parsed, document, position);
    let scope = cursor.scope();
    let resolve_table = |name: &str| {
        let lowered = name.to_ascii_lowercase();
        scope
            .get(&lowered)
            .and_then(|target| schema.table(target))
            .or_else(|| schema.table(name))
    };

    let mut items = Vec::new();
    match cursor.context(position) {
        Context::TableName => {
            items.extend(schema.tables().map(|table| table_item(table, '1')));
        }
        Context::QualifiedColumn { qualifier } => {
            // Only the qualified relation's columns make sense here; if the
            // qualifier is unknown, offering anything would be misleading.
            if let Some(table) = resolve_table(&qualifier) {
                items.extend(
                    table
                        .columns
                        .iter()
                        .map(|column| column_item(table, column, '1')),
                );
            }
        }
        Context::General => {
            // Columns of in-scope relations first, then all relations,
            // keywords, and functions; clients filter by typed prefix.
            let mut seen_columns: BTreeMap<String, CompletionItem> = BTreeMap::new();
            for table in scope.values().filter_map(|name| schema.table(name)) {
                for column in &table.columns {
                    seen_columns
                        .entry(column.name.to_ascii_lowercase())
                        .or_insert_with(|| column_item(table, column, '1'));
                }
            }
            items.extend(seen_columns.into_values());
            items.extend(schema.tables().map(|table| table_item(table, '2')));
            keyword_items(&mut items);
        }
    }
    items
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schema() -> Schema {
        let mut schema = Schema::default();
        schema.apply_sql(
            "CREATE TABLE users (id INTEGER PRIMARY KEY, email TEXT NOT NULL);
             CREATE TABLE posts (id INTEGER PRIMARY KEY, author_id INTEGER, title TEXT);
             CREATE VIEW titles AS SELECT title FROM posts;",
            DatabaseKind::Sqlite,
            None,
        );
        schema
    }

    /// Completion labels at the position marked by `|` in `sql`.
    fn labels_at(sql: &str) -> Vec<String> {
        let offset = sql.find('|').expect("marker");
        let text = sql.replace('|', "");
        let before = &text[..offset];
        let line = before.matches('\n').count() as u32;
        let character = before
            .rsplit('\n')
            .next()
            .unwrap_or(before)
            .encode_utf16()
            .count() as u32;
        let document = Document::new(text, 0);
        completions(
            &document,
            Position::new(line, character),
            &schema(),
            DatabaseKind::Sqlite,
        )
        .into_iter()
        .map(|item| item.label)
        .collect()
    }

    #[test]
    fn after_from_offers_tables_only() {
        let labels = labels_at("SELECT id FROM |");
        assert!(labels.contains(&"users".to_owned()));
        assert!(labels.contains(&"posts".to_owned()));
        assert!(labels.contains(&"titles".to_owned()));
        assert!(!labels.contains(&"email".to_owned()));
        assert!(!labels.contains(&"SELECT".to_owned()));
    }

    #[test]
    fn after_from_with_partial_word_still_offers_tables() {
        let labels = labels_at("SELECT id FROM us|");
        assert!(labels.contains(&"users".to_owned()));
        assert!(!labels.contains(&"SELECT".to_owned()));
    }

    #[test]
    fn after_insert_into_offers_tables() {
        let labels = labels_at("INSERT INTO |");
        assert!(labels.contains(&"posts".to_owned()));
        assert!(!labels.contains(&"title".to_owned()));
    }

    #[test]
    fn qualifier_offers_columns_of_that_table() {
        let labels = labels_at("SELECT u.| FROM users AS u");
        assert_eq!(labels, vec!["id".to_owned(), "email".to_owned()]);
    }

    #[test]
    fn qualifier_with_partial_column_keeps_the_qualified_context() {
        let labels = labels_at("SELECT u.em| FROM users AS u");
        assert_eq!(labels, vec!["id".to_owned(), "email".to_owned()]);
    }

    #[test]
    fn unknown_qualifier_offers_nothing() {
        assert!(labels_at("SELECT x.| FROM users").is_empty());
    }

    #[test]
    fn general_context_prefers_in_scope_columns() {
        let labels = labels_at("SELECT | FROM posts");
        assert!(labels.contains(&"title".to_owned()));
        assert!(labels.contains(&"author_id".to_owned()));
        // Out-of-scope columns are not offered unqualified.
        assert!(!labels.contains(&"email".to_owned()));
        // Tables and keywords are still available.
        assert!(labels.contains(&"users".to_owned()));
        assert!(labels.contains(&"SELECT".to_owned()));
        assert!(labels.contains(&"count".to_owned()));
    }

    #[test]
    fn scope_is_limited_to_the_cursor_statement() {
        let labels = labels_at("SELECT email FROM users; SELECT | FROM posts;");
        assert!(labels.contains(&"title".to_owned()));
        assert!(!labels.contains(&"email".to_owned()));
    }

    #[test]
    fn comma_separated_from_lists_bring_all_tables_into_scope() {
        let labels = labels_at("SELECT | FROM users, posts");
        assert!(labels.contains(&"email".to_owned()));
        assert!(labels.contains(&"title".to_owned()));
    }

    #[test]
    fn empty_document_offers_keywords_and_tables() {
        let labels = labels_at("|");
        assert!(labels.contains(&"SELECT".to_owned()));
        assert!(labels.contains(&"users".to_owned()));
    }
}
