//! Context-aware completion for SQL documents.
//!
//! Completion works from the token stream rather than the AST: the statement
//! under the cursor is usually syntactically incomplete while the user types,
//! so the context (table position, qualified column position, general
//! expression) and the in-scope tables are recovered from tokens alone.

use std::collections::BTreeMap;

use sqlparser::keywords::Keyword;
use sqlparser::tokenizer::Token;
use tower_lsp_server::ls_types::{
    CompletionItem, CompletionItemKind, CompletionTextEdit, Documentation, MarkupContent,
    MarkupKind, Position, Range, TextEdit,
};

use crate::analysis::hover;
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
    /// Inside the column list of `INSERT INTO <table> (...)`: strictly that
    /// table's columns.
    InsertColumns { table: String },
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
            _ => self
                .insert_column_list(index)
                .map(|table| Context::InsertColumns { table })
                .unwrap_or(Context::General),
        }
    }

    /// The target table when the token at `index` sits inside the column
    /// list of `INSERT INTO <table> (...)`: the innermost unmatched opening
    /// parenthesis at or before `index` directly follows the insert target.
    /// A `VALUES (...)` or subquery parenthesis is someone else's context.
    fn insert_column_list(&self, index: usize) -> Option<String> {
        let mut depth = 0usize;
        let open = (0..=index).rev().find(|&i| match self.tokens[i].0 {
            Token::RParen => {
                depth += 1;
                false
            }
            Token::LParen => match depth {
                0 => true,
                _ => {
                    depth -= 1;
                    false
                }
            },
            _ => false,
        })?;
        let table = match self.tokens.get(open.checked_sub(1)?) {
            Some((Token::Word(word), _)) if word.keyword == Keyword::NoKeyword => &word.value,
            _ => return None,
        };
        match self.tokens.get(open.checked_sub(2)?) {
            Some((Token::Word(word), _)) if word.keyword == Keyword::INTO => Some(table.clone()),
            _ => None,
        }
    }

    /// The range of the word being typed at `position`, when the cursor
    /// sits inside or at the end of one.
    fn word_range(&self, position: Position) -> Option<Range> {
        let index = self.last_before?;
        let (token, range) = &self.tokens[index];
        (matches!(token, Token::Word(_)) && position <= range.end).then_some(*range)
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
            while let Some(table) = word_at(next).filter(|word| word.keyword == Keyword::NoKeyword)
            {
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

/// The explicit edit for accepting a completion: replace the word being
/// typed (or insert at the cursor) with `new_text`. Explicit ranges keep
/// multi-word completions ("GROUP BY") from duplicating already-typed text
/// under clients' word-boundary heuristics.
fn accept_edit(replace: Range, new_text: &str) -> Option<CompletionTextEdit> {
    Some(CompletionTextEdit::Edit(TextEdit {
        range: replace,
        new_text: new_text.to_owned(),
    }))
}

fn table_item(table: &Table, sort_group: char, replace: Range) -> CompletionItem {
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
        text_edit: accept_edit(replace, &table.name),
        ..CompletionItem::default()
    }
}

fn column_item(table: &Table, column: &Column, sort_group: char, replace: Range) -> CompletionItem {
    CompletionItem {
        label: column.name.clone(),
        kind: Some(CompletionItemKind::FIELD),
        detail: Some(column.signature()),
        documentation: Some(Documentation::MarkupContent(MarkupContent {
            kind: MarkupKind::Markdown,
            value: format!("column of `{}`", table.name),
        })),
        sort_text: Some(format!("{sort_group}{}", column.name)),
        text_edit: accept_edit(replace, &column.name),
        ..CompletionItem::default()
    }
}

/// Keywords specific to one backend's SQL flavor.
fn dialect_keywords(kind: DatabaseKind) -> &'static [&'static str] {
    match kind {
        DatabaseKind::Postgres => &["ILIKE", "ON CONFLICT", "RETURNING"],
        DatabaseKind::MySql => &["AUTO_INCREMENT", "ON DUPLICATE KEY UPDATE"],
        DatabaseKind::Sqlite => &["AUTOINCREMENT", "ON CONFLICT", "RETURNING"],
    }
}

