//! The language server backend.

use dashmap::DashMap;
use sqlparser::keywords::{
    ALL_KEYWORDS, ALL_KEYWORDS_INDEX, RESERVED_FOR_COLUMN_ALIAS, RESERVED_FOR_TABLE_ALIAS,
};
use tokio::sync::{Notify, RwLock};
use tower_lsp_server::ls_types::{
    CodeActionOrCommand, CodeActionParams, CodeActionProviderCapability, CodeActionResponse,
    CompletionOptions, CompletionParams, CompletionResponse, Diagnostic, DiagnosticOptions,
    DiagnosticServerCapabilities, DidChangeTextDocumentParams, DidChangeWatchedFilesParams,
    DidChangeWatchedFilesRegistrationOptions, DidChangeWorkspaceFoldersParams,
    DidCloseTextDocumentParams, DidOpenTextDocumentParams, DidSaveTextDocumentParams,
    DocumentChanges, DocumentDiagnosticParams, DocumentDiagnosticReport,
    DocumentDiagnosticReportResult, DocumentHighlight, DocumentHighlightKind,
    DocumentHighlightParams, DocumentSymbolParams, DocumentSymbolResponse, FileSystemWatcher,
    FullDocumentDiagnosticReport, GlobPattern, GotoDefinitionParams, GotoDefinitionResponse, Hover,
    HoverParams, HoverProviderCapability, InitializeParams, InitializeResult, InitializedParams,
    Location, MessageType, OneOf, OptionalVersionedTextDocumentIdentifier, Position,
    PrepareRenameResponse, ProgressToken, Range, ReferenceParams, Registration,
    RelatedFullDocumentDiagnosticReport, RenameOptions, RenameParams, SemanticToken,
    SemanticTokens, SemanticTokensDelta, SemanticTokensDeltaParams, SemanticTokensFullDeltaResult,
    SemanticTokensFullOptions, SemanticTokensOptions, SemanticTokensParams,
    SemanticTokensRangeParams, SemanticTokensRangeResult, SemanticTokensResult,
    SemanticTokensServerCapabilities, ServerCapabilities, ServerInfo, SymbolInformation,
    SymbolKind, TextDocumentEdit, TextDocumentPositionParams, TextDocumentSyncCapability,
    TextDocumentSyncKind, TextDocumentSyncOptions, TextDocumentSyncSaveOptions, TextEdit,
    Unregistration, Uri, WorkDoneProgressOptions, WorkspaceEdit,
    WorkspaceFoldersServerCapabilities, WorkspaceServerCapabilities, WorkspaceSymbolParams,
    WorkspaceSymbolResponse,
};
use tower_lsp_server::{Client, LanguageServer, jsonrpc};

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::analysis::resolve::{self, ReferenceTarget, Resolved};
use crate::analysis::{
    actions, completion, definition, diagnostics, hover, semantic_tokens, symbols,
};
use crate::db::DatabaseKind;
use crate::document::Document;
use crate::embedded::{self, EmbeddedSql};
use crate::parse::ParsedSql;
use crate::schema::TableOrigin;
use crate::workspace::{self, DbContext, Workspace};

/// Whether `name` is usable as a bare SQL identifier: a letter or
/// underscore followed by letters, digits, or underscores.
fn is_valid_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Whether `name` is a reserved SQL word, per sqlparser's core reserved
/// sets (the keywords unusable as table or column aliases) — words like
/// `SELECT` or `ORDER` that every supported backend reserves. Non-reserved
/// keywords (`text`, `name`, ...) remain usable as identifiers.
fn is_reserved_word(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    let Ok(index) = ALL_KEYWORDS.binary_search(&upper.as_str()) else {
        return false;
    };
    let keyword = ALL_KEYWORDS_INDEX[index];
    RESERVED_FOR_TABLE_ALIAS.contains(&keyword) || RESERVED_FOR_COLUMN_ALIAS.contains(&keyword)
}

/// How an open document is served: as a SQL file, or as a Rust file whose
/// sqlx query macros embed SQL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DocumentLanguage {
    Sql,
    Rust,
}

impl DocumentLanguage {
    /// Chooses by the client-reported language id, falling back to the file
    /// extension when the id is unavailable (e.g. changes to unopened docs).
    fn detect(language_id: Option<&str>, uri: &Uri) -> DocumentLanguage {
        let is_rust = match language_id {
            Some(id) => id == "rust",
            None => uri.path().as_str().ends_with(".rs"),
        };
        if is_rust {
            DocumentLanguage::Rust
        } else {
            DocumentLanguage::Sql
        }
    }
}

/// An open editor document together with the language it is served as and
/// lazily computed analyses of its current text.
struct OpenDocument {
    document: Document,
    language: DocumentLanguage,
    /// The client's version of the document, echoed in versioned edits.
    version: i32,
    /// The SQL parse of the current text, keyed by the dialect it was
    /// parsed under (the context kind can change across reloads).
    parsed_sql: Mutex<Option<(DatabaseKind, Arc<ParsedSql>)>>,
    /// The extracted query regions of the current text (Rust documents).
    extracted: Mutex<Option<Arc<EmbeddedSql>>>,
    /// The syntax tree of the last extraction, kept aligned with the text
    /// through [`tree_sitter::Tree::edit`] so reparses are incremental.
    tree: Mutex<Option<tree_sitter::Tree>>,
    /// The most recently issued semantic-token stream and its result id,
    /// kept across edits so delta requests can diff against it.
    last_tokens: Mutex<Option<(String, Vec<SemanticToken>)>>,
}

impl OpenDocument {
    fn new(document: Document, language: DocumentLanguage, version: i32) -> OpenDocument {
        OpenDocument {
            document,
            language,
            version,
            parsed_sql: Mutex::new(None),
            extracted: Mutex::new(None),
            tree: Mutex::new(None),
            last_tokens: Mutex::new(None),
        }
    }

