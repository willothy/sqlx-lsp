//! Live-session integration tests: the server is driven over stdio (the
//! real binary, real LSP framing) while the schema sources change on disk,
//! asserting that watched-file and save notifications rebuild the index.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use serde_json::{Value, json};

/// How long to wait for any single server message before failing the test.
const MESSAGE_TIMEOUT: Duration = Duration::from_secs(30);

/// A minimal LSP client around a spawned server process.
struct LspClient {
    child: Child,
    stdin: ChildStdin,
    messages: mpsc::Receiver<Value>,
    /// Messages received while waiting for something else.
    pending: VecDeque<Value>,
    next_id: i64,
}

impl LspClient {
    /// Spawns the server, runs the initialize handshake for `root`, and
    /// waits for the initial workspace load to finish.
    fn start(root: &Path) -> LspClient {
        let mut client = Self::start_with_capabilities(root, json!({}));
        client.wait_for_load();
        client
    }

    /// Spawns the server and initializes it with the given client
    /// capabilities, without waiting for the initial load — the caller can
    /// observe every message the load produces.
    fn start_with_capabilities(root: &Path, capabilities: Value) -> LspClient {
        let mut child = Command::new(env!("CARGO_BIN_EXE_sqlx-lsp"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            // The server reads these; scrub them so the fixture's .env files
            // are the only environment under test.
            .env_remove("DATABASE_URL")
            .env_remove("SQLX_OFFLINE")
            .spawn()
            .expect("spawn sqlx-lsp");

        let stdout = child.stdout.take().expect("piped stdout");
        let stdin = child.stdin.take().expect("piped stdin");
        let (sender, messages) = mpsc::channel();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                let mut content_length = None;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 {
                        return;
                    }
                    let line = line.trim_end();
                    if line.is_empty() {
                        break;
                    }
                    if let Some(value) = line.strip_prefix("Content-Length:") {
                        content_length = value.trim().parse::<usize>().ok();
                    }
                }
                let Some(length) = content_length else {
                    return;
                };
                let mut body = vec![0u8; length];
                if reader.read_exact(&mut body).is_err() {
                    return;
                }
                let Ok(message) = serde_json::from_slice::<Value>(&body) else {
                    continue;
                };
                if sender.send(message).is_err() {
                    return;
                }
            }
        });

        let mut client = LspClient {
            child,
            stdin,
            messages,
            pending: VecDeque::new(),
            next_id: 0,
        };

        let root_uri = format!("file://{}", root.display());
        client.request(
            "initialize",
            json!({
                "processId": null,
                "rootUri": root_uri,
                "capabilities": capabilities,
                "workspaceFolders": [{ "uri": root_uri, "name": "fixture" }],
            }),
        );
        client.notify("initialized", json!({}));
        client
    }

    fn send_raw(&mut self, message: Value) {
        let body = serde_json::to_vec(&message).expect("serialize message");
        write!(self.stdin, "Content-Length: {}\r\n\r\n", body.len()).expect("write header");
        self.stdin.write_all(&body).expect("write body");
        self.stdin.flush().expect("flush");
    }

    /// The next client-bound message, transparently answering server-to-
    /// client requests (e.g. `client/registerCapability`).
    fn next_message(&mut self) -> Value {
        match self.pending.pop_front() {
            Some(message) => message,
            None => self.recv_message(),
        }
    }

    /// The next message straight off the wire, bypassing `pending`,
    /// transparently answering server-to-client requests.
    fn recv_message(&mut self) -> Value {
        loop {
            let message = self
                .messages
                .recv_timeout(MESSAGE_TIMEOUT)
                .expect("server message before timeout");
            if message.get("method").is_some() && message.get("id").is_some() {
                let id = message["id"].clone();
                self.send_raw(json!({ "jsonrpc": "2.0", "id": id, "result": null }));
                continue;
            }
            return message;
        }
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        self.send_raw(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        // Read from the wire, not `next_message`: notifications that arrive
        // ahead of the response are deferred to `pending`, and popping them
        // back here would cycle forever without ever reaching the response.
        loop {
            let message = self.recv_message();
            if message.get("id").and_then(Value::as_i64) == Some(id) {
                return message["result"].clone();
            }
            self.pending.push_back(message);
        }
    }

    fn notify(&mut self, method: &str, params: Value) {
        self.send_raw(json!({ "jsonrpc": "2.0", "method": method, "params": params }));
    }

    /// Blocks until the workspace-load summary log line arrives — the
    /// deterministic signal that a (re)load finished. Anything already
    /// buffered is drained first, so a previous load's summary can't
    /// satisfy this wait.
    fn wait_for_load(&mut self) {
        self.wait_for_log("workspace-wide index holds");
    }

    /// Blocks until a `window/logMessage` containing `needle` arrives.
    fn wait_for_log(&mut self, needle: &str) {
        loop {
            let message = self.next_message();
            let matches = message["method"] == "window/logMessage"
                && message["params"]["message"]
                    .as_str()
                    .is_some_and(|text| text.contains(needle));
            if matches {
                return;
            }
        }
    }

    fn open(&mut self, uri: &str, text: &str) {
        self.open_as(uri, "sql", text);
    }

    /// Opens a document under an explicit language id (e.g. `rust` for
    /// buffers whose sqlx macros embed SQL).
    fn open_as(&mut self, uri: &str, language_id: &str, text: &str) {
        self.notify(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": uri, "languageId": language_id, "version": 1, "text": text,
                }
            }),
        );
    }

    fn change(&mut self, uri: &str, version: i64, text: &str) {
        self.notify(
            "textDocument/didChange",
            json!({
                "textDocument": { "uri": uri, "version": version },
                "contentChanges": [{ "text": text }],
            }),
        );
    }

    /// Sends an incremental (ranged) content change.
    fn change_range(
        &mut self,
        uri: &str,
        version: i64,
        start: (u32, u32),
        end: (u32, u32),
        text: &str,
    ) {
        self.notify(
            "textDocument/didChange",
            json!({
                "textDocument": { "uri": uri, "version": version },
                "contentChanges": [{
                    "range": {
                        "start": { "line": start.0, "character": start.1 },
                        "end": { "line": end.0, "character": end.1 },
                    },
                    "text": text,
                }],
            }),
        );
    }

    fn completion_labels(&mut self, uri: &str, line: u32, character: u32) -> Vec<String> {
        let result = self.request(
            "textDocument/completion",
            json!({
                "textDocument": { "uri": uri },
                "position": { "line": line, "character": character },
            }),
        );
        result
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item["label"].as_str())
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Blocks until a `publishDiagnostics` notification for `uri` arrives
    /// and returns its diagnostics array.
    fn wait_for_diagnostics(&mut self, uri: &str) -> Vec<Value> {
        loop {
            let message = self.next_message();
            if message["method"] == "textDocument/publishDiagnostics"
                && message["params"]["uri"] == uri
            {
                return message["params"]["diagnostics"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
            }
        }
    }

    fn hover_text(&mut self, uri: &str, line: u32, character: u32) -> String {
        let result = self.request(
            "textDocument/hover",
            json!({
                "textDocument": { "uri": uri },
                "position": { "line": line, "character": character },
            }),
        );
        result["contents"]["value"]
            .as_str()
            .unwrap_or("")
            .to_owned()
    }
}

