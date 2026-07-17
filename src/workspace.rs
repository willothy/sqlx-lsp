//! Per-crate database contexts.
//!
//! sqlx's compile-time machinery is anchored at the invoking crate: its
//! `sqlx.toml`, its environment (URL variable and ancestor `.env` files),
//! and its migrations all resolve relative to `CARGO_MANIFEST_DIR`. The
//! workspace therefore holds one database context per sqlx-dependent member
//! crate, across every workspace folder, and every document is served by
//! the context of the crate that contains it. Documents outside any crate
//! are served by their folder's context (root-level configuration plus the
//! folder's crates merged); documents outside every folder fall back to a
//! workspace-wide merged view.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use tower_lsp_server::ls_types::{MessageType, Uri};

use crate::config::SqlxConfig;
use crate::db::{DatabaseKind, Detection, SqlxMember};
use crate::document::Document;
use crate::embedded::{self, MigrateSource};
use crate::introspect::{self, LiveDatabase};
use crate::parse::ParsedSql;
use crate::schema::Schema;

/// Log lines produced while loading, forwarded to the LSP client.
pub type LoadLog = Vec<(MessageType, String)>;

/// The database context one crate's SQL is served against.
#[derive(Debug)]
pub struct DbContext {
    /// The crate directory this context belongs to (the workspace root for
    /// the fallback context).
    pub root: PathBuf,
    /// The backend the crate's queries target.
    pub kind: DatabaseKind,
    /// The schema index built from the crate's migrations and, when
    /// reachable, its live database.
    pub schema: Schema,
}

/// Workspace-level state derived from configuration and schema sources on
/// disk. Rebuilt whenever migrations, manifests, `sqlx.toml`, `.env`, or the
/// set of workspace folders change.
#[derive(Debug)]
pub struct Workspace {
    /// The workspace folder roots the client provided, in the order given.
    pub roots: Vec<PathBuf>,
    /// One context per sqlx-dependent member crate, across every folder.
    pub contexts: Vec<DbContext>,
    /// One context per workspace folder: its root-level configuration and
    /// migrations, with every member context of that folder merged in
    /// (later crates win name collisions). Serves the folder's documents
    /// that belong to no member crate.
    pub folder_contexts: Vec<DbContext>,
    /// Serves documents outside every folder. Its schema merges every
    /// folder context, so detached SQL still resolves best-effort.
    pub fallback: DbContext,
    /// Every migrations directory the schema indexes were built from, for
    /// save-triggered reloads.
    pub migration_dirs: Vec<PathBuf>,
    /// Every migration file's contents, read once at load time so reference
    /// searches need no request-time disk access. Reloads rebuild the list.
    pub migration_documents: Vec<SqlDocument>,
    /// Every query source in the workspace — standalone `.sql` files and
    /// Rust sources whose macros embed SQL — so reference searches and
    /// rename cover closed files. Rebuilt on reloads and on query-source
    /// saves.
    pub query_documents: Vec<QueryDocument>,
}

/// A workspace file holding queries outside the migrations.
#[derive(Debug)]
pub enum QueryDocument {
    /// A standalone `.sql` file served as one SQL document.
    Sql(SqlDocument),
    /// A Rust source, reduced to its extracted query regions.
    Rust(RustQueryDocument),
}

/// The query regions of one Rust source on disk.
#[derive(Debug)]
pub struct RustQueryDocument {
    /// Absolute path of the file.
    pub path: PathBuf,
    /// The path as a file URI.
    pub uri: Uri,
    /// The extracted SQL regions, in host-document coordinates.
    pub extracted: embedded::EmbeddedSql,
}

impl QueryDocument {
    /// The file's path.
    pub fn path(&self) -> &Path {
        match self {
            QueryDocument::Sql(sql) => &sql.path,
            QueryDocument::Rust(rust) => &rust.path,
        }
    }

