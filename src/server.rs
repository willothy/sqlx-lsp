//! The language server backend.

use std::path::PathBuf;

use dashmap::DashMap;
use tokio::sync::RwLock;
use tower_lsp::lsp_types::{
    DidChangeTextDocumentParams, DidChangeWatchedFilesParams,
    DidChangeWatchedFilesRegistrationOptions, DidCloseTextDocumentParams,
    DidOpenTextDocumentParams, DidSaveTextDocumentParams, FileSystemWatcher, GlobPattern,
    GotoDefinitionParams, GotoDefinitionResponse, Hover, HoverParams, HoverProviderCapability,
    InitializeParams, InitializeResult, InitializedParams, MessageType, OneOf, Registration,
    SemanticTokens, SemanticTokensFullOptions, SemanticTokensOptions, SemanticTokensParams,
    SemanticTokensResult, SemanticTokensServerCapabilities, ServerCapabilities, ServerInfo,
    TextDocumentSyncCapability, TextDocumentSyncKind, TextDocumentSyncOptions,
    TextDocumentSyncSaveOptions, Url, WorkDoneProgressOptions,
};
use tower_lsp::{Client, LanguageServer, jsonrpc};

use crate::analysis::{definition, hover, semantic_tokens};
use crate::db::{DatabaseKind, Detection};
use crate::document::Document;
use crate::introspect::{self, SqliteDatabase};
use crate::schema::Schema;

/// Workspace-level state derived from configuration and schema sources on
/// disk. Rebuilt whenever migrations, `Cargo.toml`, or `.env` change.
pub struct Workspace {
    /// The workspace root, when the client provided one.
    pub root: Option<PathBuf>,
    /// The database backend detected from the `sqlx` dependency features.
    pub kind: DatabaseKind,
    /// The schema index for the workspace's database.
    pub schema: Schema,
}

impl Default for Workspace {
    fn default() -> Self {
        Workspace {
            root: None,
            // SQL parsing needs *a* dialect even before (or without)
            // successful detection; SQLite is the most permissive of the
            // supported set and this server's primary target.
            kind: DatabaseKind::Sqlite,
            schema: Schema::default(),
        }
    }
}

impl Workspace {
    /// Rebuilds the workspace state for `root`: re-detects the database
    /// backend, replays migrations, and (for SQLite) introspects the live
    /// database. Failures degrade to the previous or default state per
    /// component and are reported in the returned log lines.
    async fn load(root: PathBuf) -> (Workspace, Vec<(MessageType, String)>) {
        let mut log = Vec::new();

        let detection_root = root.clone();
        let detection =
            tokio::task::spawn_blocking(move || Detection::detect(&detection_root)).await;
        let kind = match detection {
            Ok(Ok(detection)) => {
                if detection.enabled.len() > 1 {
                    log.push((
                        MessageType::WARNING,
                        format!(
                            "multiple sqlx driver features enabled ({}); using {}",
                            detection
                                .enabled
                                .iter()
                                .map(|kind| kind.feature_name())
                                .collect::<Vec<_>>()
                                .join(", "),
                            detection.kind
                        ),
                    ));
                }
                log.push((
                    MessageType::INFO,
                    format!("detected database backend: {}", detection.kind),
                ));
                detection.kind
            }
            Ok(Err(error)) => {
                log.push((
                    MessageType::WARNING,
                    format!("database detection failed ({error}); defaulting to SQLite"),
                ));
                DatabaseKind::Sqlite
            }
            Err(join_error) => {
                log.push((
                    MessageType::ERROR,
                    format!("database detection task failed: {join_error}"),
                ));
                DatabaseKind::Sqlite
            }
        };

        let migrations_dir = root.join("migrations");
        let schema_result =
            tokio::task::spawn_blocking(move || Schema::load_migrations(&migrations_dir, kind))
                .await;
        let mut schema = match schema_result {
            Ok(Ok(schema)) => schema,
            Ok(Err(error)) => {
                log.push((
                    MessageType::WARNING,
                    format!("failed to load migrations: {error}"),
                ));
                Schema::default()
            }
            Err(join_error) => {
                log.push((
                    MessageType::ERROR,
                    format!("migration loading task failed: {join_error}"),
                ));
                Schema::default()
            }
        };

        if kind == DatabaseKind::Sqlite
            && let Some(url) = introspect::discover_database_url(&root)
        {
            match SqliteDatabase::from_url(&url, &root) {
                Ok(database) => match database.introspect().await {
                    Ok(tables) => {
                        log.push((
                            MessageType::INFO,
                            format!(
                                "introspected {} relation(s) from {}",
                                tables.len(),
                                database.path().display()
                            ),
                        ));
                        schema.merge_database_tables(tables);
                    }
                    Err(error) => log.push((MessageType::INFO, error.to_string())),
                },
                Err(error) => log.push((MessageType::INFO, error.to_string())),
            }
        }

        log.push((
            MessageType::INFO,
            format!("schema index holds {} relation(s)", schema.tables().count()),
        ));

        (
            Workspace {
                root: Some(root),
                kind,
                schema,
            },
            log,
        )
    }
}