impl LspClient {
    /// Waits for the server process to exit on its own, returning its
    /// status, or `None` if it is still running when `timeout` elapses.
    fn wait_for_exit(&mut self, timeout: Duration) -> Option<std::process::ExitStatus> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if let Ok(Some(status)) = self.child.try_wait() {
                return Some(status);
            }
            if std::time::Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for LspClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn file_uri(path: &Path) -> String {
    format!("file://{}", path.display())
}

#[test]
fn schema_changes_reload_while_the_session_is_open() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let migrations = root.join("migrations");
    std::fs::create_dir_all(&migrations).expect("mkdir");
    std::fs::write(
        migrations.join("0001_users.sql"),
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL);",
    )
    .expect("write migration");

    let mut client = LspClient::start(root);

    let query_uri = file_uri(&root.join("queries").join("get.sql"));
    client.open(&query_uri, "SELECT id FROM ");

    let labels = client.completion_labels(&query_uri, 0, 15);
    assert!(labels.contains(&"users".to_owned()), "{labels:?}");
    assert!(!labels.contains(&"posts".to_owned()), "{labels:?}");

    // A new migration lands on disk (e.g. `sqlx migrate add` in a terminal);
    // the client's file watcher reports it.
    let new_migration = migrations.join("0002_posts.sql");
    std::fs::write(
        &new_migration,
        "CREATE TABLE posts (id INTEGER PRIMARY KEY, title TEXT NOT NULL);",
    )
    .expect("write migration");
    client.notify(
        "workspace/didChangeWatchedFiles",
        json!({ "changes": [{ "uri": file_uri(&new_migration), "type": 1 }] }),
    );
    client.wait_for_load();

    let labels = client.completion_labels(&query_uri, 0, 15);
    assert!(labels.contains(&"posts".to_owned()), "{labels:?}");

    // An existing migration is edited and saved in the editor; the save
    // notification alone must refresh the index.
    let users_migration = migrations.join("0001_users.sql");
    std::fs::write(
        &users_migration,
        "CREATE TABLE users (\n  id INTEGER PRIMARY KEY,\n  name TEXT NOT NULL,\n  email TEXT NOT NULL\n);",
    )
    .expect("rewrite migration");
    client.notify(
        "textDocument/didSave",
        json!({ "textDocument": { "uri": file_uri(&users_migration) } }),
    );
    client.wait_for_load();

    client.change(&query_uri, 2, "SELECT email FROM users");
    let hover = client.hover_text(&query_uri, 0, 8);
    assert!(hover.contains("users.email TEXT NOT NULL"), "{hover}");
    // The definition location tracks the rewritten file.
    assert!(hover.contains("0001_users.sql"), "{hover}");
}