    /// Scans the workspace `roots` for query sources: `.sql` files outside
    /// the migration directories (those are cached separately) and Rust
    /// sources containing query macros. Unreadable files and Rust sources
    /// without queries are dropped; the next scan retries them.
    pub(crate) fn scan(roots: &[PathBuf], migration_dirs: &[PathBuf]) -> Vec<QueryDocument> {
        let mut documents = Vec::new();
        let mut seen = BTreeSet::new();
        for root in roots {
            for path in query_source_candidates(&normalize(root.clone())) {
                if !seen.insert(path.clone()) {
                    continue;
                }
                let is_sql = path.extension().is_some_and(|extension| extension == "sql");
                if is_sql && migration_dirs.iter().any(|dir| path.starts_with(dir)) {
                    continue;
                }
                let Some(uri) = Uri::from_file_path(&path) else {
                    continue;
                };
                let Ok(text) = std::fs::read_to_string(&path) else {
                    continue;
                };
                if is_sql {
                    documents.push(QueryDocument::Sql(SqlDocument {
                        path,
                        uri,
                        document: Document::new(text),
                        parsed: Mutex::new(None),
                    }));
                } else {
                    // Cheap pre-filter before spending a tree-sitter parse.
                    if !text.contains("query") {
                        continue;
                    }
                    let extracted = embedded::EmbeddedSql::extract(&Document::new(text));
                    if extracted.regions.is_empty() {
                        continue;
                    }
                    documents.push(QueryDocument::Rust(RustQueryDocument {
                        path,
                        uri,
                        extracted,
                    }));
                }
            }
        }
        documents.sort_by(|a, b| a.path().cmp(b.path()));
        documents
    }
}

/// One `.sql` file, read and cached when the workspace loads.
#[derive(Debug)]
pub struct SqlDocument {
    /// Absolute path of the file, as scanned from its directory.
    pub path: PathBuf,
    /// The path as a file URI.
    pub uri: Uri,
    /// The file's contents.
    pub document: Document,
    /// The SQL parse of the contents, keyed by the dialect it was parsed
    /// under (contexts of different backends can share one file through
    /// merged views).
    parsed: Mutex<Option<(DatabaseKind, Arc<ParsedSql>)>>,
}

impl SqlDocument {
    /// The parse of the contents under `kind`'s dialect, computed at most
    /// once per dialect for the lifetime of this workspace load.
    pub fn parsed(&self, kind: DatabaseKind) -> Arc<ParsedSql> {
        let mut cache = self.parsed.lock().expect("parse cache lock poisoned");
        if let Some((cached_kind, parsed)) = &*cache
            && *cached_kind == kind
        {
            return Arc::clone(parsed);
        }
        let parsed = Arc::new(ParsedSql::parse(kind.dialect(), self.document.text()));
        *cache = Some((kind, Arc::clone(&parsed)));
        parsed
    }

    /// Reads every `.sql` file under `dirs`, down-migrations included —
    /// they reference schema objects too. Unreadable files are skipped;
    /// the next reload retries them.
    fn scan(dirs: &[PathBuf]) -> Vec<SqlDocument> {
        let mut documents = Vec::new();
        for dir in dirs {
            let Ok(entries) = std::fs::read_dir(dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().is_none_or(|extension| extension != "sql") {
                    continue;
                }
                let Ok(path) = std::path::absolute(&path) else {
                    continue;
                };
                let Some(uri) = Uri::from_file_path(&path) else {
                    continue;
                };
                let Ok(text) = std::fs::read_to_string(&path) else {
                    continue;
                };
                documents.push(SqlDocument {
                    path,
                    uri,
                    document: Document::new(text),
                    parsed: Mutex::new(None),
                });
            }
        }
        documents.sort_by(|a, b| a.path.cmp(&b.path));
        documents.dedup_by(|a, b| a.path == b.path);
        documents
    }
}

impl Default for Workspace {
    fn default() -> Self {
        Workspace {
            roots: Vec::new(),
            contexts: Vec::new(),
            folder_contexts: Vec::new(),
            fallback: DbContext {
                root: PathBuf::new(),
                // SQL parsing needs *a* dialect even before (or without)
                // successful detection; SQLite is the most permissive of the
                // supported set.
                kind: DatabaseKind::Sqlite,
                schema: Schema::default(),
            },
            migration_dirs: Vec::new(),
            migration_documents: Vec::new(),
            query_documents: Vec::new(),
        }
    }
}

impl Workspace {
    /// The context serving `uri`: the deepest context crate containing the
    /// file, or the fallback.
    pub fn context_for(&self, uri: &Uri) -> &DbContext {
        let Some(path) = uri.to_file_path() else {
            return &self.fallback;
        };
        // Context roots come from `cargo metadata` canonicalized; editor
        // URIs may spell the same file through symlinks (`/var` vs
        // `/private/var` on macOS).
        let path = normalize(path.into_owned());
        // Member crates take precedence over their folder, which takes
        // precedence over the workspace-wide fallback.
        Self::deepest_containing(&self.contexts, &path)
            .or_else(|| Self::deepest_containing(&self.folder_contexts, &path))
            .unwrap_or(&self.fallback)
    }

