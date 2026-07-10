# sqlx-lsp

A language server for the SQL files in Rust projects that use
[sqlx](https://github.com/launchbadge/sqlx).

The server figures out which database your project targets by asking Cargo
which features are actually resolved for the `sqlx` dependency (so workspace
and transitive feature unification are handled correctly), builds a schema
index from your migrations, and serves editor features against that schema.

## Features

- **Database detection** — reads the resolved features of the `sqlx` package
  via `cargo metadata`. `sqlite`, `postgres`, and `mysql` select the matching
  SQL dialect for parsing; when several are enabled the server prefers
  `postgres` > `mysql` > `sqlite` and logs the ambiguity.
- **Schema index** — replays the `.sql` migrations under `migrations/` in
  sqlx version order (skipping `*.down.sql`), applying `CREATE TABLE`,
  `CREATE VIEW`, `ALTER TABLE`, and `DROP` statements. Definitions keep their
  source locations. For SQLite, if `DATABASE_URL` (from the environment or
  `.env`) points at an existing database file, the server also introspects it
  read-only and fills in any relations the migrations don't cover.
- **Completion** — context-aware: tables after `FROM`/`JOIN`/`INTO`/`UPDATE`,
  columns of the qualified relation after `alias.` or `table.`, and in-scope
  columns plus tables, keywords, and common functions elsewhere. Works on
  syntactically incomplete statements.
- **Hover** — reconstructed `CREATE`-statement summaries for tables and
  views, SQL signatures for columns, with the defining migration named.
- **Goto definition** — jumps from a table, alias, or column reference to the
  defining statement in the migration that created it.
- **Semantic tokens** — full-document highlighting with a lexical base layer
  (keywords, literals, comments, operators, placeholders, type names) and an
  AST overlay that classifies tables, columns, aliases, and function names —
  including identifiers whose names collide with keywords.

The schema index reloads automatically when migrations, `Cargo.toml`, or
`.env` change (via client file watching, with a save-based fallback).

SQLite is the primary, fully supported backend today. PostgreSQL and MySQL
projects get dialect-correct parsing and migration-based schema features;
live introspection for them is not implemented yet.

## Installation

```sh
cargo install --path .
```

## Editor setup

The server speaks LSP over stdio. Point your editor's LSP client at the
`sqlx-lsp` binary for SQL files, with the Rust project as the workspace root.

Neovim (0.11+):

```lua
vim.lsp.config("sqlx_lsp", {
  cmd = { "sqlx-lsp" },
  filetypes = { "sql" },
  root_markers = { "Cargo.toml" },
})
vim.lsp.enable("sqlx_lsp")
```

Logging goes to stderr; set `SQLX_LSP_LOG` (a `tracing` filter, e.g. `debug`)
to adjust verbosity. Schema loading progress is also reported through
`window/logMessage`.

## How detection works

`cargo metadata` resolves the full dependency graph, and the server unions
the enabled features across every `sqlx` node in the resolve graph. This is
the same feature set the sqlx macros compile against, so the dialect matches
what your queries actually run under. If detection fails (not a Rust
workspace, no `sqlx` dependency, no driver feature), the server logs a
warning and defaults to SQLite.

## Development

```sh
cargo test          # unit tests
cargo clippy --all-targets
```

The crate is a thin binary over a library (`src/lib.rs`); the interesting
modules are `db` (backend detection), `schema` (migration replay and the
schema index), `introspect` (read-only SQLite introspection), and
`analysis/*` (the four language features).