#[test]
fn references_and_rename_cover_closed_query_files() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(dir.path().join("migrations")).expect("mkdir");
    std::fs::create_dir_all(dir.path().join("src")).expect("mkdir");
    std::fs::create_dir_all(dir.path().join("queries")).expect("mkdir");
    std::fs::write(
        dir.path().join("migrations").join("1_users.sql"),
        "CREATE TABLE users (id INTEGER PRIMARY KEY);",
    )
    .expect("write migration");
    let main_rs = dir.path().join("src").join("main.rs");
    std::fs::write(
        &main_rs,
        "fn main() {\n    let _ = sqlx::query!(\"SELECT id FROM users\");\n}\n",
    )
    .expect("write main.rs");
    std::fs::write(
        dir.path().join("queries").join("get.sql"),
        "SELECT id FROM users;",
    )
    .expect("write query file");
    let mut client = LspClient::start(dir.path());

    // Neither main.rs nor get.sql is ever opened.
    let query_uri = file_uri(&dir.path().join("q.sql"));
    client.open(&query_uri, "DELETE FROM users");

    let references = |client: &mut LspClient| {
        client
            .request(
                "textDocument/references",
                json!({
                    "textDocument": { "uri": query_uri },
                    "position": { "line": 0, "character": 13 },
                    "context": { "includeDeclaration": false },
                }),
            )
            .as_array()
            .cloned()
            .unwrap_or_default()
    };

    let locations = references(&mut client);
    let uris: Vec<&str> = locations
        .iter()
        .filter_map(|location| location["uri"].as_str())
        .collect();
    assert!(uris.iter().any(|uri| uri.ends_with("q.sql")), "{uris:?}");
    assert!(uris.iter().any(|uri| uri.ends_with("main.rs")), "{uris:?}");
    assert!(uris.iter().any(|uri| uri.ends_with("get.sql")), "{uris:?}");
    let in_main = locations
        .iter()
        .filter(|location| {
            location["uri"]
                .as_str()
                .is_some_and(|uri| uri.ends_with("main.rs"))
        })
        .collect::<Vec<_>>();
    assert_eq!(in_main.len(), 1, "{in_main:?}");
    assert_eq!(in_main[0]["range"]["start"]["line"], 1);
    assert_eq!(in_main[0]["range"]["start"]["character"], 41);

    // Rename rewrites the closed files too.
    let edit = client.request(
        "textDocument/rename",
        json!({
            "textDocument": { "uri": query_uri },
            "position": { "line": 0, "character": 13 },
            "newName": "accounts",
        }),
    );
    let changes = edit["changes"].as_object().expect("changes");
    assert!(
        changes.keys().any(|uri| uri.ends_with("main.rs")),
        "{changes:?}"
    );
    assert!(
        changes.keys().any(|uri| uri.ends_with("get.sql")),
        "{changes:?}"
    );

    // A new query lands in main.rs (e.g. written by another tool); the
    // watched-file notification refreshes the index without a full reload.
    std::fs::write(
        &main_rs,
        "fn main() {\n    let _ = sqlx::query!(\"SELECT id FROM users\");\n    \
         let _ = sqlx::query!(\"DELETE FROM users\");\n}\n",
    )
    .expect("rewrite main.rs");
    client.notify(
        "workspace/didChangeWatchedFiles",
        json!({ "changes": [{ "uri": file_uri(&main_rs), "type": 2 }] }),
    );
    client.wait_for_log("query index covers");

    let locations = references(&mut client);
    let in_main = locations
        .iter()
        .filter(|location| {
            location["uri"]
                .as_str()
                .is_some_and(|uri| uri.ends_with("main.rs"))
        })
        .count();
    assert_eq!(in_main, 2, "{locations:?}");

    // Deleting a query file drops its entries from the index.
    let get_sql = dir.path().join("queries").join("get.sql");
    std::fs::remove_file(&get_sql).expect("delete query file");
    client.notify(
        "workspace/didChangeWatchedFiles",
        json!({ "changes": [{ "uri": file_uri(&get_sql), "type": 3 }] }),
    );
    client.wait_for_log("query index covers");
    let locations = references(&mut client);
    assert!(
        locations
            .iter()
            .filter_map(|location| location["uri"].as_str())
            .all(|uri| !uri.ends_with("get.sql")),
        "{locations:?}"
    );
}

#[test]
fn reference_results_track_migration_edits() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(dir.path().join("migrations")).expect("mkdir");
    let migration = dir.path().join("migrations").join("1_users.sql");
    std::fs::write(&migration, "CREATE TABLE users (id INTEGER PRIMARY KEY);")
        .expect("write migration");
    let mut client = LspClient::start(dir.path());

    let query_uri = file_uri(&dir.path().join("q.sql"));
    client.open(&query_uri, "SELECT id FROM users");

    let migration_references = |client: &mut LspClient| {
        client
            .request(
                "textDocument/references",
                json!({
                    "textDocument": { "uri": query_uri },
                    "position": { "line": 0, "character": 17 },
                    "context": { "includeDeclaration": true },
                }),
            )
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|location| {
                location["uri"]
                    .as_str()
                    .is_some_and(|uri| uri.ends_with("1_users.sql"))
            })
            .collect::<Vec<_>>()
    };

    let locations = migration_references(&mut client);
    assert_eq!(locations.len(), 1, "{locations:?}");
    assert_eq!(locations[0]["range"]["start"]["line"], 0, "{locations:?}");

    // The migration is rewritten on disk, moving the definition down a
    // line. The reload must refresh the cached migration contents — a
    // stale cache would keep reporting a line-0 reference alongside the
    // relocated declaration.
    std::fs::write(
        &migration,
        "-- users of the app\nCREATE TABLE users (id INTEGER PRIMARY KEY);",
    )
    .expect("rewrite migration");
    client.notify(
        "workspace/didChangeWatchedFiles",
        json!({ "changes": [{ "uri": file_uri(&migration), "type": 2 }] }),
    );
    client.wait_for_load();

    let locations = migration_references(&mut client);
    assert_eq!(locations.len(), 1, "{locations:?}");
    assert_eq!(locations[0]["range"]["start"]["line"], 1, "{locations:?}");
}

#[test]
fn exits_cleanly_on_the_exit_notification() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut client = LspClient::start(dir.path());

    client.request("shutdown", Value::Null);
    client.notify("exit", Value::Null);

    let status = client
        .wait_for_exit(Duration::from_secs(10))
        .expect("server exits after the exit notification");
    assert!(status.success(), "unexpected exit status: {status}");
}

#[test]
fn diagnostics_flag_syntax_errors_and_unknown_references() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(dir.path().join("migrations")).expect("mkdir");
    std::fs::write(
        dir.path().join("migrations").join("1_users.sql"),
        "CREATE TABLE users (id INTEGER PRIMARY KEY);",
    )
    .expect("write migration");
    let mut client = LspClient::start(dir.path());

    // An unknown table is a warning.
    let query_uri = file_uri(&dir.path().join("q.sql"));
    client.open(&query_uri, "SELECT id FROM posts");
    let diagnostics = client.wait_for_diagnostics(&query_uri);
    assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
    assert_eq!(diagnostics[0]["severity"], 2);
    assert!(
        diagnostics[0]["message"]
            .as_str()
            .is_some_and(|message| message.contains("unknown table")),
        "{diagnostics:?}"
    );

    // Fixing the reference clears it.
    client.change(&query_uri, 2, "SELECT id FROM users");
    let diagnostics = client.wait_for_diagnostics(&query_uri);
    assert!(diagnostics.is_empty(), "{diagnostics:?}");

    // A syntax problem is an error.
    client.change(&query_uri, 3, "SELECT FROM WHERE;");
    let diagnostics = client.wait_for_diagnostics(&query_uri);
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic["severity"] == 1),
        "{diagnostics:?}"
    );
}

