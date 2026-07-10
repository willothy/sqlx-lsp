// VS Code client for sqlx-lsp: launches the server over stdio for SQL files
// and for Rust files, where the server answers only inside sqlx query macros.

const vscode = require("vscode");
const {
  LanguageClient,
  TransportKind,
} = require("vscode-languageclient/node");

/** @type {LanguageClient | undefined} */
let client;

function serverCommand() {
  const configured = vscode.workspace
    .getConfiguration("sqlx-lsp")
    .get("serverPath");
  return typeof configured === "string" && configured.length > 0
    ? configured
    : "sqlx-lsp";
}

async function startClient() {
  const command = serverCommand();
  client = new LanguageClient(
    "sqlx-lsp",
    "sqlx LSP",
    { command, transport: TransportKind.stdio },
    {
      documentSelector: [
        { scheme: "file", language: "sql" },
        { scheme: "file", language: "rust" },
      ],
    },
  );
  try {
    await client.start();
  } catch (error) {
    client = undefined;
    const message =
      `Failed to start sqlx-lsp (\`${command}\`): ${error.message ?? error}. ` +
      "Install it with `cargo install --git https://github.com/willothy/sqlx-lsp` " +
      "or download a binary from the project's GitHub releases, then set " +
      "`sqlx-lsp.serverPath` if it is not on PATH.";
    void vscode.window.showErrorMessage(message);
  }
}

async function stopClient() {
  if (client) {
    const stopping = client;
    client = undefined;
    await stopping.stop();
  }
}

exports.activate = async function activate(context) {
  context.subscriptions.push(
    vscode.workspace.onDidChangeConfiguration(async (event) => {
      if (event.affectsConfiguration("sqlx-lsp.serverPath")) {
        await stopClient();
        await startClient();
      }
    }),
    { dispose: () => stopClient() },
  );
  await startClient();
};

exports.deactivate = function deactivate() {
  return stopClient();
};
