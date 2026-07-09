//! File-watching daemon: runs inside WSL (and later on the VPS), watches the
//! agent data dirs, and POSTs normalized `AgentSnapshot`s to the hub.
//!
//! Must run on the same filesystem as the agents — inotify does not cross the
//! WSL/Windows boundary. Watching and HTTP push arrive with TWI-3 and TWI-9.

fn main() {
    let machine = std::env::var("TWIN_MACHINE").unwrap_or_else(|_| "wsl".into());
    println!(
        "twin-collector {} — machine tag: {machine}; watching arrives with TWI-3/TWI-9",
        env!("CARGO_PKG_VERSION")
    );
}