#[test]
fn goto_definition_resolves_into_migration_files() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(dir.path().join("migrations")).expect("mkdir");
    std::fs::write(
        dir.path().join("migrations").join("1_users.sql"),
        "CREATE TABLE users (\n  id INTEGER PRIMARY KEY\n);",
    )
    .expect("write migration");
    let mut client = LspClient::start(dir.path());

    let query_uri = file_uri(&dir.path().join("q.sql"));
    client.open(&query_uri, "SELECT id FROM users");

    // The table reference jumps to the CREATE TABLE identifier.
    let location = client.request(
        "textDocument/definition",
        json!({
            "textDocument": { "uri": query_uri },
            "position": { "line": 0, "character": 17 },
        }),
    );
    assert!(
        location["uri"]
            .as_str()
            .is_some_and(|uri| uri.ends_with("1_users.sql")),
        "{location:?}"
    );
    assert_eq!(location["range"]["start"]["line"], 0, "{location:?}");
    assert_eq!(location["range"]["start"]["character"], 13, "{location:?}");

    // The column reference jumps to its own defining line.
    let location = client.request(
        "textDocument/definition",
        json!({
            "textDocument": { "uri": query_uri },
            "position": { "line": 0, "character": 8 },
        }),
    );
    assert_eq!(location["range"]["start"]["line"], 1, "{location:?}");
    assert_eq!(location["range"]["start"]["character"], 2, "{location:?}");
}

#[test]
fn rust_buffers_serve_sql_inside_query_macros() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(dir.path().join("migrations")).expect("mkdir");
    std::fs::write(
        dir.path().join("migrations").join("1_users.sql"),
        "CREATE TABLE users (id INTEGER PRIMARY KEY, email TEXT NOT NULL);",
    )
    .expect("write migration");
    let mut client = LspClient::start(dir.path());

    let main_uri = file_uri(&dir.path().join("src").join("main.rs"));
    // The SQL string spans host columns 26..46 on line 1.
    let source = "fn main() {\n    let _ = sqlx::query!(\"SELECT id FROM users\");\n}\n";
    client.open_as(&main_uri, "rust", source);
    let diagnostics = client.wait_for_diagnostics(&main_uri);
    assert!(diagnostics.is_empty(), "{diagnostics:?}");

    // Hover on `users` inside the macro string shows the table definition.
    let hover = client.hover_text(&main_uri, 1, 43);
    assert!(hover.contains("CREATE TABLE users"), "{hover}");

    // Completion after `FROM ` offers the schema's tables, with accept
    // edits in host coordinates.
    let labels = client.completion_labels(&main_uri, 1, 41);
    assert!(labels.contains(&"users".to_owned()), "{labels:?}");
    let items = client.request(
        "textDocument/completion",
        json!({
            "textDocument": { "uri": main_uri },
            "position": { "line": 1, "character": 41 },
        }),
    );
    let users = items
        .as_array()
        .and_then(|items| items.iter().find(|item| item["label"] == "users"))
        .expect("users offered");
    assert_eq!(users["textEdit"]["range"]["start"]["line"], 1, "{users:?}");
    assert_eq!(
        users["textEdit"]["range"]["start"]["character"], 41,
        "{users:?}"
    );

    // Goto definition escapes the Rust buffer into the migration file.
    let location = client.request(
        "textDocument/definition",
        json!({
            "textDocument": { "uri": main_uri },
            "position": { "line": 1, "character": 43 },
        }),
    );
    assert!(
        location["uri"]
            .as_str()
            .is_some_and(|uri| uri.ends_with("1_users.sql")),
        "{location:?}"
    );

    // The embedded SQL produces semantic tokens.
    let tokens = client.request(
        "textDocument/semanticTokens/full",
        json!({ "textDocument": { "uri": main_uri } }),
    );
    assert!(
        tokens["data"]
            .as_array()
            .is_some_and(|data| !data.is_empty()),
        "{tokens:?}"
    );

    // An unknown table inside the macro is flagged at host coordinates.
    let source = "fn main() {\n    let _ = sqlx::query!(\"SELECT id FROM posts\");\n}\n";
    client.change(&main_uri, 2, source);
    let diagnostics = client.wait_for_diagnostics(&main_uri);
    assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
    assert_eq!(
        diagnostics[0]["range"]["start"]["line"], 1,
        "{diagnostics:?}"
    );
    assert_eq!(
        diagnostics[0]["range"]["start"]["character"], 41,
        "{diagnostics:?}"
    );
}

#[test]
fn bind_parameter_mismatches_are_flagged_in_rust_buffers() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(dir.path().join("migrations")).expect("mkdir");
    std::fs::write(
        dir.path().join("migrations").join("1_users.sql"),
        "CREATE TABLE users (id INTEGER PRIMARY KEY);",
    )
    .expect("write migration");
    let mut client = LspClient::start(dir.path());

    let main_uri = file_uri(&dir.path().join("src").join("main.rs"));
    let source =
        "fn main() {\n    let _ = sqlx::query!(\"SELECT id FROM users WHERE id = ?\");\n}\n";
    client.open_as(&main_uri, "rust", source);
    let diagnostics = client.wait_for_diagnostics(&main_uri);
    assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
    assert_eq!(diagnostics[0]["severity"], 1, "{diagnostics:?}");
    assert!(
        diagnostics[0]["message"]
            .as_str()
            .is_some_and(|message| message.contains("1 bind parameter but 0 arguments")),
        "{diagnostics:?}"
    );

    // Supplying the argument clears the diagnostic.
    let source =
        "fn main() {\n    let _ = sqlx::query!(\"SELECT id FROM users WHERE id = ?\", id);\n}\n";
    client.change(&main_uri, 2, source);
    let diagnostics = client.wait_for_diagnostics(&main_uri);
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
}

