//! `impyard server start` — the composition root of the one daemon: bring up
//! the governed-egress gateway, the channel listeners, and the task-dispatch
//! loop as supervised siblings in one process (one thing to start, one thing
//! to restart after a rebuild). The machinery lives in its blocks; this file
//! only wires it. And `impyard server status` — health, computed, never
//! model-written.

use crate::action::gate;
use crate::util::BErr;
use crate::work::queue;
use std::collections::BTreeMap;
use std::time::Duration;

const BUILD: &str = concat!(env!("CARGO_PKG_VERSION"), " (", env!("IMPYARD_BUILD"), ")");

pub async fn run(cap: usize, once: bool, no_listen: bool, addr: &str) -> Result<(), BErr> {
    // Refuse to boot on broken config — better loud at start than silently
    // denying everything. Mid-flight breakage fails closed instead (the
    // gateway denies, dispatch pauses) so a bad edit never kills the daemon.
    if let Err(errors) = crate::config::load() {
        for e in &errors {
            eprintln!("config: {e}");
        }
        return Err(format!("invalid config ({} error(s)) — fix and retry, or: impyard server validate", errors.len()).into());
    }

    if let Ok(c) = crate::config::snapshot() {
        for w in &c.warnings {
            eprintln!("warning: {w}");
        }
    }

    let gateway = crate::gateway::start(addr).await?;
    eprintln!(
        "impyard server {BUILD} — gateway on {addr}; dispatch cap {cap}{}{}",
        if once { "; once" } else { "" },
        if no_listen { "; listeners off" } else { "" }
    );

    // Channel listeners: one supervised task per imp that declares one
    // ([channels] in its spec).
    let mut listeners = Vec::new();
    if !no_listen && !once {
        let plan = crate::channel::listen::plan();
        if plan.is_empty() {
            eprintln!("listeners: none configured (an imp opts in via [channels] in its imp.toml)");
        }
        for (imp, platform, credential) in plan {
            listeners.push(tokio::spawn(crate::channel::listen::supervised(imp, platform, credential)));
        }
    }

    // Dispatch runs in the foreground: with --once it drains due work and
    // returns; otherwise it loops until the process is stopped.
    let result = crate::work::dispatch::dispatch_loop(cap, once).await;
    gateway.abort();
    for l in listeners {
        l.abort();
    }
    result
}

/// `impyard server validate` — parse everything, print every error.
pub fn validate() -> Result<(), BErr> {
    match crate::config::load() {
        Ok(c) => {
            println!(
                "config valid: {} imp(s) [{}], {} grant(s), {} action(s), {} trust rule(s), {} limit(s), {} trigger(s), {} listener(s), {} exposure(s)",
                c.imps.len(),
                c.imps.join(", "),
                c.policy.rules.len(),
                c.actions.actions.len(),
                c.actions.trust.len(),
                c.budget.limits.len(),
                c.triggers.len(),
                c.listeners.len(),
                c.exposes.len(),
            );
            match &c.engine_dir {
                Some(dir) if !dir.join("box").is_dir() => {
                    println!("warning: [engine] dir {} has no box/ — sessions will fail", dir.display())
                }
                Some(dir) => println!("engine: dev override {} (mounted over the baked engine)", dir.display()),
                None => println!("engine: baked into the impyard-box image"),
            }
            if !c.connections.is_empty() {
                println!("connections: {}", c.connections.len());
            }
            for w in &c.warnings {
                println!("warning: {w}");
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

pub async fn status(json: bool) -> Result<(), BErr> {
    // Is a gateway answering on the well-known port?
    let gateway_up = tokio::time::timeout(
        Duration::from_millis(500),
        tokio::net::TcpStream::connect(("127.0.0.1", crate::gateway::PORT)),
    )
    .await
    .map(|r| r.is_ok())
    .unwrap_or(false);

    // Config parses? (It loads live — no staleness concept.)
    let config = match crate::config::load() {
        Ok(c) => format!("valid ({} imp(s))", c.imps.len()),
        Err(errors) => format!("INVALID — {} error(s); run: impyard server validate", errors.len()),
    };

    let mut queue_by_state: BTreeMap<String, usize> = BTreeMap::new();
    for t in queue::list_all() {
        *queue_by_state.entry(t.state).or_insert(0) += 1;
    }
    let gates_pending = gate::list_pending().len();
    let listeners = crate::channel::listen::active_listeners();

    if json {
        let out = serde_json::json!({
            "build": BUILD,
            "gateway": { "port": crate::gateway::PORT, "up": gateway_up },
            "config": config,
            "queue": queue_by_state,
            "gates_pending": gates_pending,
            "listeners": listeners.iter().map(|(imp, pid, since, alive)| serde_json::json!({
                "imp": imp, "pid": pid, "since": since, "alive": alive,
            })).collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    let port = crate::gateway::PORT;
    println!("impyard {BUILD}");
    println!(
        "gateway    {}",
        if gateway_up {
            format!("up on :{port}")
        } else {
            format!("DOWN (nothing on :{port}) — run: impyard server start")
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
            format!("{gates_pending} PENDING — review: impyard server gates ls")
        }
    );
    if listeners.is_empty() {
        println!("listeners  none");
    } else {
        for (imp, pid, since, alive) in listeners {
            println!(
                "listener   {imp}: {} (pid {pid}, since {since})",
                if alive { "up" } else { "STALE LOCK — process gone" }
            );
        }
    }
    Ok(())
}
