//! Collector internals: config from env, and a hub forwarder that buffers
//! through outages. The binary in `main.rs` is a thin loop over these.

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use twin_core::{AgentSnapshot, SourceRoot, UsageReport};

/// Runtime configuration, entirely from environment variables so the
/// systemd unit is the single place to tweak it.
#[derive(Debug, Clone)]
pub struct Config {
    /// Hub ingest endpoints to try, in priority order. One entry when
    /// `TWIN_HUB_URL` is set; otherwise localhost plus, under WSL, the
    /// Windows host (default gateway) so NAT networking works unconfigured.
    pub hub_urls: Vec<String>,
    /// Machine tag stamped on every snapshot (default `wsl`).
    pub machine: String,
    pub roots: Vec<SourceRoot>,
    /// Claude Code credentials file for the exact-usage poller.
    pub claude_creds: PathBuf,
}

impl Config {
    /// Read config from env, defaulting to this user's `~/.claude` and
    /// `~/.codex` inside the current filesystem namespace. Roots that don't
    /// exist are kept — the watcher picks them up if they appear later.
    pub fn from_env() -> Self {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        let claude = std::env::var("TWIN_CLAUDE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(&home).join(".claude/projects"));
        let codex = std::env::var("TWIN_CODEX_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(&home).join(".codex/sessions"));
        Self {
            hub_urls: match std::env::var("TWIN_HUB_URL") {
                Ok(url) => vec![url],
                Err(_) => default_hub_urls(),
            },
            machine: std::env::var("TWIN_MACHINE").unwrap_or_else(|_| "wsl".into()),
            roots: vec![SourceRoot::claude(claude), SourceRoot::codex(codex)],
            claude_creds: std::env::var("TWIN_CLAUDE_CREDS")
                .map(PathBuf::from)
                .unwrap_or_else(|_| twin_core::ClaudePlanPoller::default_creds_path()),
        }
    }
}

/// Default hub candidates. `127.0.0.1` covers same-machine hubs and WSL
/// mirrored networking; under WSL NAT networking the Windows host is only
/// reachable at the default gateway address, so that is appended as a
/// fallback. The forwarder sticks with whichever endpoint answers, so a
/// mirrored↔NAT switch needs no reconfiguration.
fn default_hub_urls() -> Vec<String> {
    let mut urls = vec!["http://127.0.0.1:17871".to_string()];
    if is_wsl() {
        if let Some(gateway) = std::fs::read_to_string("/proc/net/route")
            .ok()
            .and_then(|table| default_gateway(&table))
        {
            urls.push(format!("http://{gateway}:17871"));
        }
    }
    urls
}

fn is_wsl() -> bool {
    std::env::var_os("WSL_DISTRO_NAME").is_some()
        || std::fs::read_to_string("/proc/sys/kernel/osrelease")
            .map(|release| release.to_lowercase().contains("microsoft"))
            .unwrap_or(false)
}

/// Parse the IPv4 default gateway from `/proc/net/route` contents (address
/// fields are little-endian hex).
fn default_gateway(route_table: &str) -> Option<Ipv4Addr> {
    for line in route_table.lines().skip(1) {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 3 || fields[1] != "00000000" {
            continue;
        }
        if let Ok(gateway) = u32::from_str_radix(fields[2], 16) {
            if gateway != 0 {
                return Some(Ipv4Addr::from(gateway.swap_bytes()));
            }
        }
    }
    None
}

/// Sends snapshots to the hub, buffering the latest state per session while
/// the hub is unreachable and resending everything once it's back.
pub struct Forwarder {
    /// Candidate hub base URLs in priority order; `active` indexes the one
    /// currently in use. A failure advances to the next candidate, success
    /// makes the current one sticky.
    bases: Vec<String>,
    active: usize,
    /// Latest unacknowledged snapshot per session key.
    pending: HashMap<String, AgentSnapshot>,
    /// Latest unacknowledged plan-usage report (state, so newest wins).
    pending_usage: Option<UsageReport>,
    /// Latest unacknowledged stats aggregation (state, so newest wins).
    pending_stats: Option<twin_core::UsageStats>,
    backoff: Duration,
    next_attempt: Instant,
}

const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(30);

impl Forwarder {
    pub fn new(hub_urls: &[String]) -> Self {
        let bases: Vec<String> = hub_urls
            .iter()
            .map(|url| url.trim_end_matches('/').to_string())
            .collect();
        assert!(!bases.is_empty(), "at least one hub URL required");
        Self {
            bases,
            active: 0,
            pending: HashMap::new(),
            pending_usage: None,
            pending_stats: None,
            backoff: BACKOFF_MIN,
            next_attempt: Instant::now(),
        }
    }

    /// Queue snapshots (newest wins per session) and try to flush.
    pub fn send(&mut self, snapshots: Vec<AgentSnapshot>) {
        for s in snapshots {
            let key = format!("{}/{}/{}", s.machine, s.source, s.agent_id);
            self.pending.insert(key, s);
        }
        self.flush();
    }

    /// Queue the machine's plan-usage report (newest wins) and try to flush.
    /// The hub dedupes unchanged reports, so a fixed cadence is fine.
    pub fn send_usage(&mut self, report: UsageReport) {
        self.pending_usage = Some(report);
        self.flush();
    }

    /// Queue the machine's stats aggregation (newest wins) and try to flush.
    pub fn send_stats(&mut self, stats: twin_core::UsageStats) {
        self.pending_stats = Some(stats);
        self.flush();
    }

    /// How many snapshots are waiting on the hub to come back.
    pub fn pending(&self) -> usize {
        self.pending.len()
    }

    fn flush(&mut self) {
        if (self.pending.is_empty() && self.pending_usage.is_none() && self.pending_stats.is_none())
            || Instant::now() < self.next_attempt
        {
            return;
        }

        let base = &self.bases[self.active];
        let endpoint = format!("{base}/v1/snapshots");
        let usage_endpoint = format!("{base}/v1/usage");
        let stats_endpoint = format!("{base}/v1/stats");
        let result = (|| -> Result<(), ureq::Error> {
            if !self.pending.is_empty() {
                let batch: Vec<&AgentSnapshot> = self.pending.values().collect();
                ureq::post(&endpoint)
                    .timeout(Duration::from_secs(5))
                    .send_json(&batch)?;
                self.pending.clear();
            }
            if let Some(report) = &self.pending_usage {
                ureq::post(&usage_endpoint)
                    .timeout(Duration::from_secs(5))
                    .send_json(report)?;
                self.pending_usage = None;
            }
            if let Some(stats) = &self.pending_stats {
                ureq::post(&stats_endpoint)
                    .timeout(Duration::from_secs(5))
                    .send_json(stats)?;
                self.pending_stats = None;
            }
            Ok(())
        })();

        match result {
            Ok(()) => self.backoff = BACKOFF_MIN,
            Err(err) => {
                // Keep the buffers; try the next candidate endpoint after a
                // backoff. The hub restarting is normal (the Tauri app owns
                // it), and which address reaches it depends on the WSL
                // networking mode.
                eprintln!(
                    "twin-collector: hub unreachable at {base} ({err}); {} snapshot(s) buffered, retrying in {:?}",
                    self.pending.len(),
                    self.backoff
                );
                self.active = (self.active + 1) % self.bases.len();
                self.next_attempt = Instant::now() + self.backoff;
                self.backoff = (self.backoff * 2).min(BACKOFF_MAX);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gateway_parsed_from_route_table() {
        // Real /proc/net/route excerpt from WSL NAT mode: default gateway
        // 172.17.32.1 encoded little-endian as 012011AC.
        let table = "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT\n\
                     eth0\t00000000\t012011AC\t0003\t0\t0\t0\t00000000\t0\t0\t0\n\
                     eth0\t002011AC\t00000000\t0001\t0\t0\t0\t00F0FFFF\t0\t0\t0";
        assert_eq!(
            default_gateway(table),
            Some(Ipv4Addr::new(172, 17, 32, 1))
        );
    }

    #[test]
    fn no_default_route_yields_none() {
        let table = "Iface\tDestination\tGateway \tFlags\n\
                     eth0\t002011AC\t00000000\t0001\t0\t0\t0\t00F0FFFF\t0\t0\t0";
        assert_eq!(default_gateway(table), None);
    }

    #[test]
    fn zero_gateway_default_route_ignored() {
        // Point-to-point default routes have gateway 00000000 — not a host
        // address we can POST to.
        let table = "Iface\tDestination\tGateway \tFlags\n\
                     eth0\t00000000\t00000000\t0001\t0\t0\t0\t00000000\t0\t0\t0";
        assert_eq!(default_gateway(table), None);
    }
}