#[test]
fn references_span_open_documents_and_migrations() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(dir.path().join("migrations")).expect("mkdir");
    std::fs::write(
        dir.path().join("migrations").join("1_users.sql"),
        "CREATE TABLE users (id INTEGER PRIMARY KEY);",
    )
    .expect("write migration");
    let mut client = LspClient::start(dir.path());

    let query_uri = file_uri(&dir.path().join("q.sql"));
    client.open(&query_uri, "SELECT id FROM users");
    let other_uri = file_uri(&dir.path().join("r.sql"));
    client.open(&other_uri, "DELETE FROM users");

    let references = |client: &mut LspClient, include_declaration: bool| {
        client
            .request(
                "textDocument/references",
                json!({
                    "textDocument": { "uri": query_uri },
                    "position": { "line": 0, "character": 17 },
                    "context": { "includeDeclaration": include_declaration },
                }),
            )
            .as_array()
            .cloned()
            .unwrap_or_default()
    };

    // Both open documents and the migration's CREATE TABLE are found.
    let locations = references(&mut client, true);
    let uris: Vec<&str> = locations
        .iter()
        .filter_map(|location| location["uri"].as_str())
        .collect();
    assert!(uris.contains(&query_uri.as_str()), "{locations:?}");
    assert!(uris.contains(&other_uri.as_str()), "{locations:?}");
    assert!(
        uris.iter().any(|uri| uri.ends_with("1_users.sql")),
        "{locations:?}"
    );

    // Without the declaration, the defining identifier drops out and the
    // open documents remain.
    let locations = references(&mut client, false);
    assert!(
        locations
            .iter()
            .filter_map(|location| location["uri"].as_str())
            .all(|uri| !uri.ends_with("1_users.sql")),
        "{locations:?}"
    );
    assert_eq!(locations.len(), 2, "{locations:?}");
}

#[test]
fn workspace_symbols_search_the_schema_index() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(dir.path().join("migrations")).expect("mkdir");
    std::fs::write(
        dir.path().join("migrations").join("1_init.sql"),
        "CREATE TABLE users (id INTEGER PRIMARY KEY, email TEXT NOT NULL);\n\
         CREATE TABLE posts (id INTEGER PRIMARY KEY);",
    )
    .expect("write migration");
    let mut client = LspClient::start(dir.path());

    let symbols = |client: &mut LspClient, query: &str| {
        client
            .request("workspace/symbol", json!({ "query": query }))
            .as_array()
            .cloned()
            .unwrap_or_default()
    };

    // A substring query finds the table, located in its migration.
    let found = symbols(&mut client, "user");
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0]["name"], "users");
    assert!(
        found[0]["location"]["uri"]
            .as_str()
            .is_some_and(|uri| uri.ends_with("1_init.sql")),
        "{found:?}"
    );

    // Columns match too, naming their table as the container.
    let found = symbols(&mut client, "email");
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0]["containerName"], "users");

    // An empty query returns the whole index: 2 tables and 3 columns.
    assert_eq!(symbols(&mut client, "").len(), 5);
}

#[test]
fn utf8_position_encoding_negotiates_and_counts_bytes() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(dir.path().join("migrations")).expect("mkdir");
    std::fs::write(
        dir.path().join("migrations").join("1_users.sql"),
        "CREATE TABLE users (id INTEGER PRIMARY KEY);",
    )
    .expect("write migration");

    let mut client = LspClient::start_with_capabilities(
        dir.path(),
        json!({ "general": { "positionEncodings": ["utf-8", "utf-16"] } }),
    );
    client.wait_for_load();

    // '😀' is 4 UTF-8 bytes (2 UTF-16 units): in UTF-8 counting, `id`
    // spans columns 15..17; UTF-16 counting would place it at 13..15.
    let query_uri = file_uri(&dir.path().join("q.sql"));
    client.open(&query_uri, "SELECT '😀', id FROM users");
    let response = client.request(
        "textDocument/hover",
        json!({
            "textDocument": { "uri": query_uri },
            "position": { "line": 0, "character": 16 },
        }),
    );
    assert!(
        response["contents"]["value"]
            .as_str()
            .is_some_and(|value| value.contains("users.id")),
        "{response:?}"
    );
    assert_eq!(response["range"]["start"]["character"], 15, "{response:?}");
    assert_eq!(response["range"]["end"]["character"], 17, "{response:?}");
}

#[test]
fn pull_diagnostics_report_the_current_document_state() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(dir.path().join("migrations")).expect("mkdir");
    std::fs::write(
        dir.path().join("migrations").join("1_users.sql"),
        "CREATE TABLE users (id INTEGER PRIMARY KEY);",
    )
    .expect("write migration");
    let mut client = LspClient::start_with_capabilities(
        dir.path(),
        json!({ "textDocument": { "diagnostic": {} } }),
    );
    client.wait_for_load();

    let query_uri = file_uri(&dir.path().join("q.sql"));
    client.open(&query_uri, "SELECT id FROM posts");

    let report = client.request(
        "textDocument/diagnostic",
        json!({ "textDocument": { "uri": query_uri } }),
    );
    assert_eq!(report["kind"], "full", "{report:?}");
    let items = report["items"].as_array().expect("items");
    assert_eq!(items.len(), 1, "{items:?}");
    assert!(
        items[0]["message"]
            .as_str()
            .is_some_and(|message| message.contains("unknown table")),
        "{items:?}"
    );

    // After a fix, the next pull reports clean.
    client.change(&query_uri, 2, "SELECT id FROM users");
    let report = client.request(
        "textDocument/diagnostic",
        json!({ "textDocument": { "uri": query_uri } }),
    );
    assert!(
        report["items"].as_array().is_some_and(Vec::is_empty),
        "{report:?}"
    );
}

