# sqlx LSP for VS Code

VS Code client for [sqlx-lsp](https://github.com/willothy/sqlx-lsp): SQL
completion, hover, goto definition, and semantic highlighting for `.sql`
files in sqlx projects — and inside `sqlx::query!` / `query_as!` /
`query_scalar!` macros in Rust files, alongside rust-analyzer.

## Setup

1. Install the server: download a binary from the project's GitHub releases,
   or `cargo install --git https://github.com/willothy/sqlx-lsp`.
2. Install this extension from a `.vsix`:
   `code --install-extension sqlx-lsp.vsix`.
3. If `sqlx-lsp` is not on your `PATH`, point `sqlx-lsp.serverPath` at the
   binary.

## Notes

- In Rust files, SQL coloring inside the query macros comes from a TextMate
  injection grammar (VS Code consults only one semantic-token provider per
  document, and rust-analyzer claims Rust files). Completion, hover, and
  goto definition inside the macros come from the language server.
- Semantic highlighting in `.sql` files requires
  `"editor.semanticHighlighting.enabled": true` (the default in most themes).

## Packaging

```sh
npm ci
npm run package   # produces sqlx-lsp.vsix
```
