//! Headless smoke test for the LSP client: spawn sqls against the seeded DB,
//! open a document, request completion mid-query, and print the proposals.
//! No display needed.
//!
//!   cargo run --example lsp_smoke
//!
//! Exits non-zero on handshake/completion failure.

use sqweel::db::ConnectionConfig;
use sqweel::lsp::{self, LspClient};

fn main() {
    let cfg = ConnectionConfig {
        driver_id: "postgres".into(),
        host: "localhost".into(),
        port: 5432,
        database: "analytics".into(),
        username: "marwa".into(),
        password: "marwa".into(),
        ssl: false,
    };

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();

    rt.block_on(async move {
        let (client, _diags) = LspClient::start(&cfg).await.expect("LSP handshake");
        println!("lsp_smoke: initialized");

        let uri = lsp::cell_uri(1);
        // "SELECT  FROM orders" — cursor after SELECT (line 0, char 7).
        let text = "SELECT  FROM orders";
        client.did_open(&uri, text);

        // Give sqls a moment to index the document/schema.
        tokio::time::sleep(std::time::Duration::from_millis(800)).await;

        let items = client.completion(&uri, 0, 7).await.expect("completion");
        println!("lsp_smoke: {} completion items", items.len());
        for it in items.iter().take(15) {
            println!("  - {} ({:?})", it.label, it.kind);
        }
        assert!(!items.is_empty(), "expected at least one completion item");
        println!("lsp_smoke: ok");
    });
}