    /// The deepest context whose root contains `path`, if any.
    fn deepest_containing<'a>(contexts: &'a [DbContext], path: &Path) -> Option<&'a DbContext> {
        contexts
            .iter()
            .filter(|context| path.starts_with(&context.root))
            .max_by_key(|context| context.root.components().count())
    }

    /// Rebuilds the workspace state for `roots` (the client's workspace
    /// folders): re-detects the sqlx member crates of every folder and
    /// builds a database context for each crate and each folder. Failures
    /// degrade per component and are reported in the returned log lines.
    pub async fn load(roots: Vec<PathBuf>) -> (Workspace, LoadLog) {
        let mut log = Vec::new();
        let mut contexts = Vec::new();
        let mut folder_contexts = Vec::new();
        let mut migration_dirs = Vec::new();

        for root in &roots {
            // Folder roots must compare against normalized request paths in
            // `context_for`, so resolve symlinks the same way here.
            let (members, folder, dirs) =
                Self::load_folder(normalize(root.clone()), &mut log).await;
            contexts.extend(members);
            folder_contexts.push(folder);
            migration_dirs.extend(dirs);
        }

        // The fallback serves documents outside every folder; merging the
        // folder views gives detached SQL a best-effort schema.
        let mut fallback_schema = Schema::default();
        for folder in &folder_contexts {
            for table in folder.schema.tables() {
                fallback_schema.insert_table(table.clone());
            }
        }
        let fallback_kind = folder_contexts
            .first()
            .map(|folder| folder.kind)
            .unwrap_or(DatabaseKind::Sqlite);

        log.push((
            MessageType::INFO,
            format!(
                "{} context(s) across {} folder(s); workspace-wide index holds {} relation(s)",
                contexts.len(),
                folder_contexts.len(),
                fallback_schema.tables().count()
            ),
        ));

        let query_documents = QueryDocument::scan(&roots, &migration_dirs);
        (
            Workspace {
                roots,
                contexts,
                folder_contexts,
                fallback: DbContext {
                    root: PathBuf::new(),
                    kind: fallback_kind,
                    schema: fallback_schema,
                },
                migration_documents: SqlDocument::scan(&migration_dirs),
                query_documents,
                migration_dirs,
            },
            log,
        )
    }

    /// Builds the contexts of one workspace folder: one per sqlx member
    /// crate, plus the folder context serving everything else under it.
    /// Returns the member contexts, the folder context, and the migration
    /// directories loaded.
    async fn load_folder(
        root: PathBuf,
        log: &mut LoadLog,
    ) -> (Vec<DbContext>, DbContext, Vec<PathBuf>) {
        let detection_root = root.clone();
        let detection =
            tokio::task::spawn_blocking(move || Detection::detect(&detection_root)).await;
        let (global_kind, enabled, sqlx_members) = match detection {
            Ok(Ok(detection)) => {
                log.push((
                    MessageType::INFO,
                    format!(
                        "workspace default backend: {} ({} sqlx crate(s))",
                        detection.kind,
                        detection.sqlx_members.len()
                    ),
                ));
                (detection.kind, detection.enabled, detection.sqlx_members)
            }
            Ok(Err(error)) => {
                log.push((
                    MessageType::WARNING,
                    format!("database detection failed ({error}); defaulting to SQLite"),
                ));
                (DatabaseKind::Sqlite, BTreeSet::new(), Vec::new())
            }
            Err(join_error) => {
                log.push((
                    MessageType::ERROR,
                    format!("database detection task failed: {join_error}"),
                ));
                (DatabaseKind::Sqlite, BTreeSet::new(), Vec::new())
            }
        };

        let mut contexts = Vec::new();
        let mut migration_dirs = Vec::new();
        for member in sqlx_members {
            let (context, dirs) = DbContext::load(member, global_kind, enabled.clone(), log).await;
            migration_dirs.extend(dirs);
            contexts.push(context);
        }

        // The folder context: root-level configuration and environment,
        // with every member context's schema merged in.
        let fallback_root = root.clone();
        let fallback_enabled = enabled.clone();
        let fallback = tokio::task::spawn_blocking(move || {
            let mut notes = LoadLog::new();
            let config = SqlxConfig::load(&fallback_root).unwrap_or_else(|error| {
                notes.push((MessageType::WARNING, error.to_string()));
                SqlxConfig::default()
            });
            let env = introspect::discover_macro_env(&fallback_root, config.database_url_var());
            let kind = resolve_kind(
                env.database_url.as_deref(),
                &BTreeSet::new(),
                global_kind,
                &fallback_enabled,
                &mut notes,
            );
            let mut schema = Schema::default();
            let migrations = config.migrations_dir(&fallback_root);
            let mut dir = None;
            if migrations.is_dir()
                && let Err(error) = schema.apply_migrations(&migrations, kind)
            {
                notes.push((
                    MessageType::WARNING,
                    format!(
                        "failed to load migrations from {}: {error}",
                        migrations.display()
                    ),
                ));
            } else if migrations.is_dir() {
                dir = Some(migrations);
            }
            let migrations_table = config.migrations_table().to_owned();
            (env, kind, schema, dir, migrations_table, notes)
        })
        .await;

        let (
            folder_env,
            folder_kind,
            mut folder_schema,
            folder_dir,
            folder_migrations_table,
            notes,
        ) = match fallback {
            Ok(parts) => parts,
            Err(join_error) => {
                log.push((
                    MessageType::ERROR,
                    format!("workspace loading task failed: {join_error}"),
                ));
                (
                    introspect::MacroEnv::default(),
                    global_kind,
                    Schema::default(),
                    None,
                    "_sqlx_migrations".to_owned(),
                    LoadLog::new(),
                )
            }
        };
        log.extend(notes);
        if let Some(dir) = folder_dir
            && !migration_dirs.contains(&dir)
        {
            migration_dirs.push(dir);
        }

        // Only introspect at the folder level when no member context exists
        // (a plain directory of SQL, or detection failed); contexts
        // otherwise carry their own introspected schemas into the merge.
        if contexts.is_empty()
            && !folder_env.offline
            && let Some(url) = &folder_env.database_url
        {
            introspect_into(
                &mut folder_schema,
                url,
                folder_kind,
                &root,
                &folder_migrations_table,
                log,
            )
            .await;
        }

        for context in &contexts {
            for table in context.schema.tables() {
                folder_schema.insert_table(table.clone());
            }
        }

        log.push((
            MessageType::INFO,
            format!(
                "folder {}: {} member context(s), {} relation(s)",
                root.display(),
                contexts.len(),
                folder_schema.tables().count()
            ),
        ));

        (
            contexts,
            DbContext {
                root,
                kind: folder_kind,
                schema: folder_schema,
            },
            migration_dirs,
        )
    }
}

