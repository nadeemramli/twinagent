//! End-to-end: POST a snapshot at the HTTP surface, watch it arrive as a
//! WebSocket diff.

use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;
use twin_core::{AgentSnapshot, AgentSource, AgentStatus, UsageMetrics};
use twin_hub::{HubService, Store};

fn snapshot(id: &str, status: AgentStatus) -> AgentSnapshot {
    AgentSnapshot {
        agent_id: id.into(),
        source: AgentSource::ClaudeCode,
        machine: "wsl".into(),
        project: "demo".into(),
        status,
        current_task: Some("cargo test".into()),
        needs_user: matches!(status, AgentStatus::NeedsYou),
        needs_user_reason: None,
        usage: UsageMetrics::default(),
        last_activity: "2026-07-10T09:00:00Z".into(),
        jump: None,
    }
}

#[tokio::test]
async fn snapshot_flows_from_post_to_websocket_diff() {
    let service = HubService::new(Some(Store::open_in_memory().unwrap()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, twin_hub::server::router(service))
            .await
            .unwrap();
    });

    // Seed one session, then connect: the client must get it in Full.
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{addr}/v1/snapshots"))
        .json(&snapshot("s1", AgentStatus::Thinking))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202);

    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/v1/ws"))
        .await
        .unwrap();
    let first = ws.next().await.unwrap().unwrap();
    let full: serde_json::Value = serde_json::from_str(first.to_text().unwrap()).unwrap();
    assert_eq!(full["type"], "full");
    assert_eq!(full["sessions"].as_array().unwrap().len(), 1);

    // A status change arrives as an upsert diff with the needs-you edge.
    client
        .post(format!("http://{addr}/v1/snapshots"))
        .json(&vec![snapshot("s1", AgentStatus::NeedsYou)])
        .send()
        .await
        .unwrap();
    let diff = loop {
        let msg = ws.next().await.unwrap().unwrap();
        if let Message::Text(text) = msg {
            break serde_json::from_str::<serde_json::Value>(&text).unwrap();
        }
    };
    assert_eq!(diff["type"], "upsert");
    assert_eq!(diff["key"], "wsl/claude-code/s1");
    assert_eq!(diff["entered_needs_you"], true);
    assert_eq!(diff["session"]["status"], "needs_you");

    // REST view agrees.
    let sessions: Vec<AgentSnapshot> = client
        .get(format!("http://{addr}/v1/sessions"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].status, AgentStatus::NeedsYou);

    // Garbage is rejected without killing the server.
    let resp = client
        .post(format!("http://{addr}/v1/snapshots"))
        .json(&serde_json::json!({"not": "a snapshot"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 422);

    ws.send(Message::Close(None)).await.unwrap();
}
