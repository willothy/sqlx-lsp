//! The language server backend.

use std::path::PathBuf;

use dashmap::DashMap;
use tokio::sync::RwLock;
use tower_lsp::lsp_types::{
    CompletionOptions, CompletionParams, CompletionResponse, DidChangeTextDocumentParams,
    DidChangeWatchedFilesParams, DidChangeWatchedFilesRegistrationOptions,
    DidCloseTextDocumentParams, DidOpenTextDocumentParams, DidSaveTextDocumentParams,
    FileSystemWatcher, GlobPattern, GotoDefinitionParams, GotoDefinitionResponse, Hover,
    HoverParams, HoverProviderCapability, InitializeParams, InitializeResult, InitializedParams,
    MessageType, OneOf, Registration, SemanticTokens, SemanticTokensFullOptions,
    SemanticTokensOptions, SemanticTokensParams, SemanticTokensResult,
    SemanticTokensServerCapabilities, ServerCapabilities, ServerInfo, TextDocumentSyncCapability,
    TextDocumentSyncKind, TextDocumentSyncOptions, TextDocumentSyncSaveOptions, Url,
    WorkDoneProgressOptions,
};
use tower_lsp::{Client, LanguageServer, jsonrpc};

use crate::analysis::{completion, definition, hover, semantic_tokens};
use crate::db::{DatabaseKind, Detection};
use crate::document::Document;
use crate::embedded;
use crate::introspect::{self, LiveDatabase};
use crate::schema::Schema;

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
    fn detect(language_id: Option<&str>, uri: &Url) -> DocumentLanguage {
        let is_rust = match language_id {
            Some(id) => id == "rust",
            None => uri.path().ends_with(".rs"),
        };
        if is_rust {
            DocumentLanguage::Rust
        } else {
            DocumentLanguage::Sql
        }
    }
}

