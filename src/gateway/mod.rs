//! The enforcement pipe: intercept a governed call, attribute it, judge it,
//! meter it. The wire (proxy/tls/ca), policy evaluation (judge/schema/scope),
//! and metering (budget/ledger) — everything between a worker's request and
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

/// The gateway's well-known default port — what boxes are pointed at via
/// HTTP(S)_PROXY and what probes fall back to when no daemon has recorded a
/// binding. `--addr` overrides where the daemon binds.
pub const PORT: u16 = 7300;

/// The addresses the daemon binds. An explicit `--addr` wins verbatim. The
/// default is deliberate: loopback for humans and CLI probes, plus the docker
/// host-gateway IP so boxes (which reach the host through
/// host.docker.internal) can connect — instead of listening on every
/// interface.
pub fn resolve_bind_addrs(addr: Option<&str>) -> Vec<String> {
    if let Some(a) = addr {
        return vec![a.to_string()];
    }
    let mut addrs = vec![format!("127.0.0.1:{PORT}")];
    match docker_host_gateway_ip() {
        Some(ip) if ip != "127.0.0.1" => addrs.push(format!("{ip}:{PORT}")),
        Some(_) => {}
        None => eprintln!(
            "note: could not read the docker bridge address — binding loopback only; \
             boxes can't reach the gateway until docker is up (restart the server then, \
             or pass --addr 0.0.0.0:{PORT})"
        ),
    }
    addrs
}

/// The default docker bridge's gateway IP — what `host.docker.internal`
/// resolves to inside a container (docker's host-gateway alias).
fn docker_host_gateway_ip() -> Option<String> {
    let out = std::process::Command::new("docker")
        .args([
            "network",
            "inspect",
            "bridge",
            "-f",
            "{{range .IPAM.Config}}{{.Gateway}}{{end}}",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let ip = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!ip.is_empty()).then_some(ip)
}

/// The port the running (or last-running) daemon bound, from its state file —
/// the well-known PORT when none has recorded one.
pub fn recorded_port() -> u16 {
    std::fs::read_to_string(crate::paths::gateway_state_file())
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| v.get("port").and_then(|p| p.as_u64()))
        .map(|p| p as u16)
        .unwrap_or(PORT)
}

/// The addresses the running (or last-running) daemon bound, when recorded.
pub fn recorded_addrs() -> Option<Vec<String>> {
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(crate::paths::gateway_state_file()).ok()?)
            .ok()?;
    Some(
        v.get("addrs")?
            .as_array()?
            .iter()
            .filter_map(|a| a.as_str())
            .map(String::from)
            .collect(),
    )
}

fn write_state(addrs: &[String]) {
    let port = addrs
        .first()
        .and_then(|a| a.rsplit(':').next())
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(PORT);
    let state = serde_json::json!({
        "port": port,
        "addrs": addrs,
        "config_root": crate::paths::config_root().display().to_string(),
        "pid": std::process::id(),
    });
    let _ = std::fs::write(crate::paths::gateway_state_file(), format!("{state}\n"));
}

/// Remove the binding record on graceful shutdown (best-effort; a killed
/// daemon leaves it behind, which probes tolerate — they always verify by
/// asking /healthz).
pub fn clear_state() {
    let _ = std::fs::remove_file(crate::paths::gateway_state_file());
}

/// Bring up the governed-egress gateway on every `addr` and return the
/// accept-loop tasks: the CA, the TLS acceptor, the upstream client, and
/// budget counters rehydrated from the current window's usage (a restart
/// never resets budgets). An accept error is logged and retried — it must
/// never take the daemon down.
pub async fn start(addrs: &[String]) -> Result<Vec<tokio::task::JoinHandle<()>>, BErr> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .ok();

    let ca = Arc::new(ca::Ca::ensure()?);
    let tls = tls::acceptor(ca.clone());
    let client = proxy::build_client();
    ledger::rehydrate(&budget::load_budget().limits);

    let mut tasks = Vec::new();
    for addr in addrs {
        let listener = TcpListener::bind(addr).await?;
        let (tls, client, ca) = (tls.clone(), client.clone(), ca.clone());
        tasks.push(tokio::spawn(async move {
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
        }));
    }
    write_state(addrs);
    Ok(tasks)
}
