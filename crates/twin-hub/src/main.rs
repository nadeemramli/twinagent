//! Standalone hub binary — the Phase 3 VPS deployment target, and handy for
//! local development without the Tauri app.
//!
//! Config via env:
//! - `TWIN_HUB_ADDR` — bind address, default `127.0.0.1:8787`.
//! - `TWIN_HUB_DB` — SQLite path, default `twin-hub.db`; `:memory:` for none.

use std::net::SocketAddr;

use twin_hub::{HubService, Store};

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let addr: SocketAddr = std::env::var("TWIN_HUB_ADDR")
        .unwrap_or_else(|_| "127.0.0.1:8787".into())
        .parse()
        .expect("TWIN_HUB_ADDR must be host:port");
    let db = std::env::var("TWIN_HUB_DB").unwrap_or_else(|_| "twin-hub.db".into());

    let store = if db == ":memory:" {
        None
    } else {
        Some(Store::open(&db).expect("open hub database"))
    };
    let service = HubService::new(store);
    println!(
        "twin-hub {} listening on http://{addr} (db: {db}, {} sessions restored)",
        env!("CARGO_PKG_VERSION"),
        service.sessions().len()
    );

    // Housekeeping: stale-mark and prune once a minute.
    let sweeper = service.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
        loop {
            tick.tick().await;
            sweeper.sweep(chrono::Utc::now());
        }
    });

    twin_hub::server::serve(service, addr).await
}
