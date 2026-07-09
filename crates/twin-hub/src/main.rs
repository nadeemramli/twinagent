//! Standalone hub binary — the Phase 3 VPS deployment target.
//! For now it only proves the crate wires up; the HTTP/WebSocket surface
//! arrives with TWI-7.

fn main() {
    let hub = twin_hub::Hub::new();
    println!(
        "twin-hub {} — standalone mode (Phase 3); {} sessions",
        env!("CARGO_PKG_VERSION"),
        hub.len()
    );
}
