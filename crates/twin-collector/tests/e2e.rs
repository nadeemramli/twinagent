//! End-to-end: a real hub, the real collector binary, a growing transcript
//! on disk — the session must appear in the hub, and keep flowing after a
//! hub restart (reconnect with backoff).

use std::time::Duration;

use twin_core::AgentSnapshot;
use twin_hub::{HubService, Store};

fn claude_line(session: &str, ts: &str, stop: &str, input_tokens: u64) -> String {
    format!(
        r#"{{"type":"assistant","timestamp":"{ts}","sessionId":"{session}","cwd":"/proj","message":{{"id":"m-{input_tokens}","model":"claude-fable-5","stop_reason":"{stop}","content":[{{"type":"text","text":"hi"}}],"usage":{{"input_tokens":{input_tokens},"output_tokens":5,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}}}}"#
    )
}

async fn start_hub(addr: std::net::SocketAddr) -> tokio::task::JoinHandle<()> {
    let service = HubService::new(Some(Store::open_in_memory().unwrap()));
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, twin_hub::server::router(service)).await;
    })
}

async fn hub_sessions(client: &reqwest::Client, addr: std::net::SocketAddr) -> Vec<AgentSnapshot> {
    match client
        .get(format!("http://{addr}/v1/sessions"))
        .send()
        .await
    {
        Ok(resp) => resp.json().await.unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

#[tokio::test]
async fn collector_pushes_catches_up_and_survives_hub_restart() {
    let claude_dir = tempfile::tempdir().unwrap();
    let codex_dir = tempfile::tempdir().unwrap();

    // A transcript that already exists before the collector starts —
    // catch-up coverage.
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    std::fs::write(
        claude_dir.path().join("pre-existing.jsonl"),
        claude_line("pre-existing", &now, "end_turn", 10) + "\n",
    )
    .unwrap();

    // Reserve a port for the hub, then release it so the collector starts
    // against a hub that is NOT up yet (backoff coverage).
    let addr = {
        let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        probe.local_addr().unwrap()
    };

    let mut collector = std::process::Command::new(env!("CARGO_BIN_EXE_twin-collector"))
        .env("TWIN_HUB_URL", format!("http://{addr}"))
        .env("TWIN_MACHINE", "wsl-test")
        .env("TWIN_CLAUDE_DIR", claude_dir.path())
        .env("TWIN_CODEX_DIR", codex_dir.path())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();

    // Give the collector a moment alone (hub down, buffering), then start
    // the hub late.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let hub = start_hub(addr).await;

    let client = reqwest::Client::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let sessions = loop {
        let sessions = hub_sessions(&client, addr).await;
        if !sessions.is_empty() {
            break sessions;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "collector never delivered the pre-existing session"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    };
    assert_eq!(sessions[0].agent_id, "pre-existing");
    assert_eq!(sessions[0].machine, "wsl-test");
    assert_eq!(sessions[0].usage.input_tokens, 10);

    // Kill the hub, write more activity while it is down, bring it back:
    // the collector must reconnect and deliver the new session.
    hub.abort();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    std::fs::write(
        claude_dir.path().join("while-down.jsonl"),
        claude_line("while-down", &now, "end_turn", 42) + "\n",
    )
    .unwrap();
    tokio::time::sleep(Duration::from_millis(1200)).await;
    let _hub2 = start_hub(addr).await;

    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        let sessions = hub_sessions(&client, addr).await;
        if sessions.iter().any(|s| s.agent_id == "while-down") {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "collector did not recover after hub restart; sessions: {:?}",
            sessions.iter().map(|s| &s.agent_id).collect::<Vec<_>>()
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }

    collector.kill().unwrap();
    collector.wait().unwrap();
}
