# sqlx-lsp

A language server for the SQL files in Rust projects that use
[sqlx](https://github.com/launchbadge/sqlx).

The server figures out which database your project targets by asking Cargo
which features are actually resolved for the `sqlx` dependency (so workspace
and transitive feature unification are handled correctly), builds a schema
index from your migrations, and serves editor features against that schema.

## Features

Everything works in `.sql` files and *inside* sqlx's query macros in Rust
files: tree-sitter finds the SQL strings of `query!`, `query_as!`,
`query_scalar!` (and their `_unchecked` variants), and results map back to
Rust buffer coordinates, layering cleanly on top of rust-analyzer.
`query_file!` is served through the referenced `.sql` file instead.

- **Completion** — context-aware: tables after `FROM`/`JOIN`/`INTO`/`UPDATE`,
  a relation's columns after `alias.` or in an `INSERT` column list, and
  in-scope columns, tables, keywords, and functions elsewhere. Works on
  incomplete statements; accepting an item replaces the word being typed.
- **Hover** — `CREATE`-shaped summaries for tables and views, signatures for
  columns (naming the defining migration), and curated documentation for
  keywords and built-in functions.
- **Goto definition** — from any table, alias, or column reference to the
  defining statement in its migration.
- **Find references & document highlight** — every use of a table or column
  across the whole workspace: open buffers, migration files, standalone
  `.sql` files, and the query macros of closed Rust sources. Aliases and
  qualifiers count; CTEs stay scoped to their defining statement.
- **Rename** — tables and columns, rewriting queries and migrations across
  the workspace, closed files included. Validates the new name (reserved
  words, collisions), refuses objects that exist only in the live database,
  and sends versioned edits to clients that support them.
- **Diagnostics** — syntax errors, unknown tables and columns, and
  bind-parameter counts checked against a macro's arguments. Served by push
  and by pull (`textDocument/diagnostic`).
- **Quick fixes** — closest-name suggestions for misspelled tables and
  columns, ranked by edit distance.
- **Semantic tokens** — a lexical base layer plus an AST overlay that
  classifies tables, columns, aliases, and function names; full, delta, and
  range requests.
- **Symbols** — document outline of `CREATE` statements with their columns,
  and workspace-wide search over every known table and column.

Under the hood:

- **Database detection** — `cargo metadata` reports the resolved features of
  the `sqlx` dependency; `sqlite`, `postgres`, and `mysql` select the SQL
  dialect, preferring `postgres` > `mysql` > `sqlite` when several are
  enabled. Details under [How detection works](#how-detection-works).
- **Per-crate contexts** — everything resolves relative to the invoking
  crate, exactly like the sqlx macros: its `sqlx.toml`, its URL variable
  (process environment or ancestor `.env` files), its migrations (including
  `sqlx::migrate!()` targets), its backend. A workspace mixing postgres and
  sqlite crates serves each crate against the right schema and dialect;
  multi-root workspaces and folder changes mid-session are supported;
  `SQLX_OFFLINE=true` disables live introspection per context.
- **Schema index** — replays a crate's migrations in sqlx version order
  and, when `DATABASE_URL` points at a reachable database, fills in the
  relations migrations don't cover by read-only introspection — SQLite,
  PostgreSQL, and MySQL are all supported, and passwords never appear in
  logs. The index reloads automatically when migrations, `Cargo.toml`,
  `sqlx.toml`, or `.env` change (client file watching, with a save-based
  fallback).
- **Protocol** — incremental document synchronization, UTF-8 position
  encoding when the client prefers it, and work-done progress while the
  index loads.

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

`cargo metadata` resolves the full dependency graph (scanning downward for
the manifest when the editor root is a plain monorepo root). Per crate, the
backend is chosen the way the sqlx macros select a driver: the crate's
database URL scheme decides, gated on the driver features its declared sqlx
dependency (or the workspace-unified feature set) enables; without a URL,
the highest-priority enabled driver wins (postgres > mysql > sqlite). If
detection fails entirely (not a Rust workspace, no `sqlx` dependency), the
server logs a warning and defaults to SQLite.

## Development

```sh
cargo test          # tests
cargo clippy --all-targets
```

The crate is a thin binary over a library (`src/lib.rs`); the interesting
modules are `db` (backend detection), `workspace` (per-crate contexts),
`schema/` (migration replay and the schema index), `introspect` (read-only
introspection for all three backends), `analysis/*` (the language-feature
implementations), `embedded` (tree-sitter extraction of SQL from Rust query
macros), and `server` (the LSP surface).
