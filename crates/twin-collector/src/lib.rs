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
        let snapshots_endpoint = format!("{base}/v1/snapshots");
        let usage_endpoint = format!("{base}/v1/usage");
        let stats_endpoint = format!("{base}/v1/stats");

        // The three POSTs are independent. Snapshots are the product's
        // lifeblood and the ONLY payload that drives endpoint rotation and
        // backoff: a stuck usage/stats POST (schema skew, a flaky 5xx) must
        // never starve snapshot delivery or knock us off a working hub
        // (BUGHUNT #4).
        let mut snapshots_failed = false;
        if !self.pending.is_empty() {
            let batch: Vec<&AgentSnapshot> = self.pending.values().collect();
            match ureq::post(&snapshots_endpoint)
                .timeout(Duration::from_secs(5))
                .send_json(&batch)
            {
                Ok(_) => self.pending.clear(),
                // A 4xx means the hub rejected this batch's shape; retrying
                // the identical bytes forever would wedge all delivery, so
                // drop it (BUGHUNT #5). Transport errors and 5xx are
                // transient — keep the batch buffered and fail over.
                Err(ureq::Error::Status(code, _)) if (400..500).contains(&code) => {
                    eprintln!(
                        "twin-collector: hub rejected {} snapshot(s) with {code} at {base}; dropping the batch",
                        self.pending.len()
                    );
                    self.pending.clear();
                }
                Err(err) => {
                    snapshots_failed = true;
                    eprintln!(
                        "twin-collector: hub unreachable at {base} ({err}); {} snapshot(s) buffered, retrying in {:?}",
                        self.pending.len(),
                        self.backoff
                    );
                }
            }
        }

        // Usage/stats use the same drop-on-4xx / keep-on-transient split, but
        // isolated: a failure here leaves snapshot progress, the active
        // endpoint, and the backoff untouched.
        if let Some(report) = &self.pending_usage {
            match ureq::post(&usage_endpoint)
                .timeout(Duration::from_secs(5))
                .send_json(report)
            {
                Ok(_) => self.pending_usage = None,
                Err(ureq::Error::Status(code, _)) if (400..500).contains(&code) => {
                    eprintln!("twin-collector: hub rejected usage report with {code} at {base}; dropping it");
                    self.pending_usage = None;
                }
                Err(err) => {
                    eprintln!("twin-collector: usage POST to {base} failed ({err}); keeping it buffered");
                }
            }
        }
        if let Some(stats) = &self.pending_stats {
            match ureq::post(&stats_endpoint)
                .timeout(Duration::from_secs(5))
                .send_json(stats)
            {
                Ok(_) => self.pending_stats = None,
                Err(ureq::Error::Status(code, _)) if (400..500).contains(&code) => {
                    eprintln!("twin-collector: hub rejected stats with {code} at {base}; dropping it");
                    self.pending_stats = None;
                }
                Err(err) => {
                    eprintln!("twin-collector: stats POST to {base} failed ({err}); keeping it buffered");
                }
            }
        }

        if snapshots_failed {
            // Try the next candidate endpoint after a backoff. The hub
            // restarting is normal (the Tauri app owns it), and which address
            // reaches it depends on the WSL networking mode.
            self.active = (self.active + 1) % self.bases.len();
            self.next_attempt = Instant::now() + self.backoff;
            self.backoff = (self.backoff * 2).min(BACKOFF_MAX);
        } else {
            self.backoff = BACKOFF_MIN;
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

    // Independent-flush behavior (BUGHUNT #4/#5): a tiny blocking mock hub
    // that answers each path with a configured status code lets us drive the
    // Forwarder without a tokio runtime (it POSTs with blocking ureq).
    mod flush {
        use super::*;
        use std::io::{BufRead, BufReader, Read, Write};
        use std::net::TcpListener;
        use std::sync::{Arc, Mutex};

        struct MockHub {
            base: String,
            hits: Arc<Mutex<Vec<String>>>,
        }

        impl MockHub {
            fn hits(&self, path_prefix: &str) -> usize {
                self.hits
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|p| p.starts_with(path_prefix))
                    .count()
            }
        }

        fn spawn_mock(status_for: impl Fn(&str) -> u16 + Send + 'static) -> MockHub {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let hits = Arc::new(Mutex::new(Vec::new()));
            let hits_thread = hits.clone();
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(mut stream) = stream else { continue };
                    let Ok(clone) = stream.try_clone() else { continue };
                    let mut reader = BufReader::new(clone);
                    let mut request_line = String::new();
                    if reader.read_line(&mut request_line).is_err() {
                        continue;
                    }
                    let path = request_line
                        .split_whitespace()
                        .nth(1)
                        .unwrap_or("")
                        .to_string();
                    // Drain headers (capturing Content-Length) then the body,
                    // so the client's write completes before we respond.
                    let mut content_length = 0usize;
                    loop {
                        let mut line = String::new();
                        if reader.read_line(&mut line).unwrap_or(0) == 0 {
                            break;
                        }
                        if line == "\r\n" || line == "\n" {
                            break;
                        }
                        if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                            content_length = v.trim().parse().unwrap_or(0);
                        }
                    }
                    let mut body = vec![0u8; content_length];
                    let _ = reader.read_exact(&mut body);
                    hits_thread.lock().unwrap().push(path.clone());
                    let status = status_for(&path);
                    let reason = match status {
                        202 => "Accepted",
                        400 => "Bad Request",
                        422 => "Unprocessable Entity",
                        500 => "Internal Server Error",
                        503 => "Service Unavailable",
                        _ => "OK",
                    };
                    let response = format!(
                        "HTTP/1.1 {status} {reason}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    );
                    let _ = stream.write_all(response.as_bytes());
                    let _ = stream.flush();
                }
            });
            MockHub { base, hits }
        }

        fn snap(id: &str) -> AgentSnapshot {
            AgentSnapshot {
                agent_id: id.into(),
                source: twin_core::AgentSource::ClaudeCode,
                machine: "wsl".into(),
                project: "demo".into(),
                status: twin_core::AgentStatus::Done,
                git_branch: None,
                current_task: None,
                needs_user: false,
                needs_user_reason: None,
                usage: twin_core::UsageMetrics::default(),
                last_activity: "2026-07-10T09:00:00Z".into(),
                jump: None,
            }
        }

        fn usage_report() -> UsageReport {
            UsageReport {
                machine: "wsl".into(),
                claude: None,
                claude_exact: None,
                codex: None,
                reported_at: "2026-07-10T09:00:00Z".into(),
            }
        }

        #[test]
        fn usage_failure_does_not_starve_snapshots_or_rotate() {
            // Usage 5xx, snapshots fine.
            let hub = spawn_mock(|path| if path.starts_with("/v1/usage") { 500 } else { 202 });
            let mut fwd = Forwarder::new(&[hub.base.clone(), "http://127.0.0.1:9".into()]);
            fwd.send(vec![snap("a")]);
            fwd.send_usage(usage_report());

            // Snapshots delivered and cleared despite the failing usage POST.
            assert_eq!(fwd.pending(), 0, "snapshots must flush even when usage fails");
            assert!(hub.hits("/v1/snapshots") >= 1);
            // Usage stays buffered for a later retry, isolated.
            assert!(fwd.pending_usage.is_some(), "failed usage stays buffered");
            // And we did not rotate off the working endpoint or back off.
            assert_eq!(fwd.active, 0, "usage failure must not rotate the endpoint");
            assert_eq!(fwd.backoff, BACKOFF_MIN);
        }

        #[test]
        fn snapshots_4xx_drops_the_batch() {
            let hub = spawn_mock(|_| 422);
            let mut fwd = Forwarder::new(&[hub.base.clone()]);
            fwd.send(vec![snap("a")]);
            // A non-retryable 4xx must not buffer the poison batch forever.
            assert_eq!(fwd.pending(), 0, "a 4xx batch must be dropped, not re-queued");
            assert!(hub.hits("/v1/snapshots") >= 1);
        }

        #[test]
        fn snapshots_5xx_buffers_and_rotates() {
            let hub = spawn_mock(|_| 503);
            let mut fwd = Forwarder::new(&[hub.base.clone(), "http://127.0.0.1:9".into()]);
            fwd.send(vec![snap("a")]);
            // A transient 5xx keeps the batch and fails over to the next hub.
            assert_eq!(fwd.pending(), 1, "5xx must keep the batch for retry");
            assert_eq!(fwd.active, 1, "a snapshots failure rotates to the next endpoint");
        }
    }
}
