//! File-watching daemon: runs inside WSL (and later on the VPS), watches the
//! agent data dirs, and POSTs normalized `AgentSnapshot`s to the hub.
//!
//! Must run on the same filesystem as the agents — inotify does not cross
//! the WSL/Windows boundary, so the Windows app can never watch `\\wsl$`
//! paths; this daemon is the WSL side's eyes. Machine tag: `wsl`.
//!
//! On startup every existing transcript is re-read from the beginning, so
//! token totals and session state are complete even after downtime; from
//! then on the tail readers keep it incremental.

use std::time::Duration;

use twin_collector::{Config, Forwarder};
use twin_core::Pipeline;

fn main() {
    let config = Config::from_env();
    println!(
        "twin-collector {} — machine: {}, hub: {}, roots: {:?}",
        env!("CARGO_PKG_VERSION"),
        config.machine,
        config.hub_url,
        config
            .roots
            .iter()
            .map(|r| r.path.display().to_string())
            .collect::<Vec<_>>(),
    );

    let mut pipeline = Pipeline::new(
        config.machine.clone(),
        config.roots.clone(),
        Duration::from_secs(5),
    );
    let mut forwarder = Forwarder::new(&config.hub_url);

    let usage_cadence = Duration::from_secs(30);
    let mut next_usage = std::time::Instant::now();
    loop {
        let snapshots = pipeline.poll(Duration::from_millis(500));
        // send() also flushes anything buffered from an earlier outage.
        forwarder.send(snapshots);

        // Plan usage on a fixed cadence; the hub dedupes unchanged reports.
        if std::time::Instant::now() >= next_usage {
            forwarder.send_usage(pipeline.usage_report(chrono::Utc::now()));
            next_usage = std::time::Instant::now() + usage_cadence;
        }
    }
}