    /// Applies one synchronized content change, keeping the cached syntax
    /// tree aligned so the next extraction reparses incrementally.
    fn apply_content_change(&mut self, range: Option<Range>, text: String) {
        match range {
            Some(range) => {
                let edit = self.document.apply_change(range, &text);
                if let Some(tree) = self
                    .tree
                    .get_mut()
                    .expect("tree cache lock poisoned")
                    .as_mut()
                {
                    tree.edit(&embedded::input_edit(edit));
                }
            }
            None => {
                self.document.update(text);
                *self.tree.get_mut().expect("tree cache lock poisoned") = None;
            }
        }
    }

    /// The parse of the current text under `kind`'s dialect, computed at
    /// most once per text-and-dialect combination.
    fn parsed(&self, kind: DatabaseKind) -> Arc<ParsedSql> {
        let mut cache = self.parsed_sql.lock().expect("parse cache lock poisoned");
        if let Some((cached_kind, parsed)) = &*cache
            && *cached_kind == kind
        {
            return Arc::clone(parsed);
        }
        let parsed = Arc::new(ParsedSql::parse(kind.dialect(), self.document.text()));
        *cache = Some((kind, Arc::clone(&parsed)));
        parsed
    }

    /// The extracted SQL regions of the current text, computed at most once
    /// per text, reparsing incrementally from the previous tree when the
    /// text changed through ranged edits.
    fn extracted(&self) -> Arc<EmbeddedSql> {
        let mut cache = self.extracted.lock().expect("region cache lock poisoned");
        if let Some(extracted) = &*cache {
            return Arc::clone(extracted);
        }
        let mut tree = self.tree.lock().expect("tree cache lock poisoned");
        let (extracted, new_tree) = EmbeddedSql::extract_with(&self.document, tree.as_ref());
        *tree = new_tree;
        let extracted = Arc::new(extracted);
        *cache = Some(Arc::clone(&extracted));
        extracted
    }

    /// Drops cached analyses after the text changed.
    fn invalidate(&mut self) {
        *self
            .parsed_sql
            .get_mut()
            .expect("parse cache lock poisoned") = None;
        *self
            .extracted
            .get_mut()
            .expect("region cache lock poisoned") = None;
    }
}

/// The tower-lsp backend serving SQL language features. A thin handle over
/// [`ServerState`] so that the reload worker can share the state.
pub struct Backend {
    state: Arc<ServerState>,
}

impl std::ops::Deref for Backend {
    type Target = ServerState;

    fn deref(&self) -> &ServerState {
        &self.state
    }
}

/// The server's shared state: documents, workspace contexts, and the
/// machinery that rebuilds them.
pub struct ServerState {
    client: Client,
    documents: DashMap<Uri, OpenDocument>,
    workspace: RwLock<Workspace>,
    /// Set when the client does not support dynamic watcher registration
    /// (or rejected one), so reloads stop attempting it; the did_save
    /// fallback covers those clients.
    watchers_unavailable: AtomicBool,
    /// Wakes the reload worker. Requests coalesce: any number of triggers
    /// while a reload runs result in exactly one follow-up reload.
    reload_notify: Notify,
    /// Issues result ids for semantic-token streams; ids only need to be
    /// unique per session.
    next_tokens_id: AtomicU64,
    /// Whether the client accepts server-initiated work-done progress.
    progress_supported: AtomicBool,
    /// Issues unique progress tokens for reloads.
    next_progress_id: AtomicU64,
    /// Whether the client accepts versioned `documentChanges` in workspace
    /// edits.
    document_changes_supported: AtomicBool,
    /// Whether the client pulls diagnostics (`textDocument/diagnostic`);
    /// pushes are suppressed then, so documents are not reported twice.
    pull_diagnostics_supported: AtomicBool,
}

impl Backend {
    /// Creates a backend bound to `client` and spawns its reload worker.
    pub fn new(client: Client) -> Self {
        let state = Arc::new(ServerState {
            client,
            documents: DashMap::new(),
            workspace: RwLock::new(Workspace::default()),
            watchers_unavailable: AtomicBool::new(false),
            reload_notify: Notify::new(),
            next_tokens_id: AtomicU64::new(0),
            progress_supported: AtomicBool::new(false),
            next_progress_id: AtomicU64::new(0),
            document_changes_supported: AtomicBool::new(false),
            pull_diagnostics_supported: AtomicBool::new(false),
        });

        // Reloads run here rather than in notification handlers: a slow
        // load (cargo metadata, an unresponsive database) must not delay
        // the document synchronization notifications queued behind it.
        let worker = Arc::clone(&state);
        tokio::spawn(async move {
            loop {
                worker.reload_notify.notified().await;
                worker.reload_workspace().await;
            }
        });

        Backend { state }
    }
}

impl ServerState {
    /// Schedules a workspace reload on the worker and returns immediately.
    fn request_reload(&self) {
        self.reload_notify.notify_one();
    }

    /// Computes and publishes diagnostics for one open document.
    async fn publish_diagnostics_for(&self, uri: &Uri) {
        // Clients that pull diagnostics must not also receive pushes for
        // the same documents.
        if self.pull_diagnostics_supported.load(Ordering::Relaxed) {
            return;
        }
        let Some(diagnostics) = self.diagnostics_for(uri).await else {
            return;
        };
        self.client
            .publish_diagnostics(uri.clone(), diagnostics, None)
            .await;
    }

    /// The current diagnostics for `uri`, or `None` when it is not open.
    async fn diagnostics_for(&self, uri: &Uri) -> Option<Vec<Diagnostic>> {
        let open = self.documents.get(uri)?;
        let workspace = self.workspace.read().await;
        let context = workspace.context_for(uri);
        Some(match open.language {
            DocumentLanguage::Sql => {
                let parsed = open.parsed(context.kind);
                diagnostics::diagnostics(&open.document, &parsed, &context.schema)
            }
            DocumentLanguage::Rust => {
                let extracted = open.extracted();
                embedded::diagnostics(&extracted, &context.schema, context.kind)
            }
        })
    }

