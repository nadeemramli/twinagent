//! HTTP + WebSocket surface over [`HubService`].
//!
//! Localhost-only by convention: collectors POST snapshots, the widget (and
//! any other UI) holds `GET /v1/ws`. Nothing here authenticates because
//! nothing here should ever be bound to a non-loopback address in Phase 1.
//!
//! - `POST /v1/snapshots` — one `AgentSnapshot` or an array of them.
//! - `GET  /v1/sessions` — current full state as JSON.
//! - `GET  /v1/ws` — push channel: one `Event::Full`, then diffs.

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use twin_core::AgentSnapshot;

use crate::{Event, HubService};

/// Build the hub router; embed this in any axum app or serve it standalone.
pub fn router(service: HubService) -> Router {
    Router::new()
        .route("/v1/snapshots", post(ingest_snapshots))
        .route("/v1/sessions", get(list_sessions))
        .route("/v1/ws", get(ws_upgrade))
        .with_state(service)
}

/// Bind and serve until the process dies. The embedded (Tauri) case spawns
/// this on its own task.
pub async fn serve(service: HubService, addr: std::net::SocketAddr) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, router(service)).await
}

async fn ingest_snapshots(
    State(service): State<HubService>,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    // Accept a single snapshot or a batch; collectors send whatever they
    // have ready.
    let snapshots: Vec<AgentSnapshot> = match body {
        Value::Array(_) => match serde_json::from_value(body) {
            Ok(list) => list,
            Err(e) => return (StatusCode::UNPROCESSABLE_ENTITY, e.to_string()),
        },
        _ => match serde_json::from_value(body) {
            Ok(one) => vec![one],
            Err(e) => return (StatusCode::UNPROCESSABLE_ENTITY, e.to_string()),
        },
    };
    let accepted = snapshots.len();
    for snapshot in snapshots {
        service.ingest(snapshot);
    }
    (StatusCode::ACCEPTED, format!("{accepted} accepted"))
}

async fn list_sessions(State(service): State<HubService>) -> Json<Vec<AgentSnapshot>> {
    Json(service.sessions())
}

async fn ws_upgrade(
    State(service): State<HubService>,
    upgrade: WebSocketUpgrade,
) -> impl IntoResponse {
    upgrade.on_upgrade(move |socket| ws_session(service, socket))
}

async fn ws_session(service: HubService, socket: WebSocket) {
    let (mut sink, mut stream) = socket.split();

    // Subscribe BEFORE snapshotting the full state so no diff in between is
    // lost; a duplicate upsert after Full is harmless.
    let mut rx = service.subscribe();
    let full = Event::Full {
        sessions: service.sessions(),
    };
    let Ok(text) = serde_json::to_string(&full) else {
        return;
    };
    if sink.send(Message::Text(text.into())).await.is_err() {
        return;
    }

    loop {
        tokio::select! {
            event = rx.recv() => {
                let text = match event {
                    Ok(event) => serde_json::to_string(&event).ok(),
                    // Lagged: this client missed diffs — resync with a Full.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        rx = rx.resubscribe();
                        serde_json::to_string(&Event::Full {
                            sessions: service.sessions(),
                        })
                        .ok()
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                };
                let Some(text) = text else { continue };
                if sink.send(Message::Text(text.into())).await.is_err() {
                    break;
                }
            }
            inbound = stream.next() => {
                match inbound {
                    // The UI never sends anything meaningful; answer pings,
                    // drop the rest.
                    Some(Ok(Message::Ping(payload))) => {
                        let _ = sink.send(Message::Pong(payload)).await;
                    }
                    Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                    Some(Ok(_)) => {}
                }
            }
        }
    }
}
