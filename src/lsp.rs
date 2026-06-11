//! A small Language Server Protocol client driving `sqls` (a SQL language
//! server) over stdio. We spawn one server per database connection, generate
//! an `sqls` config from the [`ConnectionConfig`] so completions are
//! schema-aware, and expose async `request` / fire-and-forget `notify` plus a
//! channel of diagnostics for the UI.
//!
//! Framing is LSP's `Content-Length` header + JSON body. Requests carry an id
//! and are matched to responses via a pending-map of oneshot channels; the
//! reader task also forwards `publishDiagnostics` notifications.

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use lsp_types::{CompletionResponse, PublishDiagnosticsParams};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot};

use crate::db::ConnectionConfig;

type Pending = Arc<Mutex<HashMap<i64, oneshot::Sender<Result<Value, Value>>>>>;

struct Inner {
    out_tx: mpsc::UnboundedSender<String>,
    pending: Pending,
    next_id: AtomicI64,
}

/// A cheap-to-clone handle to a running `sqls` instance.
#[derive(Clone)]
pub struct LspClient(Arc<Inner>);

impl LspClient {
    /// Spawn `sqls` for `cfg` and complete the LSP initialize handshake.
    /// Returns the client plus a receiver of diagnostics notifications.
    ///
    /// Must run inside the tokio runtime (it spawns reader/writer tasks).
    pub async fn start(
        cfg: &ConnectionConfig,
    ) -> Result<(Self, async_channel::Receiver<PublishDiagnosticsParams>), String> {
        let config_path = write_sqls_config(cfg).map_err(|e| format!("sqls config: {e}"))?;

        let mut child = Command::new("sqls")
            .arg("-config")
            .arg(&config_path)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| format!("spawn sqls: {e}"))?;

        let mut stdin = child.stdin.take().ok_or("sqls stdin unavailable")?;
        let stdout = child.stdout.take().ok_or("sqls stdout unavailable")?;

        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<String>();
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let (diag_tx, diag_rx) = async_channel::unbounded::<PublishDiagnosticsParams>();

        // Writer task: frame and write outgoing messages; keeps the child alive.
        tokio::spawn(async move {
            let _child = child; // dropped (killed) when the task ends
            while let Some(body) = out_rx.recv().await {
                let framed = format!("Content-Length: {}\r\n\r\n{}", body.len(), body);
                if stdin.write_all(framed.as_bytes()).await.is_err() {
                    break;
                }
                let _ = stdin.flush().await;
            }
        });

        // Reader task: parse framed responses + notifications.
        let pending_r = pending.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stdout);
            while let Some(msg) = read_message(&mut reader).await {
                dispatch(msg, &pending_r, &diag_tx).await;
            }
            // Server gone: fail any in-flight requests.
            let mut map = pending_r.lock().unwrap();
            for (_, tx) in map.drain() {
                let _ = tx.send(Err(json!({"message": "language server closed"})));
            }
        });

        let client = LspClient(Arc::new(Inner {
            out_tx,
            pending,
            next_id: AtomicI64::new(1),
        }));

        // Handshake: initialize -> initialized.
        let root = uri_for_root();
        let _: Value = client
            .request(
                "initialize",
                json!({
                    "processId": std::process::id(),
                    "rootUri": root,
                    "capabilities": {
                        "textDocument": {
                            "completion": { "completionItem": { "snippetSupport": false } },
                            "publishDiagnostics": {}
                        }
                    }
                }),
            )
            .await?;
        client.notify("initialized", json!({}));

        Ok((client, diag_rx))
    }

    /// Send a request and await the typed result.
    pub async fn request<P, R>(&self, method: &str, params: P) -> Result<R, String>
    where
        P: Serialize,
        R: DeserializeOwned,
    {
        let id = self.0.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.0.pending.lock().unwrap().insert(id, tx);

        let body = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        self.0
            .out_tx
            .send(body.to_string())
            .map_err(|_| "language server not running".to_string())?;

        match rx.await {
            Ok(Ok(v)) => serde_json::from_value(v).map_err(|e| format!("decode {method}: {e}")),
            Ok(Err(e)) => Err(format!("{method} failed: {e}")),
            Err(_) => Err(format!("{method}: response dropped")),
        }
    }

    /// Send a notification (no response expected).
    pub fn notify<P: Serialize>(&self, method: &str, params: P) {
        let body = json!({"jsonrpc": "2.0", "method": method, "params": params});
        let _ = self.0.out_tx.send(body.to_string());
    }

    // --- Convenience wrappers ------------------------------------------------

    pub fn did_open(&self, uri: &str, text: &str) {
        self.notify(
            "textDocument/didOpen",
            json!({"textDocument": {"uri": uri, "languageId": "sql", "version": 1, "text": text}}),
        );
    }

    pub fn did_change(&self, uri: &str, version: i64, text: &str) {
        self.notify(
            "textDocument/didChange",
            json!({
                "textDocument": {"uri": uri, "version": version},
                "contentChanges": [{"text": text}]
            }),
        );
    }

    pub fn did_close(&self, uri: &str) {
        self.notify(
            "textDocument/didClose",
            json!({"textDocument": {"uri": uri}}),
        );
    }

    /// Request completion at a zero-based (line, character) position.
    pub async fn completion(
        &self,
        uri: &str,
        line: u32,
        character: u32,
    ) -> Result<Vec<lsp_types::CompletionItem>, String> {
        let resp: Option<CompletionResponse> = self
            .request(
                "textDocument/completion",
                json!({
                    "textDocument": {"uri": uri},
                    "position": {"line": line, "character": character}
                }),
            )
            .await?;
        Ok(match resp {
            Some(CompletionResponse::Array(items)) => items,
            Some(CompletionResponse::List(list)) => list.items,
            None => Vec::new(),
        })
    }
}

