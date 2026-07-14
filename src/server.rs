//! The language server backend.

use dashmap::DashMap;
use tokio::sync::RwLock;
use tower_lsp_server::ls_types::{
    CompletionOptions, CompletionParams, CompletionResponse, DidChangeTextDocumentParams,
    DidChangeWatchedFilesParams, DidChangeWatchedFilesRegistrationOptions,
    DidCloseTextDocumentParams, DidOpenTextDocumentParams, DidSaveTextDocumentParams,
    FileSystemWatcher, GlobPattern, GotoDefinitionParams, GotoDefinitionResponse, Hover,
    HoverParams, HoverProviderCapability, InitializeParams, InitializeResult, InitializedParams,
    MessageType, OneOf, Registration, SemanticTokens, SemanticTokensFullOptions,
    SemanticTokensOptions, SemanticTokensParams, SemanticTokensResult,
    SemanticTokensServerCapabilities, ServerCapabilities, ServerInfo, TextDocumentSyncCapability,
    TextDocumentSyncKind, TextDocumentSyncOptions, TextDocumentSyncSaveOptions, Unregistration,
    Uri, WorkDoneProgressOptions,
};
use tower_lsp_server::{Client, LanguageServer, jsonrpc};

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::analysis::{completion, definition, hover, semantic_tokens};
use crate::db::DatabaseKind;
use crate::document::Document;
use crate::embedded::{self, EmbeddedSql};
use crate::parse::ParsedSql;
use crate::workspace::Workspace;

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
    /// The SQL parse of the current text, keyed by the dialect it was
    /// parsed under (the context kind can change across reloads).
    parsed_sql: Mutex<Option<(DatabaseKind, Arc<ParsedSql>)>>,
    /// The extracted query regions of the current text (Rust documents).
    extracted: Mutex<Option<Arc<EmbeddedSql>>>,
}

impl OpenDocument {
    fn new(document: Document, language: DocumentLanguage) -> OpenDocument {
        OpenDocument {
            document,
            language,
            parsed_sql: Mutex::new(None),
            extracted: Mutex::new(None),
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
    /// per text.
    fn extracted(&self) -> Arc<EmbeddedSql> {
        let mut cache = self.extracted.lock().expect("region cache lock poisoned");
        if let Some(extracted) = &*cache {
            return Arc::clone(extracted);
        }
        let extracted = Arc::new(EmbeddedSql::extract(&self.document));
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

/// The tower-lsp backend serving SQL language features.
pub struct Backend {
    client: Client,
    documents: DashMap<Uri, OpenDocument>,
    workspace: RwLock<Workspace>,
    /// Set once the client rejects dynamic watcher registration, so reloads
    /// stop retrying (the did_save fallback covers those clients).
    watchers_unavailable: AtomicBool,
}

impl Backend {
    /// Creates a backend bound to `client`.
    pub fn new(client: Client) -> Self {
        Backend {
            client,
            documents: DashMap::new(),
            workspace: RwLock::new(Workspace::default()),
            watchers_unavailable: AtomicBool::new(false),
        }
    }

    /// Rebuilds workspace state, forwards the resulting log lines to the
    /// client, and refreshes the file watchers to cover the migration
    /// directories the new state actually consumes.
    async fn reload_workspace(&self) {
        let root = self.workspace.read().await.root.clone();
        let Some(root) = root else {
            self.client
                .log_message(
                    MessageType::WARNING,
                    "no workspace root; schema features are unavailable",
                )
                .await;
            return;
        };

        let (workspace, log) = Workspace::load(root).await;
        *self.workspace.write().await = workspace;
        for (message_type, message) in log {
            self.client.log_message(message_type, message).await;
        }
        self.register_watchers().await;
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
        // `root_uri` is deprecated in the protocol but still the only root
        // older clients send; keep it as the fallback.
        #[allow(deprecated)]
        let root = params
            .workspace_folders
            .as_ref()
            .and_then(|folders| folders.first())
            .map(|folder| folder.uri.clone())
            .or(params.root_uri)
            .and_then(|uri| uri.to_file_path().map(|path| path.into_owned()));
        self.workspace.write().await.root = root;

        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Options(
                    TextDocumentSyncOptions {
                        open_close: Some(true),
                        change: Some(TextDocumentSyncKind::FULL),
                        save: Some(TextDocumentSyncSaveOptions::Supported(true)),
                        ..TextDocumentSyncOptions::default()
                    },
                )),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                definition_provider: Some(OneOf::Left(true)),
                completion_provider: Some(CompletionOptions {
                    trigger_characters: Some(vec![".".to_owned()]),
                    ..CompletionOptions::default()
                }),
                semantic_tokens_provider: Some(
                    SemanticTokensServerCapabilities::SemanticTokensOptions(
                        SemanticTokensOptions {
                            work_done_progress_options: WorkDoneProgressOptions::default(),
                            legend: semantic_tokens::legend(),
                            range: None,
                            full: Some(SemanticTokensFullOptions::Bool(true)),
                        },
                    ),
                ),
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
        self.reload_workspace().await;
    }

    async fn shutdown(&self) -> jsonrpc::Result<()> {
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let document = params.text_document;
        let language = DocumentLanguage::detect(Some(&document.language_id), &document.uri);
        self.documents.insert(
            document.uri,
            OpenDocument::new(Document::new(document.text), language),
        );
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        // Full sync: the last change carries the complete text.
        let Some(change) = params.content_changes.into_iter().next_back() else {
            return;
        };
        let uri = params.text_document.uri;
        match self.documents.get_mut(&uri) {
            Some(mut open) => {
                open.document.update(change.text);
                open.invalidate();
            }
            None => {
                let language = DocumentLanguage::detect(None, &uri);
                self.documents
                    .insert(uri, OpenDocument::new(Document::new(change.text), language));
            }
        }
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        if self.affects_workspace(&params.text_document.uri).await {
            self.reload_workspace().await;
        }
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        self.documents.remove(&params.text_document.uri);
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
        let data = match open.language {
            DocumentLanguage::Sql => {
                let parsed = open.parsed(kind);
                semantic_tokens::semantic_tokens(&open.document, &parsed)
            }
            DocumentLanguage::Rust => {
                let extracted = open.extracted();
                embedded::embedded_semantic_tokens(&extracted, kind)
            }
        };
        Ok(Some(SemanticTokensResult::Tokens(SemanticTokens {
            result_id: None,
            data,
        })))
    }

    async fn did_change_watched_files(&self, params: DidChangeWatchedFilesParams) {
        for change in &params.changes {
            if self.affects_workspace(&change.uri).await {
                self.reload_workspace().await;
                return;
            }
        }
    }
}