/// The tower-lsp backend serving SQL language features.
pub struct Backend {
    client: Client,
    documents: DashMap<Url, Document>,
    workspace: RwLock<Workspace>,
}

impl Backend {
    /// Creates a backend bound to `client`.
    pub fn new(client: Client) -> Self {
        Backend {
            client,
            documents: DashMap::new(),
            workspace: RwLock::new(Workspace::default()),
        }
    }

    /// Rebuilds workspace state and forwards the resulting log lines to the
    /// client.
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
    }

    /// Whether a changed file affects workspace state (migrations, manifest,
    /// or environment) rather than just an open document.
    fn affects_workspace(uri: &Url) -> bool {
        let Ok(path) = uri.to_file_path() else {
            return false;
        };
        let is_migration = path.extension().is_some_and(|ext| ext == "sql")
            && path
                .components()
                .any(|component| component.as_os_str() == "migrations");
        let file_name = path.file_name().and_then(|name| name.to_str());
        is_migration || matches!(file_name, Some("Cargo.toml") | Some(".env"))
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, params: InitializeParams) -> jsonrpc::Result<InitializeResult> {
        let root = params
            .workspace_folders
            .as_ref()
            .and_then(|folders| folders.first())
            .map(|folder| folder.uri.clone())
            .or(params.root_uri)
            .and_then(|uri| uri.to_file_path().ok());
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
        })
    }

    async fn initialized(&self, _params: InitializedParams) {
        self.reload_workspace().await;

        // Watch the files the schema index is derived from. Clients without
        // dynamic-registration support reject this; the did_save fallback
        // still keeps the index fresh for files edited in the editor.
        let watchers = ["**/migrations/**/*.sql", "**/Cargo.toml", "**/.env"]
            .into_iter()
            .map(|glob| FileSystemWatcher {
                glob_pattern: GlobPattern::String(glob.to_owned()),
                kind: None,
            })
            .collect();
        let options = DidChangeWatchedFilesRegistrationOptions { watchers };
        let registration = Registration {
            id: "sqlx-lsp.watched-files".to_owned(),
            method: "workspace/didChangeWatchedFiles".to_owned(),
            register_options: serde_json::to_value(options).ok(),
        };
        if let Err(error) = self.client.register_capability(vec![registration]).await {
            self.client
                .log_message(
                    MessageType::INFO,
                    format!("file watching unavailable ({error}); relying on saves"),
                )
                .await;
        }
    }

    async fn shutdown(&self) -> jsonrpc::Result<()> {
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let document = params.text_document;
        self.documents
            .insert(document.uri, Document::new(document.text, document.version));
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        // Full sync: the last change carries the complete text.
        let Some(change) = params.content_changes.into_iter().next_back() else {
            return;
        };
        let uri = params.text_document.uri;
        let version = params.text_document.version;
        match self.documents.get_mut(&uri) {
            Some(mut document) => document.update(change.text, version),
            None => {
                self.documents
                    .insert(uri, Document::new(change.text, version));
            }
        }
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        if Self::affects_workspace(&params.text_document.uri) {
            self.reload_workspace().await;
        }
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        self.documents.remove(&params.text_document.uri);
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> jsonrpc::Result<Option<GotoDefinitionResponse>> {
        let position_params = params.text_document_position_params;
        let Some(document) = self.documents.get(&position_params.text_document.uri) else {
            return Ok(None);
        };
        let workspace = self.workspace.read().await;
        Ok(definition::definition(
            &document,
            position_params.position,
            &workspace.schema,
            workspace.kind,
        )
        .map(GotoDefinitionResponse::Scalar))
    }

    async fn hover(&self, params: HoverParams) -> jsonrpc::Result<Option<Hover>> {
        let position_params = params.text_document_position_params;
        let Some(document) = self.documents.get(&position_params.text_document.uri) else {
            return Ok(None);
        };
        let workspace = self.workspace.read().await;
        Ok(hover::hover(
            &document,
            position_params.position,
            &workspace.schema,
            workspace.kind,
        ))
    }

    async fn semantic_tokens_full(
        &self,
        params: SemanticTokensParams,
    ) -> jsonrpc::Result<Option<SemanticTokensResult>> {
        let Some(document) = self.documents.get(&params.text_document.uri) else {
            return Ok(None);
        };
        let kind = self.workspace.read().await.kind;
        let data = semantic_tokens::semantic_tokens(&document, kind);
        Ok(Some(SemanticTokensResult::Tokens(SemanticTokens {
            result_id: None,
            data,
        })))
    }

    async fn did_change_watched_files(&self, params: DidChangeWatchedFilesParams) {
        if params
            .changes
            .iter()
            .any(|change| Self::affects_workspace(&change.uri))
        {
            self.reload_workspace().await;
        }
    }
}