/// Route one incoming message to the matching request or the diagnostics channel.
async fn dispatch(
    msg: Value,
    pending: &Pending,
    diag_tx: &async_channel::Sender<PublishDiagnosticsParams>,
) {
    // A response carries an id and no method.
    if let Some(id) = msg.get("id").and_then(Value::as_i64) {
        if msg.get("method").is_none() {
            if let Some(tx) = pending.lock().unwrap().remove(&id) {
                let result = match msg.get("error") {
                    Some(err) => Err(err.clone()),
                    None => Ok(msg.get("result").cloned().unwrap_or(Value::Null)),
                };
                let _ = tx.send(result);
            }
            return;
        }
        // Server-to-client request: sqls doesn't rely on these; ignore.
        return;
    }

    if let Some("textDocument/publishDiagnostics") = msg.get("method").and_then(Value::as_str)
        && let Some(params) = msg.get("params").cloned()
        && let Ok(p) = serde_json::from_value::<PublishDiagnosticsParams>(params)
    {
        let _ = diag_tx.send(p).await;
    }
}

/// Read one `Content-Length`-framed JSON message. Returns `None` at EOF.
async fn read_message<R: AsyncReadExt + Unpin>(reader: &mut R) -> Option<Value> {
    // Read headers line by line until a blank line.
    let mut content_length: Option<usize> = None;
    let mut line = Vec::new();
    loop {
        line.clear();
        // Read a single line terminated by \n.
        loop {
            let mut byte = [0u8; 1];
            match reader.read_exact(&mut byte).await {
                Ok(_) => {
                    line.push(byte[0]);
                    if byte[0] == b'\n' {
                        break;
                    }
                }
                Err(_) => return None,
            }
        }
        let text = String::from_utf8_lossy(&line);
        let trimmed = text.trim_end();
        if trimmed.is_empty() {
            break; // end of headers
        }
        if let Some(value) = trimmed.strip_prefix("Content-Length:") {
            content_length = value.trim().parse().ok();
        }
    }

    let len = content_length?;
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf).await.ok()?;
    serde_json::from_slice(&buf).ok()
}

/// A synthetic root URI for the in-memory cell documents.
fn uri_for_root() -> String {
    format!("file://{}/sqweel", std::env::temp_dir().display())
}

/// Build a per-cell document URI.
pub fn cell_uri(id: u64) -> String {
    format!("file://{}/sqweel/cell-{id}.sql", std::env::temp_dir().display())
}

/// Write an `sqls` config pointing at the given connection and return its path.
fn write_sqls_config(cfg: &ConnectionConfig) -> std::io::Result<std::path::PathBuf> {
    let path = std::env::temp_dir().join(format!("sqweel-sqls-{}.yml", std::process::id()));
    std::fs::write(&path, sqls_config_yaml(cfg))?;
    Ok(path)
}

/// Render the `sqls` YAML config for a Postgres connection.
fn sqls_config_yaml(cfg: &ConnectionConfig) -> String {
    let sslmode = if cfg.ssl { "require" } else { "disable" };
    let dsn = format!(
        "host={} port={} user={} password={} dbname={} sslmode={}",
        cfg.host, cfg.port, cfg.username, cfg.password, cfg.database, sslmode
    );
    format!(
        "connections:\n  - alias: sqweel\n    driver: postgresql\n    dataSourceName: {dsn}\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> ConnectionConfig {
        ConnectionConfig {
            driver_id: "postgres".into(),
            host: "localhost".into(),
            port: 5432,
            database: "analytics".into(),
            username: "marwa".into(),
            password: "marwa".into(),
            ssl: false,
        }
    }

    #[test]
    fn config_has_postgres_dsn() {
        let yaml = sqls_config_yaml(&cfg());
        assert!(yaml.contains("driver: postgresql"), "{yaml}");
        assert!(
            yaml.contains("host=localhost port=5432 user=marwa password=marwa dbname=analytics sslmode=disable"),
            "{yaml}"
        );
    }

    #[test]
    fn cell_uri_is_unique_per_id() {
        assert_ne!(cell_uri(1), cell_uri(2));
        assert!(cell_uri(3).ends_with("cell-3.sql"));
    }
}
