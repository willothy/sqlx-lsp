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
                "capabilities": {},
                "workspaceFolders": [{ "uri": root_uri, "name": "fixture" }],
            }),
        );
        client.notify("initialized", json!({}));
        client.wait_for_load();
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
        loop {
            let message = match self.pending.pop_front() {
                Some(message) => message,
                None => self
                    .messages
                    .recv_timeout(MESSAGE_TIMEOUT)
                    .expect("server message before timeout"),
            };
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
        loop {
            let message = self.next_message();
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
    /// deterministic signal that a (re)load finished.
    fn wait_for_load(&mut self) {
        // Drain anything already buffered so a previous load's summary can't
        // satisfy this wait.
        loop {
            let message = self.next_message();
            let is_summary = message["method"] == "window/logMessage"
                && message["params"]["message"]
                    .as_str()
                    .is_some_and(|text| text.contains("workspace-wide index holds"));
            if is_summary {
                return;
            }
        }
    }

    fn open(&mut self, uri: &str, text: &str) {
        self.notify(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": uri, "languageId": "sql", "version": 1, "text": text,
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