    /// Republishes diagnostics for every open document, after the schema
    /// contexts changed.
    async fn publish_all_diagnostics(&self) {
        let uris: Vec<Uri> = self
            .documents
            .iter()
            .map(|entry| entry.key().clone())
            .collect();
        for uri in uris {
            self.publish_diagnostics_for(&uri).await;
        }
    }

    /// The reference target at `position` in `uri` and every location that
    /// resolves to it — in the requesting document, other open documents of
    /// the same context, and the context's migration files — ordered and
    /// deduplicated. `include_declaration` controls whether the
    /// schema-recorded defining identifier is part of the result. `None`
    /// when the document is not open or the position holds no resolvable
    /// reference.
    async fn reference_locations(
        &self,
        uri: &Uri,
        position: Position,
        include_declaration: bool,
    ) -> Option<(ReferenceTarget, Vec<Location>)> {
        let workspace = self.workspace.read().await;
        let context = workspace.context_for(uri);
        let (target, own_ranges) = self.own_references(uri, position, context)?;

        let mut locations: Vec<Location> = own_ranges
            .into_iter()
            .map(|range| Location::new(uri.clone(), range))
            .collect();

        // A query-local relation exists only inside the requesting document;
        // schema objects are searched across the context.
        if !target.is_document_local() {
            self.collect_context_references(&workspace, uri, context, &target, &mut locations);
        }

        let declaration = match &target {
            ReferenceTarget::Table { name } => context
                .schema
                .table(name)
                .and_then(|table| table.location.clone()),
            ReferenceTarget::Column { table, column } => {
                context.schema.table(table).and_then(|table| {
                    table
                        .column(column)
                        .and_then(|column| column.location.clone())
                        .or_else(|| table.location.clone())
                })
            }
            ReferenceTarget::LocalTable { .. } | ReferenceTarget::LocalColumn { .. } => None,
        }
        .map(Location::from);
        if include_declaration {
            if let Some(declaration) = declaration
                && !locations.contains(&declaration)
            {
                locations.push(declaration);
            }
        } else if let Some(declaration) = declaration {
            locations.retain(|location| *location != declaration);
        }

        locations.sort_by(|a, b| {
            (a.uri.as_str(), a.range.start.line, a.range.start.character).cmp(&(
                b.uri.as_str(),
                b.range.start.line,
                b.range.start.character,
            ))
        });
        locations.dedup();

        Some((target, locations))
    }

    /// The reference target at `position` in `uri` and the requesting
    /// document's own matching ranges, in host coordinates.
    fn own_references(
        &self,
        uri: &Uri,
        position: Position,
        context: &DbContext,
    ) -> Option<(ReferenceTarget, Vec<Range>)> {
        let open = self.documents.get(uri)?;
        match open.language {
            DocumentLanguage::Sql => {
                let parsed = open.parsed(context.kind);
                let target =
                    resolve::reference_target(&open.document, &parsed, position, &context.schema)?;
                let ranges =
                    resolve::references_to(&open.document, &parsed, &context.schema, &target);
                Some((target, ranges))
            }
            DocumentLanguage::Rust => {
                let extracted = open.extracted();
                embedded::references_at(&extracted, position, &context.schema, context.kind)
            }
        }
    }

    /// Resolves the schema reference at `position` for a rename request,
    /// with its range in host coordinates.
    async fn resolve_for_rename(&self, uri: &Uri, position: Position) -> Option<Resolved> {
        let workspace = self.workspace.read().await;
        let context = workspace.context_for(uri);
        let open = self.documents.get(uri)?;
        match open.language {
            DocumentLanguage::Sql => {
                let parsed = open.parsed(context.kind);
                resolve::resolve_at(&open.document, &parsed, position, &context.schema)
            }
            DocumentLanguage::Rust => {
                let extracted = open.extracted();
                embedded::resolve_at(&extracted, position, &context.schema, context.kind)
            }
        }
    }

    /// Whether the resolved object can be renamed: its definition must live
    /// in workspace sources. Renaming an object known only from live-
    /// database introspection would rewrite queries against a relation the
    /// database still knows by its old name. `Err` carries the reason shown
    /// to the user.
    fn renameable(resolved: &Resolved) -> Result<(), String> {
        let (Resolved::Table { table, .. } | Resolved::Column { table, .. }) = resolved;
        match table.origin {
            TableOrigin::Database => Err(format!(
                "cannot rename `{}`: it is defined only in the live database",
                table.name
            )),
            TableOrigin::Migration | TableOrigin::Query => Ok(()),
        }
    }

    /// Whether renaming `resolved` to `new_name` collides with an existing
    /// schema object: another relation with that name, or another column of
    /// the same relation. Renaming into a collision would silently merge
    /// every reference with the existing object's. Query-local relations are
    /// exempt — shadowing a schema name is legal SQL. `Err` carries the
    /// reason shown to the user.
    fn collision(resolved: &Resolved, new_name: &str, context: &DbContext) -> Result<(), String> {
        match resolved {
            Resolved::Table { table, .. } => {
                if table.origin != TableOrigin::Query
                    && !table.name.eq_ignore_ascii_case(new_name)
                    && context.schema.table(new_name).is_some()
                {
                    return Err(format!("a table or view named `{new_name}` already exists"));
                }
            }
            Resolved::Column { table, column, .. } => {
                if !column.name.eq_ignore_ascii_case(new_name) && table.column(new_name).is_some() {
                    return Err(format!(
                        "`{}` already has a column named `{new_name}`",
                        table.name
                    ));
                }
            }
        }
        Ok(())
    }

