//! Embedded collector: run a [`twin_core::Pipeline`] on a background thread
//! and feed snapshots straight into a [`HubService`] — no HTTP hop.
//!
//! This is how the Tauri app watches the Windows-side agent dirs (TWI-8):
//! the app IS the Windows collector. The WSL side stays a separate daemon
//! because file events cannot cross the WSL/Windows boundary.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use twin_core::{Pipeline, SourceRoot};

use crate::HubService;

/// Handle to a running embedded collector; dropping it does NOT stop the
/// thread — call [`EmbeddedCollector::stop`] for a clean shutdown.
pub struct EmbeddedCollector {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl EmbeddedCollector {
    /// Watch `roots`, tagging snapshots with `machine`, ingesting into
    /// `service`. Roots that don't exist yet are picked up when they appear.
    pub fn spawn(service: HubService, machine: impl Into<String>, roots: Vec<SourceRoot>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = stop.clone();
        let machine = machine.into();
        let thread = std::thread::Builder::new()
            .name("twin-embedded-collector".into())
            .spawn(move || {
                let mut pipeline = Pipeline::new(machine, roots, Duration::from_secs(5));
                // Exact Claude plan windows from this machine's own Claude
                // Code credentials (internally rate-limited).
                let mut plan_poller =
                    twin_core::ClaudePlanPoller::new(twin_core::ClaudePlanPoller::default_creds_path());
                let usage_cadence = Duration::from_secs(30);
                let mut next_usage = std::time::Instant::now();
                while !stop_flag.load(Ordering::Relaxed) {
                    for snapshot in pipeline.poll(Duration::from_millis(500)) {
                        service.ingest(snapshot);
                    }
                    // Plan usage on a fixed cadence; the hub dedupes
                    // unchanged reports.
                    if std::time::Instant::now() >= next_usage {
                        let now = chrono::Utc::now();
                        let mut report = pipeline.usage_report(now);
                        report.claude_exact = plan_poller.tick(now);
                        service.report_usage(report);
                        next_usage = std::time::Instant::now() + usage_cadence;
                    }
                }
            })
            .expect("spawn embedded collector thread");
        Self {
            stop,
            thread: Some(thread),
        }
    }

    /// Signal the thread and wait for it to finish (returns within one poll
    /// interval).
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Store;

    #[test]
    fn embedded_collector_feeds_the_hub_directly() {
        let dir = tempfile::tempdir().unwrap();
        let service = HubService::new(Some(Store::open_in_memory().unwrap()));
        let collector = EmbeddedCollector::spawn(
            service.clone(),
            "windows",
            vec![SourceRoot::claude(dir.path())],
        );

        let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        std::fs::write(
            dir.path().join("sess-9.jsonl"),
            format!(
                r#"{{"type":"assistant","timestamp":"{now}","sessionId":"sess-9","cwd":"C:\\proj","message":{{"id":"m1","model":"claude-fable-5","stop_reason":"end_turn","content":[{{"type":"text","text":"hi"}}],"usage":{{"input_tokens":7,"output_tokens":3,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}}}}"#
            ) + "\n",
        )
        .unwrap();

        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let sessions = service.sessions();
            if let Some(s) = sessions.first() {
                assert_eq!(s.agent_id, "sess-9");
                assert_eq!(s.machine, "windows");
                assert_eq!(s.usage.input_tokens, 7);
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "embedded collector never delivered the session"
            );
            std::thread::sleep(Duration::from_millis(100));
        }
        collector.stop();
    }
}
