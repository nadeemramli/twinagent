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
use twin_core::{ClaudePlanPoller, Pipeline};

fn main() {
    let config = Config::from_env();
    println!(
        "twin-collector {} — machine: {}, hub candidates: {:?}, roots: {:?}",
        env!("CARGO_PKG_VERSION"),
        config.machine,
        config.hub_urls,
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
    let mut forwarder = Forwarder::new(&config.hub_urls);
    // Exact account-level Claude windows via the user's own OAuth token;
    // internally rate-limited, so ticking every usage cadence is fine.
    let mut plan_poller = ClaudePlanPoller::new(config.claude_creds.clone());

    let usage_cadence = Duration::from_secs(30);
    let stats_cadence = Duration::from_secs(300);
    let mut next_usage = std::time::Instant::now();
    let mut next_stats = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        let snapshots = pipeline.poll(Duration::from_millis(500));
        // send() also flushes anything buffered from an earlier outage.
        forwarder.send(snapshots);

        // Plan usage on a fixed cadence; the hub dedupes unchanged reports.
        if std::time::Instant::now() >= next_usage {
            let now = chrono::Utc::now();
            let mut report = pipeline.usage_report(now);
            report.claude_exact = plan_poller.tick(now);
            forwarder.send_usage(report);
            next_usage = std::time::Instant::now() + usage_cadence;
        }

        // Historical stats less often — the pane fetches on open, so this
        // just keeps the hub's copy fresh. First send waits out the boot
        // re-read so it isn't a partial aggregation.
        if std::time::Instant::now() >= next_stats {
            forwarder.send_stats(pipeline.usage_stats(chrono::Utc::now()));
            next_stats = std::time::Instant::now() + stats_cadence;
        }
    }
}