/// An open editor document together with the language it is served as.
struct OpenDocument {
    document: Document,
    language: DocumentLanguage,
}

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
        let mut member_roots = Vec::new();
        let mut enabled = std::collections::BTreeSet::new();
        let mut kind = match detection {
            Ok(Ok(detection)) => {
                member_roots = detection.member_roots.clone();
                enabled = detection.enabled.clone();
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

        // Editors often use the repository root as the LSP workspace root
        // while migrations and `.env` files live next to the crate manifests
        // below it, so every member crate directory is a search root too.
        let mut search_roots = vec![root.clone()];
        for member in member_roots {
            if !search_roots.contains(&member) {
                search_roots.push(member);
            }
        }

        // The database URL, when present, decides the backend the same way
        // the sqlx macros select a driver: by URL scheme, gated on the
        // driver feature being enabled. Feature priority is only the
        // fallback for workspaces with no URL.
        let database_url =
            introspect::discover_database_url(search_roots.iter().map(PathBuf::as_path));
        if let Some(url) = &database_url
            && let Some(scheme_kind) = DatabaseKind::from_url_scheme(url)
            && scheme_kind != kind
        {
            if enabled.is_empty() || enabled.contains(&scheme_kind) {
                log.push((
                    MessageType::INFO,
                    format!("DATABASE_URL scheme selects {scheme_kind}"),
                ));
                kind = scheme_kind;
            } else {
                log.push((
                    MessageType::WARNING,
                    format!(
                        "DATABASE_URL is a {scheme_kind} URL but the sqlx `{}` feature is not \
                         enabled; staying on {kind}",
                        scheme_kind.feature_name()
                    ),
                ));
            }
        }

        let load_roots = search_roots.clone();
        let schema_result = tokio::task::spawn_blocking(move || {
            let mut schema = Schema::default();
            let mut loaded = Vec::new();
            let mut failed = Vec::new();
            for dir in load_roots {
                let migrations = dir.join("migrations");
                if !migrations.is_dir() {
                    continue;
                }
                match schema.apply_migrations(&migrations, kind) {
                    Ok(()) => loaded.push(migrations),
                    Err(error) => failed.push((migrations, error)),
                }
            }
            (schema, loaded, failed)
        })
        .await;
        let mut schema = match schema_result {
            Ok((schema, loaded, failed)) => {
                for migrations in loaded {
                    log.push((
                        MessageType::INFO,
                        format!("replayed migrations from {}", migrations.display()),
                    ));
                }
                for (migrations, error) in failed {
                    log.push((
                        MessageType::WARNING,
                        format!(
                            "failed to load migrations from {}: {error}",
                            migrations.display()
                        ),
                    ));
                }
                schema
            }
            Err(join_error) => {
                log.push((
                    MessageType::ERROR,
                    format!("migration loading task failed: {join_error}"),
                ));
                Schema::default()
            }
        };

        if let Some(url) = database_url {
            match LiveDatabase::from_url(&url, kind, &root) {
                Ok(database) => match database.introspect().await {
                    Ok(tables) => {
                        log.push((
                            MessageType::INFO,
                            format!(
                                "introspected {} relation(s) from {}",
                                tables.len(),
                                database.describe()
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
    documents: DashMap<Url, OpenDocument>,
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
        let language = DocumentLanguage::detect(Some(&document.language_id), &document.uri);
        self.documents.insert(
            document.uri,
            OpenDocument {
                document: Document::new(document.text, document.version),
                language,
            },
        );
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        // Full sync: the last change carries the complete text.
        let Some(change) = params.content_changes.into_iter().next_back() else {
            return;
        };
        let uri = params.text_document.uri;
        let version = params.text_document.version;
        match self.documents.get_mut(&uri) {
            Some(mut open) => open.document.update(change.text, version),
            None => {
                let language = DocumentLanguage::detect(None, &uri);
                self.documents.insert(
                    uri,
                    OpenDocument {
                        document: Document::new(change.text, version),
                        language,
                    },
                );
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

    async fn completion(
        &self,
        params: CompletionParams,
    ) -> jsonrpc::Result<Option<CompletionResponse>> {
        let position_params = params.text_document_position;
        let Some(open) = self.documents.get(&position_params.text_document.uri) else {
            return Ok(None);
        };
        let workspace = self.workspace.read().await;
        let items = match open.language {
            DocumentLanguage::Sql => completion::completions(
                &open.document,
                position_params.position,
                &workspace.schema,
                workspace.kind,
            ),
            DocumentLanguage::Rust => embedded::completions(
                &open.document,
                position_params.position,
                &workspace.schema,
                workspace.kind,
            ),
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
        let location = match open.language {
            DocumentLanguage::Sql => definition::definition(
                &open.document,
                position_params.position,
                &workspace.schema,
                workspace.kind,
            ),
            DocumentLanguage::Rust => embedded::definition(
                &open.document,
                position_params.position,
                &workspace.schema,
                workspace.kind,
            ),
        };
        Ok(location.map(GotoDefinitionResponse::Scalar))
    }

    async fn hover(&self, params: HoverParams) -> jsonrpc::Result<Option<Hover>> {
        let position_params = params.text_document_position_params;
        let Some(open) = self.documents.get(&position_params.text_document.uri) else {
            return Ok(None);
        };
        let workspace = self.workspace.read().await;
        Ok(match open.language {
            DocumentLanguage::Sql => hover::hover(
                &open.document,
                position_params.position,
                &workspace.schema,
                workspace.kind,
            ),
            DocumentLanguage::Rust => embedded::hover(
                &open.document,
                position_params.position,
                &workspace.schema,
                workspace.kind,
            ),
        })
    }

    async fn semantic_tokens_full(
        &self,
        params: SemanticTokensParams,
    ) -> jsonrpc::Result<Option<SemanticTokensResult>> {
        let Some(open) = self.documents.get(&params.text_document.uri) else {
            return Ok(None);
        };
        let kind = self.workspace.read().await.kind;
        let data = match open.language {
            DocumentLanguage::Sql => semantic_tokens::semantic_tokens(&open.document, kind),
            DocumentLanguage::Rust => embedded::embedded_semantic_tokens(&open.document, kind),
        };
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