impl DbContext {
    /// Builds the context for one sqlx-dependent member crate: reads its
    /// `sqlx.toml` and environment, resolves the backend, replays the
    /// migrations it consumes, and introspects its live database when
    /// reachable. Returns the context and the migration directories it
    /// loaded.
    async fn load(
        member: SqlxMember,
        global_kind: DatabaseKind,
        enabled: BTreeSet<DatabaseKind>,
        log: &mut LoadLog,
    ) -> (DbContext, Vec<PathBuf>) {
        let root = member.root.clone();

        let blocking_root = root.clone();
        let loaded = tokio::task::spawn_blocking(move || {
            let mut notes = LoadLog::new();
            let config = SqlxConfig::load(&blocking_root).unwrap_or_else(|error| {
                notes.push((MessageType::WARNING, error.to_string()));
                SqlxConfig::default()
            });
            let env = introspect::discover_macro_env(&blocking_root, config.database_url_var());
            let kind = resolve_kind(
                env.database_url.as_deref(),
                &member.drivers,
                global_kind,
                &enabled,
                &mut notes,
            );

            let mut schema = Schema::default();
            let mut applied = Vec::new();
            for dir in migration_dirs_for(&blocking_root, &config, &mut notes) {
                if !dir.is_dir() {
                    continue;
                }
                match schema.apply_migrations(&dir, kind) {
                    Ok(()) => applied.push(dir),
                    Err(error) => notes.push((
                        MessageType::WARNING,
                        format!("failed to load migrations from {}: {error}", dir.display()),
                    )),
                }
            }
            let migrations_table = config.migrations_table().to_owned();
            (env, kind, schema, applied, migrations_table, notes)
        })
        .await;

        let (env, kind, mut schema, applied, migrations_table, notes) = match loaded {
            Ok(parts) => parts,
            Err(join_error) => {
                log.push((
                    MessageType::ERROR,
                    format!("context loading task failed: {join_error}"),
                ));
                (
                    introspect::MacroEnv::default(),
                    global_kind,
                    Schema::default(),
                    Vec::new(),
                    "_sqlx_migrations".to_owned(),
                    LoadLog::new(),
                )
            }
        };
        log.extend(notes);

        if env.offline {
            log.push((
                MessageType::INFO,
                format!(
                    "SQLX_OFFLINE is set for {}; skipping live introspection",
                    root.display()
                ),
            ));
        } else if let Some(url) = &env.database_url {
            introspect_into(&mut schema, url, kind, &root, &migrations_table, log).await;
        }

        log.push((
            MessageType::INFO,
            format!(
                "context {}: {kind}, {} relation(s)",
                root.display(),
                schema.tables().count()
            ),
        ));

        (DbContext { root, kind, schema }, applied)
    }
}

