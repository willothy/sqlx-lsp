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
  source locations. If `DATABASE_URL` (from the environment or `.env`) points
  at a reachable database, the server also introspects it and fills in any
  relations the migrations don't cover: SQLite files are opened read-only,
  and PostgreSQL is queried through its system catalogs on a session forced
  to `default_transaction_read_only`, covering every table, view, and
  materialized view visible on the search path. Passwords never appear in
  logs or error messages.
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

- **Rust buffers** — all of the above also work *inside* sqlx's query macros
  in Rust files. Tree-sitter locates the SQL string of `query!`, `query_as!`,
  `query_scalar!` (and their `_unchecked` variants, bare or
  `sqlx::`-qualified), and the features run on the embedded SQL with results
  mapped back to Rust buffer coordinates. Semantic tokens cover only the SQL
  strings, layering cleanly on top of rust-analyzer's highlighting. Raw
  strings (`r#"..."#`) are handled losslessly; plain strings are read
  verbatim, without decoding escape sequences. `query_file!` is intentionally
  not handled here — the referenced `.sql` file is served directly.

The schema index reloads automatically when migrations, `Cargo.toml`, or
`.env` change (via client file watching, with a save-based fallback).

SQLite and PostgreSQL are fully supported, including live introspection.
MySQL projects get dialect-correct parsing and migration-based schema
features; live introspection for MySQL is not implemented yet.

## Installation

Prebuilt binaries for Linux (gnu/musl), macOS, and Windows are attached to
the [GitHub releases](https://github.com/willothy/sqlx-lsp/releases) — the
rolling `nightly` prerelease tracks `main`. Or build from source:

```sh
cargo install --git https://github.com/willothy/sqlx-lsp
```

## Editor setup

The server speaks LSP over stdio. Point your editor's LSP client at the
`sqlx-lsp` binary for SQL files — and for Rust files too if you want
features inside the query macros; the server runs happily alongside
rust-analyzer and only answers for the embedded SQL.

Neovim (0.11+):

```lua
vim.lsp.config("sqlx_lsp", {
  cmd = { "sqlx-lsp" },
  filetypes = { "sql", "rust" },
  root_markers = { "Cargo.toml" },
})
vim.lsp.enable("sqlx_lsp")
```

VS Code: install `sqlx-lsp.vsix` from the
[releases page](https://github.com/willothy/sqlx-lsp/releases) with
`code --install-extension sqlx-lsp.vsix`. The extension lives in
[`editors/vscode`](editors/vscode) and adds a TextMate injection grammar for
SQL coloring inside the query macros (VS Code takes semantic tokens from
only one provider per document, and rust-analyzer claims Rust files). Set
`sqlx-lsp.serverPath` if the binary is not on `PATH`.

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
cargo test          # tests
cargo clippy --all-targets
```

The crate is a thin binary over a library (`src/lib.rs`); the interesting
modules are `db` (backend detection), `schema` (migration replay and the
schema index), `introspect` (read-only SQLite introspection), `analysis/*`
(the four language features), and `embedded` (tree-sitter extraction of SQL
from Rust query macros).