    /// Extends `locations` with references to `target` found in every other
    /// open document served by `context` and in the context's migration
    /// files on disk. Migration files that are open are skipped — their
    /// buffer contents were already searched.
    fn collect_context_references(
        &self,
        workspace: &Workspace,
        origin: &Uri,
        context: &DbContext,
        target: &ReferenceTarget,
        locations: &mut Vec<Location>,
    ) {
        let mut open_paths = HashSet::new();
        for entry in self.documents.iter() {
            let entry_uri = entry.key();
            if let Some(path) = entry_uri.to_file_path() {
                open_paths.insert(workspace::normalize(path.into_owned()));
            }
            if entry_uri == origin || !std::ptr::eq(workspace.context_for(entry_uri), context) {
                continue;
            }
            match entry.language {
                DocumentLanguage::Sql => {
                    let parsed = entry.parsed(context.kind);
                    for range in
                        resolve::references_to(&entry.document, &parsed, &context.schema, target)
                    {
                        locations.push(Location::new(entry_uri.clone(), range));
                    }
                }
                DocumentLanguage::Rust => {
                    let extracted = entry.extracted();
                    for range in
                        embedded::references_to(&extracted, &context.schema, context.kind, target)
                    {
                        locations.push(Location::new(entry_uri.clone(), range));
                    }
                }
            }
        }

        for migration in &workspace.migration_documents {
            if !migration.path.starts_with(&context.root)
                || open_paths.contains(&workspace::normalize(migration.path.clone()))
            {
                continue;
            }
            let parsed = migration.parsed(context.kind);
            for range in
                resolve::references_to(&migration.document, &parsed, &context.schema, target)
            {
                locations.push(Location::new(migration.uri.clone(), range));
            }
        }
    }

    /// The full semantic-token stream for `open` under `kind`.
    fn compute_tokens(&self, open: &OpenDocument, kind: DatabaseKind) -> Vec<SemanticToken> {
        match open.language {
            DocumentLanguage::Sql => {
                let parsed = open.parsed(kind);
                semantic_tokens::semantic_tokens(&open.document, &parsed)
            }
            DocumentLanguage::Rust => {
                let extracted = open.extracted();
                embedded::embedded_semantic_tokens(&extracted, kind)
            }
        }
    }

    /// Stores `data` as the document's latest issued token stream and
    /// returns the result id future delta requests will reference.
    fn remember_tokens(&self, open: &OpenDocument, data: Vec<SemanticToken>) -> String {
        let result_id = self
            .next_tokens_id
            .fetch_add(1, Ordering::Relaxed)
            .to_string();
        *open.last_tokens.lock().expect("token cache lock poisoned") =
            Some((result_id.clone(), data));
        result_id
    }

    /// Rebuilds workspace state, forwards the resulting log lines to the
    /// client, and refreshes the file watchers to cover the migration
    /// directories the new state actually consumes.
    async fn reload_workspace(&self) {
        let roots = self.workspace.read().await.roots.clone();
        if roots.is_empty() {
            self.client
                .log_message(
                    MessageType::WARNING,
                    "no workspace folders; schema features are unavailable",
                )
                .await;
            return;
        }

        // Loads can take seconds (cargo metadata, live introspection); show
        // progress when the client supports server-initiated reporting.
        let progress = if self.progress_supported.load(Ordering::Relaxed) {
            let token = ProgressToken::String(format!(
                "sqlx-lsp/reload/{}",
                self.next_progress_id.fetch_add(1, Ordering::Relaxed)
            ));
            match self.client.create_work_done_progress(token.clone()).await {
                Ok(()) => Some(
                    self.client
                        .progress(token, "sqlx-lsp: loading workspace")
                        .with_message(format!("indexing {} folder(s)", roots.len()))
                        .begin()
                        .await,
                ),
                Err(_) => None,
            }
        } else {
            None
        };

        let (workspace, log) = Workspace::load(roots).await;
        let contexts = workspace.contexts.len();
        *self.workspace.write().await = workspace;
        for (message_type, message) in log {
            self.client.log_message(message_type, message).await;
        }
        self.register_watchers().await;
        // The contexts changed, so every open document's diagnostics may
        // have too.
        self.publish_all_diagnostics().await;

        if let Some(progress) = progress {
            progress
                .finish_with_message(format!("{contexts} context(s)"))
                .await;
        }
    }

    /// (Re)registers the watched-file globs: the conventional schema sources
    /// plus a glob per migration directory currently in use, so custom
    /// `migrate!()` targets and `sqlx.toml` overrides reload on change too.
    async fn register_watchers(&self) {
        if self.watchers_unavailable.load(Ordering::Relaxed) {
            return;
        }

        let mut globs = vec![
            "**/migrations/**/*.sql".to_owned(),
            "**/Cargo.toml".to_owned(),
            "**/sqlx.toml".to_owned(),
            "**/.env".to_owned(),
        ];
        for dir in &self.workspace.read().await.migration_dirs {
            globs.push(format!("{}/**/*.sql", dir.display()));
        }
        let watchers = globs
            .into_iter()
            .map(|glob| FileSystemWatcher {
                glob_pattern: GlobPattern::String(glob),
                kind: None,
            })
            .collect();

        // Replace any previous registration under the same id; clients that
        // have nothing registered yet simply reject the unregistration.
        let _ = self
            .client
            .unregister_capability(vec![Unregistration {
                id: "sqlx-lsp.watched-files".to_owned(),
                method: "workspace/didChangeWatchedFiles".to_owned(),
            }])
            .await;
        let options = DidChangeWatchedFilesRegistrationOptions { watchers };
        let registration = Registration {
            id: "sqlx-lsp.watched-files".to_owned(),
            method: "workspace/didChangeWatchedFiles".to_owned(),
            register_options: serde_json::to_value(options).ok(),
        };
        if let Err(error) = self.client.register_capability(vec![registration]).await {
            self.watchers_unavailable.store(true, Ordering::Relaxed);
            self.client
                .log_message(
                    MessageType::INFO,
                    format!("file watching unavailable ({error}); relying on saves"),
                )
                .await;
        }
    }

