//! The enforcement pipe: intercept a governed call, attribute it, judge it,
//! meter it. The wire (proxy/tls/ca), policy evaluation (judge/schema/scope),
//! and metering (budget/ledger) — everything between an imp's request and
//! the world.

pub mod budget;
pub mod ca;
pub mod judge;
pub mod ledger;
pub mod proxy;
pub mod schema;
pub mod scope;
pub mod tls;

use crate::util::BErr;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;

/// The gateway's well-known port — what boxes are pointed at via HTTP(S)_PROXY
/// and what `server status` probes. `--addr` overrides where the daemon binds.
pub const PORT: u16 = 7300;

/// Bring up the governed-egress gateway on `addr` and return its accept-loop
/// task: the CA, the TLS acceptor, the upstream client, and budget counters
/// rehydrated from the current window's usage (a restart never resets
/// budgets). An accept error is logged and retried — it must never take the
/// daemon down.
pub async fn start(addr: &str) -> Result<tokio::task::JoinHandle<()>, BErr> {
    rustls::crypto::ring::default_provider().install_default().ok();

    let ca = Arc::new(ca::Ca::ensure()?);
    let tls = tls::acceptor(ca.clone());
    let client = proxy::build_client();
    ledger::rehydrate(&budget::load_budget().limits);

    let listener = TcpListener::bind(addr).await?;
    Ok(tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _peer)) => {
                    let (tls, client, ca) = (tls.clone(), client.clone(), ca.clone());
                    tokio::spawn(proxy::serve(stream, tls, client, ca));
                }
                Err(e) => {
                    eprintln!("gateway: accept failed: {e}; retrying");
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
            }
        }
    }))
}
