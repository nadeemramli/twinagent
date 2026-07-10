//! Windows toasts on needs-you transitions (TWI-14). Subscribes to the
//! embedded hub's event stream and fires a toast on the edge into
//! needs-you, with a per-session cooldown so a flapping session doesn't
//! spam the notification center.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use tauri::{AppHandle, Manager};
use tauri_plugin_notification::NotificationExt;
use tokio::sync::broadcast::error::RecvError;
use twin_hub::{Event, HubService};

/// A session that flaps in and out of needs-you fires at most one toast
/// per this window.
const COOLDOWN: Duration = Duration::from_secs(120);

/// Managed on/off switch, flipped live from the settings pane (TWI-15).
pub struct ToastsEnabled(pub AtomicBool);

pub fn spawn(app: AppHandle, hub: HubService) {
    let mut rx = hub.subscribe();
    tauri::async_runtime::spawn(async move {
        let mut last_toast: HashMap<String, Instant> = HashMap::new();
        loop {
            match rx.recv().await {
                Ok(Event::Upsert {
                    logical_key,
                    session,
                    entered_needs_you: true,
                    ..
                }) => {
                    if !app.state::<ToastsEnabled>().0.load(Ordering::Relaxed) {
                        continue;
                    }
                    let now = Instant::now();
                    if last_toast
                        .get(&logical_key)
                        .is_some_and(|t| now.duration_since(*t) < COOLDOWN)
                    {
                        continue;
                    }
                    last_toast.insert(logical_key, now);

                    let project = session
                        .project
                        .rsplit(['/', '\\'])
                        .next()
                        .filter(|p| !p.is_empty())
                        .unwrap_or(&session.project);
                    let source = match session.source {
                        twin_core::AgentSource::ClaudeCode => "Claude Code".to_string(),
                        twin_core::AgentSource::Codex => "Codex".to_string(),
                        other => other.to_string(),
                    };
                    let title = format!("{source} needs you — {project}");
                    let body = match &session.needs_user_reason {
                        Some(reason) => format!("[{}] {reason}", session.machine),
                        None => format!("[{}] waiting on your input", session.machine),
                    };
                    if let Err(err) = app
                        .notification()
                        .builder()
                        .title(title)
                        .body(body)
                        .show()
                    {
                        eprintln!("twinagent: toast failed: {err}");
                    }
                }
                Ok(_) => {}
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => break,
            }
        }
    });
}
