//! Roster gateway (Rust) — the trusted core. Port in progress (docs/rust-port.md).
//! P1: TLS-terminating forward proxy. Judge/vault/metering land in P2–P4.

mod ca;
mod judge;
mod providers;
mod proxy;
mod schema;
mod tls;
mod util;
mod vault;

use ca::Ca;
use std::sync::Arc;
use tokio::net::TcpListener;

const ADDR: &str = "0.0.0.0:7300";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    rustls::crypto::ring::default_provider().install_default().ok();

    let ca = Arc::new(Ca::ensure()?);
    let tls = tls::acceptor(ca.clone());
    let client = proxy::build_client();

    let listener = TcpListener::bind(ADDR).await?;
    eprintln!("roster-gateway listening on {ADDR} (P1: TLS-terminating forward proxy)");

    loop {
        let (stream, _peer) = listener.accept().await?;
        let (tls, client, ca) = (tls.clone(), client.clone(), ca.clone());
        tokio::spawn(proxy::serve(stream, tls, client, ca));
    }
}