#[test]
fn quick_fixes_suggest_replacements_for_misspelled_names() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(dir.path().join("migrations")).expect("mkdir");
    std::fs::write(
        dir.path().join("migrations").join("1_users.sql"),
        "CREATE TABLE users (id INTEGER PRIMARY KEY);",
    )
    .expect("write migration");
    let mut client = LspClient::start(dir.path());

    let query_uri = file_uri(&dir.path().join("q.sql"));
    client.open(&query_uri, "SELECT id FROM usrs");

    let actions = client.request(
        "textDocument/codeAction",
        json!({
            "textDocument": { "uri": query_uri },
            "range": {
                "start": { "line": 0, "character": 15 },
                "end": { "line": 0, "character": 19 },
            },
            "context": { "diagnostics": [] },
        }),
    );
    let actions = actions.as_array().expect("actions");
    assert_eq!(actions.len(), 1, "{actions:?}");
    assert_eq!(actions[0]["title"], "Replace with `users`");
    assert_eq!(actions[0]["kind"], "quickfix");
    let edits = &actions[0]["edit"]["changes"][&query_uri];
    assert_eq!(edits[0]["newText"], "users", "{actions:?}");
    assert_eq!(edits[0]["range"]["start"]["character"], 15);
}

#[test]
fn document_symbols_outline_ddl_files() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(dir.path().join("migrations")).expect("mkdir");
    std::fs::write(
        dir.path().join("migrations").join("1_users.sql"),
        "CREATE TABLE users (id INTEGER PRIMARY KEY);",
    )
    .expect("write migration");
    let mut client = LspClient::start(dir.path());

    let migration_uri = file_uri(&dir.path().join("migrations").join("1_users.sql"));
    client.open(
        &migration_uri,
        "CREATE TABLE users (id INTEGER PRIMARY KEY);",
    );

    let symbols = client.request(
        "textDocument/documentSymbol",
        json!({ "textDocument": { "uri": migration_uri } }),
    );
    let symbols = symbols.as_array().expect("symbols");
    assert_eq!(symbols.len(), 1, "{symbols:?}");
    assert_eq!(symbols[0]["name"], "users");
    let children = symbols[0]["children"].as_array().expect("columns");
    assert_eq!(children.len(), 1, "{children:?}");
    assert_eq!(children[0]["name"], "id");
}

#[test]
fn document_highlight_marks_occurrences_in_the_requesting_document() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(dir.path().join("migrations")).expect("mkdir");
    std::fs::write(
        dir.path().join("migrations").join("1_users.sql"),
        "CREATE TABLE users (id INTEGER PRIMARY KEY);",
    )
    .expect("write migration");
    let mut client = LspClient::start(dir.path());

    let query_uri = file_uri(&dir.path().join("q.sql"));
    client.open(&query_uri, "SELECT id FROM users; SELECT id FROM users");

    let highlights = client.request(
        "textDocument/documentHighlight",
        json!({
            "textDocument": { "uri": query_uri },
            "position": { "line": 0, "character": 17 },
        }),
    );
    let highlights = highlights.as_array().expect("highlights");
    let starts: Vec<_> = highlights
        .iter()
        .map(|highlight| highlight["range"]["start"]["character"].as_u64())
        .collect();
    assert_eq!(starts, vec![Some(15), Some(37)], "{highlights:?}");
}

#[test]
fn rename_rewrites_references_across_documents_and_migrations() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(dir.path().join("migrations")).expect("mkdir");
    std::fs::write(
        dir.path().join("migrations").join("1_users.sql"),
        "CREATE TABLE users (id INTEGER PRIMARY KEY);",
    )
    .expect("write migration");
    let mut client = LspClient::start(dir.path());

    let query_uri = file_uri(&dir.path().join("q.sql"));
    client.open(&query_uri, "SELECT id FROM users");

    // prepareRename highlights the identifier under the cursor.
    let prepared = client.request(
        "textDocument/prepareRename",
        json!({
            "textDocument": { "uri": query_uri },
            "position": { "line": 0, "character": 17 },
        }),
    );
    assert_eq!(prepared["start"]["character"], 15, "{prepared:?}");
    assert_eq!(prepared["end"]["character"], 20, "{prepared:?}");

    // The rename edits both the query and the migration's CREATE TABLE.
    let edit = client.request(
        "textDocument/rename",
        json!({
            "textDocument": { "uri": query_uri },
            "position": { "line": 0, "character": 17 },
            "newName": "accounts",
        }),
    );
    let changes = edit["changes"].as_object().expect("changes map");
    assert_eq!(changes.len(), 2, "{changes:?}");

    let query_edits = changes[&query_uri].as_array().expect("query edits");
    assert_eq!(query_edits.len(), 1, "{query_edits:?}");
    assert_eq!(query_edits[0]["range"]["start"]["character"], 15);
    assert_eq!(query_edits[0]["newText"], "accounts");

    let (migration_uri, migration_edits) = changes
        .iter()
        .find(|(uri, _)| uri.ends_with("1_users.sql"))
        .expect("migration edits");
    let migration_edits = migration_edits.as_array().expect("array");
    assert_eq!(migration_edits.len(), 1, "{migration_uri}");
    assert_eq!(migration_edits[0]["range"]["start"]["character"], 13);
    assert_eq!(migration_edits[0]["newText"], "accounts");

    // An invalid identifier is rejected rather than applied.
    let rejected = client.request(
        "textDocument/rename",
        json!({
            "textDocument": { "uri": query_uri },
            "position": { "line": 0, "character": 17 },
            "newName": "1bad name",
        }),
    );
    assert!(rejected.is_null(), "{rejected:?}");
}

#[test]
fn rename_rejects_reserved_words_and_collisions() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(dir.path().join("migrations")).expect("mkdir");
    std::fs::write(
        dir.path().join("migrations").join("1_init.sql"),
        "CREATE TABLE users (id INTEGER PRIMARY KEY, email TEXT NOT NULL);\n\
         CREATE TABLE posts (id INTEGER PRIMARY KEY);",
    )
    .expect("write migration");
    let mut client = LspClient::start(dir.path());

    let query_uri = file_uri(&dir.path().join("q.sql"));
    client.open(&query_uri, "SELECT id, email FROM users");
    let rename = |client: &mut LspClient, character: u32, new_name: &str| {
        client.request(
            "textDocument/rename",
            json!({
                "textDocument": { "uri": query_uri },
                "position": { "line": 0, "character": character },
                "newName": new_name,
            }),
        )
    };

    // A reserved word is rejected.
    assert!(rename(&mut client, 23, "select").is_null());
    // A name already taken by another relation is rejected.
    assert!(rename(&mut client, 23, "posts").is_null());
    // A column rename cannot collide with a sibling column.
    assert!(rename(&mut client, 7, "email").is_null());
    // A clean rename still goes through.
    assert!(!rename(&mut client, 23, "accounts").is_null());
}