fn keyword_items(items: &mut Vec<CompletionItem>, kind: DatabaseKind, replace: Range) {
    let keywords = KEYWORDS.iter().chain(dialect_keywords(kind));
    items.extend(keywords.map(|keyword| CompletionItem {
        label: (*keyword).to_owned(),
        kind: Some(CompletionItemKind::KEYWORD),
        documentation: curated_documentation(hover::keyword_documentation(keyword)),
        sort_text: Some(format!("3{keyword}")),
        text_edit: accept_edit(replace, keyword),
        ..CompletionItem::default()
    }));
    items.extend(FUNCTIONS.iter().map(|function| CompletionItem {
        label: (*function).to_owned(),
        kind: Some(CompletionItemKind::FUNCTION),
        documentation: curated_documentation(hover::function_documentation(function)),
        sort_text: Some(format!("4{function}")),
        text_edit: accept_edit(replace, function),
        ..CompletionItem::default()
    }));
}

/// Wraps a curated documentation line for a completion item.
fn curated_documentation(doc: Option<&str>) -> Option<Documentation> {
    doc.map(|doc| {
        Documentation::MarkupContent(MarkupContent {
            kind: MarkupKind::Markdown,
            value: doc.to_owned(),
        })
    })
}

/// Computes completion items for `position` in `document`. `kind` selects
/// the backend-specific keywords offered in general contexts.
pub fn completions(
    document: &Document,
    parsed: &ParsedSql,
    position: Position,
    schema: &Schema,
    kind: DatabaseKind,
) -> Vec<CompletionItem> {
    let cursor = Cursor::locate(parsed, document, position);
    let scope = cursor.scope();
    // Relations the document's statements define themselves (CTEs and
    // derived subqueries). Only available while the statement parses — a
    // partially typed name after a dot is enough to keep it parseable.
    let locals = crate::analysis::resolve::query_local_tables(&parsed.statements, schema);
    let resolve_table = |name: &str| {
        locals
            .iter()
            .find(|table| table.name.eq_ignore_ascii_case(name))
            .or_else(|| {
                let lowered = name.to_ascii_lowercase();
                scope
                    .get(&lowered)
                    .and_then(|target| schema.table(target))
                    .or_else(|| schema.table(name))
            })
    };

    // Accepting an item replaces the word being typed, or inserts at the
    // cursor when there is none.
    let replace = cursor.word_range(position).unwrap_or(Range {
        start: position,
        end: position,
    });

    let mut items = Vec::new();
    match cursor.context(position) {
        Context::TableName => {
            items.extend(locals.iter().map(|table| table_item(table, '1', replace)));
            items.extend(schema.tables().map(|table| table_item(table, '1', replace)));
        }
        Context::QualifiedColumn { qualifier } => {
            // Only the qualified relation's columns make sense here; if the
            // qualifier is unknown, offering anything would be misleading.
            if let Some(table) = resolve_table(&qualifier) {
                items.extend(
                    table
                        .columns
                        .iter()
                        .map(|column| column_item(table, column, '1', replace)),
                );
            }
        }
        Context::InsertColumns { table } => {
            // Strictly the insert target's columns: nothing else is valid
            // in an INSERT column list.
            if let Some(table) = resolve_table(&table) {
                items.extend(
                    table
                        .columns
                        .iter()
                        .map(|column| column_item(table, column, '1', replace)),
                );
            }
        }
        Context::General => {
            // Columns of in-scope relations first, then all relations,
            // keywords, and functions; clients filter by typed prefix.
            let mut seen_columns: BTreeMap<String, CompletionItem> = BTreeMap::new();
            for table in scope.values().filter_map(|name| resolve_table(name)) {
                for column in &table.columns {
                    seen_columns
                        .entry(column.name.to_ascii_lowercase())
                        .or_insert_with(|| column_item(table, column, '1', replace));
                }
            }
            items.extend(seen_columns.into_values());
            items.extend(schema.tables().map(|table| table_item(table, '2', replace)));
            keyword_items(&mut items, kind, replace);
        }
    }
    items
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::DatabaseKind;

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
        let document = Document::new(text);
        let parsed = ParsedSql::parse(DatabaseKind::Sqlite.dialect(), document.text());
        completions(
            &document,
            &parsed,
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
    fn insert_column_lists_offer_only_the_target_columns() {
        let labels = labels_at("INSERT INTO posts (|");
        assert_eq!(
            labels,
            vec!["id".to_owned(), "author_id".to_owned(), "title".to_owned()]
        );

        // Later in the list, and mid-word, the context holds.
        let labels = labels_at("INSERT INTO posts (author_id, t|");
        assert_eq!(
            labels,
            vec!["id".to_owned(), "author_id".to_owned(), "title".to_owned()]
        );
    }

    #[test]
    fn insert_values_parens_are_not_a_column_list() {
        let labels = labels_at("INSERT INTO posts (author_id) VALUES (|");
        assert!(labels.contains(&"SELECT".to_owned()));
    }

    #[test]
    fn unknown_insert_targets_offer_nothing() {
        assert!(labels_at("INSERT INTO nope (|").is_empty());
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

    #[test]
    fn cte_columns_complete_after_a_qualifier() {
        // The partially typed column keeps the statement parseable, which is
        // what makes the CTE's derived columns available.
        let labels =
            labels_at("WITH recent AS (SELECT id, title FROM posts) SELECT recent.t| FROM recent");
        assert_eq!(labels, vec!["id".to_owned(), "title".to_owned()]);
    }

    #[test]
    fn cte_names_complete_after_from() {
        let labels = labels_at("WITH recent AS (SELECT id FROM posts) SELECT id FROM re|");
        assert!(labels.contains(&"recent".to_owned()));
        assert!(labels.contains(&"users".to_owned()));
    }

    #[test]
    fn items_replace_the_word_being_typed() {
        // "SELECT id FROM us|" — accepting an item must replace `us`.
        let sql = "SELECT id FROM us";
        let document = Document::new(sql.to_owned());
        let parsed = ParsedSql::parse(DatabaseKind::Sqlite.dialect(), document.text());
        let items = completions(
            &document,
            &parsed,
            Position::new(0, 17),
            &schema(),
            DatabaseKind::Sqlite,
        );
        let users = items
            .iter()
            .find(|item| item.label == "users")
            .expect("users offered");
        let Some(CompletionTextEdit::Edit(edit)) = &users.text_edit else {
            panic!("expected a plain text edit");
        };
        assert_eq!(edit.range.start, Position::new(0, 15));
        assert_eq!(edit.range.end, Position::new(0, 17));
        assert_eq!(edit.new_text, "users");

        // With no word under the cursor the edit inserts at the position.
        let sql = "SELECT id FROM ";
        let document = Document::new(sql.to_owned());
        let parsed = ParsedSql::parse(DatabaseKind::Sqlite.dialect(), document.text());
        let items = completions(
            &document,
            &parsed,
            Position::new(0, 15),
            &schema(),
            DatabaseKind::Sqlite,
        );
        let users = items
            .iter()
            .find(|item| item.label == "users")
            .expect("users offered");
        let Some(CompletionTextEdit::Edit(edit)) = &users.text_edit else {
            panic!("expected a plain text edit");
        };
        assert_eq!(edit.range.start, Position::new(0, 15));
        assert_eq!(edit.range.end, Position::new(0, 15));
    }

    #[test]
    fn keyword_and_function_items_carry_curated_documentation() {
        let document = Document::new(String::new());
        let parsed = ParsedSql::parse(DatabaseKind::Sqlite.dialect(), document.text());
        let items = completions(
            &document,
            &parsed,
            Position::new(0, 0),
            &schema(),
            DatabaseKind::Sqlite,
        );
        let doc_of = |label: &str| {
            let item = items
                .iter()
                .find(|item| item.label == label)
                .unwrap_or_else(|| panic!("{label} offered"));
            match &item.documentation {
                Some(Documentation::MarkupContent(markup)) => Some(markup.value.as_str()),
                _ => None,
            }
        };

        // Multi-word keywords document their construct.
        assert!(doc_of("GROUP BY").is_some_and(|doc| doc.contains("GROUP BY")));
        assert!(doc_of("count").is_some_and(|doc| doc.contains("counts rows")));
    }

    #[test]
    fn keywords_follow_the_backend_dialect() {
        let labels_for = |kind: DatabaseKind| {
            let document = Document::new(String::new());
            let parsed = ParsedSql::parse(kind.dialect(), document.text());
            completions(&document, &parsed, Position::new(0, 0), &schema(), kind)
                .into_iter()
                .map(|item| item.label)
                .collect::<Vec<_>>()
        };

        let sqlite = labels_for(DatabaseKind::Sqlite);
        assert!(sqlite.contains(&"RETURNING".to_owned()));
        assert!(sqlite.contains(&"AUTOINCREMENT".to_owned()));
        assert!(!sqlite.contains(&"AUTO_INCREMENT".to_owned()));

        let mysql = labels_for(DatabaseKind::MySql);
        assert!(mysql.contains(&"AUTO_INCREMENT".to_owned()));
        assert!(!mysql.contains(&"RETURNING".to_owned()));

        let postgres = labels_for(DatabaseKind::Postgres);
        assert!(postgres.contains(&"ILIKE".to_owned()));
        assert!(postgres.contains(&"RETURNING".to_owned()));
    }
}
