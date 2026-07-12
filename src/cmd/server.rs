//! `roster server run` — the one daemon: the governed-egress gateway, the
//! task-dispatch loop, and the channel listeners, supervised siblings in one
//! process (one thing to start, one thing to restart after a rebuild). And
//! `roster server status` — health, computed, never model-written.

use super::BErr;
use crate::ca::Ca;
use crate::{budget, gate, ledger, proxy, queue, tls};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;

const BUILD: &str = concat!(env!("CARGO_PKG_VERSION"), " (", env!("ROSTER_BUILD"), ")");

/// The gateway's well-known port — what boxes are pointed at via HTTP(S)_PROXY
/// and what `status` probes. `--addr` overrides where `run` binds.
const GATEWAY_PORT: u16 = 7300;

pub async fn run(cap: usize, once: bool, no_listen: bool, addr: &str) -> Result<(), BErr> {
    // Refuse to boot on broken config — better loud at start than silently
    // denying everything. Mid-flight breakage fails closed instead (the
    // gateway denies, dispatch pauses) so a bad edit never kills the daemon.
    if let Err(errors) = crate::config::load() {
        for e in &errors {
            eprintln!("config: {e}");
        }
        return Err(format!("invalid config ({} error(s)) — fix and retry, or: roster server validate", errors.len()).into());
    }

    rustls::crypto::ring::default_provider().install_default().ok();

    let ca = Arc::new(Ca::ensure()?);
    let tls = tls::acceptor(ca.clone());
    let client = proxy::build_client();

    // Rebuild budget counters from the current window's usage so a restart
    // doesn't reset budgets.
    ledger::rehydrate(&budget::load_budget().limits);

    let listener = TcpListener::bind(addr).await?;
    eprintln!(
        "roster server {BUILD} — gateway on {addr}; dispatch cap {cap}{}{}",
        if once { "; once" } else { "" },
        if no_listen { "; listeners off" } else { "" }
    );

    // The gateway: accept loop, one task per connection. An accept error is
    // logged and retried — it must never take the daemon down.
    let gateway = tokio::spawn(async move {
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
    });

    // Channel listeners: one per worker that declares one ([channels] in its
    // spec, compiled at deploy). Each is supervised — an exit or error restarts
    // it with backoff and never takes the gateway or dispatch down with it.
    let mut listeners = Vec::new();
    if !no_listen && !once {
        let plan = listener_plan();
        if plan.is_empty() {
            eprintln!("listeners: none configured (a worker opts in via [channels] in its worker.toml)");
        }
        for (worker, credential) in plan {
            listeners.push(tokio::spawn(supervise_listener(worker, credential)));
        }
    }

    // Dispatch runs in the foreground: with --once it drains due work and
    // returns; otherwise it loops until the process is stopped.
    let result = crate::cmd::supervise::dispatch_loop(cap, once).await;
    gateway.abort();
    for l in listeners {
        l.abort();
    }
    result
}

/// (worker, vault credential) pairs — straight from live config.
fn listener_plan() -> Vec<(String, String)> {
    crate::config::snapshot().map(|c| c.listeners.clone()).unwrap_or_default()
}

/// `roster server validate` — parse everything, print every error.
pub fn validate() -> Result<(), BErr> {
    match crate::config::load() {
        Ok(c) => {
            println!(
                "config valid: {} worker(s) [{}], {} grant(s), {} action(s), {} trust rule(s), {} limit(s), {} trigger(s), {} listener(s)",
                c.workers.len(),
                c.workers.join(", "),
                c.policy.rules.len(),
                c.actions.actions.len(),
                c.actions.trust.len(),
                c.budget.limits.len(),
                c.triggers.len(),
                c.listeners.len(),
            );
            match &c.engine_dir {
                Some(dir) if !dir.join("box").is_dir() => {
                    println!("warning: [engine] dir {} has no box/ — agent runs will fail", dir.display())
                }
                Some(_) => {}
                None => println!("note: [engine] dir is unset — agent runs need it until pi is baked into the box image"),
            }
            Ok(())
        }
        Err(errors) => {
            for e in &errors {
                eprintln!("config: {e}");
            }
            Err(format!("{} error(s)", errors.len()).into())
        }
    }
}

async fn supervise_listener(worker: String, credential: String) {
    let mut backoff = 5u64;
    loop {
        match crate::cmd::listen::listen_worker(&worker, &credential).await {
            Ok(()) => eprintln!("listener {worker}: disconnected; reconnecting in {backoff}s"),
            Err(e) => eprintln!("listener {worker}: {e}; retrying in {backoff}s"),
        }
        tokio::time::sleep(Duration::from_secs(backoff)).await;
        backoff = (backoff * 2).min(300);
    }
}

pub async fn status(json: bool) -> Result<(), BErr> {
    // Is a gateway answering on the well-known port?
    let gateway_up = tokio::time::timeout(
        Duration::from_millis(500),
        tokio::net::TcpStream::connect(("127.0.0.1", GATEWAY_PORT)),
    )
    .await
    .map(|r| r.is_ok())
    .unwrap_or(false);

    // Config parses? (It loads live — no staleness concept.)
    let config = match crate::config::load() {
        Ok(c) => format!("valid ({} worker(s))", c.workers.len()),
        Err(errors) => format!("INVALID — {} error(s); run: roster server validate", errors.len()),
    };

    let mut queue_by_state: BTreeMap<String, usize> = BTreeMap::new();
    for t in queue::list_all() {
        *queue_by_state.entry(t.state).or_insert(0) += 1;
    }
    let gates_pending = gate::list_pending().len();
    let listeners = crate::cmd::listen::active_listeners();

    if json {
        let out = serde_json::json!({
            "build": BUILD,
            "gateway": { "port": GATEWAY_PORT, "up": gateway_up },
            "config": config,
            "queue": queue_by_state,
            "gates_pending": gates_pending,
            "listeners": listeners.iter().map(|(worker, pid, since, alive)| serde_json::json!({
                "worker": worker, "pid": pid, "since": since, "alive": alive,
            })).collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    println!("roster {BUILD}");
    println!(
        "gateway    {}",
        if gateway_up {
            format!("up on :{GATEWAY_PORT}")
        } else {
            format!("DOWN (nothing on :{GATEWAY_PORT}) — run: roster server run")
        }
    );
    println!("config     {config}");
    let queue_line = if queue_by_state.is_empty() {
        "empty".to_string()
    } else {
        queue_by_state
            .iter()
            .map(|(state, n)| format!("{n} {state}"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    println!("queue      {queue_line}");
    println!(
        "gates      {}",
        if gates_pending == 0 {
            "none pending".to_string()
        } else {
            format!("{gates_pending} PENDING — review: roster server gates ls")
        }
    );
    if listeners.is_empty() {
        println!("listeners  none");
    } else {
        for (worker, pid, since, alive) in listeners {
            println!(
                "listener   {worker}: {} (pid {pid}, since {since})",
                if alive { "up" } else { "STALE LOCK — process gone" }
            );
        }
    }
    Ok(())
}
