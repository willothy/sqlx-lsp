//! The language server backend.

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
use crate::document::Document;
use crate::embedded;
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
    /// configuration, or environment) rather than just an open document.
    async fn affects_workspace(&self, uri: &Url) -> bool {
        let Ok(path) = uri.to_file_path() else {
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
        let watchers = [
            "**/migrations/**/*.sql",
            "**/Cargo.toml",
            "**/sqlx.toml",
            "**/.env",
        ]
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
                document: Document::new(document.text),
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
        match self.documents.get_mut(&uri) {
            Some(mut open) => open.document.update(change.text),
            None => {
                let language = DocumentLanguage::detect(None, &uri);
                self.documents.insert(
                    uri,
                    OpenDocument {
                        document: Document::new(change.text),
                        language,
                    },
                );
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
            DocumentLanguage::Sql => completion::completions(
                &open.document,
                position_params.position,
                &context.schema,
                context.kind,
            ),
            DocumentLanguage::Rust => embedded::completions(
                &open.document,
                position_params.position,
                &context.schema,
                context.kind,
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
        let context = workspace.context_for(&position_params.text_document.uri);
        let location = match open.language {
            DocumentLanguage::Sql => definition::definition(
                &open.document,
                position_params.position,
                &context.schema,
                context.kind,
            ),
            DocumentLanguage::Rust => embedded::definition(
                &open.document,
                position_params.position,
                &context.schema,
                context.kind,
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
        let context = workspace.context_for(&position_params.text_document.uri);
        Ok(match open.language {
            DocumentLanguage::Sql => hover::hover(
                &open.document,
                position_params.position,
                &context.schema,
                context.kind,
            ),
            DocumentLanguage::Rust => embedded::hover(
                &open.document,
                position_params.position,
                &context.schema,
                context.kind,
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
        let kind = self
            .workspace
            .read()
            .await
            .context_for(&params.text_document.uri)
            .kind;
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
        for change in &params.changes {
            if self.affects_workspace(&change.uri).await {
                self.reload_workspace().await;
                return;
            }
        }
    }
}