/// How long a live database gets to answer introspection before the load
/// proceeds without it. Unreachable hosts can otherwise stall a reload for
/// the operating system's TCP timeout, which is measured in minutes.
const INTROSPECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Introspects the database at `url` and merges the result into `schema`,
/// logging the outcome. Failures are informational: an unreachable database
/// only means the live layer is missing.
async fn introspect_into(
    schema: &mut Schema,
    url: &str,
    kind: DatabaseKind,
    root: &Path,
    migrations_table: &str,
    log: &mut LoadLog,
) {
    match LiveDatabase::from_url(url, kind, root) {
        Ok(database) => {
            match tokio::time::timeout(INTROSPECT_TIMEOUT, database.introspect(migrations_table))
                .await
            {
                Ok(Ok(tables)) => {
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
                Ok(Err(error)) => log.push((MessageType::INFO, error.to_string())),
                Err(_elapsed) => log.push((
                    MessageType::WARNING,
                    format!(
                        "introspection of {} timed out after {}s; continuing without it",
                        database.describe(),
                        INTROSPECT_TIMEOUT.as_secs()
                    ),
                )),
            }
        }
        Err(error) => log.push((MessageType::INFO, error.to_string())),
    }
}

/// Chooses a context's backend the way the sqlx macros select a driver: the
/// URL scheme decides, gated on that driver being available (the member's
/// declared drivers when it declares any, the workspace-unified feature set
/// otherwise). Without a URL, the highest-priority declared driver wins,
/// then the workspace default.
fn resolve_kind(
    url: Option<&str>,
    declared: &BTreeSet<DatabaseKind>,
    global_kind: DatabaseKind,
    enabled: &BTreeSet<DatabaseKind>,
    log: &mut LoadLog,
) -> DatabaseKind {
    let available = if declared.is_empty() {
        enabled
    } else {
        declared
    };
    if let Some(scheme_kind) = url.and_then(DatabaseKind::from_url_scheme) {
        if available.is_empty() || available.contains(&scheme_kind) {
            return scheme_kind;
        }
        log.push((
            MessageType::WARNING,
            format!(
                "database URL is a {scheme_kind} URL but the sqlx `{}` feature is not enabled \
                 here; ignoring the scheme",
                scheme_kind.feature_name()
            ),
        ));
    }
    DatabaseKind::ALL
        .into_iter()
        .find(|kind| declared.contains(kind))
        .or_else(|| {
            DatabaseKind::ALL
                .into_iter()
                .find(|kind| enabled.contains(kind))
        })
        .unwrap_or(global_kind)
}

/// The migration directories a crate consumes: one per `sqlx::migrate!`
/// invocation in its sources, resolved the way the macro resolves them
/// (relative to the crate root, defaulting to the configured migrations
/// directory). A crate that embeds no migrator gets the configured default,
/// which is also what sqlx-cli consumes.
fn migration_dirs_for(crate_root: &Path, config: &SqlxConfig, notes: &mut LoadLog) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    let push = |dir: PathBuf, dirs: &mut Vec<PathBuf>| {
        if !dirs.contains(&dir) {
            dirs.push(dir);
        }
    };

    for file in rust_sources(crate_root) {
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        // Cheap pre-filter before spending a tree-sitter parse.
        if !text.contains("migrate!") {
            continue;
        }
        for source in embedded::migrate_sources(&text) {
            match source {
                MigrateSource::Default => {
                    push(config.migrations_dir(crate_root), &mut dirs);
                }
                MigrateSource::Path(path) => {
                    if Path::new(&path).is_absolute() {
                        // The macro itself rejects absolute paths.
                        notes.push((
                            MessageType::WARNING,
                            format!(
                                "ignoring absolute migrate!() path {path} in {}",
                                file.display()
                            ),
                        ));
                        continue;
                    }
                    push(crate_root.join(&path), &mut dirs);
                }
            }
        }
    }

    if dirs.is_empty() {
        dirs.push(config.migrations_dir(crate_root));
    }
    dirs
}