    /// Whether a changed file affects workspace state (migrations, manifest,
    /// configuration, or environment) rather than just an open document.
    async fn affects_workspace(&self, uri: &Uri) -> bool {
        let Some(path) = uri.to_file_path() else {
            return false;
        };
        let file_name = path.file_name().and_then(|name| name.to_str());
        if matches!(
            file_name,
            Some("Cargo.toml") | Some(".env") | Some("sqlx.toml")
        ) {
            return true;
        }
        if path.extension().is_none_or(|extension| extension != "sql") {
            return false;
        }
        // Conventional migration directories, plus whichever directories the
        // loaded contexts actually consume (migrate!() targets and sqlx.toml
        // overrides).
        if path
            .components()
            .any(|component| component.as_os_str() == "migrations")
        {
            return true;
        }
        self.workspace
            .read()
            .await
            .migration_dirs
            .iter()
            .any(|dir| path.starts_with(dir))
    }
}

impl LanguageServer for Backend {
    async fn initialize(&self, params: InitializeParams) -> jsonrpc::Result<InitializeResult> {
        let mut roots: Vec<PathBuf> = Vec::new();
        for folder in params.workspace_folders.as_deref().unwrap_or_default() {
            if let Some(path) = folder.uri.to_file_path() {
                let path = path.into_owned();
                if !roots.contains(&path) {
                    roots.push(path);
                }
            }
        }
        // `root_uri` is deprecated in the protocol but still the only root
        // older clients send; keep it as the fallback.
        #[allow(deprecated)]
        if roots.is_empty()
            && let Some(uri) = &params.root_uri
            && let Some(path) = uri.to_file_path()
        {
            roots.push(path.into_owned());
        }
        self.workspace.write().await.roots = roots;

        // A server must not register capabilities dynamically unless the
        // client opted in; without the opt-in the did_save fallback keeps
        // the schema index fresh instead.
        let watched_files_registration = params
            .capabilities
            .workspace
            .as_ref()
            .and_then(|workspace| workspace.did_change_watched_files.as_ref())
            .and_then(|capability| capability.dynamic_registration)
            .unwrap_or(false);
        self.watchers_unavailable
            .store(!watched_files_registration, Ordering::Relaxed);
        let progress_supported = params
            .capabilities
            .window
            .as_ref()
            .and_then(|window| window.work_done_progress)
            .unwrap_or(false);
        self.progress_supported
            .store(progress_supported, Ordering::Relaxed);
        let document_changes_supported = params
            .capabilities
            .workspace
            .as_ref()
            .and_then(|workspace| workspace.workspace_edit.as_ref())
            .and_then(|edit| edit.document_changes)
            .unwrap_or(false);
        self.document_changes_supported
            .store(document_changes_supported, Ordering::Relaxed);
        let pull_diagnostics_supported = params
            .capabilities
            .text_document
            .as_ref()
            .is_some_and(|text_document| text_document.diagnostic.is_some());
        self.pull_diagnostics_supported
            .store(pull_diagnostics_supported, Ordering::Relaxed);

        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Options(
                    TextDocumentSyncOptions {
                        open_close: Some(true),
                        change: Some(TextDocumentSyncKind::INCREMENTAL),
                        save: Some(TextDocumentSyncSaveOptions::Supported(true)),
                        ..TextDocumentSyncOptions::default()
                    },
                )),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                definition_provider: Some(OneOf::Left(true)),
                references_provider: Some(OneOf::Left(true)),
                document_highlight_provider: Some(OneOf::Left(true)),
                workspace_symbol_provider: Some(OneOf::Left(true)),
                document_symbol_provider: Some(OneOf::Left(true)),
                code_action_provider: Some(CodeActionProviderCapability::Simple(true)),
                diagnostic_provider: Some(DiagnosticServerCapabilities::Options(
                    DiagnosticOptions {
                        identifier: Some("sqlx-lsp".to_owned()),
                        // Migration edits change the schema other documents
                        // resolve against.
                        inter_file_dependencies: true,
                        workspace_diagnostics: false,
                        work_done_progress_options: WorkDoneProgressOptions::default(),
                    },
                )),
                rename_provider: Some(OneOf::Right(RenameOptions {
                    prepare_provider: Some(true),
                    work_done_progress_options: WorkDoneProgressOptions::default(),
                })),
                completion_provider: Some(CompletionOptions {
                    trigger_characters: Some(vec![".".to_owned()]),
                    ..CompletionOptions::default()
                }),
                semantic_tokens_provider: Some(
                    SemanticTokensServerCapabilities::SemanticTokensOptions(
                        SemanticTokensOptions {
                            work_done_progress_options: WorkDoneProgressOptions::default(),
                            legend: semantic_tokens::legend(),
                            range: Some(true),
                            full: Some(SemanticTokensFullOptions::Delta { delta: Some(true) }),
                        },
                    ),
                ),
                workspace: Some(WorkspaceServerCapabilities {
                    workspace_folders: Some(WorkspaceFoldersServerCapabilities {
                        supported: Some(true),
                        change_notifications: Some(OneOf::Left(true)),
                    }),
                    file_operations: None,
                }),
                ..ServerCapabilities::default()
            },
            server_info: Some(ServerInfo {
                name: env!("CARGO_PKG_NAME").to_owned(),
                version: Some(env!("CARGO_PKG_VERSION").to_owned()),
            }),
            ..InitializeResult::default()
        })
    }

    async fn initialized(&self, _params: InitializedParams) {
        // Loads the workspace and registers the file watchers derived from
        // it. Clients without dynamic-registration support reject the
        // watchers; the did_save fallback still keeps the index fresh.
        self.request_reload();
    }

    async fn shutdown(&self) -> jsonrpc::Result<()> {
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let document = params.text_document;
        let language = DocumentLanguage::detect(Some(&document.language_id), &document.uri);
        let uri = document.uri.clone();
        self.documents.insert(
            document.uri,
            OpenDocument::new(Document::new(document.text), language, document.version),
        );
        self.publish_diagnostics_for(&uri).await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri;
        match self.documents.get_mut(&uri) {
            Some(mut open) => {
                // Changes apply in order; each range refers to the document
                // state left by the previous change. A change without a
                // range replaces the whole text.
                for change in params.content_changes {
                    open.apply_content_change(change.range, change.text);
                }
                open.version = params.text_document.version;
                open.invalidate();
            }
            None => {
                // An unopened document can only be synchronized from a
                // full-text change.
                let Some(change) = params
                    .content_changes
                    .into_iter()
                    .rfind(|change| change.range.is_none())
                else {
                    return;
                };
                let language = DocumentLanguage::detect(None, &uri);
                self.documents.insert(
                    uri.clone(),
                    OpenDocument::new(
                        Document::new(change.text),
                        language,
                        params.text_document.version,
                    ),
                );
            }
        }
        self.publish_diagnostics_for(&uri).await;
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        if self.affects_workspace(&params.text_document.uri).await {
            self.request_reload();
        }
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        self.documents.remove(&params.text_document.uri);
        // Closed documents keep no diagnostics; pulling clients discard
        // them on close themselves.
        if !self.pull_diagnostics_supported.load(Ordering::Relaxed) {
            self.client
                .publish_diagnostics(params.text_document.uri, Vec::new(), None)
                .await;
        }
    }

    async fn completion(
        &self,
        params: CompletionParams,
    ) -> jsonrpc::Result<Option<CompletionResponse>> {
        let position_params = params.text_document_position;
        let Some(open) = self.documents.get(&position_params.text_document.uri) else {
            return Ok(None);
        };
        let workspace = self.workspace.read().await;
        let context = workspace.context_for(&position_params.text_document.uri);
        let items = match open.language {
            DocumentLanguage::Sql => {
                let parsed = open.parsed(context.kind);
                completion::completions(
                    &open.document,
                    &parsed,
                    position_params.position,
                    &context.schema,
                    context.kind,
                )
            }
            DocumentLanguage::Rust => {
                let extracted = open.extracted();
                embedded::completions(
                    &extracted,
                    position_params.position,
                    &context.schema,
                    context.kind,
                )
            }
        };
        if items.is_empty() {
            return Ok(None);
        }
        Ok(Some(CompletionResponse::Array(items)))
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> jsonrpc::Result<Option<GotoDefinitionResponse>> {
        let position_params = params.text_document_position_params;
        let Some(open) = self.documents.get(&position_params.text_document.uri) else {
            return Ok(None);
        };
        let workspace = self.workspace.read().await;
        let context = workspace.context_for(&position_params.text_document.uri);
        let location = match open.language {
            DocumentLanguage::Sql => {
                let parsed = open.parsed(context.kind);
                definition::definition(
                    &open.document,
                    &parsed,
                    position_params.position,
                    &context.schema,
                )
            }
            DocumentLanguage::Rust => {
                let extracted = open.extracted();
                embedded::definition(
                    &extracted,
                    position_params.position,
                    &context.schema,
                    context.kind,
                )
            }
        };
        Ok(location.map(GotoDefinitionResponse::Scalar))
    }

    async fn references(&self, params: ReferenceParams) -> jsonrpc::Result<Option<Vec<Location>>> {
        let uri = params.text_document_position.text_document.uri;
        let position = params.text_document_position.position;
        let Some((_, locations)) = self
            .reference_locations(&uri, position, params.context.include_declaration)
            .await
        else {
            return Ok(None);
        };
        if locations.is_empty() {
            return Ok(None);
        }
        Ok(Some(locations))
    }

    // `SymbolInformation` carries a deprecated-but-mandatory field.
    #[allow(deprecated)]
    async fn symbol(
        &self,
        params: WorkspaceSymbolParams,
    ) -> jsonrpc::Result<Option<WorkspaceSymbolResponse>> {
        let query = params.query.to_ascii_lowercase();
        let matches = |name: &str| query.is_empty() || name.to_ascii_lowercase().contains(&query);

        let workspace = self.workspace.read().await;
        // Folder contexts merge their members' tables (same locations), so
        // symbols dedup by identity.
        let mut seen = HashSet::new();
        let mut symbols = Vec::new();
        let contexts = workspace
            .contexts
            .iter()
            .chain(workspace.folder_contexts.iter());
        for context in contexts {
            for table in context.schema.tables() {
                if let Some(location) = &table.location
                    && matches(&table.name)
                    && seen.insert((
                        table.name.clone(),
                        None::<String>,
                        location.uri.as_str().to_owned(),
                        location.range.start.line,
                        location.range.start.character,
                    ))
                {
                    symbols.push(SymbolInformation {
                        name: table.name.clone(),
                        kind: match table.kind {
                            crate::schema::TableKind::Table => SymbolKind::CLASS,
                            crate::schema::TableKind::View => SymbolKind::INTERFACE,
                        },
                        tags: None,
                        deprecated: None,
                        location: location.clone().into(),
                        container_name: None,
                    });
                }
                for column in &table.columns {
                    if let Some(location) = &column.location
                        && matches(&column.name)
                        && seen.insert((
                            column.name.clone(),
                            Some(table.name.clone()),
                            location.uri.as_str().to_owned(),
                            location.range.start.line,
                            location.range.start.character,
                        ))
                    {
                        symbols.push(SymbolInformation {
                            name: column.name.clone(),
                            kind: SymbolKind::FIELD,
                            tags: None,
                            deprecated: None,
                            location: location.clone().into(),
                            container_name: Some(table.name.clone()),
                        });
                    }
                }
            }
        }

        if symbols.is_empty() {
            return Ok(None);
        }
        symbols.sort_by(|a, b| {
            (
                &a.name,
                a.location.uri.as_str(),
                a.location.range.start.line,
            )
                .cmp(&(
                    &b.name,
                    b.location.uri.as_str(),
                    b.location.range.start.line,
                ))
        });
        Ok(Some(WorkspaceSymbolResponse::Flat(symbols)))
    }

    async fn diagnostic(
        &self,
        params: DocumentDiagnosticParams,
    ) -> jsonrpc::Result<DocumentDiagnosticReportResult> {
        let items = self
            .diagnostics_for(&params.text_document.uri)
            .await
            .unwrap_or_default();
        Ok(DocumentDiagnosticReportResult::Report(
            DocumentDiagnosticReport::Full(RelatedFullDocumentDiagnosticReport {
                related_documents: None,
                full_document_diagnostic_report: FullDocumentDiagnosticReport {
                    result_id: None,
                    items,
                },
            }),
        ))
    }

    async fn code_action(
        &self,
        params: CodeActionParams,
    ) -> jsonrpc::Result<Option<CodeActionResponse>> {
        let uri = params.text_document.uri;
        let Some(open) = self.documents.get(&uri) else {
            return Ok(None);
        };
        let workspace = self.workspace.read().await;
        let context = workspace.context_for(&uri);
        let actions = match open.language {
            DocumentLanguage::Sql => {
                let parsed = open.parsed(context.kind);
                actions::quick_fixes(&open.document, &parsed, &context.schema, &uri, params.range)
            }
            DocumentLanguage::Rust => {
                let extracted = open.extracted();
                embedded::quick_fixes(
                    &extracted,
                    &context.schema,
                    context.kind,
                    &uri,
                    params.range,
                )
            }
        };
        if actions.is_empty() {
            return Ok(None);
        }
        Ok(Some(
            actions
                .into_iter()
                .map(CodeActionOrCommand::CodeAction)
                .collect(),
        ))
    }

    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> jsonrpc::Result<Option<DocumentSymbolResponse>> {
        let Some(open) = self.documents.get(&params.text_document.uri) else {
            return Ok(None);
        };
        // Rust documents get their outline from rust-analyzer.
        if open.language != DocumentLanguage::Sql {
            return Ok(None);
        }
        let kind = self
            .workspace
            .read()
            .await
            .context_for(&params.text_document.uri)
            .kind;
        let parsed = open.parsed(kind);
        let symbols = symbols::document_symbols(&open.document, &parsed);
        if symbols.is_empty() {
            return Ok(None);
        }
        Ok(Some(DocumentSymbolResponse::Nested(symbols)))
    }

    async fn document_highlight(
        &self,
        params: DocumentHighlightParams,
    ) -> jsonrpc::Result<Option<Vec<DocumentHighlight>>> {
        let uri = params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;
        let workspace = self.workspace.read().await;
        let context = workspace.context_for(&uri);
        let Some((_, ranges)) = self.own_references(&uri, position, context) else {
            return Ok(None);
        };
        if ranges.is_empty() {
            return Ok(None);
        }
        Ok(Some(
            ranges
                .into_iter()
                .map(|range| DocumentHighlight {
                    range,
                    kind: Some(DocumentHighlightKind::TEXT),
                })
                .collect(),
        ))
    }

    async fn prepare_rename(
        &self,
        params: TextDocumentPositionParams,
    ) -> jsonrpc::Result<Option<PrepareRenameResponse>> {
        let Some(resolved) = self
            .resolve_for_rename(&params.text_document.uri, params.position)
            .await
        else {
            return Ok(None);
        };
        if let Err(message) = ServerState::renameable(&resolved) {
            return Err(jsonrpc::Error::invalid_params(message));
        }
        let (Resolved::Table { range, .. } | Resolved::Column { range, .. }) = resolved;
        Ok(Some(PrepareRenameResponse::Range(range)))
    }

    async fn rename(&self, params: RenameParams) -> jsonrpc::Result<Option<WorkspaceEdit>> {
        let uri = params.text_document_position.text_document.uri;
        let position = params.text_document_position.position;
        if !is_valid_identifier(&params.new_name) {
            return Err(jsonrpc::Error::invalid_params(format!(
                "`{}` is not a valid identifier",
                params.new_name
            )));
        }
        let Some(resolved) = self.resolve_for_rename(&uri, position).await else {
            return Ok(None);
        };
        if let Err(message) = ServerState::renameable(&resolved) {
            return Err(jsonrpc::Error::invalid_params(message));
        }
        {
            let workspace = self.workspace.read().await;
            let context = workspace.context_for(&uri);
            if is_reserved_word(&params.new_name) {
                return Err(jsonrpc::Error::invalid_params(format!(
                    "`{}` is a reserved word",
                    params.new_name
                )));
            }
            if let Err(message) = ServerState::collision(&resolved, &params.new_name, context) {
                return Err(jsonrpc::Error::invalid_params(message));
            }
        }

        // The edit set is the full reference set, declaration included —
        // renaming must touch the defining identifier.
        let Some((_, locations)) = self.reference_locations(&uri, position, true).await else {
            return Ok(None);
        };
        if locations.is_empty() {
            return Ok(None);
        }
        // Locations arrive sorted by URI, so adjacent grouping is complete.
        let mut grouped: Vec<(Uri, Vec<TextEdit>)> = Vec::new();
        for location in locations {
            let edit = TextEdit {
                range: location.range,
                new_text: params.new_name.clone(),
            };
            match grouped.last_mut() {
                Some((uri, edits)) if *uri == location.uri => edits.push(edit),
                _ => grouped.push((location.uri, vec![edit])),
            }
        }

        // Versioned document edits let the client refuse a rename computed
        // against a buffer state it has since changed; the plain changes
        // map serves clients without that support.
        if self.document_changes_supported.load(Ordering::Relaxed) {
            let edits = grouped
                .into_iter()
                .map(|(uri, edits)| TextDocumentEdit {
                    text_document: OptionalVersionedTextDocumentIdentifier {
                        version: self.documents.get(&uri).map(|open| open.version),
                        uri,
                    },
                    edits: edits.into_iter().map(OneOf::Left).collect(),
                })
                .collect();
            Ok(Some(WorkspaceEdit {
                document_changes: Some(DocumentChanges::Edits(edits)),
                ..WorkspaceEdit::default()
            }))
        } else {
            Ok(Some(WorkspaceEdit {
                changes: Some(grouped.into_iter().collect()),
                ..WorkspaceEdit::default()
            }))
        }
    }

    async fn hover(&self, params: HoverParams) -> jsonrpc::Result<Option<Hover>> {
        let position_params = params.text_document_position_params;
        let Some(open) = self.documents.get(&position_params.text_document.uri) else {
            return Ok(None);
        };
        let workspace = self.workspace.read().await;
        let context = workspace.context_for(&position_params.text_document.uri);
        Ok(match open.language {
            DocumentLanguage::Sql => {
                let parsed = open.parsed(context.kind);
                hover::hover(
                    &open.document,
                    &parsed,
                    position_params.position,
                    &context.schema,
                )
            }
            DocumentLanguage::Rust => {
                let extracted = open.extracted();
                embedded::hover(
                    &extracted,
                    position_params.position,
                    &context.schema,
                    context.kind,
                )
            }
        })
    }

    async fn semantic_tokens_full(
        &self,
        params: SemanticTokensParams,
    ) -> jsonrpc::Result<Option<SemanticTokensResult>> {
        let Some(open) = self.documents.get(&params.text_document.uri) else {
            return Ok(None);
        };
        let kind = self
            .workspace
            .read()
            .await
            .context_for(&params.text_document.uri)
            .kind;
        let data = self.compute_tokens(&open, kind);
        let result_id = self.remember_tokens(&open, data.clone());
        Ok(Some(SemanticTokensResult::Tokens(SemanticTokens {
            result_id: Some(result_id),
            data,
        })))
    }

    async fn semantic_tokens_full_delta(
        &self,
        params: SemanticTokensDeltaParams,
    ) -> jsonrpc::Result<Option<SemanticTokensFullDeltaResult>> {
        let Some(open) = self.documents.get(&params.text_document.uri) else {
            return Ok(None);
        };
        let kind = self
            .workspace
            .read()
            .await
            .context_for(&params.text_document.uri)
            .kind;
        let data = self.compute_tokens(&open, kind);

        let previous = open
            .last_tokens
            .lock()
            .expect("token cache lock poisoned")
            .take()
            .filter(|(id, _)| *id == params.previous_result_id);
        let Some((_, previous_data)) = previous else {
            // Unknown or stale baseline: fall back to a full stream.
            let result_id = self.remember_tokens(&open, data.clone());
            return Ok(Some(SemanticTokensFullDeltaResult::Tokens(
                SemanticTokens {
                    result_id: Some(result_id),
                    data,
                },
            )));
        };

        let edit = semantic_tokens::token_edit(&previous_data, &data);
        let result_id = self.remember_tokens(&open, data);
        Ok(Some(SemanticTokensFullDeltaResult::TokensDelta(
            SemanticTokensDelta {
                result_id: Some(result_id),
                edits: vec![edit],
            },
        )))
    }

    async fn semantic_tokens_range(
        &self,
        params: SemanticTokensRangeParams,
    ) -> jsonrpc::Result<Option<SemanticTokensRangeResult>> {
        let Some(open) = self.documents.get(&params.text_document.uri) else {
            return Ok(None);
        };
        let kind = self
            .workspace
            .read()
            .await
            .context_for(&params.text_document.uri)
            .kind;
        let mut segments = match open.language {
            DocumentLanguage::Sql => {
                let parsed = open.parsed(kind);
                semantic_tokens::segments(&open.document, &parsed)
            }
            DocumentLanguage::Rust => {
                let extracted = open.extracted();
                embedded::embedded_token_segments(&extracted, kind)
            }
        };
        let range = params.range;
        segments.retain(|segment| {
            (segment.line > range.start.line
                || (segment.line == range.start.line
                    && segment.start + segment.length > range.start.character))
                && (segment.line < range.end.line
                    || (segment.line == range.end.line && segment.start < range.end.character))
        });
        Ok(Some(SemanticTokensRangeResult::Tokens(SemanticTokens {
            result_id: None,
            data: semantic_tokens::encode(segments),
        })))
    }

    async fn did_change_workspace_folders(&self, params: DidChangeWorkspaceFoldersParams) {
        {
            let mut workspace = self.workspace.write().await;
            for removed in &params.event.removed {
                if let Some(path) = removed.uri.to_file_path() {
                    workspace.roots.retain(|root| root != &*path);
                }
            }
            for added in &params.event.added {
                if let Some(path) = added.uri.to_file_path() {
                    let path = path.into_owned();
                    if !workspace.roots.contains(&path) {
                        workspace.roots.push(path);
                    }
                }
            }
        }
        self.request_reload();
    }

    async fn did_change_watched_files(&self, params: DidChangeWatchedFilesParams) {
        for change in &params.changes {
            if self.affects_workspace(&change.uri).await {
                self.request_reload();
                return;
            }
        }
    }
}