#[test]
fn rename_uses_versioned_edits_when_the_client_supports_them() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(dir.path().join("migrations")).expect("mkdir");
    std::fs::write(
        dir.path().join("migrations").join("1_users.sql"),
        "CREATE TABLE users (id INTEGER PRIMARY KEY);",
    )
    .expect("write migration");
    let mut client = LspClient::start_with_capabilities(
        dir.path(),
        json!({ "workspace": { "workspaceEdit": { "documentChanges": true } } }),
    );
    client.wait_for_load();

    let query_uri = file_uri(&dir.path().join("q.sql"));
    client.open(&query_uri, "SELECT 1 FROM users");
    client.change(&query_uri, 7, "SELECT id FROM users");

    let edit = client.request(
        "textDocument/rename",
        json!({
            "textDocument": { "uri": query_uri },
            "position": { "line": 0, "character": 17 },
            "newName": "accounts",
        }),
    );
    assert!(edit["changes"].is_null(), "{edit:?}");
    let document_edits = edit["documentChanges"].as_array().expect("documentChanges");
    assert_eq!(document_edits.len(), 2, "{document_edits:?}");

    // The open buffer's edit carries its latest synchronized version; the
    // unopened migration file carries null.
    let for_query = document_edits
        .iter()
        .find(|edit| edit["textDocument"]["uri"] == query_uri)
        .expect("query edit");
    assert_eq!(for_query["textDocument"]["version"], 7, "{for_query:?}");
    let for_migration = document_edits
        .iter()
        .find(|edit| edit["textDocument"]["uri"] != query_uri)
        .expect("migration edit");
    assert!(
        for_migration["textDocument"]["version"].is_null(),
        "{for_migration:?}"
    );
}

#[test]
fn rename_of_query_local_relations_stays_in_the_document() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(dir.path().join("migrations")).expect("mkdir");
    std::fs::write(
        dir.path().join("migrations").join("1_users.sql"),
        "CREATE TABLE users (id INTEGER PRIMARY KEY);",
    )
    .expect("write migration");
    let mut client = LspClient::start(dir.path());

    let query_uri = file_uri(&dir.path().join("q.sql"));
    client.open(
        &query_uri,
        "WITH r AS (SELECT id FROM users) SELECT id FROM r",
    );

    // Renaming the CTE touches only its definition and use in this file.
    let edit = client.request(
        "textDocument/rename",
        json!({
            "textDocument": { "uri": query_uri },
            "position": { "line": 0, "character": 5 },
            "newName": "recent",
        }),
    );
    let changes = edit["changes"].as_object().expect("changes map");
    assert_eq!(changes.len(), 1, "{changes:?}");
    let edits = changes[&query_uri].as_array().expect("edits");
    let starts: Vec<_> = edits
        .iter()
        .map(|edit| edit["range"]["start"]["character"].as_u64())
        .collect();
    assert_eq!(starts, vec![Some(5), Some(48)], "{edits:?}");
}

#[test]
fn reloads_report_work_done_progress() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut client = LspClient::start_with_capabilities(
        dir.path(),
        json!({ "window": { "workDoneProgress": true } }),
    );

    // Observe every message of the initial load: a progress begin and end
    // must arrive before the load summary.
    let mut kinds = Vec::new();
    loop {
        let message = client.next_message();
        if message["method"] == "$/progress"
            && let Some(kind) = message["params"]["value"]["kind"].as_str()
        {
            kinds.push(kind.to_owned());
        }
        let is_summary = message["method"] == "window/logMessage"
            && message["params"]["message"]
                .as_str()
                .is_some_and(|text| text.contains("workspace-wide index holds"));
        if is_summary {
            break;
        }
    }
    assert!(kinds.contains(&"begin".to_owned()), "{kinds:?}");

    // The end notification may arrive just after the summary; drain until
    // it shows up.
    while !kinds.contains(&"end".to_owned()) {
        let message = client.next_message();
        if message["method"] == "$/progress"
            && let Some(kind) = message["params"]["value"]["kind"].as_str()
        {
            kinds.push(kind.to_owned());
        }
    }
}

#[test]
fn incremental_changes_update_the_document() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(dir.path().join("migrations")).expect("mkdir");
    std::fs::write(
        dir.path().join("migrations").join("1_users.sql"),
        "CREATE TABLE users (id INTEGER PRIMARY KEY, email TEXT NOT NULL);",
    )
    .expect("write migration");
    let mut client = LspClient::start(dir.path());

    let query_uri = file_uri(&dir.path().join("q.sql"));
    client.open(&query_uri, "SELECT id FROM users");

    // Replace `id` with `email` via a ranged change.
    client.change_range(&query_uri, 2, (0, 7), (0, 9), "email");
    let hover = client.hover_text(&query_uri, 0, 8);
    assert!(hover.contains("users.email TEXT NOT NULL"), "{hover}");

    // A second ranged change appends a WHERE clause; completion after the
    // rewritten text still sees the document consistently.
    client.change_range(&query_uri, 3, (0, 23), (0, 23), " WHERE ");
    let labels = client.completion_labels(&query_uri, 0, 30);
    assert!(labels.contains(&"email".to_owned()), "{labels:?}");
}