/// Resolves symlinks in `path` so it compares against canonical context
/// roots. Files that don't exist yet (unsaved buffers) resolve through
/// their nearest existing ancestor.
pub(crate) fn normalize(path: PathBuf) -> PathBuf {
    if let Ok(canonical) = std::fs::canonicalize(&path) {
        return canonical;
    }
    let mut missing = Vec::new();
    for ancestor in path.ancestors().skip(1) {
        if let Ok(canonical) = std::fs::canonicalize(ancestor) {
            let mut resolved = canonical;
            let existing_len = ancestor.components().count();
            missing.extend(path.components().skip(existing_len));
            for component in &missing {
                resolved.push(component);
            }
            return resolved;
        }
    }
    path
}

/// Every `.rs` file under `dir`. The walker respects .gitignore and skips
/// hidden directories, keeping build and dependency output out.
fn rust_sources(dir: &Path) -> Vec<PathBuf> {
    let mut sources: Vec<PathBuf> = ignore::WalkBuilder::new(dir)
        .filter_entry(|entry| entry.file_name() != "node_modules" && entry.file_name() != "target")
        .build()
        .filter_map(|entry| entry.ok())
        .map(ignore::DirEntry::into_path)
        .filter(|path| path.extension().is_some_and(|extension| extension == "rs"))
        .collect();
    sources.sort();
    sources
}

/// Every `.rs` and `.sql` file under `dir`, walked the way [`rust_sources`]
/// walks: .gitignore respected, hidden directories and build output
/// skipped.
fn query_source_candidates(dir: &Path) -> Vec<PathBuf> {
    let mut sources: Vec<PathBuf> = ignore::WalkBuilder::new(dir)
        .filter_entry(|entry| entry.file_name() != "node_modules" && entry.file_name() != "target")
        .build()
        .filter_map(|entry| entry.ok())
        .map(ignore::DirEntry::into_path)
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "rs" || extension == "sql")
        })
        .collect();
    sources.sort();
    sources
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context(root: &Path, kind: DatabaseKind) -> DbContext {
        DbContext {
            root: root.to_owned(),
            kind,
            schema: Schema::default(),
        }
    }

    #[test]
    fn query_scan_covers_rust_and_sql_sources_outside_migrations() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::create_dir_all(root.join("src")).expect("mkdir");
        std::fs::create_dir_all(root.join("queries")).expect("mkdir");
        std::fs::create_dir_all(root.join("migrations")).expect("mkdir");
        std::fs::write(
            root.join("src").join("main.rs"),
            r#"fn main() { let _ = sqlx::query!("SELECT id FROM users"); }"#,
        )
        .expect("write main.rs");
        std::fs::write(root.join("src").join("lib.rs"), "pub fn f() {}").expect("write lib.rs");
        std::fs::write(
            root.join("queries").join("get.sql"),
            "SELECT id FROM users;",
        )
        .expect("write query");
        std::fs::write(
            root.join("migrations").join("1_users.sql"),
            "CREATE TABLE users (id INTEGER);",
        )
        .expect("write migration");

        let migration_dirs = vec![normalize(root.join("migrations"))];
        let documents = QueryDocument::scan(&[root.to_owned()], &migration_dirs);
        let names: Vec<_> = documents
            .iter()
            .filter_map(|document| document.path().file_name())
            .collect();
        // The migration is cached separately, and lib.rs has no queries.
        assert_eq!(names, vec!["get.sql", "main.rs"], "{documents:?}");

        let QueryDocument::Rust(rust) = &documents[1] else {
            panic!("main.rs is a Rust query source");
        };
        assert_eq!(rust.extracted.regions.len(), 1);
    }

    #[test]
    fn context_routing_picks_the_deepest_containing_crate() {
        let workspace = Workspace {
            roots: vec![PathBuf::from("/repo")],
            contexts: vec![
                context(Path::new("/repo/services"), DatabaseKind::MySql),
                context(Path::new("/repo/services/api"), DatabaseKind::Postgres),
                context(Path::new("/repo/tools"), DatabaseKind::Sqlite),
            ],
            folder_contexts: vec![context(Path::new("/repo"), DatabaseKind::MySql)],
            fallback: context(Path::new(""), DatabaseKind::Sqlite),
            migration_dirs: Vec::new(),
            migration_documents: Vec::new(),
            query_documents: Vec::new(),
        };
        let uri = |path: &str| Uri::from_file_path(path).expect("valid path");

        let api = workspace.context_for(&uri("/repo/services/api/queries/get.sql"));
        assert_eq!(api.kind, DatabaseKind::Postgres);
        let services = workspace.context_for(&uri("/repo/services/worker/src/main.rs"));
        assert_eq!(services.kind, DatabaseKind::MySql);
        // Inside the folder but outside every member crate: the folder
        // context serves it.
        let shared = workspace.context_for(&uri("/repo/docs/example.sql"));
        assert!(std::ptr::eq(shared, &workspace.folder_contexts[0]));
        // Outside every folder: the workspace-wide fallback.
        let detached = workspace.context_for(&uri("/elsewhere/example.sql"));
        assert!(std::ptr::eq(detached, &workspace.fallback));
    }

    /// Two unrelated folders in one workspace: each gets its own folder
    /// context and schema, and only detached documents see the merged view.
    #[tokio::test]
    async fn multiple_workspace_folders_get_isolated_contexts() {
        let dir_a = tempfile::tempdir().expect("tempdir");
        let dir_b = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir_a.path().join("migrations")).expect("mkdir");
        std::fs::create_dir_all(dir_b.path().join("migrations")).expect("mkdir");
        std::fs::write(
            dir_a.path().join("migrations").join("1_users.sql"),
            "CREATE TABLE users (id INTEGER PRIMARY KEY);",
        )
        .expect("write migration");
        std::fs::write(
            dir_b.path().join("migrations").join("1_posts.sql"),
            "CREATE TABLE posts (id INTEGER PRIMARY KEY);",
        )
        .expect("write migration");

        let (workspace, _log) =
            Workspace::load(vec![dir_a.path().to_owned(), dir_b.path().to_owned()]).await;
        assert_eq!(workspace.folder_contexts.len(), 2);

        let uri = |path: PathBuf| Uri::from_file_path(path).expect("valid path");
        let in_a = workspace.context_for(&uri(dir_a.path().join("q.sql")));
        assert!(in_a.schema.table("users").is_some());
        assert!(in_a.schema.table("posts").is_none());

        let in_b = workspace.context_for(&uri(dir_b.path().join("q.sql")));
        assert!(in_b.schema.table("posts").is_some());
        assert!(in_b.schema.table("users").is_none());

        assert!(workspace.fallback.schema.table("users").is_some());
        assert!(workspace.fallback.schema.table("posts").is_some());
    }

    #[test]
    fn kind_resolution_matches_sqlx_driver_selection() {
        let mut log = LoadLog::new();
        let declared: BTreeSet<_> = [DatabaseKind::Sqlite].into();
        let enabled: BTreeSet<_> = [DatabaseKind::Postgres, DatabaseKind::Sqlite].into();

        // The URL scheme decides when its driver is available.
        assert_eq!(
            resolve_kind(
                Some("sqlite://app.db"),
                &declared,
                DatabaseKind::Postgres,
                &enabled,
                &mut log
            ),
            DatabaseKind::Sqlite
        );
        // Without a URL, the member's declared driver beats the workspace
        // default.
        assert_eq!(
            resolve_kind(None, &declared, DatabaseKind::Postgres, &enabled, &mut log),
            DatabaseKind::Sqlite
        );
        // A URL whose driver the member does not enable is ignored, with a
        // warning.
        let before = log.len();
        assert_eq!(
            resolve_kind(
                Some("mysql://db/app"),
                &declared,
                DatabaseKind::Postgres,
                &enabled,
                &mut log
            ),
            DatabaseKind::Sqlite
        );
        assert_eq!(log.len(), before + 1);
        // No declared drivers: the workspace-unified set gates the scheme.
        assert_eq!(
            resolve_kind(
                Some("postgres://db/app"),
                &BTreeSet::new(),
                DatabaseKind::Sqlite,
                &enabled,
                &mut log
            ),
            DatabaseKind::Postgres
        );
    }

    #[test]
    fn migration_dirs_follow_migrate_invocations() {
        let dir = tempfile::tempdir().expect("tempdir");
        let crate_root = dir.path();
        std::fs::create_dir_all(crate_root.join("src")).expect("mkdir");
        std::fs::write(
            crate_root.join("src").join("main.rs"),
            r#"fn main() { let _ = sqlx::migrate!("../shared/migrations"); }"#,
        )
        .expect("write source");

        let mut notes = LoadLog::new();
        let config = SqlxConfig::default();
        let dirs = migration_dirs_for(crate_root, &config, &mut notes);
        assert_eq!(dirs, vec![crate_root.join("../shared/migrations")]);
    }

    #[test]
    fn migration_dirs_default_when_no_migrator_is_embedded() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("src")).expect("mkdir");
        std::fs::write(dir.path().join("src").join("lib.rs"), "pub fn f() {}")
            .expect("write source");

        let mut notes = LoadLog::new();
        let config = SqlxConfig::default();
        let dirs = migration_dirs_for(dir.path(), &config, &mut notes);
        assert_eq!(dirs, vec![dir.path().join("./migrations")]);
    }

    /// The end-to-end mixed-backend scenario: one workspace, one postgres
    /// crate and one sqlite crate, each with its own migrations and
    /// environment. Exercises `cargo metadata`, per-crate config/env
    /// discovery, scheme-based kind resolution, and document routing.
    #[tokio::test]
    async fn mixed_backend_workspace_builds_isolated_contexts() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nresolver = \"2\"\nmembers = [\"pg-svc\", \"lite-svc\"]\n",
        )
        .expect("write workspace manifest");

        let write_member = |name: &str, feature: &str| {
            let member = root.join(name);
            std::fs::create_dir_all(member.join("src")).expect("mkdir");
            std::fs::create_dir_all(member.join("migrations")).expect("mkdir");
            std::fs::write(
                member.join("Cargo.toml"),
                format!(
                    "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n\
                     [dependencies]\nsqlx = {{ version = \"=0.9.0\", default-features = false, \
                     features = [\"{feature}\"] }}\n"
                ),
            )
            .expect("write member manifest");
            std::fs::write(member.join("src").join("lib.rs"), "").expect("write lib");
            member
        };

        let pg = write_member("pg-svc", "postgres");
        std::fs::write(
            pg.join("migrations").join("1_users.sql"),
            "CREATE TABLE users (id BIGINT PRIMARY KEY, email TEXT NOT NULL);",
        )
        .expect("write migration");

        let lite = write_member("lite-svc", "sqlite");
        std::fs::write(
            lite.join("migrations").join("1_cache.sql"),
            "CREATE TABLE cache_entries (key TEXT PRIMARY KEY, value BLOB);",
        )
        .expect("write migration");
        std::fs::write(lite.join(".env"), "DATABASE_URL=sqlite://cache.db\n").expect("write .env");

        let (workspace, log) = Workspace::load(vec![root.to_owned()]).await;
        let dump = || {
            log.iter()
                .map(|(_, line)| line.as_str())
                .collect::<Vec<_>>()
                .join("\n")
        };

        assert_eq!(workspace.contexts.len(), 2, "{}", dump());

        let uri = |path: PathBuf| Uri::from_file_path(path).expect("valid path");
        let pg_context = workspace.context_for(&uri(pg.join("src").join("main.rs")));
        assert_eq!(pg_context.kind, DatabaseKind::Postgres, "{}", dump());
        assert!(pg_context.schema.table("users").is_some(), "{}", dump());
        assert!(pg_context.schema.table("cache_entries").is_none());

        let lite_context = workspace.context_for(&uri(lite.join("queries").join("get.sql")));
        assert_eq!(lite_context.kind, DatabaseKind::Sqlite, "{}", dump());
        assert!(lite_context.schema.table("cache_entries").is_some());
        assert!(lite_context.schema.table("users").is_none());

        // Shared documents outside both crates see the merged view.
        let shared = workspace.context_for(&uri(root.join("notes.sql")));
        assert!(shared.schema.table("users").is_some());
        assert!(shared.schema.table("cache_entries").is_some());
    }
}