#[test]
fn semantic_token_deltas_and_ranges_follow_edits() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(dir.path().join("migrations")).expect("mkdir");
    std::fs::write(
        dir.path().join("migrations").join("1_users.sql"),
        "CREATE TABLE users (id INTEGER PRIMARY KEY);",
    )
    .expect("write migration");
    let mut client = LspClient::start(dir.path());

    let query_uri = file_uri(&dir.path().join("q.sql"));
    client.open(&query_uri, "SELECT id FROM users");

    let full = client.request(
        "textDocument/semanticTokens/full",
        json!({ "textDocument": { "uri": query_uri } }),
    );
    let result_id = full["resultId"].as_str().expect("result id").to_owned();
    let mut data: Vec<u64> = full["data"]
        .as_array()
        .expect("data")
        .iter()
        .filter_map(serde_json::Value::as_u64)
        .collect();
    assert!(!data.is_empty());

    // Edit the document; the delta relative to the previous result id must
    // patch the old stream into the new one.
    client.change(&query_uri, 2, "SELECT id FROM users WHERE id = 1");
    let delta = client.request(
        "textDocument/semanticTokens/full/delta",
        json!({
            "textDocument": { "uri": query_uri },
            "previousResultId": result_id,
        }),
    );
    let edits = delta["edits"].as_array().expect("delta edits");
    for edit in edits {
        let start = edit["start"].as_u64().expect("start") as usize;
        let delete_count = edit["deleteCount"].as_u64().expect("deleteCount") as usize;
        let inserted: Vec<u64> = edit["data"]
            .as_array()
            .map(|values| {
                values
                    .iter()
                    .filter_map(serde_json::Value::as_u64)
                    .collect()
            })
            .unwrap_or_default();
        data.splice(start..start + delete_count, inserted);
    }

    let fresh = client.request(
        "textDocument/semanticTokens/full",
        json!({ "textDocument": { "uri": query_uri } }),
    );
    let fresh_data: Vec<u64> = fresh["data"]
        .as_array()
        .expect("data")
        .iter()
        .filter_map(serde_json::Value::as_u64)
        .collect();
    assert_eq!(data, fresh_data);

    // A range request returns only the tokens inside the range.
    let ranged = client.request(
        "textDocument/semanticTokens/range",
        json!({
            "textDocument": { "uri": query_uri },
            "range": {
                "start": { "line": 0, "character": 21 },
                "end": { "line": 0, "character": 26 },
            },
        }),
    );
    let ranged_len = ranged["data"].as_array().expect("data").len();
    assert!(ranged_len > 0, "range request returned no tokens");
    assert!(ranged_len < fresh_data.len(), "range did not filter");
}

#[test]
fn workspace_folder_changes_rebuild_contexts_mid_session() {
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

    let mut client = LspClient::start(dir_a.path());

    // A document in folder B, which is not part of the workspace yet: it is
    // served by the workspace-wide fallback, which only knows folder A.
    let query_uri = file_uri(&dir_b.path().join("q.sql"));
    client.open(&query_uri, "SELECT id FROM ");
    let labels = client.completion_labels(&query_uri, 0, 15);
    assert!(labels.contains(&"users".to_owned()), "{labels:?}");
    assert!(!labels.contains(&"posts".to_owned()), "{labels:?}");

    // The user adds folder B to the workspace.
    client.notify(
        "workspace/didChangeWorkspaceFolders",
        json!({
            "event": {
                "added": [{ "uri": file_uri(dir_b.path()), "name": "b" }],
                "removed": [],
            }
        }),
    );
    client.wait_for_load();

    // The document now belongs to folder B's context, which is isolated
    // from folder A.
    let labels = client.completion_labels(&query_uri, 0, 15);
    assert!(labels.contains(&"posts".to_owned()), "{labels:?}");
    assert!(!labels.contains(&"users".to_owned()), "{labels:?}");

    // Removing folder A drops its schema from the workspace-wide view too.
    client.notify(
        "workspace/didChangeWorkspaceFolders",
        json!({
            "event": {
                "added": [],
                "removed": [{ "uri": file_uri(dir_a.path()), "name": "a" }],
            }
        }),
    );
    client.wait_for_load();

    let in_a_uri = file_uri(&dir_a.path().join("q.sql"));
    client.open(&in_a_uri, "SELECT id FROM ");
    let labels = client.completion_labels(&in_a_uri, 0, 15);
    assert!(!labels.contains(&"users".to_owned()), "{labels:?}");
}

#[tokio::test]
async fn env_changes_pick_up_live_introspection_mid_session() {
    use sqlx::sqlite::SqliteConnectOptions;
    use sqlx::{ConnectOptions, Connection};

    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    std::fs::create_dir_all(root.join("migrations")).expect("mkdir");
    std::fs::write(
        root.join("migrations").join("0001_users.sql"),
        "CREATE TABLE users (id INTEGER PRIMARY KEY);",
    )
    .expect("write migration");

    // A live database that migrations know nothing about.
    let mut connection = SqliteConnectOptions::new()
        .filename(root.join("live.db"))
        .create_if_missing(true)
        .connect()
        .await
        .expect("create db");
    sqlx::query("CREATE TABLE sessions (token TEXT PRIMARY KEY, user_id INTEGER)")
        .execute(&mut connection)
        .await
        .expect("create table");
    connection.close().await.expect("close");

    let root = root.to_owned();
    tokio::task::spawn_blocking(move || {
        let mut client = LspClient::start(&root);
        let query_uri = file_uri(&root.join("q.sql"));
        client.open(&query_uri, "SELECT token FROM ");

        let labels = client.completion_labels(&query_uri, 0, 18);
        assert!(!labels.contains(&"sessions".to_owned()), "{labels:?}");

        // DATABASE_URL appears mid-session.
        let env_file = root.join(".env");
        std::fs::write(&env_file, "DATABASE_URL=sqlite://live.db\n").expect("write .env");
        client.notify(
            "workspace/didChangeWatchedFiles",
            json!({ "changes": [{ "uri": file_uri(&env_file), "type": 1 }] }),
        );
        client.wait_for_load();

        let labels = client.completion_labels(&query_uri, 0, 18);
        assert!(labels.contains(&"sessions".to_owned()), "{labels:?}");
        assert!(labels.contains(&"users".to_owned()), "{labels:?}");
    })
    .await
    .expect("blocking client task");
}
